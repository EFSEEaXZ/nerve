//! HTTP transports: Streamable HTTP (POST/GET/DELETE /mcp) and legacy SSE (GET /sse + POST /messages).

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};

use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

use super::dispatch::{self, Kind};
use super::{is_final_response, rpc_err};
use crate::server::HttpCtx;

const SESSION_HEADER: &str = "mcp-session-id";

fn session_header(headers: &HeaderMap) -> Option<String> {
    headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).map(|a| a.contains("text/event-stream")).unwrap_or(false)
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
        .map(|a| a.split(',').any(|t| t.trim().split(';').next() == Some("application/json")))
        .unwrap_or(false)
}

fn with_session(mut resp: Response, sid: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(sid) {
        resp.headers_mut().insert(SESSION_HEADER, v);
    }
    resp
}

fn json_resp(status: StatusCode, body: Value, sid: Option<&str>) -> Response {
    let resp = (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response();
    match sid {
        Some(s) => with_session(resp, s),
        None => resp,
    }
}

fn event(v: &Value) -> Event {
    Event::default().event("message").data(v.to_string())
}

fn sse_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

fn keepalive(h: &HttpCtx) -> KeepAlive {
    KeepAlive::new().interval(Duration::from_secs(h.app.cfg.exec.keepalive_secs.max(1)))
}

/// POST /mcp
pub async fn post_mcp(State(h): State<HttpCtx>, headers: HeaderMap, body: Bytes) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json_resp(StatusCode::BAD_REQUEST, rpc_err(&Value::Null, -32700, format!("parse error: {e}")), None),
    };
    let sid = session_header(&headers);

    // JSON-RPC batch (protocol 2025-03-26)
    if let Value::Array(batch) = msg {
        let sess = h.app.sessions.get_or_create(sid.as_deref());
        let mut out = Vec::new();
        for m in batch {
            if let Some(r) = dispatch::handle_message(&h.app, &sess, m, None).await {
                out.push(r);
            }
        }
        if out.is_empty() {
            return with_session(StatusCode::ACCEPTED.into_response(), &sess.id);
        }
        return json_resp(StatusCode::OK, Value::Array(out), Some(&sess.id));
    }

    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let sess = if method == "initialize" { h.app.sessions.create(None) } else { h.app.sessions.get_or_create(sid.as_deref()) };

    match dispatch::kind(&msg) {
        Kind::Request => {}
        Kind::Notification | Kind::Response => {
            dispatch::handle_message(&h.app, &sess, msg, None).await;
            return with_session(StatusCode::ACCEPTED.into_response(), &sess.id);
        }
        Kind::Invalid => {
            return json_resp(StatusCode::BAD_REQUEST, rpc_err(&Value::Null, -32600, "invalid request"), Some(&sess.id));
        }
    }

    // Tool calls stream over SSE: progress notifications + keep-alive pings keep tunnels/proxies from
    // cutting long-running calls. Everything else is a plain JSON response.
    if method == "tools/call" && accepts_sse(&headers) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let app = h.app.clone();
        let s2 = sess.clone();
        tokio::spawn(async move {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match AssertUnwindSafe(dispatch::handle_message(&app, &s2, msg, Some(tx.clone()))).catch_unwind().await {
                Ok(Some(resp)) => { let _ = tx.send(resp); }
                Ok(None) => {}
                Err(_) => {
                    let key = match &id { Value::String(s) => s.clone(), v => v.to_string() };
                    s2.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                    tracing::error!(session = %s2.id, request = %id, "streamed dispatch panicked");
                    let _ = tx.send(rpc_err(&id, -32603, "internal dispatch error"));
                }
            }
        });
        // Never re-execute a request when switching transports. Buffer the first event;
        // final results from quick calls use JSON, progress/slow calls stay streamed.
        let first = if accepts_json(&headers) && h.app.cfg.server.json_grace_ms > 0 {
            match tokio::time::timeout(Duration::from_millis(h.app.cfg.server.json_grace_ms.min(1000)), rx.recv()).await {
                Ok(Some(v)) if is_final_response(&v) => {
                    tracing::debug!(session = %sess.id, "quick tool response: JSON");
                    return json_resp(StatusCode::OK, v, Some(&sess.id));
                }
                Ok(v) => v,
                Err(_) => None,
            }
        } else { None };
        let stream = futures_util::stream::unfold((rx, first, false), |(mut rx, first, done)| async move {
            if done { return None; }
            let v = match first { Some(v) => v, None => rx.recv().await? };
            let fin = is_final_response(&v);
            Some((Ok::<Event, Infallible>(event(&v)), (rx, None, fin)))
        });
        let resp = Sse::new(stream).keep_alive(keepalive(&h)).into_response();
        return with_session(sse_headers(resp), &sess.id);
    }

    let resp = dispatch::handle_message(&h.app, &sess, msg, None).await.unwrap_or(Value::Null);
    json_resp(StatusCode::OK, resp, Some(&sess.id))
}

