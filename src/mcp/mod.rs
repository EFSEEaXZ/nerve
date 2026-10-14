//! Hand-rolled MCP core: JSON-RPC helpers, tool framework, sessions.

pub mod dispatch;
pub mod transport;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::server::AppState;

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

// ---------------------------------------------------------------- JSON-RPC

pub fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn rpc_err(id: &Value, code: i64, msg: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg.into()}})
}

pub fn rpc_notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn is_final_response(v: &Value) -> bool {
    v.get("id").is_some() && (v.get("result").is_some() || v.get("error").is_some())
}

// ---------------------------------------------------------------- errors

pub trait OrErr<T> {
    fn ctx(self, what: impl std::fmt::Display) -> Result<T, String>;
}

impl<T, E: std::fmt::Display> OrErr<T> for Result<T, E> {
    fn ctx(self, what: impl std::fmt::Display) -> Result<T, String> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

pub fn need<T>(v: Option<T>, name: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("missing required parameter `{name}`"))
}

pub async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("worker failed: {e}"))?
}

// ---------------------------------------------------------------- tool output

pub struct ToolOutput {
    pub content: Vec<Value>,
    pub structured: Option<Value>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self { content: vec![json!({"type": "text", "text": s.into()})], structured: None, is_error: false }
    }
    pub fn error(s: impl Into<String>) -> Self {
        let mut o = Self::text(s);
        o.is_error = true;
        o
    }
    pub fn empty() -> Self {
        Self { content: vec![], structured: None, is_error: false }
    }
    pub fn push_text(&mut self, s: impl Into<String>) {
        self.content.push(json!({"type": "text", "text": s.into()}));
    }
    pub fn push_image(&mut self, data_b64: String, mime: &str) {
        self.content.push(json!({"type": "image", "data": data_b64, "mimeType": mime}));
    }
    pub fn with_structured(mut self, v: Value) -> Self {
        self.structured = Some(v);
        self
    }
    pub fn error_if(mut self, cond: bool) -> Self {
        self.is_error = self.is_error || cond;
        self
    }
    /// Keep large text responses below client/proxy parsing budgets, without losing output.
    pub fn bound_text(mut self, dir: &std::path::Path, limit: usize) -> Self {
        let size: usize = self.content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str())).map(str::len).sum();
        let structured_size = self.structured.as_ref().map(|v| v.to_string().len()).unwrap_or(0);
        if limit == 0 || size + structured_size <= limit { return self; }
        let path = dir.join(format!("result-{}.json", crate::util::rand_token(8)));
        let full = json!({"content": self.content, "structuredContent": self.structured, "isError": self.is_error});
        if let Err(e) = crate::util::atomic_write(&path, full.to_string().as_bytes()) {
            return Self::error(format!("response too large; could not save full result: {e}"));
        }
        let text = self.content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n");
        // UTF-8 safe; 4-byte characters also fit within the configured budget.
        let chars = limit.saturating_sub(1024) / 4;
        let head: String = text.chars().take(chars * 3 / 4).collect();
        let tail: String = text.chars().rev().take(chars / 4).collect::<Vec<_>>().into_iter().rev().collect();
        self.content.retain(|c| c.get("type").and_then(|t| t.as_str()) != Some("text"));
        self.content.insert(0, json!({"type": "text", "text": format!("{head}\n…[large response saved in full: {}; use fs read with max_bytes/offset, do not reload the entire result]…\n{tail}", path.display())}));
        self.structured = Some(json!({"offloaded": true, "full_result_path": path.display().to_string()}));
        tracing::info!(bytes = size + structured_size, path = %path.display(), "large tool text response offloaded");
        self
    }
    pub fn into_result(self, include_structured: bool) -> Value {
        let content = if self.content.is_empty() { vec![json!({"type": "text", "text": "ok"})] } else { self.content };
        let mut r = json!({"content": content, "isError": self.is_error});
        if include_structured {
            if let Some(s) = self.structured {
                r["structuredContent"] = s;
            }
        }
        r
    }
}

// ---------------------------------------------------------------- progress

