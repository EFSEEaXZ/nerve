//! Plugins: external tool providers that speak MCP over stdio (newline-delimited JSON-RPC).
//!
//! Every folder `<data>/plugins/<name>/` with a `plugin.toml` is a plugin. nerve starts it, asks for
//! its tools and exposes them next to the built-in ones. The folders are watched: editing, adding,
//! removing or disabling a plugin hot-reloads it and clients get `notifications/tools/list_changed`.
//! Plugins may call nerve's built-in tools back (`nerve/call`). Python plugins get the tiny SDK in
//! `<data>/plugins/_sdk` on their PYTHONPATH.

mod bundled;
mod proc;

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp::{ToolCtx, ToolOutput, rpc_notification};
use crate::server::App;
use proc::Proc;

pub const MANIFEST: &str = "plugin.toml";
const INIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    pub version: String,
    /// argv; empty = auto (server.py / server.js / server.ps1 / server.exe)
    pub command: Vec<String>,
    pub enabled: Option<bool>,
    pub env: HashMap<String, String>,
    /// Prefix tool names with `<plugin>_` unless they already start with it (default true).
    pub prefix: Option<bool>,
    /// Per-call timeout in ms (0 = none).
    pub timeout_ms: u64,
}

/// Shared ring buffer + file for a plugin's stderr / log messages.
pub struct Log {
    path: PathBuf,
    recent: Mutex<VecDeque<String>>,
}