/// GET /mcp — server->client notification stream, or legacy SSE when no session header is sent.
pub async fn get_mcp(State(h): State<HttpCtx>, headers: HeaderMap) -> Response {
    if !accepts_sse(&headers) {
        return (StatusCode::METHOD_NOT_ALLOWED, "use POST with JSON-RPC, or GET with Accept: text/event-stream").into_response();
    }
    match session_header(&headers) {
        Some(sid) => {
            let sess = h.app.sessions.get_or_create(Some(&sid));
            let rx = sess.notify.subscribe();
            let stream = BroadcastStream::new(rx).filter_map(|r| r.ok().map(|v| Ok::<Event, Infallible>(event(&v))));
            let resp = Sse::new(stream).keep_alive(keepalive(&h)).into_response();
            with_session(sse_headers(resp), &sess.id)
        }
        None => legacy_sse(&h),
    }
}

/// DELETE /mcp — client ends its session.
pub async fn delete_mcp(State(h): State<HttpCtx>, headers: HeaderMap) -> Response {
    if let Some(sid) = session_header(&headers) {
        h.app.sessions.remove(&sid);
    }
    StatusCode::OK.into_response()
}

/// GET /sse — legacy (2024-11-05) SSE transport.
pub async fn get_sse(State(h): State<HttpCtx>) -> Response {
    legacy_sse(&h)
}

fn legacy_sse(h: &HttpCtx) -> Response {
    let sess = h.app.sessions.create(None);
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    *sess.legacy.lock().unwrap() = Some(tx);
    let endpoint = format!("/messages?sessionId={}", sess.id);
    let first = tokio_stream::once(Ok::<Event, Infallible>(Event::default().event("endpoint").data(endpoint)));
    let rest = UnboundedReceiverStream::new(rx).map(|v| Ok::<Event, Infallible>(event(&v)));
    let resp = Sse::new(first.chain(rest)).keep_alive(keepalive(h)).into_response();
    sse_headers(resp)
}

/// POST /messages?sessionId=... — legacy SSE transport input.
pub async fn post_messages(State(h): State<HttpCtx>, Query(q): Query<HashMap<String, String>>, body: Bytes) -> Response {
    let Some(sid) = q.get("sessionId").or_else(|| q.get("session_id")) else {
        return (StatusCode::BAD_REQUEST, "missing sessionId").into_response();
    };
    let Some(sess) = h.app.sessions.get(sid) else {
        return (StatusCode::NOT_FOUND, "unknown session; reconnect to /sse").into_response();
    };
    let Some(tx) = sess.legacy.lock().unwrap().clone() else {
        return (StatusCode::GONE, "session has no SSE stream").into_response();
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("parse error: {e}")).into_response(),
    };
    let msgs = match msg {
        Value::Array(a) => a,
        m => vec![m],
    };
    for m in msgs {
        let app = h.app.clone();
        let s = sess.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(r) = dispatch::handle_message(&app, &s, m, Some(tx.clone())).await {
                let _ = tx.send(r);
            }
        });
    }
    StatusCode::ACCEPTED.into_response()
}

fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    let h = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        h.rsplit_once(':').map(|(a, p)| if p.chars().all(|c| c.is_ascii_digit()) { a.to_string() } else { h.clone() }).unwrap_or(h.clone())
    };
    matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1") || h.ends_with(".localhost")
}

/// Local-listener guard: blocks browsers on foreign sites (Origin) and DNS rebinding (Host).
/// Without it, any web page you visit could POST to the unauthenticated local port.
pub async fn local_guard(State(h): State<HttpCtx>, req: Request, next: Next) -> Response {
    if req.method() != Method::OPTIONS {
        if let Some(host) = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()) {
            if !is_loopback_host(host) {
                tracing::warn!("local listener: rejected Host {host}");
                return (StatusCode::FORBIDDEN, "host not allowed").into_response();
            }
        }
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let allowed = h.app.cfg.server.allowed_origins.iter().any(|o| o.eq_ignore_ascii_case(origin))
                || url::Url::parse(origin).ok().and_then(|u| u.host_str().map(is_loopback_host)).unwrap_or(false);
            if !allowed {
                tracing::warn!("local listener: rejected Origin {origin}");
                return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
            }
        }
    }
    next.run(req).await
}

pub async fn root(State(h): State<HttpCtx>, headers: HeaderMap) -> Response {
    let browser = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    if browser && !h.public && h.app.cfg.ui.dashboard {
        return axum::response::Redirect::temporary("/ui").into_response();
    }
    json_resp(
        StatusCode::OK,
        json!({"name": "nerve", "version": env!("CARGO_PKG_VERSION"), "mcp": "/mcp", "sse": "/sse", "public": h.public}),
        None,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_json_only_when_offered() {
        let mut h = HeaderMap::new();
        assert!(!accepts_json(&h));
        h.insert(header::ACCEPT, HeaderValue::from_static("text/event-stream"));
        assert!(!accepts_json(&h));
        h.insert(header::ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));
        assert!(accepts_json(&h));
    }
}
