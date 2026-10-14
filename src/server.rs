//! Server bootstrap: state, listeners (local + public), logging, maintenance, shutdown.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderName;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::prelude::*;

use crate::auth::{self, Store};
use crate::config::{Config, Paths};
use crate::control::{self, Lifecycle, Quit};
use crate::jobs::Jobs;
use crate::mcp::{Registry, Sessions, transport};
use crate::tunnel::Tunnel;

pub struct App {
    pub cfg: Config,
    pub paths: Paths,
    pub started: Instant,
    pub store: Store,
    pub sessions: Sessions,
    pub registry: Registry,
    pub plugins: Arc<crate::plugins::Plugins>,
    pub jobs: Jobs,
    pub tunnel: Tunnel,
    pub http: reqwest::Client,
    pub hostname: String,
    pub default_dir: PathBuf,
    pub life: Lifecycle,
    pub updates: crate::updates::Updates,
    pub control_key: String,
    pub supervised: bool,
    pub elevated: bool,
}

pub type AppState = Arc<App>;

#[derive(Clone)]
pub struct HttpCtx {
    pub app: AppState,
    pub public: bool,
}

fn init_logging(cfg: &Config, paths: &Paths) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("nerve")
        .filename_suffix("log")
        .max_log_files(cfg.log.keep_days.max(1))
        .build(&paths.logs)
        .context("init log file")?;
    let (nb, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_env("NERVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("{},hyper=warn,reqwest=warn,h2=warn", cfg.log.level)));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(nb).with_ansi(false))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    Ok(guard)
}

pub fn status_json(app: &App) -> Value {
    let (clients, tokens) = app.store.counts();
    json!({
        "ok": true,
        "name": "nerve",
        "version": env!("CARGO_PKG_VERSION"),
        "host": app.hostname,
        "os": std::env::consts::OS,
        "pid": std::process::id(),
        "uptime_ms": app.started.elapsed().as_millis() as u64,
        "tools": app.registry.names().into_iter().chain(app.plugins.names()).collect::<Vec<_>>(),
        "plugins": app.plugins.summary(),
        "sessions": app.sessions.count(),
        "jobs_running": app.jobs.running_count(),
        "local_addr": app.cfg.server.local_addr,
        "public_addr": app.cfg.server.public_addr,
        "public_url": app.tunnel.url().or_else(|| (!app.cfg.server.public_url.is_empty()).then(|| app.cfg.server.public_url.clone())),
        "tunnel": app.tunnel.status(),
        "auth": {"pin_set": app.store.has_pin(), "oauth_clients": clients, "api_tokens": tokens},
        "data_dir": app.paths.data,
        "default_dir": app.default_dir,
        "exe": control::current_exe(),
        "updates": app.updates.status(app.cfg.updates.automatic),
        "supervised": app.supervised,
        "elevated": app.elevated,
        "session": crate::util::session_id(),
    })
}

#[derive(serde::Deserialize)]
struct ControlReq {
    action: String,
    #[serde(default)]
    keep_tunnel: Option<bool>,
}

/// Supervisor -> server control channel (local listener only, needs `<data>/control.key`).
async fn control_endpoint(State(h): State<HttpCtx>, headers: axum::http::HeaderMap, axum::Json(req): axum::Json<ControlReq>) -> Response {
    let key = headers.get("x-nerve-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !crate::util::ct_eq(key, &h.app.control_key) {
        return (axum::http::StatusCode::FORBIDDEN, "bad control key").into_response();
    }
    let d = Duration::from_millis(150);
    match req.action.as_str() {
        "shutdown" | "stop" => h.app.life.request(Quit::Stop, req.keep_tunnel.unwrap_or(false), d),
        "restart" => h.app.life.request(Quit::Restart, req.keep_tunnel.unwrap_or(true), d),
        "update" => { crate::updates::Updates::request(h.app.clone()); }
        "ping" => {}
        other => return (axum::http::StatusCode::BAD_REQUEST, format!("unknown action {other}")).into_response(),
    }
    axum::Json(json!({"ok": true, "pid": std::process::id()})).into_response()
}

async fn health(State(h): State<HttpCtx>) -> Response {
    let body = if h.public { json!({"ok": true, "name": "nerve"}) } else { status_json(&h.app) };
    axum::Json(body).into_response()
}

fn router(h: HttpCtx) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([HeaderName::from_static("mcp-session-id"), HeaderName::from_static("www-authenticate")]);
    let mut r: Router<HttpCtx> = Router::new()
        .route("/mcp", post(transport::post_mcp).get(transport::get_mcp).delete(transport::delete_mcp))
        .route("/sse", get(transport::get_sse))
        .route("/messages", post(transport::post_messages))
        .route("/message", post(transport::post_messages))
        .route("/health", get(health))
        .route("/", get(transport::root).post(transport::post_mcp).delete(transport::delete_mcp))
        .merge(auth::oauth::routes());
    if !h.public {
        r = r.route("/_nerve/control", post(control_endpoint));
        if h.app.cfg.ui.dashboard {
            r = r.merge(crate::dashboard::routes());
        }
    }
    if h.public || h.app.cfg.auth.local_requires_auth {
        r = r.layer(axum::middleware::from_fn_with_state(h.clone(), auth::require_auth));
    }
    if !h.public {
        r = r.layer(axum::middleware::from_fn_with_state(h.clone(), transport::local_guard));
    }
    r.layer(DefaultBodyLimit::disable()).layer(cors).with_state(h)
}

