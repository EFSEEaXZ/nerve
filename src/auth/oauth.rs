//! Built-in OAuth 2.1 authorization server (DCR + PKCE + PIN consent page).

use std::collections::HashMap;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::{CodeRec, PinError, issuer};
use crate::server::HttpCtx;
use crate::util::{b64url, html_escape, now_secs};

pub fn routes() -> Router<HttpCtx> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(protected_resource))
        .route("/.well-known/oauth-protected-resource/{*rest}", get(protected_resource))
        .route("/.well-known/oauth-authorization-server", get(as_metadata))
        .route("/.well-known/oauth-authorization-server/{*rest}", get(as_metadata))
        .route("/.well-known/openid-configuration", get(as_metadata))
        .route("/.well-known/openid-configuration/{*rest}", get(as_metadata))
        .route("/oauth/register", post(register))
        .route("/register", post(register))
        .route("/oauth/authorize", get(authorize_page).post(authorize_submit))
        .route("/authorize", get(authorize_page).post(authorize_submit))
        .route("/oauth/token", post(token))
        .route("/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

fn json_ok(status: StatusCode, v: Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], v.to_string()).into_response()
}

fn oauth_err(error: &str, desc: &str) -> Response {
    tracing::info!("oauth error: {error}: {desc}");
    json_ok(StatusCode::BAD_REQUEST, json!({"error": error, "error_description": desc}))
}

async fn protected_resource(State(h): State<HttpCtx>, headers: HeaderMap) -> Response {
    let iss = issuer(&h.app, &headers);
    json_ok(
        StatusCode::OK,
        json!({
            "resource": format!("{iss}/mcp"),
            "authorization_servers": [iss],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["mcp"],
            "resource_name": "nerve",
        }),
    )
}

