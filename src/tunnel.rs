//! Managed tunnel providers: zrok, cloudflared, ngrok, tailscale, or a custom command.
//!
//! The tunnel runs as a *detached* process writing to `logs/tunnel.out`. nerve supervises it
//! (restart with backoff) and adopts the still-running process after a nerve restart/update, so the
//! public URL survives restarts. It is only stopped on a real shutdown (or a config change).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::{Paths, TunnelCfg};
use crate::server::AppState;

#[derive(Clone, Serialize, Default)]
pub struct TunnelStatus {
    pub provider: String,
    pub state: String,
    pub url: Option<String>,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub adopted: bool,
    pub last_line: Option<String>,
}

pub struct Tunnel {
    status: RwLock<TunnelStatus>,
    restart: Notify,
}

impl Tunnel {
    pub fn new(provider: &str) -> Self {
        Self {
            status: RwLock::new(TunnelStatus { provider: provider.to_string(), state: "idle".into(), ..Default::default() }),
            restart: Notify::new(),
        }
    }
    pub fn status(&self) -> TunnelStatus {
        self.status.read().unwrap().clone()
    }
    pub fn url(&self) -> Option<String> {
        self.status.read().unwrap().url.clone()
    }
    pub fn request_restart(&self) {
        self.restart.notify_one();
    }
    fn set(&self, f: impl FnOnce(&mut TunnelStatus)) {
        f(&mut self.status.write().unwrap());
    }
}

struct Plan {
    program: String,
    args: Vec<String>,
    /// Windows: appended verbatim (cmd.exe has its own quoting rules).
    raw: Option<String>,
    /// Commands run before every fresh start (e.g. free a stale zrok share).
    pre: Vec<Vec<String>>,
    /// Commands run after a real stop (cleanup).
    post: Vec<Vec<String>>,
    url_re: Regex,
    /// Prepended to the regex capture (zrok prints bare host names).
    url_prefix: &'static str,
    fixed_url: Option<String>,
    online_marker: Option<&'static str>,
}

impl Plan {
    fn spec(&self) -> String {
        crate::util::sha256_hex(format!("{}\u{1f}{}\u{1f}{:?}", self.program, self.args.join("\u{1f}"), self.raw))[..16].to_string()
    }
}

pub fn zrok_bin(cfg: &TunnelCfg, paths: &Paths) -> String {
    if !cfg.zrok_bin.trim().is_empty() {
        return cfg.zrok_bin.trim().to_string();
    }
    let local = paths.data.join("tools").join(format!("zrok2{}", std::env::consts::EXE_SUFFIX));
    if local.is_file() {
        return local.to_string_lossy().to_string();
    }
    crate::util::which("zrok2").map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "zrok2".into())
}