async fn serve(addr: &str, h: HttpCtx, shutdown: CancellationToken) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let sa: SocketAddr = addr.parse().with_context(|| format!("bad listen address `{addr}`"))?;
    // retry for a while: during a restart/update the previous instance may still hold the port
    let mut tries = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(sa).await {
            Ok(l) => break l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 60 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(anyhow::anyhow!("cannot listen on {sa}: {e} (port busy?)")),
        }
    };
    let kind = if h.public { "public" } else { "local" };
    tracing::info!("{kind} listener on http://{sa}/mcp");
    let app = router(h);
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown.cancelled_owned()).await {
            tracing::error!("{kind} listener failed: {e}");
        }
    }))
}

/// Returns the process exit code (75 = "restart me" for the supervisor).
pub async fn run(cfg: Config, paths: Paths) -> anyhow::Result<i32> {
    let _guard = init_logging(&cfg, &paths)?;
    if let Some(dir) = control::current_exe().parent() {
        control::cleanup_old_binaries(dir);
    }
    let store = Store::open(&paths)?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("nerve/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;
    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "pc".into());
    let app = Arc::new(App {
        default_dir: cfg.default_dir(),
        tunnel: Tunnel::new(&cfg.tunnel.provider),
        jobs: Jobs::load(&paths.jobs),
        registry: crate::tools::registry(),
        plugins: Arc::new(crate::plugins::Plugins::new(&cfg, &paths)),
        sessions: Sessions::default(),
        started: Instant::now(),
        store,
        http,
        hostname,
        life: Lifecycle::default(),
        updates: crate::updates::Updates::default(),
        control_key: control::control_key(&paths),
        supervised: control::supervised(),
        elevated: crate::util::is_elevated(),
        cfg,
        paths,
    });
    let shutdown = CancellationToken::new();

    // plugins: load once before serving (so the first tools/list has them), then watch for changes
    app.plugins.attach(&app);
    if tokio::time::timeout(Duration::from_secs(20), app.plugins.scan(None)).await.is_err() {
        tracing::warn!("plugins still loading after 20 s; continuing in the background");
    }
    tokio::spawn(app.plugins.clone().watch(shutdown.clone()));

    let mut servers = vec![serve(&app.cfg.server.local_addr, HttpCtx { app: app.clone(), public: false }, shutdown.clone()).await?];
    let public_enabled = !app.cfg.server.public_addr.trim().is_empty();
    if public_enabled {
        servers.push(serve(&app.cfg.server.public_addr, HttpCtx { app: app.clone(), public: true }, shutdown.clone()).await?);
        if !app.store.has_pin() {
            tracing::warn!("PIN is not set: remote OAuth logins are disabled. Run `nerve pin set`.");
        }
    }
    tokio::spawn(crate::tunnel::run(app.clone(), shutdown.clone()));
    tokio::spawn(crate::updates::run(app.clone(), shutdown.clone()));

    // maintenance
    {
        let app = app.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            let mut n = 0u64;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shutdown.cancelled() => break,
                }
                n += 1;
                app.store.flush();
                if n % 20 == 0 {
                    app.store.gc();
                    let dropped = app.sessions.gc(3600 * 1000);
                    if dropped > 0 {
                        tracing::debug!("dropped {dropped} idle session(s)");
                    }
                }
            }
        });
    }

    #[cfg(windows)]
    if app.cfg.ui.tray && crate::util::session_id().unwrap_or(0) != 0 {
        crate::tray::start(app.clone(), tokio::runtime::Handle::current());
    }

    tracing::info!(
        "nerve {} ready: {} tools (+{} from plugins), pid {}, {}{}data {}",
        env!("CARGO_PKG_VERSION"),
        app.registry.len(),
        app.plugins.names().len(),
        std::process::id(),
        if app.supervised { "supervised, " } else { "" },
        if app.elevated { "elevated, " } else { "" },
        app.paths.data.display()
    );

    tokio::select! {
        _ = wait_for_signal() => app.life.request(Quit::Stop, false, Duration::ZERO),
        _ = app.life.token.cancelled() => {}
    }
    let reason = app.life.reason().unwrap_or(Quit::Stop);
    tracing::info!("shutting down ({reason:?}{})", if app.life.keep_tunnel() { ", keeping tunnel" } else { "" });
    shutdown.cancel();
    for s in servers {
        let _ = tokio::time::timeout(Duration::from_secs(5), s).await;
    }
    // give the tunnel task a moment to hand over / clean up
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !matches!(app.tunnel.status().state.as_str(), "stopped" | "handed over" | "disabled" | "idle")
        && !app.tunnel.status().state.starts_with("error")
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    app.store.flush();
    #[cfg(windows)]
    crate::tray::stop();
    if reason == Quit::Restart {
        if app.supervised {
            tracing::info!("exiting with code {} so the supervisor restarts us", control::RESTART_EXIT_CODE);
            return Ok(control::RESTART_EXIT_CODE);
        }
        let exe = app.life.successor().unwrap_or_else(control::current_exe);
        match control::spawn_successor(&exe, &app.paths) {
            Ok(pid) => tracing::info!("started successor {} (pid {pid})", exe.display()),
            Err(e) => tracing::error!("cannot start successor {}: {e}", exe.display()),
        }
    }
    Ok(0)
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}