#[derive(Clone)]
pub struct Progress {
    token: Option<Value>,
    tx: Option<mpsc::UnboundedSender<Value>>,
    n: Arc<AtomicU64>,
}

impl Progress {
    pub fn new(token: Option<Value>, tx: Option<mpsc::UnboundedSender<Value>>) -> Self {
        Self { token, tx, n: Arc::new(AtomicU64::new(0)) }
    }
    pub fn enabled(&self) -> bool {
        self.token.is_some() && self.tx.is_some()
    }
    /// Send a progress notification (monotonic counter as progress value).
    pub fn send(&self, message: impl Into<String>, total: Option<f64>) {
        if let (Some(t), Some(tx)) = (&self.token, &self.tx) {
            let n = self.n.fetch_add(1, Ordering::Relaxed) + 1;
            let mut p = json!({"progressToken": t, "progress": n, "message": message.into()});
            if let Some(total) = total {
                p["total"] = json!(total);
            }
            let _ = tx.send(rpc_notification("notifications/progress", p));
        }
    }
    /// Send a progress notification with an explicit (increasing) value.
    pub fn send_value(&self, progress: f64, total: Option<f64>, message: impl Into<String>) {
        if let (Some(t), Some(tx)) = (&self.token, &self.tx) {
            let mut p = json!({"progressToken": t, "progress": progress, "message": message.into()});
            if let Some(total) = total {
                p["total"] = json!(total);
            }
            let _ = tx.send(rpc_notification("notifications/progress", p));
        }
    }
}

// ---------------------------------------------------------------- tools

#[derive(Clone)]
pub struct ToolCtx {
    pub app: AppState,
    pub progress: Progress,
    pub cancel: CancellationToken,
}

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolOutput> + Send>>;
type Handler = Arc<dyn Fn(Value, ToolCtx) -> ToolFuture + Send + Sync>;

pub struct ToolDef {
    pub name: String,
    pub json: Value,
    handler: Handler,
}

/// Build a typed tool: arguments are deserialized into `A`, whose JSON schema becomes `inputSchema`.
pub fn tool<A, F, Fut>(name: &str, title: &str, description: &str, annotations: Value, f: F) -> ToolDef
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    F: Fn(A, ToolCtx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolOutput, String>> + Send + 'static,
{
    let mut ann = annotations;
    if let Some(o) = ann.as_object_mut() {
        o.insert("title".into(), json!(title));
    }
    let json = json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema::<A>(),
        "annotations": ann,
    });
    let f = Arc::new(f);
    let handler: Handler = Arc::new(move |args: Value, ctx: ToolCtx| {
        let f = f.clone();
        Box::pin(async move {
            let args = if args.is_null() { json!({}) } else { args };
            match serde_json::from_value::<A>(args) {
                Ok(a) => match f(a, ctx).await {
                    Ok(o) => o,
                    Err(e) => ToolOutput::error(e),
                },
                Err(e) => ToolOutput::error(format!("invalid arguments: {e}")),
            }
        })
    });
    ToolDef { name: name.to_string(), json, handler }
}

/// Generate a compact, maximally client-compatible JSON schema (flat object, inlined, no null unions).
pub fn input_schema<A: JsonSchema>() -> Value {
    let settings = schemars::generate::SchemaSettings::draft2020_12().with(|s| {
        s.inline_subschemas = true;
    });
    let schema = settings.into_generator().into_root_schema_for::<A>();
    let mut v = serde_json::to_value(&schema).unwrap_or_else(|_| json!({}));
    simplify_schema(&mut v);
    if let Some(o) = v.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
        o.remove("$defs");
        o.remove("definitions");
        o.remove("description");
        o.insert("type".into(), json!("object"));
        o.entry("properties").or_insert_with(|| json!({}));
    }
    v
}