impl Log {
    fn new(path: PathBuf) -> Self {
        Self { path, recent: Mutex::new(VecDeque::new()) }
    }
    pub fn line(&self, s: &str) {
        let s = s.trim_end();
        {
            let mut r = self.recent.lock().unwrap();
            r.push_back(s.to_string());
            while r.len() > 200 {
                r.pop_front();
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{} {s}", chrono::Local::now().format("%H:%M:%S"));
        }
    }
    fn tail(&self, n: usize) -> Vec<String> {
        let r = self.recent.lock().unwrap();
        r.iter().skip(r.len().saturating_sub(n)).cloned().collect()
    }
    fn rotate(&self) {
        if std::fs::metadata(&self.path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
    }
}

struct Entry {
    name: String,
    dir: PathBuf,
    manifest: Manifest,
    fingerprint: u64,
    state: String,
    error: Option<String>,
    tools: Vec<Value>,
    proc: Option<Arc<Proc>>,
    log: Arc<Log>,
    restarts: u32,
    loaded_ms: u64,
}

impl Entry {
    fn summary(&self, exposed: &[(String, String)]) -> Value {
        json!({
            "name": self.name,
            "version": self.manifest.version,
            "description": self.manifest.description,
            "state": self.state,
            "error": self.error,
            "tools": exposed.iter().filter(|(_, p)| p == &self.name).map(|(t, _)| t.clone()).collect::<Vec<_>>(),
            "pid": self.proc.as_ref().filter(|p| p.alive()).map(|p| p.pid),
            "dir": self.dir,
            "restarts": self.restarts,
            "loaded": self.loaded_ms,
        })
    }
}

pub struct Plugins {
    pub dir: PathBuf,
    logs: PathBuf,
    enabled: bool,
    python: String,
    app: OnceLock<Weak<App>>,
    entries: tokio::sync::Mutex<HashMap<String, Entry>>,
    /// exposed tool name -> (plugin, tool name inside the plugin)
    routes: Mutex<HashMap<String, (String, String)>>,
    list: Mutex<Arc<Vec<Value>>>,
    summary: Mutex<Vec<Value>>,
}

fn skip_name(n: &str) -> bool {
    n.starts_with('.') || n == "__pycache__" || n == "node_modules" || n == "target" || n.ends_with(".pyc") || n.ends_with(".log")
}

/// Cheap change detector: names, sizes and mtimes of the plugin's files (depth <= 3) + disabled marker.
fn fingerprint(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    dir.join(".disabled").exists().hash(&mut h);
    fn walk(d: &Path, depth: u32, h: &mut std::collections::hash_map::DefaultHasher) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut items: Vec<_> = rd.flatten().collect();
        items.sort_by_key(|e| e.file_name());
        for e in items {
            let name = e.file_name().to_string_lossy().to_string();
            if skip_name(&name) {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            name.hash(h);
            if md.is_dir() {
                if depth < 3 {
                    walk(&e.path(), depth + 1, h);
                }
            } else {
                md.len().hash(h);
                md.modified().ok().map(crate::util::to_ms).hash(h);
            }
        }
    }
    walk(dir, 0, &mut h);
    h.finish()
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn exposed_name(plugin: &str, tool: &str, prefix: bool) -> String {
    let p = sanitize(plugin);
    let t = sanitize(tool);
    if !prefix || t == p || t.starts_with(&format!("{p}_")) { t } else { format!("{p}_{t}") }
}

impl Plugins {
    pub fn new(cfg: &crate::config::Config, paths: &crate::config::Paths) -> Self {
        let dir = if cfg.plugins.dir.trim().is_empty() { paths.data.join("plugins") } else { PathBuf::from(cfg.plugins.dir.trim()) };
        let logs = paths.logs.join("plugins");
        let _ = std::fs::create_dir_all(&logs);
        if cfg.plugins.enabled {
            let _ = std::fs::create_dir_all(&dir);
            if cfg.plugins.bundled {
                bundled::install(&dir);
            }
        }
        Self {
            dir,
            logs,
            enabled: cfg.plugins.enabled,
            python: cfg.plugins.python.trim().to_string(),
            app: OnceLock::new(),
            entries: tokio::sync::Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            list: Mutex::new(Arc::new(vec![])),
            summary: Mutex::new(vec![]),
        }
    }

    pub fn attach(&self, app: &Arc<App>) {
        let _ = self.app.set(Arc::downgrade(app));
    }

    fn app(&self) -> Option<Arc<App>> {
        self.app.get().and_then(|w| w.upgrade())
    }

    pub fn sdk_dir(&self) -> PathBuf {
        self.dir.join("_sdk")
    }

    /// Exposed tool definitions (for tools/list).
    pub fn tools(&self) -> Arc<Vec<Value>> {
        self.list.lock().unwrap().clone()
    }

    pub fn names(&self) -> Vec<String> {
        self.list.lock().unwrap().iter().filter_map(|t| t["name"].as_str().map(String::from)).collect()
    }

    pub fn summary(&self) -> Vec<Value> {
        self.summary.lock().unwrap().clone()
    }

    pub fn has(&self, tool: &str) -> bool {
        self.routes.lock().unwrap().contains_key(tool)
    }

    fn resolve_command(&self, dir: &Path, m: &Manifest) -> Result<(String, Vec<String>), String> {
        let mut argv = m.command.clone();
        if argv.is_empty() {
            argv = if dir.join("server.py").exists() {
                vec!["python".into(), "-u".into(), "server.py".into()]
            } else if dir.join("server.js").exists() {
                vec!["node".into(), "server.js".into()]
            } else if dir.join("server.mjs").exists() {
                vec!["node".into(), "server.mjs".into()]
            } else if dir.join("server.ps1").exists() {
                vec!["powershell".into(), "-NoProfile".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-File".into(), "server.ps1".into()]
            } else if dir.join("server.exe").exists() {
                vec!["server.exe".into()]
            } else {
                return Err("plugin.toml has no `command` and no server.py/server.js/server.ps1/server.exe found".into());
            };
        }
        let prog = argv.remove(0);
        // the process runs from the plugins root (Windows can't delete a folder that is some process's
        // cwd), so make arguments naming files of the plugin absolute
        for a in argv.iter_mut() {
            let p = dir.join(a.as_str());
            if !a.starts_with('-') && Path::new(a.as_str()).is_relative() && p.exists() {
                *a = p.to_string_lossy().to_string();
            }
        }
        let local = dir.join(&prog);
        let program = if matches!(prog.as_str(), "python" | "python3" | "py") {
            self.find_python().ok_or("Python not found (set [plugins] python in config.toml)")?
        } else if local.is_file() {
            local.to_string_lossy().to_string()
        } else {
            crate::util::which(&prog).map(|p| p.to_string_lossy().to_string()).unwrap_or(prog)
        };
        Ok((program, argv))
    }

    fn find_python(&self) -> Option<String> {
        if !self.python.is_empty() {
            return Some(self.python.clone());
        }
        for n in ["python", "python3", "py"] {
            if let Some(p) = crate::util::which(n) {
                // the Microsoft Store stub only opens the Store
                if p.to_string_lossy().to_lowercase().contains("windowsapps") {
                    continue;
                }
                return Some(p.to_string_lossy().to_string());
            }
        }
        None
    }

    async fn start(&self, e: &mut Entry) {
        e.log.rotate();
        e.log.line(&format!("--- starting {} {}", e.name, e.manifest.version));
        e.tools.clear();
        e.error = None;
        let (program, args) = match self.resolve_command(&e.dir, &e.manifest) {
            Ok(x) => x,
            Err(err) => {
                e.state = "error".into();
                e.error = Some(err.clone());
                e.log.line(&format!("error: {err}"));
                return;
            }
        };
        let mut env: Vec<(String, String)> = e.manifest.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let sdk = self.sdk_dir().to_string_lossy().to_string();
        let pp = match std::env::var("PYTHONPATH") {
            Ok(old) if !old.is_empty() => format!("{sdk}{}{old}", if cfg!(windows) { ';' } else { ':' }),
            _ => sdk.clone(),
        };
        env.push(("PYTHONPATH".into(), pp));
        env.push(("PYTHONUNBUFFERED".into(), "1".into()));
        env.push(("PYTHONUTF8".into(), "1".into()));
        env.push(("PYTHONIOENCODING".into(), "utf-8".into()));
        env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
        env.push(("NERVE_PLUGIN".into(), e.name.clone()));
        env.push(("NERVE_PLUGIN_DIR".into(), e.dir.to_string_lossy().to_string()));
        env.push(("NERVE_SDK".into(), sdk));
        if let Some(app) = self.app() {
            env.push(("NERVE_DATA".into(), app.paths.data.to_string_lossy().to_string()));
            env.push(("NERVE_OUT".into(), app.paths.out.to_string_lossy().to_string()));
            env.push(("NERVE_URL".into(), format!("http://{}", app.cfg.server.local_addr)));
        }
        let p = match Proc::spawn(&program, &args, &self.dir, &env, e.log.clone(), self.app.get().cloned()) {
            Ok(p) => Arc::new(p),
            Err(err) => {
                e.state = "error".into();
                e.error = Some(format!("cannot start `{program}`: {err}"));
                e.log.line(e.error.as_deref().unwrap_or(""));
                return;
            }
        };
        let init = p
            .request(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "nerve", "version": env!("CARGO_PKG_VERSION")}}),
                Some(INIT_TIMEOUT),
                None,
            )
            .await;
        let res = match init {
            Ok(_) => {
                let _ = p.notify("notifications/initialized", json!({})).await;
                p.request("tools/list", json!({}), Some(INIT_TIMEOUT), None).await
            }
            Err(err) => Err(err),
        };
        match res {
            Ok(v) => {
                e.tools = v.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                e.state = "running".into();
                e.proc = Some(p);
                e.loaded_ms = crate::util::now_ms();
                e.log.line(&format!("ready: {} tool(s)", e.tools.len()));
            }
            Err(err) => {
                let tail = e.log.tail(6).join("\n");
                e.state = "error".into();
                e.error = Some(format!("handshake failed: {err}\n{tail}"));
                e.log.line(&format!("handshake failed: {err}"));
                p.kill();
            }
        }
    }

    fn stop(e: &mut Entry, why: &str) {
        if let Some(p) = e.proc.take() {
            e.log.line(&format!("--- stopping ({why})"));
            p.kill();
        }
    }

    async fn load(&self, name: &str, dir: &Path, fp: u64, prev: Option<Entry>) -> Entry {
        let log = prev.as_ref().map(|p| p.log.clone()).unwrap_or_else(|| Arc::new(Log::new(self.logs.join(format!("{name}.log")))));
        let restarts = prev.as_ref().map(|p| p.restarts + 1).unwrap_or(0);
        if let Some(mut p) = prev {
            Self::stop(&mut p, "reload");
        }
        let mut e = Entry {
            name: name.to_string(),
            dir: dir.to_path_buf(),
            manifest: Manifest::default(),
            fingerprint: fp,
            state: "stopped".into(),
            error: None,
            tools: vec![],
            proc: None,
            log,
            restarts,
            loaded_ms: 0,
        };
        match std::fs::read_to_string(dir.join(MANIFEST)).map_err(|x| x.to_string()).and_then(|t| toml::from_str::<Manifest>(&t).map_err(|x| x.to_string())) {
            Ok(m) => e.manifest = m,
            Err(err) => {
                e.state = "error".into();
                e.error = Some(format!("bad {MANIFEST}: {err}"));
                return e;
            }
        }
        if e.manifest.enabled == Some(false) || dir.join(".disabled").exists() {
            e.state = "disabled".into();
            return e;
        }
        self.start(&mut e).await;
        e
    }

    /// Look for added/changed/removed plugins. `force` reloads the named plugin (or all with "*").
    pub async fn scan(&self, force: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        let mut found: Vec<(String, PathBuf)> = vec![];
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for d in rd.flatten() {
                let n = d.file_name().to_string_lossy().to_string();
                if n.starts_with('_') || n.starts_with('.') || !d.path().join(MANIFEST).is_file() {
                    continue;
                }
                found.push((n, d.path()));
            }
        }
        let mut entries = self.entries.lock().await;
        let mut changed = false;
        let names: Vec<String> = entries.keys().cloned().collect();
        for n in names {
            if !found.iter().any(|(f, _)| f == &n) {
                if let Some(mut e) = entries.remove(&n) {
                    Self::stop(&mut e, "removed");
                    tracing::info!(plugin = %n, "plugin removed");
                    changed = true;
                }
            }
        }
        for (name, dir) in found {
            let fp = fingerprint(&dir);
            let forced = force.is_some_and(|f| f == "*" || f == name);
            let dead = entries.get(&name).is_some_and(|e| e.state == "running" && !e.proc.as_ref().is_some_and(|p| p.alive()));
            let same = entries.get(&name).is_some_and(|e| e.fingerprint == fp);
            if same && !forced && !dead {
                continue;
            }
            // a file is still being written? wait for it to settle
            if !forced && entries.contains_key(&name) {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if fingerprint(&dir) != fp {
                    continue;
                }
            }
            let prev = entries.remove(&name);
            let e = self.load(&name, &dir, fp, prev).await;
            tracing::info!(plugin = %name, state = %e.state, tools = e.tools.len(), "plugin loaded");
            entries.insert(name, e);
            changed = true;
        }
        if changed || force.is_some() {
            self.rebuild(&entries);
        }
        drop(entries);
        if changed {
            if let Some(app) = self.app() {
                app.sessions.broadcast(rpc_notification("notifications/tools/list_changed", json!({})));
            }
        }
        changed
    }

