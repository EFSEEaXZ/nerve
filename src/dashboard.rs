//! Local web dashboard (http://127.0.0.1:8100/ui): status, sessions, jobs, tools, plugins, skills,
//! access (PIN / OAuth clients / API tokens), logs and config.
//! Mounted on the local listener only (loopback Host/Origin guard applies; never exposed publicly).

use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::control::Quit;
use crate::mcp::{Progress, ToolCtx};
use crate::server::HttpCtx;
use crate::util::now_ms;

pub fn routes() -> Router<HttpCtx> {
    Router::new()
        .route("/ui", get(page))
        .route("/assets/onest-latin.woff2", get(font_latin))
        .route("/assets/onest-cyrillic.woff2", get(font_cyrillic))
        .route("/api/state", get(state))
        .route("/api/log", get(log))
        .route("/api/action", post(action))
        .route("/api/tools", get(tools))
        .route("/api/plugin", get(plugin_info))
        .route("/api/config", get(config_text))
}

/// Plugin folder names are plain identifiers; anything else could escape the plugins dir.
fn plugin_dir(app: &crate::server::App, name: &str) -> Result<std::path::PathBuf, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!("bad plugin name `{name}`"));
    }
    let dir = app.plugins.dir.join(name);
    if !dir.join(crate::plugins::MANIFEST).is_file() {
        return Err(format!("no plugin `{name}`"));
    }
    Ok(dir)
}

/// Every tool the server exposes, with its schema, grouped by where it comes from.
async fn tools(State(h): State<HttpCtx>) -> Response {
    let app = &h.app;
    let builtin = app.registry.list().get("tools").cloned().unwrap_or_else(|| json!([]));
    let plugins = json!(*app.plugins.tools());
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(json!({"builtin": builtin, "plugins": plugins}))).into_response()
}

#[derive(Deserialize)]
struct NameQ {
    name: String,
    n: Option<usize>,
}

async fn plugin_info(State(h): State<HttpCtx>, Query(q): Query<NameQ>) -> Response {
    let app = &h.app;
    let dir = match plugin_dir(app, &q.name) {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    let manifest = std::fs::read_to_string(dir.join(crate::plugins::MANIFEST)).unwrap_or_default();
    let log = app.plugins.log_tail(&q.name, q.n.unwrap_or(200).clamp(1, 5000)).await.unwrap_or_default();
    let summary = app.plugins.summary().into_iter().find(|s| s["name"] == q.name.as_str()).unwrap_or(Value::Null);
    axum::Json(json!({"ok": true, "summary": summary, "manifest": manifest, "manifest_path": dir.join(crate::plugins::MANIFEST), "log": log}))
        .into_response()
}

async fn config_text(State(h): State<HttpCtx>) -> Response {
    let path = h.app.paths.config.clone();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(json!({"path": path, "text": text}))).into_response()
}

/// Forward a request to the `skills` plugin and hand its text answer back.
async fn skills_call(app: &crate::server::AppState, args: Value) -> Response {
    const ALLOWED: &[&str] = &["index", "source", "save", "edit", "attach", "delete", "install", "dir"];
    let act = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
    if !ALLOWED.contains(&act) {
        return fail(format!("skills action `{act}` is not available from the dashboard"));
    }
    let ctx = ToolCtx { app: app.clone(), progress: Progress::new(None, None), cancel: tokio_util::sync::CancellationToken::new() };
    let Some(out) = app.plugins.call("skills", args, ctx).await else {
        return fail("the skills plugin is not running (Plugins → skills)");
    };
    let text: Vec<&str> = out.content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str())).collect();
    let text = text.join("\n");
    if out.is_error {
        return fail(text);
    }
    axum::Json(json!({"ok": true, "message": text.lines().next().unwrap_or("done"), "text": text})).into_response()
}

async fn font_latin() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=86400")], include_bytes!("assets/onest-latin.woff2").as_slice())
}
async fn font_cyrillic() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=86400")], include_bytes!("assets/onest-cyrillic.woff2").as_slice())
}

async fn page() -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(include_str!("dashboard.html"))).into_response()
}