fn simplify_schema(v: &mut Value) {
    let Value::Object(o) = v else { return };
    // type: ["x", "null"] -> "x"
    let new_type = match o.get("type") {
        Some(Value::Array(types)) => {
            let nn: Vec<Value> = types.iter().filter(|t| t.as_str() != Some("null")).cloned().collect();
            if nn.len() == 1 { Some(nn[0].clone()) } else { None }
        }
        _ => None,
    };
    if let Some(t) = new_type {
        o.insert("type".into(), t);
    }
    // anyOf/oneOf [X, {type: null}] -> X
    for key in ["anyOf", "oneOf"] {
        let merged = match o.get(key) {
            Some(Value::Array(arr)) if arr.len() == 2 => {
                let nn: Vec<Value> =
                    arr.iter().filter(|s| s.get("type").and_then(|t| t.as_str()) != Some("null")).cloned().collect();
                if nn.len() == 1 { Some(nn[0].clone()) } else { None }
            }
            _ => None,
        };
        if let Some(Value::Object(inner)) = merged {
            o.remove(key);
            for (k, val) in inner {
                o.entry(k).or_insert(val);
            }
        }
    }
    o.remove("format");
    if o.get("default") == Some(&Value::Null) {
        o.remove("default");
    }
    if let Some(Value::Number(n)) = o.get("minimum") {
        if n.as_f64() == Some(0.0) {
            o.remove("minimum");
        }
    }
    if let Some(Value::Number(n)) = o.get("maximum") {
        if matches!(n.as_f64(), Some(255.0) | Some(65535.0) | Some(4294967295.0)) {
            o.remove("maximum");
        }
    }
    if let Some(Value::Object(props)) = o.get_mut("properties") {
        for (_, s) in props.iter_mut() {
            simplify_schema(s);
        }
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(s) = o.get_mut(key) {
            simplify_schema(s);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(a)) = o.get_mut(key) {
            for s in a {
                simplify_schema(s);
            }
        }
    }
}

pub struct Registry {
    tools: Vec<ToolDef>,
    index: HashMap<String, usize>,
    list: Arc<Value>,
}

impl Registry {
    pub fn new(tools: Vec<ToolDef>) -> Self {
        let index = tools.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        let list = Arc::new(json!({"tools": tools.iter().map(|t| t.json.clone()).collect::<Vec<_>>()}));
        Self { tools, index, list }
    }
    pub fn list(&self) -> Arc<Value> {
        self.list.clone()
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name.clone()).collect()
    }
    /// Run a tool isolated in its own task: a panic becomes an error result, never a crash.
    pub async fn call(&self, name: &str, args: Value, ctx: ToolCtx) -> Option<ToolOutput> {
        let i = *self.index.get(name)?;
        let fut = (self.tools[i].handler)(args, ctx);
        Some(match tokio::spawn(fut).await {
            Ok(o) => o,
            Err(e) if e.is_panic() => {
                let p = e.into_panic();
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                tracing::error!(tool = name, "tool panicked: {msg}");
                ToolOutput::error(format!("internal error (tool panicked): {msg}"))
            }
            Err(_) => ToolOutput::error("tool task was cancelled"),
        })
    }
}

// ---------------------------------------------------------------- sessions

pub struct Session {
    pub id: String,
    pub created: u64,
    pub last_seen: AtomicU64,
    pub protocol: Mutex<String>,
    pub client: Mutex<String>,
    pub inflight: Mutex<HashMap<String, CancellationToken>>,
    pub notify: broadcast::Sender<Value>,
    pub legacy: Mutex<Option<mpsc::UnboundedSender<Value>>>,
}