    fn rebuild(&self, entries: &HashMap<String, Entry>) {
        let builtin: Vec<String> = self.app().map(|a| a.registry.names()).unwrap_or_default();
        let mut routes = HashMap::new();
        let mut list = vec![];
        let mut exposed: Vec<(String, String)> = vec![];
        let mut conflicts: HashMap<String, Vec<String>> = HashMap::new();
        let mut names: Vec<&String> = entries.keys().collect();
        names.sort();
        for n in names {
            let e = &entries[n];
            if e.state != "running" {
                continue;
            }
            let prefix = e.manifest.prefix.unwrap_or(true);
            for t in &e.tools {
                let Some(orig) = t.get("name").and_then(|v| v.as_str()) else { continue };
                let name = exposed_name(&e.name, orig, prefix);
                if builtin.contains(&name) || routes.contains_key(&name) {
                    conflicts.entry(e.name.clone()).or_default().push(name);
                    continue;
                }
                let mut def = t.clone();
                def["name"] = json!(name);
                let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                def["description"] = json!(format!("[plugin {}] {desc}", e.name));
                if def.get("inputSchema").is_none() {
                    def["inputSchema"] = json!({"type": "object", "properties": {}});
                }
                routes.insert(name.clone(), (e.name.clone(), orig.to_string()));
                exposed.push((name, e.name.clone()));
                list.push(def);
            }
        }
        let mut summary: Vec<Value> = entries.values().map(|e| e.summary(&exposed)).collect();
        for s in summary.iter_mut() {
            if let Some(c) = conflicts.get(s["name"].as_str().unwrap_or("")) {
                s["error"] = json!(format!("tool name(s) already taken, skipped: {}", c.join(", ")));
            }
        }
        summary.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        *self.routes.lock().unwrap() = routes;
        *self.list.lock().unwrap() = Arc::new(list);
        *self.summary.lock().unwrap() = summary;
    }