async fn state(State(h): State<HttpCtx>) -> Response {
    let app = &h.app;
    let mut jobs = app.jobs.list();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.started));
    let jobs: Vec<Value> = jobs
        .iter()
        .take(100)
        .map(|j| {
            json!({"id": j.id, "pid": j.pid, "label": j.label, "command": crate::util::one_line(&j.command, 160),
                   "status": j.status(), "running": j.running(), "started": j.started, "elapsed_ms": j.elapsed_ms()})
        })
        .collect();
    let clients: Vec<Value> = app
        .store
        .list_clients()
        .iter()
        .map(|c| json!({"id": c.client_id, "name": c.name, "redirect": c.redirect_uris.join(", "), "created": c.created, "last_used": c.last_used}))
        .collect();
    let tokens: Vec<Value> =
        app.store.list_api_tokens().iter().map(|t| json!({"id": t.id, "name": t.name, "created": t.created, "last_used": t.last_used})).collect();
    let body = json!({
        "now": now_ms(),
        "status": crate::server::status_json(app),
        "sessions": app.sessions.summary(),
        "jobs": jobs,
        "clients": clients,
        "tokens": tokens,
    });
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

#[derive(Deserialize)]
struct LogQ {
    kind: Option<String>,
    id: Option<String>,
    n: Option<usize>,
    grep: Option<String>,
}

fn latest_log(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("nerve")).unwrap_or(false))
        .collect();
    v.sort();
    v.pop()
}

async fn log(State(h): State<HttpCtx>, Query(q): Query<LogQ>) -> Response {
    let app = &h.app;
    let n = q.n.unwrap_or(200).clamp(1, 5000);
    let path = match q.kind.as_deref().unwrap_or("nerve") {
        "tunnel" => Some(app.paths.logs.join("tunnel.out")),
        "job" => q.id.as_deref().and_then(|id| app.jobs.get(id)).map(|j| j.stdout),
        "job_err" => q.id.as_deref().and_then(|id| app.jobs.get(id)).map(|j| j.stderr),
        _ => latest_log(&app.paths.logs),
    };
    let Some(path) = path else { return (StatusCode::NOT_FOUND, "no such log").into_response() };
    let grep = q.grep.filter(|g| !g.is_empty()).and_then(|g| regex::RegexBuilder::new(&g).case_insensitive(true).build().ok());
    let text = tokio::task::spawn_blocking(move || {
        let want = if grep.is_some() { 20_000 } else { n };
        let (t, _) = crate::jobs::tail_lines(&path, want, 2_000_000).unwrap_or_default();
        match grep {
            Some(re) => {
                let v: Vec<&str> = t.lines().filter(|l| re.is_match(l)).collect();
                v[v.len().saturating_sub(n)..].join("\n")
            }
            None => t,
        }
    })
    .await
    .unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], text).into_response()
}

#[derive(Deserialize)]
struct ActionReq {
    action: String,
    id: Option<String>,
    name: Option<String>,
    pin: Option<String>,
    path: Option<String>,
    text: Option<String>,
    args: Option<Value>,
}

fn ok(msg: impl Into<String>) -> Response {
    axum::Json(json!({"ok": true, "message": msg.into()})).into_response()
}
fn fail(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({"ok": false, "message": msg.into()}))).into_response()
}