fn plan(cfg: &TunnelCfg, paths: &Paths, provider: &str, port: u16, public_url: &str) -> Result<Plan, String> {
    let target = format!("http://127.0.0.1:{port}");
    let fixed = |s: &str| (!s.trim().is_empty()).then(|| s.trim().trim_end_matches('/').to_string());
    let none = || Regex::new(r"$^").unwrap();
    Ok(match provider {
        "zrok" | "zrok2" => {
            let bin = zrok_bin(cfg, paths);
            let ns = if cfg.zrok_namespace.trim().is_empty() { "public".to_string() } else { cfg.zrok_namespace.trim().to_string() };
            let name = cfg.zrok_name.trim().to_string();
            let mut args: Vec<String> = ["share", "public", &target, "--headless", "--force-local"].iter().map(|s| s.to_string()).collect();
            let mut pre = vec![];
            let mut fixed_url = fixed(public_url);
            if !name.is_empty() {
                args.push("--name-selection".into());
                args.push(format!("{ns}:{name}"));
                // a share killed without cleanup keeps the name busy ("already in use") until deleted
                pre.push(vec![bin.clone(), "delete".into(), "share".into(), format!("{ns}:{name}")]);
                if ns == "public" {
                    fixed_url = Some(format!("https://{name}.shares.zrok.io"));
                }
            }
            Plan {
                program: bin,
                args,
                raw: None,
                post: pre.clone(),
                pre,
                url_re: Regex::new(r"endpoints:(?:\\n|\s)+([A-Za-z0-9][A-Za-z0-9.-]*\.[A-Za-z]{2,})").unwrap(),
                url_prefix: "https://",
                fixed_url,
                online_marker: Some("access your zrok share"),
            }
        }
        "cloudflared" | "cloudflare" => {
            if cfg.cloudflared_token.trim().is_empty() {
                Plan {
                    program: cfg.cloudflared_bin.clone(),
                    args: vec!["tunnel".into(), "--no-autoupdate".into(), "--url".into(), target],
                    raw: None,
                    pre: vec![],
                    post: vec![],
                    // quick-tunnel hosts are hyphenated words (never `api.trycloudflare.com`)
                    url_re: Regex::new(r"https://[a-z0-9]+(?:-[a-z0-9]+)+\.trycloudflare\.com").unwrap(),
                    url_prefix: "",
                    fixed_url: None,
                    online_marker: Some("Registered tunnel connection"),
                }
            } else {
                Plan {
                    program: cfg.cloudflared_bin.clone(),
                    args: vec!["tunnel".into(), "--no-autoupdate".into(), "run".into(), "--token".into(), cfg.cloudflared_token.trim().into()],
                    raw: None,
                    pre: vec![],
                    post: vec![],
                    url_re: none(),
                    url_prefix: "",
                    fixed_url: fixed(public_url),
                    online_marker: Some("Registered tunnel connection"),
                }
            }
        }
        "ngrok" => {
            let mut args = vec!["http".into(), port.to_string(), "--log".into(), "stdout".into(), "--log-format".into(), "logfmt".into()];
            let domain = cfg.ngrok_domain.trim().trim_start_matches("https://").trim_end_matches('/').to_string();
            if !domain.is_empty() {
                args.push("--url".into());
                args.push(domain.clone());
            }
            if !cfg.ngrok_authtoken.trim().is_empty() {
                args.push("--authtoken".into());
                args.push(cfg.ngrok_authtoken.trim().into());
            }
            Plan {
                program: cfg.ngrok_bin.clone(),
                args,
                raw: None,
                pre: vec![],
                post: vec![],
                url_re: Regex::new(r#"url=(https://[^\s"]+)"#).unwrap(),
                url_prefix: "",
                fixed_url: (!domain.is_empty()).then(|| format!("https://{domain}")),
                online_marker: Some("started tunnel"),
            }
        }
        "tailscale" => Plan {
            program: cfg.tailscale_bin.clone(),
            args: vec!["funnel".into(), port.to_string()],
            raw: None,
            pre: vec![],
            post: vec![],
            url_re: Regex::new(r"https://[A-Za-z0-9.-]+\.ts\.net").unwrap(),
            url_prefix: "",
            fixed_url: fixed(public_url),
            online_marker: None,
        },
        "command" => {
            if cfg.command.trim().is_empty() {
                return Err("tunnel.command is empty".into());
            }
            let cmd = cfg.command.replace("{port}", &port.to_string());
            let (program, args, raw) = if cfg!(windows) {
                // `cmd /s /c "<line>"` strips exactly the outer quotes and runs <line> verbatim
                ("cmd".to_string(), vec!["/d".into(), "/s".into(), "/c".into()], Some(format!("\"{cmd}\"")))
            } else {
                ("sh".to_string(), vec!["-c".into(), cmd], None)
            };
            Plan {
                program,
                args,
                raw,
                pre: vec![],
                post: vec![],
                url_re: Regex::new(r#"https://[^\s"'<>]+"#).unwrap(),
                url_prefix: "",
                fixed_url: fixed(public_url),
                online_marker: None,
            }
        }
        other => return Err(format!("unknown tunnel provider `{other}`")),
    })
}

/// Persisted so the next nerve instance can adopt the running tunnel.
#[derive(Serialize, Deserialize, Clone)]
struct Saved {
    pid: u32,
    spec: String,
    provider: String,
    out: PathBuf,
    started: u64,
}

fn state_path(app: &AppState) -> PathBuf {
    app.paths.data.join("tunnel.json")
}

fn load_saved(app: &AppState) -> Option<Saved> {
    serde_json::from_slice(&std::fs::read(state_path(app)).ok()?).ok()
}

fn save(app: &AppState, s: &Saved) {
    let _ = crate::util::atomic_write(&state_path(app), serde_json::to_string(s).unwrap_or_default().as_bytes());
}

fn forget(app: &AppState) {
    let _ = std::fs::remove_file(state_path(app));
}

/// Is `pid` alive and (by name) the tunnel program?
fn is_tunnel_proc(pid: u32, program: &str) -> bool {
    let stem = Path::new(program).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    match crate::util::process_name(pid) {
        Some(n) => !stem.is_empty() && n.to_lowercase().contains(&stem),
        None => false,
    }
}

async fn run_aux(cmds: &[Vec<String>]) {
    for c in cmds {
        let Some((prog, args)) = c.split_first() else { continue };
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(crate::util::CREATE_NO_WINDOW);
        match tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
            Ok(Ok(o)) => {
                let msg = String::from_utf8_lossy(if o.stderr.is_empty() { &o.stdout } else { &o.stderr }).to_string();
                tracing::debug!("tunnel aux `{}`: {} {}", args.join(" "), o.status, crate::util::one_line(&msg, 200));
            }
            Ok(Err(e)) => tracing::debug!("tunnel aux `{prog}`: {e}"),
            Err(_) => tracing::warn!("tunnel aux `{}` timed out", args.join(" ")),
        }
    }
}

fn spawn(plan: &Plan, logs: &Path) -> std::io::Result<(std::process::Child, PathBuf)> {
    let mut out_path = logs.join("tunnel.out");
    let file = match std::fs::File::create(&out_path) {
        Ok(f) => f,
        Err(_) => {
            out_path = logs.join(format!("tunnel-{}.out", crate::util::now_ms()));
            std::fs::File::create(&out_path)?
        }
    };
    let err = file.try_clone()?;
    let mut c = std::process::Command::new(&plan.program);
    c.args(&plan.args).stdin(Stdio::null()).stdout(file).stderr(err);
    #[cfg(windows)]
    if let Some(r) = &plan.raw {
        use std::os::windows::process::CommandExt;
        c.raw_arg(r);
    }
    let child = crate::util::spawn_detached(c)?;
    Ok((child, out_path))
}

enum Exit {
    Shutdown,
    Restart,
    Exited(String),
}

pub async fn run(app: AppState, shutdown: CancellationToken) {
    let cfg = app.cfg.tunnel.clone();
    let provider = cfg.provider.trim().to_lowercase();
    let _ = std::fs::remove_file(app.paths.data.join("tunnel.pid")); // phase-1 leftover
    if provider.is_empty() || provider == "none" {
        // provider switched off: stop a tunnel left running by a previous instance
        if let Some(s) = load_saved(&app) {
            if crate::util::pid_alive(s.pid) {
                crate::util::kill_tree(s.pid, true);
            }
            forget(&app);
        }
        app.tunnel.set(|s| s.state = "disabled".into());
        return;
    }
    let port = match app.cfg.server.public_addr.parse::<std::net::SocketAddr>() {
        Ok(a) => a.port(),
        Err(_) => {
            app.tunnel.set(|s| s.state = "error: public listener is disabled (server.public_addr)".into());
            return;
        }
    };
    let plan = match plan(&cfg, &app.paths, &provider, port, &app.cfg.server.public_url) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("tunnel: {e}");
            app.tunnel.set(|s| s.state = format!("error: {e}"));
            return;
        }
    };
    let spec = plan.spec();

    // adopt a tunnel left running by the previous instance (restart/update), or clean it up
    let mut adopt: Option<Saved> = None;
    if let Some(s) = load_saved(&app) {
        let alive = is_tunnel_proc(s.pid, &plan.program) || (crate::util::pid_alive(s.pid) && s.provider == provider);
        if alive && s.spec == spec {
            tracing::info!("tunnel: adopting running {} (pid {})", s.provider, s.pid);
            adopt = Some(s);
        } else {
            if alive {
                tracing::info!("tunnel: stopping previous {} (pid {}, config changed)", s.provider, s.pid);
                crate::util::kill_tree(s.pid, true);
            }
            forget(&app);
        }
    }

    let mut backoff = 1u64;
    loop {
        let started = Instant::now();
        let mut child: Option<std::process::Child> = None;
        let (pid, out_path, adopted) = match adopt.take() {
            Some(s) => (s.pid, s.out, true),
            None => {
                run_aux(&plan.pre).await;
                match spawn(&plan, &app.paths.logs) {
                    Ok((c, out)) => {
                        let pid = c.id();
                        save(&app, &Saved { pid, spec: spec.clone(), provider: provider.clone(), out: out.clone(), started: crate::util::now_ms() });
                        child = Some(c);
                        tracing::info!("tunnel: started {provider} (pid {pid})");
                        (pid, out, false)
                    }
                    Err(e) => {
                        let msg = format!("cannot start `{}`: {e}", plan.program);
                        tracing::error!("tunnel: {msg}");
                        app.tunnel.set(|s| s.state = format!("error: {msg}"));
                        (0, PathBuf::new(), false)
                    }
                }
            }
        };
        let exit = if pid == 0 {
            Exit::Exited("spawn failed".into())
        } else {
            app.tunnel.set(|s| {
                s.state = "starting".into();
                s.pid = Some(pid);
                s.adopted = adopted;
                s.url = plan.fixed_url.clone();
            });
            let mut offset = 0u64;
            let mut pending = String::new();
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break Exit::Shutdown,
                    _ = app.tunnel.restart.notified() => break Exit::Restart,
                    _ = tick.tick() => {
                        if let Ok((text, next)) = crate::jobs::read_from(&out_path, offset, 256 * 1024) {
                            offset = next;
                            pending.push_str(&text);
                            while let Some(nl) = pending.find('\n') {
                                let line: String = pending.drain(..=nl).collect();
                                on_line(&app, &plan, line.trim_end());
                            }
                        }
                        let alive = match child.as_mut() {
                            Some(c) => matches!(c.try_wait(), Ok(None)),
                            None => crate::util::pid_alive(pid),
                        };
                        if !alive {
                            let code = child.as_mut().and_then(|c| c.try_wait().ok().flatten()).map(|s| format!("{s}")).unwrap_or_else(|| "exited".into());
                            break Exit::Exited(code);
                        }
                    }
                }
            }
        };
        match exit {
            Exit::Shutdown => {
                if app.life.keep_tunnel() && pid != 0 {
                    tracing::info!("tunnel: leaving pid {pid} running for the next instance");
                    app.tunnel.set(|s| s.state = "handed over".into());
                } else {
                    if pid != 0 {
                        crate::util::kill_tree(pid, true);
                    }
                    forget(&app);
                    run_aux(&plan.post).await;
                    app.tunnel.set(|s| s.state = "stopped".into());
                }
                return;
            }
            Exit::Restart => {
                tracing::info!("tunnel: restart requested");
                if pid != 0 {
                    crate::util::kill_tree(pid, true);
                }
                forget(&app);
                backoff = 1;
            }
            Exit::Exited(why) => {
                let last = app.tunnel.status().last_line.unwrap_or_default();
                tracing::warn!("tunnel: exited ({why}); restarting in {backoff}s. last output: {last}");
                forget(&app);
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        app.tunnel.set(|s| {
            s.state = "restarting".into();
            s.restarts += 1;
            s.pid = None;
            s.adopted = false;
            s.url = plan.fixed_url.clone();
        });
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            _ = shutdown.cancelled() => {
                app.tunnel.set(|s| s.state = "stopped".into());
                return;
            }
        }
        backoff = (backoff * 2).min(30);
    }
}

fn on_line(app: &AppState, plan: &Plan, line: &str) {
    if line.is_empty() {
        return;
    }
    let lower = line.to_ascii_lowercase();
    let noisy = lower.contains("fail") || lower.contains("error") || lower.contains(" err ");
    let found = if noisy {
        None
    } else {
        plan.url_re.captures(line).map(|c| format!("{}{}", plan.url_prefix, c.get(1).unwrap_or(c.get(0).unwrap()).as_str()))
    };
    let online = match plan.online_marker {
        Some(m) => line.contains(m),
        None => found.is_some(),
    };
    app.tunnel.set(|s| {
        if let Some(u) = &found {
            if s.url.is_none() || plan.fixed_url.is_none() {
                s.url = Some(u.trim_end_matches('/').to_string());
            }
        }
        if online && s.state != "online" {
            s.state = "online".into();
            tracing::info!("tunnel: online {}", s.url.clone().unwrap_or_default());
        }
        s.last_line = Some(line.chars().take(300).collect());
    });
}