    /// Background watcher: rescans the plugin folder every second.
    pub async fn watch(self: Arc<Self>, shutdown: tokio_util::sync::CancellationToken) {
        if !self.enabled {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(1000));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = shutdown.cancelled() => break,
            }
            self.scan(None).await;
        }
        let mut entries = self.entries.lock().await;
        for e in entries.values_mut() {
            Self::stop(e, "shutdown");
        }
    }

    pub async fn set_enabled(&self, name: &str, on: bool) -> Result<(), String> {
        let dir = self.dir.join(name);
        if !dir.join(MANIFEST).is_file() {
            return Err(format!("no plugin `{name}` in {}", self.dir.display()));
        }
        let marker = dir.join(".disabled");
        if on {
            let _ = std::fs::remove_file(&marker);
        } else {
            std::fs::write(&marker, b"disabled via nerve\n").map_err(|e| e.to_string())?;
        }
        self.scan(Some(name)).await;
        Ok(())
    }

    pub async fn log_tail(&self, name: &str, n: usize) -> Result<String, String> {
        let path = self.logs.join(format!("{name}.log"));
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let lines: Vec<&str> = text.lines().collect();
        Ok(lines[lines.len().saturating_sub(n)..].join("\n"))
    }

    /// Call a plugin tool by its exposed name. `None` = no such tool.
    pub async fn call(&self, tool: &str, args: Value, ctx: ToolCtx) -> Option<ToolOutput> {
        let (plugin, inner) = self.routes.lock().unwrap().get(tool).cloned()?;
        let mut proc_ = {
            let entries = self.entries.lock().await;
            entries.get(&plugin).and_then(|e| e.proc.clone().map(|p| (p, e.manifest.timeout_ms)))
        };
        if !proc_.as_ref().is_some_and(|(p, _)| p.alive()) {
            // crashed since the last call: restart once
            self.scan(Some(&plugin)).await;
            let entries = self.entries.lock().await;
            proc_ = entries.get(&plugin).and_then(|e| e.proc.clone().map(|p| (p, e.manifest.timeout_ms)));
        }
        let Some((p, timeout_ms)) = proc_ else {
            let err = self.summary().into_iter().find(|s| s["name"] == plugin.as_str()).and_then(|s| s["error"].as_str().map(String::from));
            return Some(ToolOutput::error(format!("plugin `{plugin}` is not running{}", err.map(|e| format!(": {e}")).unwrap_or_default())));
        };
        let timeout = (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms));
        let mut params = json!({"name": inner, "arguments": if args.is_null() { json!({}) } else { args }});
        let token = ctx.progress.enabled().then(|| p.register_progress(ctx.progress.clone()));
        if let Some(t) = &token {
            params["_meta"] = json!({"progressToken": t});
        }
        let r = p.request("tools/call", params, timeout, Some(&ctx.cancel)).await;
        if let Some(t) = &token {
            p.unregister_progress(t);
        }
        Some(match r {
            Ok(v) => to_output(v, &ctx).await,
            Err(e) => ToolOutput::error(format!("plugin `{plugin}`: {e}")),
        })
    }
}