async fn action(State(h): State<HttpCtx>, axum::Json(a): axum::Json<ActionReq>) -> Response {
    let app = h.app.clone();
    let id = a.id.clone().unwrap_or_default();
    tracing::info!("dashboard: action {} {}", a.action, id);
    match a.action.as_str() {
        "tunnel_restart" => {
            app.tunnel.request_restart();
            ok("tunnel restarting")
        }
        "restart" => {
            app.life.request(Quit::Restart, true, Duration::from_millis(300));
            ok("nerve is restarting")
        }
        "theme" => {
            let theme = a.text.unwrap_or_default();
            if !matches!(theme.as_str(), "dark" | "light") { return fail("unknown theme"); }
            match crate::util::atomic_write(&app.paths.data.join("ui-theme.txt"), theme.as_bytes()) {
                Ok(()) => ok("theme saved"), Err(e) => fail(e.to_string())
            }
        }
        "update_check" => {
            match app.updates.check(&app).await {
                Ok(_) => ok(app.updates.status(app.cfg.updates.automatic)["message"].as_str().unwrap_or("checked")),
                Err(e) => fail(e)
            }
        }
        "update" => {
            if a.path.is_some() { return fail("Updates come only from EFSEEaXZ/nerve releases; custom URLs are not accepted"); }
            if crate::updates::Updates::request(app) { ok("GitHub update queued; see Settings for progress") }
            else { ok("an update is already in progress") }
        }
        "job_kill" => {
            let Some(j) = app.jobs.get(&id) else { return fail("no such job") };
            if !j.running() {
                return ok(format!("job {id} already {}", j.status()));
            }
            let pid = j.pid;
            let _ = tokio::task::spawn_blocking(move || crate::util::kill_tree(pid, true)).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            if crate::util::pid_alive(pid) {
                return fail(format!("job {id} (pid {pid}) is still alive"));
            }
            app.jobs.update(&id, |j| {
                if j.finished.is_none() {
                    j.finished = Some(now_ms());
                    j.killed = true;
                }
            });
            ok(format!("killed job {id}"))
        }
        "jobs_clean" => {
            let done: Vec<String> = app.jobs.list().into_iter().filter(|m| !m.running()).map(|m| m.id).collect();
            for id in &done {
                app.jobs.remove(id, true);
            }
            ok(format!("removed {} finished jobs", done.len()))
        }
        "client_revoke" => match app.store.revoke_client(&id) {
            Ok(true) => ok("client revoked"),
            Ok(false) => fail("no such client"),
            Err(e) => fail(e.to_string()),
        },
        "clients_revoke_all" => match app.store.revoke_all_clients() {
            Ok(n) => ok(format!("revoked {n} clients")),
            Err(e) => fail(e.to_string()),
        },
        "token_create" => {
            let name = a.name.unwrap_or_default();
            if name.trim().is_empty() {
                return fail("token name is required");
            }
            match app.store.create_api_token(name.trim()) {
                Ok((id, token)) => axum::Json(json!({"ok": true, "message": format!("token {id} created — copy it now, it is shown once"), "token": token})).into_response(),
                Err(e) => fail(e.to_string()),
            }
        }
        "token_revoke" => match app.store.revoke_api_token(&id) {
            Ok(true) => ok("token revoked"),
            Ok(false) => fail("no such token"),
            Err(e) => fail(e.to_string()),
        },
        "pin_set" => {
            let pin = a.pin.unwrap_or_default();
            if pin.chars().count() < 4 {
                return fail("PIN must be at least 4 characters");
            }
            match app.store.set_pin(Some(&pin)) {
                Ok(()) => ok("PIN changed"),
                Err(e) => fail(e.to_string()),
            }
        }
        "plugin_enable" | "plugin_disable" => {
            let name = a.name.unwrap_or_default();
            if let Err(e) = plugin_dir(&app, &name) {
                return fail(e);
            }
            let on = a.action == "plugin_enable";
            match app.plugins.set_enabled(&name, on).await {
                Ok(()) => ok(format!("{name} {}", if on { "enabled" } else { "disabled" })),
                Err(e) => fail(e),
            }
        }
        "plugin_reload" => {
            let name = a.name.unwrap_or_default();
            if let Err(e) = plugin_dir(&app, &name) {
                return fail(e);
            }
            app.plugins.scan(Some(&name)).await;
            ok(format!("{name} reloaded"))
        }
        "plugin_save" => {
            let name = a.name.unwrap_or_default();
            let dir = match plugin_dir(&app, &name) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let text = a.text.unwrap_or_default();
            if let Err(e) = toml::from_str::<crate::plugins::Manifest>(&text) {
                return fail(format!("plugin.toml is not valid: {e}"));
            }
            match std::fs::write(dir.join(crate::plugins::MANIFEST), text) {
                // the watcher picks the change up and restarts the plugin within ~1 s
                Ok(()) => ok(format!("{name}: saved, reloading")),
                Err(e) => fail(e.to_string()),
            }
        }
        "config_save" => {
            let text = a.text.unwrap_or_default();
            if let Err(e) = toml::from_str::<crate::config::Config>(&text) {
                return fail(format!("config.toml is not valid: {e}"));
            }
            let path = app.paths.config.clone();
            let _ = std::fs::copy(&path, path.with_extension("toml.bak"));
            match std::fs::write(&path, text) {
                Ok(()) => ok("config saved — restart nerve to apply"),
                Err(e) => fail(e.to_string()),
            }
        }
        "plugin_new" => {
            // same scaffold as the `plugin {action:"new"}` tool
            let args = json!({"action": "new", "name": a.name.unwrap_or_default(), "description": a.text.unwrap_or_default()});
            let ctx = ToolCtx { app: app.clone(), progress: Progress::new(None, None), cancel: tokio_util::sync::CancellationToken::new() };
            match app.registry.call("plugin", args, ctx).await {
                Some(o) => {
                    let text: Vec<&str> = o.content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str())).collect();
                    let first = text.first().and_then(|t| t.lines().next()).unwrap_or("created").to_string();
                    if o.is_error { fail(text.join("\n")) } else { ok(first) }
                }
                None => fail("the plugin tool is not available"),
            }
        }
        "skills" => skills_call(&app, a.args.unwrap_or_else(|| json!({}))).await,
        other => fail(format!("unknown action {other}")),
    }
}