impl Session {
    fn new(id: String) -> Self {
        let now = crate::util::now_ms();
        Self {
            id,
            created: now,
            last_seen: AtomicU64::new(now),
            protocol: Mutex::new(PROTOCOL_VERSIONS[0].into()),
            client: Mutex::new(String::new()),
            inflight: Mutex::new(HashMap::new()),
            notify: broadcast::channel(64).0,
            legacy: Mutex::new(None),
        }
    }
    pub fn touch(&self) {
        self.last_seen.store(crate::util::now_ms(), Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Arc<Session>>>,
}

impl Sessions {
    pub fn create(&self, id: Option<String>) -> Arc<Session> {
        let id = id.unwrap_or_else(|| crate::util::rand_token(18));
        let s = Arc::new(Session::new(id.clone()));
        self.map.lock().unwrap().insert(id, s.clone());
        s
    }
    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.map.lock().unwrap().get(id).cloned()
    }
    /// Lenient lookup: unknown ids (e.g. after a server restart/update) are silently re-created,
    /// so clients never see "session not found" and never have to reconnect.
    pub fn get_or_create(&self, id: Option<&str>) -> Arc<Session> {
        match id {
            Some(id) if !id.is_empty() => {
                if let Some(s) = self.get(id) {
                    return s;
                }
                self.create(Some(id.to_string()))
            }
            _ => self.create(None),
        }
    }
    pub fn remove(&self, id: &str) -> bool {
        self.map.lock().unwrap().remove(id).is_some()
    }
    pub fn count(&self) -> usize {
        self.map.lock().unwrap().len()
    }
    /// Send a notification to every session's server->client stream.
    pub fn broadcast(&self, msg: Value) {
        for s in self.map.lock().unwrap().values() {
            let _ = s.notify.send(msg.clone());
            if let Some(tx) = s.legacy.lock().unwrap().as_ref() {
                let _ = tx.send(msg.clone());
            }
        }
    }
    pub fn summary(&self) -> Vec<Value> {
        let m = self.map.lock().unwrap();
        let mut v: Vec<Value> = m
            .values()
            .map(|s| {
                json!({
                    "id": s.id,
                    "created": s.created,
                    "client": *s.client.lock().unwrap(),
                    "protocol": *s.protocol.lock().unwrap(),
                    "last_seen": s.last_seen.load(Ordering::Relaxed),
                    "inflight": s.inflight.lock().unwrap().len(),
                })
            })
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x["last_seen"].as_u64().unwrap_or(0)));
        v
    }
    /// Drop sessions idle for longer than `idle_ms` with nothing in flight.
    pub fn gc(&self, idle_ms: u64) -> usize {
        let now = crate::util::now_ms();
        let mut m = self.map.lock().unwrap();
        let before = m.len();
        m.retain(|_, s| {
            now.saturating_sub(s.last_seen.load(Ordering::Relaxed)) < idle_ms
                || !s.inflight.lock().unwrap().is_empty()
                || s.notify.receiver_count() > 0
                || s.legacy.lock().unwrap().as_ref().is_some_and(|tx| !tx.is_closed())
        });
        before - m.len()
    }
}
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn oversized_unicode_output_is_saved_losslessly() {
        let text = "界🙂".repeat(2000);
        let dir = std::env::temp_dir();
        let output = ToolOutput::text(text.clone()).bound_text(&dir, 2048);
        let path = output.structured.as_ref().unwrap()["full_result_path"].as_str().unwrap();
        let stored: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(stored["content"][0]["text"], text);
        assert!(output.content[0]["text"].as_str().unwrap().len() < 2048);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn final_response_excludes_progress() {
        assert!(is_final_response(&rpc_ok(&json!(1), json!({}))));
        assert!(is_final_response(&rpc_err(&json!(1), -32603, "error")));
        assert!(!is_final_response(&rpc_notification("notifications/progress", json!({}))));
    }
    #[test]
    fn gc_drops_idle_but_keeps_inflight_and_streams() {
        let sessions = Sessions::default();
        let idle = sessions.create(None);
        let busy = sessions.create(None);
        let streamed = sessions.create(None);
        for s in [&idle, &busy, &streamed] { s.last_seen.store(0, Ordering::Relaxed); }
        busy.inflight.lock().unwrap().insert("1".into(), CancellationToken::new());
        let rx = streamed.notify.subscribe();
        assert_eq!(sessions.gc(1000), 1);
        assert!(sessions.get(&idle.id).is_none());
        assert!(sessions.get(&busy.id).is_some());
        assert!(sessions.get(&streamed.id).is_some());
        drop(rx);
        busy.inflight.lock().unwrap().clear();
        assert_eq!(sessions.gc(1000), 2);
    }
    #[test]
    fn gc_keeps_legacy_until_receiver_closes() {
        let sessions = Sessions::default();
        let s = sessions.create(None);
        s.last_seen.store(0, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        *s.legacy.lock().unwrap() = Some(tx);
        assert_eq!(sessions.gc(1000), 0);
        drop(rx);
        assert_eq!(sessions.gc(1000), 1);
    }
}