async fn as_metadata(State(h): State<HttpCtx>, headers: HeaderMap) -> Response {
    let iss = issuer(&h.app, &headers);
    json_ok(
        StatusCode::OK,
        json!({
            "issuer": iss,
            "authorization_endpoint": format!("{iss}/oauth/authorize"),
            "token_endpoint": format!("{iss}/oauth/token"),
            "registration_endpoint": format!("{iss}/oauth/register"),
            "revocation_endpoint": format!("{iss}/oauth/revoke"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256", "plain"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
            "scopes_supported": ["mcp"],
        }),
    )
}

fn redirect_ok(uri: &str) -> bool {
    match url::Url::parse(uri) {
        Ok(u) => !matches!(u.scheme(), "javascript" | "data" | "file" | "vbscript"),
        Err(_) => false,
    }
}

async fn register(State(h): State<HttpCtx>, headers: HeaderMap, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let redirect_uris: Vec<String> = v
        .get("redirect_uris")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if redirect_uris.is_empty() || !redirect_uris.iter().all(|u| redirect_ok(u)) {
        return oauth_err("invalid_redirect_uri", "redirect_uris must be absolute URIs");
    }
    let name = v.get("client_name").and_then(|x| x.as_str()).unwrap_or("MCP client").to_string();
    let method = v.get("token_endpoint_auth_method").and_then(|x| x.as_str()).unwrap_or("none").to_string();
    let with_secret = method != "none";
    let (c, secret) = match h.app.store.register_client(&name, redirect_uris, with_secret) {
        Ok(x) => x,
        Err(e) => return oauth_err("server_error", &e.to_string()),
    };
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    tracing::info!("oauth: registered client `{}` ({}) ua={ua}", c.name, c.client_id);
    let mut resp = json!({
        "client_id": c.client_id,
        "client_id_issued_at": c.created,
        "client_name": c.name,
        "redirect_uris": c.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
        "scope": "mcp",
    });
    if let Some(s) = secret {
        resp["client_secret"] = json!(s);
        resp["client_secret_expires_at"] = json!(0);
    }
    json_ok(StatusCode::CREATED, resp)
}

/// Exact match, except loopback redirects may use any port (RFC 8252 §7.3).
fn redirect_matches(registered: &[String], given: &str) -> bool {
    if registered.iter().any(|r| r == given) {
        return true;
    }
    let Ok(g) = url::Url::parse(given) else { return false };
    let loopback = |u: &url::Url| matches!(u.host_str(), Some("127.0.0.1") | Some("localhost") | Some("[::1]"));
    if !loopback(&g) {
        return false;
    }
    registered.iter().filter_map(|r| url::Url::parse(r).ok()).any(|r| {
        loopback(&r) && r.scheme() == g.scheme() && r.host_str() == g.host_str() && r.path() == g.path()
    })
}

struct AuthReq {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    state: String,
    challenge: String,
    method: String,
}

fn validate(h: &HttpCtx, p: &HashMap<String, String>) -> Result<AuthReq, String> {
    let client_id = p.get("client_id").cloned().unwrap_or_default();
    let client = h.app.store.get_client(&client_id).ok_or("Неизвестный клиент. Удалите и заново добавьте подключение в приложении.")?;
    let redirect_uri = match p.get("redirect_uri").filter(|s| !s.is_empty()) {
        Some(r) => r.clone(),
        None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
        None => return Err("Не указан redirect_uri.".into()),
    };
    if !redirect_matches(&client.redirect_uris, &redirect_uri) {
        return Err("redirect_uri не совпадает с зарегистрированным.".into());
    }
    if let Some(rt) = p.get("response_type") {
        if rt != "code" {
            return Err("Поддерживается только response_type=code.".into());
        }
    }
    Ok(AuthReq {
        client_id,
        client_name: client.name,
        redirect_uri,
        state: p.get("state").cloned().unwrap_or_default(),
        challenge: p.get("code_challenge").cloned().unwrap_or_default(),
        method: p.get("code_challenge_method").cloned().unwrap_or_else(|| "plain".into()),
    })
}

fn page(title: &str, inner: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title><style>
*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#0e0f13;color:#e8e8ea;font:15px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}}
.card{{width:min(420px,92vw);background:#17181d;border:1px solid #2a2c33;border-radius:16px;padding:28px;box-shadow:0 10px 40px #0008}}
h1{{font-size:20px;margin:0 0 6px}}.logo{{font-size:28px}}.muted{{color:#9a9ca5;font-size:13px}}.row{{margin:14px 0}}
.kv{{background:#101116;border:1px solid #24262d;border-radius:10px;padding:10px 12px;font-size:13px}}.kv b{{color:#fff}}
input[type=password]{{width:100%;padding:12px 14px;border-radius:10px;border:1px solid #34363f;background:#0f1014;color:#fff;font-size:18px;letter-spacing:2px}}
input:focus{{outline:none;border-color:#7c6cff}}.btns{{display:flex;gap:10px;margin-top:16px}}
button{{flex:1;padding:12px;border-radius:10px;border:0;font-size:15px;cursor:pointer}}.ok{{background:#7c6cff;color:#fff}}.no{{background:#262830;color:#ddd}}
.err{{background:#3a1d22;border:1px solid #6b2a33;color:#ffb4bd;border-radius:10px;padding:10px 12px;margin:12px 0;font-size:14px}}
</style></head><body><div class="card"><div class="logo">⚡</div>{inner}</div></body></html>"#
    ))
}

fn render_form(h: &HttpCtx, r: &AuthReq, error: Option<&str>) -> Html<String> {
    let host = url::Url::parse(&r.redirect_uri).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| r.redirect_uri.clone());
    let locked = h.app.store.lockout_remaining();
    let hidden = [
        ("client_id", &r.client_id),
        ("redirect_uri", &r.redirect_uri),
        ("state", &r.state),
        ("code_challenge", &r.challenge),
        ("code_challenge_method", &r.method),
    ]
    .iter()
    .map(|(k, v)| format!(r#"<input type="hidden" name="{k}" value="{}">"#, html_escape(v)))
    .collect::<String>();
    let err = error.map(|e| format!(r#"<div class="err">{}</div>"#, html_escape(e))).unwrap_or_default();
    let body = if !h.app.store.has_pin() {
        format!(
            r#"<h1>PIN не задан</h1><p class="muted">Удалённый вход отключён, пока на ПК не задан PIN.</p>
<div class="kv">На компьютере <b>{}</b> выполните:<br><code>nerve pin set</code></div>"#,
            html_escape(&h.app.hostname)
        )
    } else if locked > 0 {
        format!(r#"<h1>Вход временно заблокирован</h1><p class="muted">Слишком много неверных PIN. Попробуйте через {} мин.</p>"#, locked.div_ceil(60))
    } else {
        format!(
            r#"<h1>Доступ к компьютеру</h1><p class="muted">Приложение запрашивает полный доступ к nerve.</p>
<div class="kv">Приложение: <b>{name}</b><br>Вернёт на: <b>{host}</b><br>Компьютер: <b>{pc}</b></div>{err}
<form method="post" autocomplete="on">{hidden}<div class="row"><input type="password" name="pin" placeholder="PIN" autofocus autocomplete="current-password" required></div>
<div class="btns"><button class="no" name="action" value="deny" formnovalidate>Отклонить</button><button class="ok" name="action" value="allow">Разрешить</button></div></form>"#,
            name = html_escape(&r.client_name),
            host = html_escape(&host),
            pc = html_escape(&h.app.hostname),
        )
    };
    page("nerve — вход", &body)
}

async fn authorize_page(State(h): State<HttpCtx>, Query(p): Query<HashMap<String, String>>) -> Response {
    match validate(&h, &p) {
        Ok(r) => render_form(&h, &r, None).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, page("nerve — ошибка", &format!("<h1>Ошибка</h1><div class=\"err\">{}</div>", html_escape(&e)))).into_response(),
    }
}

fn redirect_with(uri: &str, params: &[(&str, &str)]) -> Response {
    match url::Url::parse(uri) {
        Ok(mut u) => {
            {
                let mut q = u.query_pairs_mut();
                for (k, v) in params {
                    if !v.is_empty() {
                        q.append_pair(k, v);
                    }
                }
            }
            Redirect::to(u.as_str()).into_response()
        }
        Err(_) => (StatusCode::BAD_REQUEST, "bad redirect_uri").into_response(),
    }
}

async fn authorize_submit(State(h): State<HttpCtx>, body: Bytes) -> Response {
    let p: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let r = match validate(&h, &p) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, page("nerve — ошибка", &format!("<h1>Ошибка</h1><div class=\"err\">{}</div>", html_escape(&e)))).into_response(),
    };
    if p.get("action").map(String::as_str) == Some("deny") {
        tracing::info!("oauth: user denied `{}`", r.client_name);
        return redirect_with(&r.redirect_uri, &[("error", "access_denied"), ("state", &r.state)]);
    }
    let pin = p.get("pin").cloned().unwrap_or_default();
    let app = h.app.clone();
    let (maxf, lock) = (app.cfg.auth.max_pin_failures, app.cfg.auth.lockout_secs);
    let res = tokio::task::spawn_blocking(move || app.store.verify_pin(&pin, maxf, lock)).await;
    match res {
        Ok(Ok(())) => {
            let code = h.app.store.create_code(CodeRec {
                client_id: r.client_id.clone(),
                redirect_uri: r.redirect_uri.clone(),
                challenge: (!r.challenge.is_empty()).then(|| r.challenge.clone()),
                method: r.method.clone(),
                exp: now_secs() + 600,
            });
            tracing::info!("oauth: PIN accepted, authorized `{}`", r.client_name);
            redirect_with(&r.redirect_uri, &[("code", &code), ("state", &r.state)])
        }
        Ok(Err(PinError::Wrong(left))) => {
            tracing::warn!("oauth: wrong PIN for `{}` ({left} attempts left)", r.client_name);
            render_form(&h, &r, Some(&format!("Неверный PIN. Осталось попыток: {left}"))).into_response()
        }
        Ok(Err(PinError::Locked(secs))) => {
            tracing::warn!("oauth: login attempt for `{}` while locked ({secs}s left)", r.client_name);
            render_form(&h, &r, None).into_response()
        }
        Ok(Err(PinError::NoPin)) => render_form(&h, &r, None).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn parse_params(headers: &HeaderMap, body: &Bytes) -> HashMap<String, String> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if ct.contains("json") {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) {
            return m.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
        }
    }
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, b) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = crate::util::b64_decode(b).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (id, secret) = s.split_once(':')?;
    let dec = |x: &str| url::form_urlencoded::parse(format!("a={x}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default();
    Some((dec(id), dec(secret)))
}

async fn token(State(h): State<HttpCtx>, headers: HeaderMap, body: Bytes) -> Response {
    let p = parse_params(&headers, &body);
    let (basic_id, basic_secret) = basic_auth(&headers).map(|(a, b)| (Some(a), Some(b))).unwrap_or((None, None));
    let client_id = p.get("client_id").cloned().or(basic_id);
    let secret = p.get("client_secret").cloned().or(basic_secret);
    let cfg = &h.app.cfg.auth;
    let grant = p.get("grant_type").map(String::as_str).unwrap_or("");
    match grant {
        "authorization_code" => {
            let code = p.get("code").cloned().unwrap_or_default();
            let Some(rec) = h.app.store.take_code(&code) else {
                return oauth_err("invalid_grant", "code is invalid, used or expired");
            };
            if let Some(cid) = &client_id {
                if cid != &rec.client_id {
                    return oauth_err("invalid_grant", "client_id mismatch");
                }
            }
            if let Some(ru) = p.get("redirect_uri") {
                if ru != &rec.redirect_uri {
                    return oauth_err("invalid_grant", "redirect_uri mismatch");
                }
            }
            let Some(client) = h.app.store.get_client(&rec.client_id) else {
                return oauth_err("invalid_client", "unknown client");
            };
            if !h.app.store.client_secret_ok(&client, secret.as_deref()) {
                return oauth_err("invalid_client", "bad client secret");
            }
            match &rec.challenge {
                Some(ch) => {
                    let verifier = p.get("code_verifier").cloned().unwrap_or_default();
                    let ok = if rec.method.eq_ignore_ascii_case("S256") {
                        use sha2::Digest;
                        b64url(&sha2::Sha256::digest(verifier.as_bytes())) == *ch
                    } else {
                        verifier == *ch
                    };
                    if !ok {
                        return oauth_err("invalid_grant", "PKCE verification failed");
                    }
                }
                None => {
                    if client.secret_hash.is_none() || secret.is_none() {
                        return oauth_err("invalid_request", "PKCE code_challenge is required");
                    }
                }
            }
            match h.app.store.issue_tokens(&rec.client_id, cfg.access_ttl_secs, cfg.refresh_ttl_secs) {
                Ok((a, r)) => {
                    tracing::info!("oauth: issued tokens to `{}`", client.name);
                    json_ok(
                        StatusCode::OK,
                        json!({"access_token": a, "token_type": "Bearer", "expires_in": cfg.access_ttl_secs, "refresh_token": r, "scope": "mcp"}),
                    )
                }
                Err(e) => oauth_err("server_error", &e.to_string()),
            }
        }
        "refresh_token" => {
            let rt = p.get("refresh_token").cloned().unwrap_or_default();
            match h.app.store.refresh(&rt, cfg.access_ttl_secs, cfg.refresh_ttl_secs) {
                Ok(Some((a, r, _))) => json_ok(
                    StatusCode::OK,
                    json!({"access_token": a, "token_type": "Bearer", "expires_in": cfg.access_ttl_secs, "refresh_token": r, "scope": "mcp"}),
                ),
                Ok(None) => oauth_err("invalid_grant", "refresh token is invalid or expired"),
                Err(e) => oauth_err("server_error", &e.to_string()),
            }
        }
        _ => oauth_err("unsupported_grant_type", "use authorization_code or refresh_token"),
    }
}

async fn revoke(State(h): State<HttpCtx>, headers: HeaderMap, body: Bytes) -> Response {
    let p = parse_params(&headers, &body);
    if let Some(t) = p.get("token") {
        let _ = h.app.store.revoke_token(t);
    }
    StatusCode::OK.into_response()
}