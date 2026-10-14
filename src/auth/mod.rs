//! Auth: persistent store (hashed tokens, argon2 PIN), bearer middleware, issuer detection.

pub mod oauth;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Paths;
use crate::server::{App, HttpCtx};
use crate::util::{atomic_write, now_ms, now_secs, rand_token, sha256_hex};

#[derive(Serialize, Deserialize, Clone)]
pub struct Client {
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub secret_hash: Option<String>,
    pub created: u64,
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TokenRec {
    pub client_id: String,
    pub exp: u64,
    #[serde(default)]
    pub created: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub created: u64,
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Data {
    #[serde(default)]
    pin_hash: Option<String>,
    #[serde(default)]
    clients: HashMap<String, Client>,
    /// sha256(token) -> record
    #[serde(default)]
    access: HashMap<String, TokenRec>,
    #[serde(default)]
    refresh: HashMap<String, TokenRec>,
    /// sha256(token) -> record
    #[serde(default)]
    api_tokens: HashMap<String, ApiToken>,
}

pub struct CodeRec {
    pub client_id: String,
    pub redirect_uri: String,
    pub challenge: Option<String>,
    pub method: String,
    pub exp: u64,
}

pub enum PinError {
    NoPin,
    Locked(u64),
    Wrong(u32),
}

pub struct Store {
    path: PathBuf,
    data: Mutex<Data>,
    mtime: Mutex<Option<SystemTime>>,
    last_check: AtomicU64,
    dirty: AtomicBool,
    codes: Mutex<HashMap<String, CodeRec>>,
    /// (consecutive failures, locked until secs)
    fails: Mutex<(u32, u64)>,
}

fn hash_secret(s: &str) -> String {
    sha256_hex(s.as_bytes())
}

impl Store {
    pub fn open(paths: &Paths) -> anyhow::Result<Self> {
        let path = paths.data.join("auth.json");
        let data: Data = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(_) => Data::default(),
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Ok(Self {
            path,
            data: Mutex::new(data),
            mtime: Mutex::new(mtime),
            last_check: AtomicU64::new(now_ms()),
            dirty: AtomicBool::new(false),
            codes: Mutex::new(HashMap::new()),
            fails: Mutex::new((0, 0)),
        })
    }

    fn save(&self, d: &Data) -> std::io::Result<()> {
        atomic_write(&self.path, serde_json::to_string_pretty(d).unwrap_or_default().as_bytes())?;
        *self.mtime.lock().unwrap() = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut Data) -> T) -> std::io::Result<T> {
        self.reload_if_changed();
        let mut d = self.data.lock().unwrap();
        let r = f(&mut d);
        self.save(&d)?;
        Ok(r)
    }

    /// Pick up edits made by the CLI (`nerve pin set`, `nerve token create`) while the server runs.
    pub fn reload_if_changed(&self) {
        let now = now_ms();
        if now.saturating_sub(self.last_check.load(Ordering::Relaxed)) < 1000 {
            return;
        }
        self.last_check.store(now, Ordering::Relaxed);
        let cur = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        let known = *self.mtime.lock().unwrap();
        if cur.is_some() && cur != known {
            if let Ok(b) = std::fs::read(&self.path) {
                if let Ok(d) = serde_json::from_slice::<Data>(&b) {
                    *self.data.lock().unwrap() = d;
                    *self.mtime.lock().unwrap() = cur;
                    tracing::info!("auth store reloaded from disk");
                }
            }
        }
    }

    /// Persist in-memory `last_used` updates (called periodically).
    pub fn flush(&self) {
        if self.dirty.load(Ordering::Relaxed) {
            let d = self.data.lock().unwrap().clone();
            let _ = self.save(&d);
        }
    }

    pub fn gc(&self) {
        let t = now_secs();
        self.codes.lock().unwrap().retain(|_, c| c.exp > t);
        let mut d = self.data.lock().unwrap();
        let before = d.access.len() + d.refresh.len();
        d.access.retain(|_, r| r.exp > t);
        d.refresh.retain(|_, r| r.exp > t);
        if d.access.len() + d.refresh.len() != before {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    // ------------------------------------------------------------ PIN

    pub fn has_pin(&self) -> bool {
        self.reload_if_changed();
        self.data.lock().unwrap().pin_hash.is_some()
    }

    pub fn set_pin(&self, pin: Option<&str>) -> anyhow::Result<()> {
        let hash = match pin {
            Some(p) => {
                let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
                Some(Argon2::default().hash_password(p.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?.to_string())
            }
            None => None,
        };
        self.mutate(|d| d.pin_hash = hash)?;
        Ok(())
    }

    pub fn lockout_remaining(&self) -> u64 {
        let f = self.fails.lock().unwrap();
        f.1.saturating_sub(now_secs())
    }

    /// Verify the PIN with global lockout after `max_failures` wrong attempts. CPU-heavy: call from a blocking task.
    pub fn verify_pin(&self, pin: &str, max_failures: u32, lockout_secs: u64) -> Result<(), PinError> {
        let locked = self.lockout_remaining();
        if locked > 0 {
            return Err(PinError::Locked(locked));
        }
        self.reload_if_changed();
        let Some(hash) = self.data.lock().unwrap().pin_hash.clone() else {
            return Err(PinError::NoPin);
        };
        let ok = PasswordHash::new(&hash).map(|h| Argon2::default().verify_password(pin.as_bytes(), &h).is_ok()).unwrap_or(false);
        let mut f = self.fails.lock().unwrap();
        if ok {
            *f = (0, 0);
            Ok(())
        } else {
            f.0 += 1;
            if f.0 >= max_failures.max(1) {
                f.1 = now_secs() + lockout_secs;
                f.0 = 0;
                tracing::warn!("oauth: too many wrong PINs — locked for {lockout_secs}s");
                Err(PinError::Locked(lockout_secs))
            } else {
                Err(PinError::Wrong(max_failures.max(1) - f.0))
            }
        }
    }

    // ------------------------------------------------------------ clients & codes

    pub fn register_client(&self, name: &str, redirect_uris: Vec<String>, with_secret: bool) -> std::io::Result<(Client, Option<String>)> {
        let secret = with_secret.then(|| format!("nrvs_{}", rand_token(24)));
        let c = Client {
            client_id: format!("nrvc_{}", rand_token(12)),
            name: name.chars().take(80).collect(),
            redirect_uris,
            secret_hash: secret.as_deref().map(hash_secret),
            created: now_secs(),
            last_used: 0,
        };
        let c2 = c.clone();
        self.mutate(move |d| {
            d.clients.insert(c2.client_id.clone(), c2);
        })?;
        Ok((c, secret))
    }

    pub fn get_client(&self, id: &str) -> Option<Client> {
        self.reload_if_changed();
        self.data.lock().unwrap().clients.get(id).cloned()
    }

    pub fn client_secret_ok(&self, client: &Client, secret: Option<&str>) -> bool {
        match (&client.secret_hash, secret) {
            (Some(h), Some(s)) => crate::util::ct_eq(h, &hash_secret(s)),
            (Some(_), None) => true, // PKCE is the real proof; tolerate clients that omit the secret
            (None, _) => true,
        }
    }

    pub fn create_code(&self, rec: CodeRec) -> String {
        let code = rand_token(24);
        self.codes.lock().unwrap().insert(code.clone(), rec);
        code
    }

    pub fn take_code(&self, code: &str) -> Option<CodeRec> {
        let c = self.codes.lock().unwrap().remove(code)?;
        (c.exp > now_secs()).then_some(c)
    }

    // ------------------------------------------------------------ tokens

    pub fn issue_tokens(&self, client_id: &str, access_ttl: u64, refresh_ttl: u64) -> std::io::Result<(String, String)> {
        let access = format!("nrv_{}", rand_token(32));
        let refresh = format!("nrvr_{}", rand_token(32));
        let t = now_secs();
        let (a, r, cid) = (hash_secret(&access), hash_secret(&refresh), client_id.to_string());
        self.mutate(move |d| {
            d.access.insert(a, TokenRec { client_id: cid.clone(), exp: t + access_ttl, created: t });
            d.refresh.insert(r, TokenRec { client_id: cid.clone(), exp: t + refresh_ttl, created: t });
            if let Some(c) = d.clients.get_mut(&cid) {
                c.last_used = t;
            }
        })?;
        Ok((access, refresh))
    }

    /// Rotate a refresh token. Returns (access, refresh, client_id).
    pub fn refresh(&self, token: &str, access_ttl: u64, refresh_ttl: u64) -> std::io::Result<Option<(String, String, String)>> {
        let h = hash_secret(token);
        let rec = {
            self.reload_if_changed();
            let d = self.data.lock().unwrap();
            d.refresh.get(&h).cloned()
        };
        let Some(rec) = rec else { return Ok(None) };
        if rec.exp <= now_secs() || self.get_client(&rec.client_id).is_none() {
            return Ok(None);
        }
        self.mutate(|d| {
            d.refresh.remove(&h);
        })?;
        let (a, r) = self.issue_tokens(&rec.client_id, access_ttl, refresh_ttl)?;
        Ok(Some((a, r, rec.client_id)))
    }

    pub fn revoke_token(&self, token: &str) -> std::io::Result<()> {
        let h = hash_secret(token);
        self.mutate(|d| {
            d.access.remove(&h);
            d.refresh.remove(&h);
        })
    }

    /// Validate a bearer token (OAuth access token or static API token). Returns the principal name.
    pub fn check_bearer(&self, token: &str) -> Option<String> {
        self.reload_if_changed();
        let h = hash_secret(token);
        let t = now_secs();
        let mut d = self.data.lock().unwrap();
        if let Some(rec) = d.access.get(&h) {
            if rec.exp > t {
                let cid = rec.client_id.clone();
                return match d.clients.get_mut(&cid) {
                    Some(c) => {
                        if t.saturating_sub(c.last_used) > 60 {
                            c.last_used = t;
                            self.dirty.store(true, Ordering::Relaxed);
                        }
                        Some(c.name.clone())
                    }
                    None => None, // client revoked
                };
            }
            return None;
        }
        if let Some(k) = d.api_tokens.get_mut(&h) {
            if t.saturating_sub(k.last_used) > 60 {
                k.last_used = t;
                self.dirty.store(true, Ordering::Relaxed);
            }
            return Some(format!("token:{}", k.name));
        }
        None
    }

    // ------------------------------------------------------------ admin

    pub fn list_clients(&self) -> Vec<Client> {
        self.reload_if_changed();
        let mut v: Vec<Client> = self.data.lock().unwrap().clients.values().cloned().collect();
        v.sort_by_key(|c| c.created);
        v
    }

    pub fn revoke_client(&self, id: &str) -> std::io::Result<bool> {
        self.mutate(|d| {
            let found = d.clients.remove(id).is_some();
            d.access.retain(|_, r| r.client_id != id);
            d.refresh.retain(|_, r| r.client_id != id);
            found
        })
    }

    pub fn revoke_all_clients(&self) -> std::io::Result<usize> {
        self.mutate(|d| {
            let n = d.clients.len();
            d.clients.clear();
            d.access.clear();
            d.refresh.clear();
            n
        })
    }

    pub fn create_api_token(&self, name: &str) -> std::io::Result<(String, String)> {
        let token = format!("nrvk_{}", rand_token(32));
        let id = rand_token(6);
        let rec = ApiToken { id: id.clone(), name: name.to_string(), created: now_secs(), last_used: 0 };
        let h = hash_secret(&token);
        self.mutate(move |d| {
            d.api_tokens.insert(h, rec);
        })?;
        Ok((id, token))
    }

    pub fn list_api_tokens(&self) -> Vec<ApiToken> {
        self.reload_if_changed();
        self.data.lock().unwrap().api_tokens.values().cloned().collect()
    }

    pub fn revoke_api_token(&self, id: &str) -> std::io::Result<bool> {
        self.mutate(|d| {
            let before = d.api_tokens.len();
            d.api_tokens.retain(|_, t| t.id != id);
            before != d.api_tokens.len()
        })
    }

    pub fn counts(&self) -> (usize, usize) {
        let d = self.data.lock().unwrap();
        (d.clients.len(), d.api_tokens.len())
    }
}

// ---------------------------------------------------------------- HTTP helpers

fn hdr<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("").trim()).filter(|s| !s.is_empty())
}

/// Public base URL as seen by the client.
pub fn issuer(app: &App, headers: &HeaderMap) -> String {
    if !app.cfg.server.public_url.trim().is_empty() {
        return app.cfg.server.public_url.trim().trim_end_matches('/').to_string();
    }
    if let Some(host) = hdr(headers, "x-forwarded-host").or_else(|| hdr(headers, "host")) {
        let local = host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]");
        let proto = hdr(headers, "x-forwarded-proto").unwrap_or(if local { "http" } else { "https" });
        return format!("{proto}://{host}");
    }
    if let Some(u) = app.tunnel.url() {
        return u;
    }
    format!("http://{}", app.cfg.server.public_addr)
}

pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, tok) = v.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| tok.trim().to_string()).filter(|t| !t.is_empty())
}

fn unauthorized(issuer: &str, has_token: bool) -> Response {
    let mut challenge = format!("Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource\"");
    if has_token {
        challenge.push_str(", error=\"invalid_token\"");
    }
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        json!({"error": "unauthorized", "error_description": "OAuth (PIN) or API token required"}).to_string(),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// Require a valid bearer token on MCP endpoints.
pub async fn require_auth(State(h): State<HttpCtx>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let protected = (path == "/" && req.method() != axum::http::Method::GET) || path == "/mcp" || path.starts_with("/mcp/") || path == "/sse" || path.starts_with("/message");
    if !protected || req.method() == axum::http::Method::OPTIONS {
        return next.run(req).await;
    }
    let tok = bearer(req.headers());
    if let Some(t) = &tok {
        if h.app.store.check_bearer(t).is_some() {
            return next.run(req).await;
        }
    }
    tracing::debug!(path, has_token = tok.is_some(), "auth: rejected");
    unauthorized(&issuer(&h.app, req.headers()), tok.is_some())
}