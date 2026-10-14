//! Transport-agnostic MCP method dispatch.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{PROTOCOL_VERSIONS, Progress, Session, ToolCtx, rpc_err, rpc_ok};
use crate::server::AppState;

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Request,
    Notification,
    Response,
    Invalid,
}

pub fn kind(msg: &Value) -> Kind {
    let has_method = msg.get("method").and_then(|m| m.as_str()).is_some();
    let has_id = msg.get("id").map(|i| !i.is_null()).unwrap_or(false);
    match (has_method, has_id) {
        (true, true) => Kind::Request,
        (true, false) => Kind::Notification,
        (false, true) => Kind::Response,
        _ => Kind::Invalid,
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn instructions(app: &AppState) -> String {
    format!(
        "nerve {ver} on host `{host}` ({os}). Relative paths resolve against {dir}.\n\
         - exec: shell commands, argv, or inline code (python/node/...). timeout_ms 0 = wait forever (default). \
         background:true = detached job that survives server restarts -> use `job` (logs/wait/kill). \
         Long output is clipped inline but always saved to a file (path shown).\n\
         - fs: read/write/edit (exact replace)/list/find/grep and file ops.\n\
         - image: view images within the client's size budget; use region or tile for full detail.\n\
         - sys: system info, processes, kill, env, server status.  net: http requests/downloads, tcp checks.\n\
         - ui: desktop (screenshots, OCR, UI tree, mouse/keyboard).  browser: real Chrome over CDP.\n\
         - plugin: extra hot-reloaded tools from the plugins folder (e.g. blender, unreal); `plugin new` scaffolds one.",
        ver = env!("CARGO_PKG_VERSION"),
        host = app.hostname,
        os = std::env::consts::OS,
        dir = app.default_dir.display(),
    )
}

/// Handle one JSON-RPC message. Returns a response for requests, `None` otherwise.
/// `out` receives progress notifications (only used for streamed responses).
pub async fn handle_message(
    app: &AppState,
    sess: &Arc<Session>,
    msg: Value,
    out: Option<mpsc::UnboundedSender<Value>>,
) -> Option<Value> {
    sess.touch();
    match kind(&msg) {
        Kind::Request => {}
        Kind::Notification => {
            handle_notification(sess, &msg);
            return None;
        }
        Kind::Response => return None,
        Kind::Invalid => return Some(rpc_err(&Value::Null, -32600, "invalid request")),
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSIONS[0]);
            let version = if PROTOCOL_VERSIONS.contains(&requested) { requested } else { PROTOCOL_VERSIONS[0] };
            *sess.protocol.lock().unwrap() = version.to_string();
            let ci = params.get("clientInfo");
            let client = format!(
                "{} {}",
                ci.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("?"),
                ci.and_then(|c| c.get("version")).and_then(|v| v.as_str()).unwrap_or("")
            )
            .trim()
            .to_string();
            tracing::info!(session = %sess.id, protocol = version, "initialize from {client}");
            *sess.client.lock().unwrap() = client;
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {
                    "tools": {"listChanged": true},
                    "logging": {},
                },
                "serverInfo": {
                    "name": "nerve",
                    "title": "nerve",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": instructions(app),
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            let mut l = (*app.registry.list()).clone();
            let extra = app.plugins.tools();
            if !extra.is_empty() {
                if let Some(arr) = l.get_mut("tools").and_then(|t| t.as_array_mut()) {
                    arr.extend(extra.iter().cloned());
                }
            }
            Ok(l)
        }
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let token = params.get("_meta").and_then(|m| m.get("progressToken")).cloned();
            let cancel = CancellationToken::new();
            let key = id_key(&id);
            sess.inflight.lock().unwrap().insert(key.clone(), cancel.clone());
            let ctx = ToolCtx { app: app.clone(), progress: Progress::new(token, out), cancel };
            let t0 = Instant::now();
            let res = if app.plugins.has(&name) { app.plugins.call(&name, args, ctx).await } else { app.registry.call(&name, args, ctx).await };
            sess.inflight.lock().unwrap().remove(&key);
            match res {
                None => Err((-32602, format!("unknown tool: {name}"))),
                Some(o) => {
                    sess.touch();
                    let ms = t0.elapsed().as_millis();
                    if o.is_error {
                        tracing::info!(session = %sess.id, request = %id, tool = %name, ms, "call -> error");
                    } else {
                        tracing::info!(session = %sess.id, request = %id, tool = %name, ms, "call -> ok");
                    }
                    Ok(o.bound_text(&app.paths.out, app.cfg.output.max_text_bytes).into_result(app.cfg.output.structured))
                }
            }
        }
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        "logging/setLevel" => Ok(json!({})),
        "completion/complete" => Ok(json!({"completion": {"values": [], "hasMore": false}})),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => rpc_ok(&id, r),
        Err((code, msg)) => rpc_err(&id, code, msg),
    })
}

fn handle_notification(sess: &Arc<Session>, msg: &Value) {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if method == "notifications/cancelled" {
        if let Some(rid) = msg.get("params").and_then(|p| p.get("requestId")) {
            if let Some(tok) = sess.inflight.lock().unwrap().get(&id_key(rid)) {
                tracing::info!(session = %sess.id, "request {} cancelled by client", id_key(rid));
                tok.cancel();
            }
        }
    }
}