/// Convert a plugin's CallToolResult into our output. Image items may carry a local `path`
/// instead of data: nerve loads and fits them into the client's image budget.
async fn to_output(v: Value, ctx: &ToolCtx) -> ToolOutput {
    let mut out = ToolOutput::empty();
    out.is_error = v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
    out.structured = v.get("structuredContent").cloned();
    let items = v.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    for it in items {
        let path = it.get("path").and_then(|p| p.as_str()).map(String::from);
        match (it.get("type").and_then(|t| t.as_str()), path) {
            (Some("image"), Some(path)) if it.get("data").is_none() => {
                let cfg = ctx.app.cfg.image.clone();
                let budget = it.get("max_bytes").and_then(|b| b.as_u64()).map(|b| b as usize).unwrap_or(cfg.max_bytes);
                let r = crate::mcp::blocking(move || {
                    let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
                    let img = crate::tools::image::decode_bytes(&bytes)?;
                    let o = crate::tools::image::ViewOpts {
                        region: None,
                        tile: None,
                        scale: None,
                        budget,
                        max_dim: cfg.max_dim,
                        fmt: crate::tools::image::Fmt::Auto,
                        min_q: cfg.min_quality,
                        origin: (0, 0),
                        coord: "original",
                    };
                    let mut t = ToolOutput::empty();
                    let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(path.clone());
                    crate::tools::image::view_into(&img, &name, &o, &mut t)?;
                    Ok(t)
                })
                .await;
                match r {
                    Ok(t) => out.content.extend(t.content),
                    Err(e) => out.push_text(format!("(image not loaded: {e})")),
                }
            }
            _ => out.content.push(it),
        }
    }
    out
}
