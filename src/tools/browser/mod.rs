//! `browser`: a real Chrome/Edge driven over the DevTools protocol.
//!
//! nerve launches the browser with its own persistent profile (`<data>/browser/profile`; Chrome refuses
//! remote debugging on the default profile) as a detached process, so it survives nerve restarts and is
//! adopted again through `DevToolsActivePort`. Pages are read as a compact outline with numbered refs
//! (`snapshot`), and acted on with real input events (`click`, `type`, `key`, `scroll`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::mcp::{ToolCtx, ToolDef, ToolOutput, tool};
use crate::tools::image::{self as img, Fmt, ViewOpts};
use crate::util;

mod cdp;
mod js;
use cdp::{Cdp, Event};

const T: Duration = Duration::from_secs(30);

#[derive(Deserialize, JsonSchema)]
pub struct BrowserArgs {
    /// open | snapshot | click | type | key | select | hover | scroll | wait | screenshot | text | html | eval |
    /// tabs | tab | close | back | forward | reload | console | upload | downloads | status | launch | quit
    action: String,
    /// open: URL (scheme optional).
    url: Option<String>,
    /// open: in a new tab.
    new_tab: Option<bool>,
    /// tab/close: 1-based tab index from `tabs`.
    index: Option<usize>,
    /// Element ref number from the last `snapshot`, e.g. 12.
    #[serde(rename = "ref")]
    #[schemars(rename = "ref")]
    ref_: Option<Value>,
    /// Element CSS selector (or XPath starting with / or xpath=).
    selector: Option<String>,
    /// type: text to enter. click/hover/screenshot: pick the element by its visible text/label instead of ref.
    text: Option<String>,
    /// select: option value or label. wait: substring of the URL is `url`.
    value: Option<String>,
    /// type: replace the current content (default true).
    clear: Option<bool>,
    /// type: press Enter afterwards.
    submit: Option<bool>,
    /// key: chords like "Enter", "ctrl+a", "shift+Tab", "Escape", "ArrowDown*3".
    keys: Option<String>,
    /// click/hover/scroll: viewport CSS-pixel coordinates (from a screenshot) instead of an element.
    x: Option<f64>,
    y: Option<f64>,
    /// scroll: pixels down (+) / up (-); default one screen.
    dy: Option<f64>,
    /// scroll: pixels right (+) / left (-).
    dx: Option<f64>,
    /// click: left (default) | right | middle.
    button: Option<String>,
    /// click: 2 = double-click.
    count: Option<u32>,
    /// eval: JavaScript (expression, statements with `return`, or top-level await).
    js: Option<String>,
    /// screenshot: whole page instead of the viewport.
    full_page: Option<bool>,
    /// snapshot: keep only lines containing this.
    filter: Option<String>,
    /// snapshot: only interactive elements (no text).
    interactive_only: Option<bool>,
    /// snapshot/text/html: max characters returned.
    max_chars: Option<usize>,
    /// text: start offset (for paging long pages).
    offset: Option<usize>,
    /// wait/open: timeout in ms (wait default 10000, open 30000).
    timeout_ms: Option<u64>,
    /// wait: until the text/selector disappears instead.
    gone: Option<bool>,
    /// upload: files for an <input type=file>.
    paths: Option<Vec<String>>,
    /// launch: headless (no window).
    headless: Option<bool>,
    /// console: clear after reading.
    clear_log: Option<bool>,
    // ---- screenshot tuning (same as the image tool) ----
    max_bytes: Option<usize>,
    max_dim: Option<u32>,
    scale: Option<f64>,
    format: Option<String>,
    tile: Option<String>,
    region: Option<Vec<f64>>,
}

const DESC: &str = "A real Chrome/Edge (own persistent profile: log in once, stays logged in), driven over CDP. \
open {url, new_tab?}; snapshot {filter?, interactive_only?, max_chars?} (page outline: headings, text and numbered [ref] \
elements with role/name/value); click {ref|selector|text|x,y, button?, count?}; type {ref|selector|text?, text, clear?=true, submit?} \
(no target = focused element); key {keys: \"Enter\", \"ctrl+a\", \"ArrowDown*3\"}; select {ref|selector, value}; hover; \
scroll {dy?|ref?}; wait {text?|selector?|value(url part)?, gone?, timeout_ms?}; screenshot {full_page?, ref?|selector?} \
(viewport CSS px: click {x,y} with the mapped coordinates); text {selector?, offset?, max_chars?}; html {selector?}; \
eval {js}; tabs; tab {index}; close {index?}; back; forward; reload; console {clear_log?} (console, errors, dialogs); \
upload {ref|selector, paths}; downloads; status; launch {headless?}; quit. Actions report navigation, new tabs and new JS errors. \
Typical loop: open -> snapshot -> click/type by ref -> snapshot.";

pub fn def() -> ToolDef {
    tool::<BrowserArgs, _, _>("browser", "Browser", DESC, json!({"destructiveHint": true, "openWorldHint": true}), run)
}

// ------------------------------------------------------------------ state

struct Browser {
    cdp: Arc<Cdp>,
    pid: Option<u32>,
    origin: &'static str,
    headless: bool,
    profile: Option<PathBuf>,
    current: Option<String>,
    order: Vec<String>,
    sessions: HashMap<String, String>,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    logs: StdMutex<HashMap<String, VecDeque<String>>>,
    downloads: StdMutex<VecDeque<String>>,
    dl_names: StdMutex<HashMap<String, String>>,
}

impl Shared {
    fn log(&self, session: &str, line: String) {
        let mut m = self.logs.lock().unwrap();
        let q = m.entry(session.to_string()).or_default();
        if q.len() >= 300 {
            q.pop_front();
        }
        q.push_back(format!("{} {line}", chrono::Local::now().format("%H:%M:%S")));
    }
    fn count(&self, session: &str) -> usize {
        self.logs.lock().unwrap().get(session).map_or(0, |q| q.len())
    }
    fn since(&self, session: &str, n: usize) -> Vec<String> {
        self.logs.lock().unwrap().get(session).map(|q| q.iter().skip(n).cloned().collect()).unwrap_or_default()
    }
}

static BROWSER: tokio::sync::Mutex<Option<Browser>> = tokio::sync::Mutex::const_new(None);

fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

fn remote_str(v: &Value) -> String {
    match v.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(x) if !x.is_null() => x.to_string(),
        _ => v.get("description").and_then(Value::as_str).or(v.get("type").and_then(Value::as_str)).unwrap_or("?").to_string(),
    }
}

/// Background listener: console, exceptions, dialogs (auto-accepted), downloads.
fn pump(cdp: Arc<Cdp>, shared: Arc<Shared>) {
    let mut rx = cdp.events.subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let s = ev.session.clone().unwrap_or_default();
            let p = &ev.params;
            match ev.method.as_str() {
                cdp::CLOSED => break,
                "Runtime.consoleAPICalled" => {
                    let kind = p["type"].as_str().unwrap_or("log");
                    let args: Vec<String> = p["args"].as_array().map(|a| a.iter().map(remote_str).collect()).unwrap_or_default();
                    shared.log(&s, format!("[{kind}] {}", util::clip_middle(&args.join(" "), 600)));
                }
                "Runtime.exceptionThrown" => {
                    let d = &p["exceptionDetails"];
                    let msg = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("exception");
                    shared.log(&s, format!("[exception] {}", first_lines(msg, 4)));
                }
                "Log.entryAdded" => {
                    let e = &p["entry"];
                    let lvl = e["level"].as_str().unwrap_or("info");
                    if lvl == "error" || lvl == "warning" {
                        let url = e["url"].as_str().map(|u| format!(" ({})", util::one_line(u, 120))).unwrap_or_default();
                        shared.log(&s, format!("[{lvl}] {}{url}", util::one_line(e["text"].as_str().unwrap_or(""), 400)));
                    }
                }
                "Page.javascriptDialogOpening" => {
                    let kind = p["type"].as_str().unwrap_or("alert").to_string();
                    shared.log(&s, format!("[dialog {kind}] \"{}\" -> accepted automatically", util::one_line(p["message"].as_str().unwrap_or(""), 300)));
                    let (c, sess) = (cdp.clone(), ev.session.clone());
                    let prompt = p["defaultPrompt"].as_str().unwrap_or("").to_string();
                    tokio::spawn(async move {
                        let _ = c.call(sess.as_deref(), "Page.handleJavaScriptDialog", json!({"accept": true, "promptText": prompt}), Duration::from_secs(5)).await;
                    });
                }
                "Browser.downloadWillBegin" => {
                    let name = p["suggestedFilename"].as_str().unwrap_or("?").to_string();
                    shared.dl_names.lock().unwrap().insert(p["guid"].as_str().unwrap_or("").to_string(), name.clone());
                    push_dl(&shared, format!("started {name}"));
                }
                "Browser.downloadProgress" => {
                    let st = p["state"].as_str().unwrap_or("");
                    if st == "completed" || st == "canceled" {
                        let name = shared.dl_names.lock().unwrap().remove(p["guid"].as_str().unwrap_or("")).unwrap_or_else(|| "?".into());
                        push_dl(&shared, format!("{st} {name}"));
                    }
                }
                _ => {}
            }
        }
    });
}

fn push_dl(shared: &Shared, line: String) {
    let mut q = shared.downloads.lock().unwrap();
    if q.len() >= 100 {
        q.pop_front();
    }
    q.push_back(format!("{} {line}", chrono::Local::now().format("%H:%M:%S")));
}

fn profile_dir(ctx: &ToolCtx) -> PathBuf {
    let p = ctx.app.cfg.browser.profile.trim();
    if p.is_empty() { ctx.app.paths.data.join("browser").join("profile") } else { PathBuf::from(p) }
}

fn downloads_dir(ctx: &ToolCtx) -> PathBuf {
    ctx.app.paths.out.join("downloads")
}

fn find_chrome(cfg_path: &str) -> Result<PathBuf, String> {
    let cfg_path = cfg_path.trim();
    if !cfg_path.is_empty() {
        let p = PathBuf::from(cfg_path);
        return if p.exists() { Ok(p) } else { util::which(cfg_path).ok_or(format!("[browser] path `{cfg_path}` not found")) };
    }
    let mut c: Vec<PathBuf> = vec![];
    #[cfg(windows)]
    for rel in [r"Google\Chrome\Application\chrome.exe", r"Microsoft\Edge\Application\msedge.exe", r"Chromium\Application\chrome.exe", r"BraveSoftware\Brave-Browser\Application\brave.exe"] {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Ok(b) = std::env::var(base) {
                c.push(PathBuf::from(b).join(rel));
            }
        }
    }
    #[cfg(target_os = "macos")]
    for p in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ] {
        c.push(p.into());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    for n in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "brave-browser"] {
        if let Some(p) = util::which(n) {
            c.push(p);
        }
    }
    c.into_iter().find(|p| p.exists()).ok_or_else(|| "no Chrome/Edge/Chromium found; set [browser] path in config.toml".into())
}

fn read_port_file(profile: &Path) -> Option<(u16, String)> {
    let s = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut l = s.lines();
    let port = l.next()?.trim().parse().ok()?;
    Some((port, l.next()?.trim().to_string()))
}

async fn connect_or_launch(ctx: &ToolCtx, headless: bool) -> Result<Browser, String> {
    let cfg = &ctx.app.cfg.browser;
    if !cfg.cdp_url.trim().is_empty() {
        let base = cfg.cdp_url.trim().trim_end_matches('/');
        let ws = if base.starts_with("ws") {
            base.to_string()
        } else {
            let v: Value = ctx.app.http.get(format!("{base}/json/version")).send().await.map_err(|e| format!("{base}: {e}"))?.json().await.map_err(|e| format!("{base}/json/version: {e}"))?;
            v["webSocketDebuggerUrl"].as_str().ok_or("no webSocketDebuggerUrl in /json/version")?.to_string()
        };
        return Browser::init(Cdp::connect(&ws).await?, None, "external", false, None, ctx).await;
    }
    let profile = profile_dir(ctx);
    if let Some((port, path)) = read_port_file(&profile) {
        if let Ok(c) = Cdp::connect(&format!("ws://127.0.0.1:{port}{path}")).await {
            return Browser::init(c, None, "adopted", false, Some(profile), ctx).await;
        }
    }
    let exe = find_chrome(&cfg.path)?;
    std::fs::create_dir_all(&profile).map_err(|e| format!("create {}: {e}", profile.display()))?;
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
    let mut c = std::process::Command::new(&exe);
    c.arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--disable-search-engine-choice-screen", "--hide-crash-restore-bubble"]);
    if headless {
        c.arg("--headless=new");
    }
    if !cfg.window_size.trim().is_empty() {
        c.arg(format!("--window-size={}", cfg.window_size.trim()));
    } else if headless {
        c.arg("--window-size=1366,900");
    }
    c.args(&cfg.args).arg("about:blank");
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = util::spawn_detached(c).map_err(|e| format!("failed to start {}: {e}", exe.display()))?;
    let pid = child.id();
    drop(child);
    let t0 = Instant::now();
    let mut last = String::new();
    while t0.elapsed() < Duration::from_secs(20) {
        if let Some((port, path)) = read_port_file(&profile) {
            match Cdp::connect(&format!("ws://127.0.0.1:{port}{path}")).await {
                Ok(cdp) => return Browser::init(cdp, Some(pid), "launched", headless, Some(profile), ctx).await,
                Err(e) => last = e,
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("{} started (pid {pid}) but DevTools never came up ({last}). Is this profile open in another window? {}", exe.display(), profile.display()))
}

impl Browser {
    async fn init(cdp: Arc<Cdp>, pid: Option<u32>, origin: &'static str, headless: bool, profile: Option<PathBuf>, ctx: &ToolCtx) -> Result<Browser, String> {
        let shared = Arc::new(Shared::default());
        pump(cdp.clone(), shared.clone());
        cdp.call(None, "Target.setDiscoverTargets", json!({"discover": true}), T).await?;
        let dl = downloads_dir(ctx);
        let _ = std::fs::create_dir_all(&dl);
        let _ = cdp.call(None, "Browser.setDownloadBehavior", json!({"behavior": "allow", "downloadPath": dl.display().to_string(), "eventsEnabled": true}), T).await;
        Ok(Browser { cdp, pid, origin, headless, profile, current: None, order: vec![], sessions: HashMap::new(), shared })
    }

    /// Page targets in stable (first-seen) order.
    async fn pages(&mut self) -> Result<Vec<Value>, String> {
        let r = self.cdp.call(None, "Target.getTargets", json!({}), T).await?;
        let all: Vec<Value> = r["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t["type"] == "page" && !t["url"].as_str().unwrap_or("").starts_with("devtools://"))
            .collect();
        let ids: Vec<String> = all.iter().filter_map(|t| t["targetId"].as_str().map(String::from)).collect();
        self.order.retain(|id| ids.contains(id));
        self.sessions.retain(|id, _| ids.contains(id));
        for id in &ids {
            if !self.order.contains(id) {
                self.order.push(id.clone());
            }
        }
        let mut out = vec![];
        for id in &self.order {
            if let Some(t) = all.iter().find(|t| t["targetId"] == id.as_str()) {
                out.push(t.clone());
            }
        }
        Ok(out)
    }

    async fn attach(&mut self, tid: &str) -> Result<String, String> {
        if let Some(s) = self.sessions.get(tid) {
            return Ok(s.clone());
        }
        let r = self.cdp.call(None, "Target.attachToTarget", json!({"targetId": tid, "flatten": true}), T).await?;
        let s = r["sessionId"].as_str().ok_or("attachToTarget returned no sessionId")?.to_string();
        for m in ["Page.enable", "Runtime.enable", "Log.enable"] {
            let _ = self.cdp.call(Some(&s), m, json!({}), T).await;
        }
        self.sessions.insert(tid.to_string(), s.clone());
        Ok(s)
    }

    /// Session of the current tab (first tab, or a new blank one, if none is chosen).
    async fn page(&mut self) -> Result<String, String> {
        let pages = self.pages().await?;
        let tid = match &self.current {
            Some(c) if pages.iter().any(|t| t["targetId"] == c.as_str()) => c.clone(),
            _ => match pages.first() {
                Some(t) => t["targetId"].as_str().unwrap_or("").to_string(),
                None => self.cdp.call(None, "Target.createTarget", json!({"url": "about:blank"}), T).await?["targetId"].as_str().unwrap_or("").to_string(),
            },
        };
        self.current = Some(tid.clone());
        self.attach(&tid).await
    }

    async fn call(&self, s: &str, method: &str, params: Value) -> Result<Value, String> {
        self.cdp.call(Some(s), method, params, T).await
    }

    async fn eval_t(&self, s: &str, expr: &str, timeout: Duration) -> Result<Value, String> {
        let r = self
            .cdp
            .call(Some(s), "Runtime.evaluate", json!({"expression": expr, "returnByValue": true, "awaitPromise": true, "userGesture": true}), timeout)
            .await?;
        if let Some(d) = r.get("exceptionDetails") {
            let msg = d["exception"]["description"].as_str().or(d["exception"]["value"].as_str()).or(d["text"].as_str()).unwrap_or("JavaScript error");
            return Err(first_lines(msg, 1).trim_start_matches("Error: ").to_string());
        }
        Ok(r["result"]["value"].clone())
    }

    async fn eval(&self, s: &str, expr: &str) -> Result<Value, String> {
        self.eval_t(s, expr, T).await
    }

    async fn state(&self, s: &str) -> (String, String) {
        match self.eval_t(s, js::STATE, Duration::from_secs(3)).await {
            Ok(v) => (v[1].as_str().unwrap_or("").to_string(), v[2].as_str().unwrap_or("").to_string()),
            Err(_) => (String::new(), String::new()),
        }
    }
}

// ------------------------------------------------------------------ helpers

fn spec_of(a: &BrowserArgs, text_targets: bool) -> Result<Value, String> {
    let mut spec = json!({});
    if let Some(r) = &a.ref_ {
        let n = match r {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().trim_matches(|c| c == '[' || c == ']').parse().ok(),
            _ => None,
        };
        spec["ref"] = json!(n.ok_or("ref must be a number from snapshot, e.g. 12")?);
    } else if let Some(s) = a.selector.as_deref().filter(|s| !s.trim().is_empty()) {
        spec["selector"] = json!(s.trim());
    } else if text_targets {
        if let Some(t) = a.text.as_deref().filter(|s| !s.trim().is_empty()) {
            spec["text"] = json!(t);
        }
    }
    Ok(spec)
}

fn has_target(spec: &Value) -> bool {
    spec.as_object().is_some_and(|o| !o.is_empty())
}

struct Found {
    x: f64,
    y: f64,
    l: f64,
    t: f64,
    w: f64,
    h: f64,
    desc: String,
    covered: String,
}

async fn find(b: &Browser, s: &str, spec: &Value) -> Result<Found, String> {
    let v = b.eval(s, &js::find(spec)).await?;
    let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
    Ok(Found {
        x: f("x"),
        y: f("y"),
        l: f("l"),
        t: f("t"),
        w: f("w"),
        h: f("h"),
        desc: v["desc"].as_str().unwrap_or("").to_string(),
        covered: v["covered"].as_str().unwrap_or("").to_string(),
    })
}

async fn mouse(b: &Browser, s: &str, kind: &str, x: f64, y: f64, button: &str, buttons: u32, count: u32) -> Result<(), String> {
    b.call(s, "Input.dispatchMouseEvent", json!({"type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": count}))
        .await
        .map(|_| ())
}

async fn click_at(b: &Browser, s: &str, x: f64, y: f64, button: &str, count: u32) -> Result<(), String> {
    let mask = match button {
        "right" => 2,
        "middle" => 4,
        _ => 1,
    };
    mouse(b, s, "mouseMoved", x, y, "none", 0, 0).await?;
    for i in 1..=count {
        mouse(b, s, "mousePressed", x, y, button, mask, i).await?;
        mouse(b, s, "mouseReleased", x, y, button, 0, i).await?;
    }
    Ok(())
}

fn key_def(part: &str, shift: bool) -> Result<(String, String, i64, Option<String>), String> {
    let n = part.to_ascii_lowercase();
    let named = |k: &str, vk: i64, t: Option<&str>| Ok((k.to_string(), k.to_string(), vk, t.map(String::from)));
    match n.as_str() {
        "enter" | "return" => named("Enter", 13, Some("\r")),
        "tab" => named("Tab", 9, None),
        "esc" | "escape" => named("Escape", 27, None),
        "backspace" | "bksp" => named("Backspace", 8, None),
        "delete" | "del" => named("Delete", 46, None),
        "insert" | "ins" => named("Insert", 45, None),
        "home" => named("Home", 36, None),
        "end" => named("End", 35, None),
        "pageup" | "pgup" => named("PageUp", 33, None),
        "pagedown" | "pgdn" => named("PageDown", 34, None),
        "left" | "arrowleft" => named("ArrowLeft", 37, None),
        "up" | "arrowup" => named("ArrowUp", 38, None),
        "right" | "arrowright" => named("ArrowRight", 39, None),
        "down" | "arrowdown" => named("ArrowDown", 40, None),
        "space" | "spacebar" => Ok((" ".into(), "Space".into(), 32, Some(" ".into()))),
        "plus" => Ok(("+".into(), "Equal".into(), 187, Some("+".into()))),
        "minus" => Ok(("-".into(), "Minus".into(), 189, Some("-".into()))),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let k: i64 = f[1..].parse().map_err(|_| format!("bad key `{part}`"))?;
            if !(1..=24).contains(&k) {
                return Err(format!("bad key `{part}`"));
            }
            let name = format!("F{k}");
            Ok((name.clone(), name, 111 + k, None))
        }
        _ => {
            let mut it = part.chars();
            let c = it.next().ok_or("empty key")?;
            if it.next().is_some() {
                return Err(format!("unknown key `{part}`"));
            }
            if c.is_ascii_alphabetic() {
                let up = c.to_ascii_uppercase();
                let k = if shift { up } else { c.to_ascii_lowercase() };
                Ok((k.to_string(), format!("Key{up}"), up as i64, Some(k.to_string())))
            } else if c.is_ascii_digit() {
                Ok((c.to_string(), format!("Digit{c}"), c as i64, Some(c.to_string())))
            } else {
                Ok((c.to_string(), String::new(), 0, Some(c.to_string())))
            }
        }
    }
}

async fn press(b: &Browser, s: &str, chord: &str) -> Result<(), String> {
    let parts: Vec<String> = if chord.ends_with("++") {
        let mut p: Vec<String> = chord[..chord.len() - 2].split('+').filter(|x| !x.is_empty()).map(String::from).collect();
        p.push("plus".into());
        p
    } else if chord == "+" {
        vec!["plus".into()]
    } else {
        chord.split('+').map(String::from).collect()
    };
    let (mut mods, mut key) = (0i64, None);
    for p in &parts {
        match p.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => mods |= 2,
            "alt" | "option" => mods |= 1,
            "shift" => mods |= 8,
            "meta" | "cmd" | "command" | "win" | "super" => mods |= 4,
            _ => key = Some(p.trim().to_string()),
        }
    }
    let key = key.ok_or(format!("`{chord}` has no key besides modifiers"))?;
    let (k, code, vk, text) = key_def(&key, mods & 8 != 0)?;
    let text = if mods & 7 != 0 { None } else { text };
    let mut down = json!({"type": if text.is_some() { "keyDown" } else { "rawKeyDown" }, "modifiers": mods, "key": k, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk});
    if let Some(t) = &text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    if mods == 2 && k.eq_ignore_ascii_case("a") {
        down["commands"] = json!(["selectAll"]);
    }
    b.call(s, "Input.dispatchKeyEvent", down).await?;
    b.call(s, "Input.dispatchKeyEvent", json!({"type": "keyUp", "modifiers": mods, "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk}))
        .await?;
    Ok(())
}

async fn wait_event(rx: &mut broadcast::Receiver<Arc<Event>>, s: &str, names: &[&str], max: Duration, new_tabs: &mut Vec<String>) -> Option<String> {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        };
        if ev.method == "Target.targetCreated" {
            let ti = &ev.params["targetInfo"];
            if ti["type"] == "page" {
                if let Some(id) = ti["targetId"].as_str() {
                    new_tabs.push(id.to_string());
                }
            }
            continue;
        }
        if ev.session.as_deref() == Some(s) && names.contains(&ev.method.as_str()) {
            return Some(ev.method.clone());
        }
    }
}

/// After an input action: follow a navigation / new tab and describe where we are.
async fn after(b: &mut Browser, s: &str, mut rx: broadcast::Receiver<Arc<Event>>, log0: usize, url0: &str, max: Duration) -> Result<String, String> {
    let mut new_tabs = vec![];
    let started = wait_event(&mut rx, s, &["Page.frameStartedLoading", "Page.frameRequestedNavigation", "Page.navigatedWithinDocument"], Duration::from_millis(400), &mut new_tabs).await;
    if let Some(m) = &started {
        if m != "Page.navigatedWithinDocument" {
            let t0 = Instant::now();
            loop {
                let left = max.saturating_sub(t0.elapsed());
                match wait_event(&mut rx, s, &["Page.loadEventFired", "Page.frameStoppedLoading"], left, &mut new_tabs).await.as_deref() {
                    Some("Page.loadEventFired") | None => break,
                    _ => {
                        if let Ok(v) = b.eval_t(s, js::STATE, Duration::from_secs(2)).await {
                            if v[0] == "complete" {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    let mut out = String::new();
    let mut s = s.to_string();
    if let Some(tid) = new_tabs.last().cloned() {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pages = b.pages().await?;
        if let Some(i) = pages.iter().position(|t| t["targetId"] == tid.as_str()) {
            b.current = Some(tid.clone());
            s = b.attach(&tid).await?;
            out.push_str(&format!("opened new tab [{}] and switched to it\n", i + 1));
            let _ = b.eval_t(&s, "new Promise(r => document.readyState === 'complete' ? r() : addEventListener('load', () => r(), { once: true }))", Duration::from_secs(10)).await;
        }
    }
    let (url, title) = b.state(&s).await;
    let nav = if url != url0 && !url.is_empty() { "navigated -> " } else { "" };
    out.push_str(&format!("{nav}\"{}\" {url}", util::one_line(&title, 120)));
    let fresh: Vec<String> = b.shared.since(&s, log0).into_iter().filter(|l| l.contains("[error]") || l.contains("[exception]") || l.contains("[dialog")).collect();
    if !fresh.is_empty() {
        out.push_str(&format!("\n{} new console event(s):", fresh.len()));
        for l in fresh.iter().take(5) {
            out.push_str(&format!("\n  {}", util::one_line(l, 300)));
        }
    }
    Ok(out)
}

fn view_opts(a: &BrowserArgs, ctx: &ToolCtx, origin: (i32, i32), coord: &'static str) -> Result<ViewOpts, String> {
    let cfg = &ctx.app.cfg.image;
    let fmt = match a.format.as_deref().unwrap_or("auto") {
        "auto" | "" => Fmt::Auto,
        "png" => Fmt::Png,
        "jpeg" | "jpg" => Fmt::Jpeg,
        o => return Err(format!("unknown format `{o}` (auto|png|jpeg)")),
    };
    Ok(ViewOpts {
        region: a.region.clone(),
        tile: a.tile.clone(),
        scale: a.scale,
        budget: a.max_bytes.unwrap_or(cfg.max_bytes).max(4096),
        max_dim: a.max_dim.unwrap_or(cfg.max_dim),
        fmt,
        min_q: cfg.min_quality,
        origin,
        coord,
    })
}

fn normalize_url(u: &str) -> String {
    let u = u.trim();
    if u.contains("://") || u.starts_with("about:") || u.starts_with("data:") || u.starts_with("file:") || u.starts_with("chrome:") || u.starts_with("javascript:") {
        return u.to_string();
    }
    if u.starts_with("localhost") || u.starts_with("127.") || u.starts_with("[::1]") {
        return format!("http://{u}");
    }
    if Path::new(u).is_absolute() && Path::new(u).exists() {
        return url::Url::from_file_path(u).map(|x| x.to_string()).unwrap_or_else(|_| u.to_string());
    }
    format!("https://{u}")
}

// ------------------------------------------------------------------ dispatch

async fn run(a: BrowserArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let action = a.action.trim().to_ascii_lowercase();
    let mut guard = BROWSER.lock().await;
    if guard.as_ref().is_some_and(|b| !b.cdp.alive()) {
        *guard = None;
    }
    match action.as_str() {
        "status" => {
            let Some(b) = guard.as_mut() else {
                let prof = profile_dir(&ctx);
                let exe = find_chrome(&ctx.app.cfg.browser.path).map(|p| p.display().to_string()).unwrap_or_else(|e| e);
                return Ok(ToolOutput::text(format!("browser not running (starts on first use)\nexecutable: {exe}\nprofile: {}", prof.display())));
            };
            let v = b.cdp.call(None, "Browser.getVersion", json!({}), T).await?;
            let pages = b.pages().await?;
            let cur = b.current.clone();
            let mut s = format!(
                "{} ({}{}{})\nprofile: {}\ntabs: {}",
                v["product"].as_str().unwrap_or("?"),
                b.origin,
                b.pid.map(|p| format!(", pid {p}")).unwrap_or_default(),
                if b.headless { ", headless" } else { "" },
                b.profile.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(external)".into()),
                pages.len()
            );
            if let Some(t) = pages.iter().find(|t| Some(t["targetId"].as_str().unwrap_or("").to_string()) == cur) {
                s.push_str(&format!("\ncurrent: \"{}\" {}", t["title"].as_str().unwrap_or(""), t["url"].as_str().unwrap_or("")));
            }
            return Ok(ToolOutput::text(s));
        }
        "quit" | "close_browser" => {
            let Some(b) = guard.take() else { return Ok(ToolOutput::text("browser was not running")) };
            let _ = b.cdp.call(None, "Browser.close", json!({}), Duration::from_secs(5)).await;
            return Ok(ToolOutput::text("browser closed (profile and logins kept)"));
        }
        "launch" => {
            let want = a.headless.unwrap_or(ctx.app.cfg.browser.headless);
            if let Some(b) = guard.as_ref() {
                if b.headless != want && b.origin != "external" {
                    let _ = b.cdp.call(None, "Browser.close", json!({}), Duration::from_secs(5)).await;
                    *guard = None;
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
            }
            if guard.is_none() {
                *guard = Some(connect_or_launch(&ctx, want).await?);
            }
            let b = guard.as_mut().unwrap();
            let v = b.cdp.call(None, "Browser.getVersion", json!({}), T).await?;
            return Ok(ToolOutput::text(format!("{} ready ({}{})", v["product"].as_str().unwrap_or("browser"), b.origin, if b.headless { ", headless" } else { "" })));
        }
        _ => {}
    }
    if guard.is_none() {
        *guard = Some(connect_or_launch(&ctx, ctx.app.cfg.browser.headless).await?);
    }
    let b = guard.as_mut().unwrap();
    let r = act(b, &action, &a, &ctx).await;
    if !b.cdp.alive() {
        *guard = None;
    }
    r
}

async fn act(b: &mut Browser, action: &str, a: &BrowserArgs, ctx: &ToolCtx) -> Result<ToolOutput, String> {
    match action {
        "tabs" => {
            let pages = b.pages().await?;
            let cur = b.current.clone().unwrap_or_default();
            let mut s = format!("{} tab(s) (* = current)", pages.len());
            for (i, t) in pages.iter().enumerate() {
                let star = if t["targetId"] == cur.as_str() { "*" } else { " " };
                s.push_str(&format!("\n{star}[{}] \"{}\" {}", i + 1, util::one_line(t["title"].as_str().unwrap_or(""), 100), t["url"].as_str().unwrap_or("")));
            }
            Ok(ToolOutput::text(s))
        }
        "tab" | "switch" => {
            let pages = b.pages().await?;
            let i = a.index.ok_or("tab needs `index` (1-based, see tabs)")?;
            let t = pages.get(i.wrapping_sub(1)).ok_or(format!("no tab [{i}]; there are {}", pages.len()))?;
            let tid = t["targetId"].as_str().unwrap_or("").to_string();
            let _ = b.cdp.call(None, "Target.activateTarget", json!({"targetId": tid}), T).await;
            b.current = Some(tid.clone());
            let s = b.attach(&tid).await?;
            let (url, title) = b.state(&s).await;
            Ok(ToolOutput::text(format!("switched to tab [{i}] \"{title}\" {url}")))
        }
        "close" | "close_tab" => {
            let pages = b.pages().await?;
            let tid = match a.index {
                Some(i) => pages.get(i.wrapping_sub(1)).ok_or(format!("no tab [{i}]"))?["targetId"].as_str().unwrap_or("").to_string(),
                None => b.current.clone().ok_or("no current tab")?,
            };
            b.cdp.call(None, "Target.closeTarget", json!({"targetId": tid}), T).await?;
            if b.current.as_deref() == Some(tid.as_str()) {
                b.current = None;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            let n = b.pages().await?.len();
            Ok(ToolOutput::text(format!("tab closed; {n} left")))
        }
        "open" | "goto" | "navigate" => {
            let url = normalize_url(a.url.as_deref().or(a.text.as_deref()).ok_or("open needs `url`")?);
            let max = Duration::from_millis(a.timeout_ms.unwrap_or(30000));
            let s = if a.new_tab.unwrap_or(false) {
                let tid = b.cdp.call(None, "Target.createTarget", json!({"url": "about:blank"}), T).await?["targetId"].as_str().unwrap_or("").to_string();
                b.pages().await?;
                b.current = Some(tid.clone());
                b.attach(&tid).await?
            } else {
                b.page().await?
            };
            let mut rx = b.cdp.events.subscribe();
            let log0 = b.shared.count(&s);
            let r = b.call(&s, "Page.navigate", json!({"url": url})).await?;
            if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                return Err(format!("could not open {url}: {e}"));
            }
            let mut nt = vec![];
            let t0 = Instant::now();
            let loaded = wait_event(&mut rx, &s, &["Page.loadEventFired"], max, &mut nt).await.is_some();
            let mut out = after(b, &s, rx, log0, "", Duration::from_millis(1)).await?;
            out = out.trim_start_matches("navigated -> ").to_string();
            let ms = t0.elapsed().as_millis();
            let tail = if loaded { format!("loaded in {ms} ms") } else { format!("still loading after {ms} ms") };
            Ok(ToolOutput::text(format!("{out}\n{tail} - next: snapshot")))
        }
        "back" | "forward" | "reload" => {
            let s = b.page().await?;
            let rx = b.cdp.events.subscribe();
            let log0 = b.shared.count(&s);
            let (url0, _) = b.state(&s).await;
            match action {
                "reload" => {
                    b.call(&s, "Page.reload", json!({})).await?;
                }
                _ => {
                    let h = b.call(&s, "Page.getNavigationHistory", json!({})).await?;
                    let i = h["currentIndex"].as_i64().unwrap_or(0) + if action == "back" { -1 } else { 1 };
                    let e = h["entries"].as_array().and_then(|v| v.get(i.max(0) as usize).cloned()).filter(|_| i >= 0).ok_or(format!("nothing to go {action} to"))?;
                    b.call(&s, "Page.navigateToHistoryEntry", json!({"entryId": e["id"]})).await?;
                }
            }
            let out = after(b, &s, rx, log0, if action == "reload" { "" } else { &url0 }, Duration::from_secs(20)).await?;
            Ok(ToolOutput::text(out))
        }
        "snapshot" | "read" | "outline" => {
            let s = b.page().await?;
            let max = a.max_chars.unwrap_or(15000).clamp(500, 200_000);
            let v = b.eval(&s, &js::snapshot(max, a.filter.as_deref().unwrap_or(""), a.interactive_only.unwrap_or(false))).await?;
            let lines: Vec<&str> = v["lines"].as_array().map(|l| l.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let (vh, dh, sy) = (v["vh"].as_f64().unwrap_or(0.0), v["dh"].as_f64().unwrap_or(0.0), v["sy"].as_f64().unwrap_or(0.0));
            let mut out = format!(
                "\"{}\" {}\nviewport {}x{} at y={} of {} px · {} refs (⋯ = off-screen; refs reset on every snapshot)\n",
                v["title"].as_str().unwrap_or(""),
                v["url"].as_str().unwrap_or(""),
                v["vw"],
                vh,
                sy,
                dh,
                v["refs"]
            );
            out.push_str(&lines.join("\n"));
            let more = v["more"].as_u64().unwrap_or(0);
            if more > 0 {
                out.push_str(&format!("\n… {more} more chars cut: raise max_chars, use filter, interactive_only, or text"));
            }
            if lines.is_empty() {
                out.push_str("(nothing visible matched)");
            }
            Ok(ToolOutput::text(out))
        }
        "click" | "double_click" | "right_click" | "hover" => {
            let s = b.page().await?;
            let spec = spec_of(a, true)?;
            let (x, y, what, warn) = if has_target(&spec) {
                let f = find(b, &s, &spec).await?;
                let warn = if f.covered.is_empty() { String::new() } else { format!("\nnote: covered by <{}> at that point; the click went to it (close overlays or try another ref)", f.covered) };
                (f.x, f.y, f.desc, warn)
            } else if let (Some(x), Some(y)) = (a.x, a.y) {
                (x, y, String::new(), String::new())
            } else {
                return Err(format!("{action} needs ref (from snapshot), selector, text, or x+y"));
            };
            let rx = b.cdp.events.subscribe();
            let log0 = b.shared.count(&s);
            let (url0, _) = b.state(&s).await;
            let verb = if action == "hover" {
                mouse(b, &s, "mouseMoved", x, y, "none", 0, 0).await?;
                "hovered".to_string()
            } else {
                let button = if action == "right_click" { "right".to_string() } else { a.button.clone().unwrap_or_else(|| "left".into()).to_ascii_lowercase() };
                let count = if action == "double_click" { 2 } else { a.count.unwrap_or(1).clamp(1, 3) };
                click_at(b, &s, x, y, &button, count).await?;
                match (button.as_str(), count) {
                    ("left", 1) => "clicked".to_string(),
                    ("left", 2) => "double-clicked".to_string(),
                    (bt, n) => format!("{bt}-clicked x{n}"),
                }
            };
            let target = if what.is_empty() { String::new() } else { format!(" {what}") };
            let st = after(b, &s, rx, log0, &url0, Duration::from_secs(15)).await?;
            Ok(ToolOutput::text(format!("{verb}{target} at ({x:.0},{y:.0}){warn}\n{st}")))
        }
        "type" | "fill" => {
            let s = b.page().await?;
            let text = a.text.as_deref().or(a.value.as_deref()).ok_or("type needs `text`")?;
            let spec = spec_of(a, false)?;
            let f = find(b, &s, &spec).await?;
            let clear = a.clear.unwrap_or(true);
            let focused = b.eval(&s, &js::fill(js::FOCUS, "__CLEAR__", &json!(clear))).await?.as_bool().unwrap_or(false);
            if !focused {
                click_at(b, &s, f.x, f.y, "left", 1).await?;
                if clear {
                    press(b, &s, "ctrl+a").await?;
                }
            }
            let rx = b.cdp.events.subscribe();
            let log0 = b.shared.count(&s);
            let (url0, _) = b.state(&s).await;
            if text.is_empty() {
                if clear {
                    press(b, &s, "Delete").await?;
                }
            } else {
                b.call(&s, "Input.insertText", json!({"text": text})).await?;
            }
            if a.submit.unwrap_or(false) {
                press(b, &s, "Enter").await?;
            }
            let st = after(b, &s, rx, log0, &url0, Duration::from_secs(15)).await?;
            let n = text.chars().count();
            Ok(ToolOutput::text(format!(
                "typed {n} chars into {}{}{}\n{st}",
                f.desc,
                if clear { " (replaced)" } else { "" },
                if a.submit.unwrap_or(false) { " + Enter" } else { "" }
            )))
        }
        "key" | "press" => {
            let s = b.page().await?;
            let spec = a.keys.as_deref().or(a.text.as_deref()).ok_or("key needs `keys`, e.g. \"Enter\" or \"ctrl+a\"")?.to_string();
            let tgt = spec_of(a, false)?;
            if has_target(&tgt) {
                find(b, &s, &tgt).await?;
                b.eval(&s, &js::fill(js::FOCUS, "__CLEAR__", &json!(false))).await?;
            }
            let rx = b.cdp.events.subscribe();
            let log0 = b.shared.count(&s);
            let (url0, _) = b.state(&s).await;
            let mut done = vec![];
            for chord in spec.split([' ', ',']).map(str::trim).filter(|c| !c.is_empty()) {
                let (c, times) = match chord.rsplit_once('*') {
                    Some((c, n)) if !n.is_empty() && n.chars().all(|d| d.is_ascii_digit()) => (c, n.parse::<u32>().unwrap_or(1).clamp(1, 100)),
                    _ => (chord, 1),
                };
                for _ in 0..times {
                    press(b, &s, c).await?;
                }
                done.push(chord.to_string());
            }
            let st = after(b, &s, rx, log0, &url0, Duration::from_secs(15)).await?;
            Ok(ToolOutput::text(format!("pressed {}\n{st}", done.join(" "))))
        }
        "select" => {
            let s = b.page().await?;
            let spec = spec_of(a, true)?;
            if !has_target(&spec) {
                return Err("select needs ref or selector of the <select>".into());
            }
            let v = a.value.as_deref().ok_or("select needs `value` (option value or label)")?;
            let f = find(b, &s, &spec).await?;
            let rx = b.cdp.events.subscribe();
            let log0 = b.shared.count(&s);
            let (url0, _) = b.state(&s).await;
            let label = b.eval(&s, &js::fill(js::SELECT, "__VALUE__", &json!(v))).await?;
            let st = after(b, &s, rx, log0, &url0, Duration::from_secs(15)).await?;
            Ok(ToolOutput::text(format!("selected \"{}\" in {}\n{st}", label.as_str().unwrap_or(v), f.desc)))
        }
        "scroll" => {
            let s = b.page().await?;
            let spec = spec_of(a, true)?;
            if has_target(&spec) && a.dy.is_none() && a.dx.is_none() {
                let f = find(b, &s, &spec).await?;
                return Ok(ToolOutput::text(format!("scrolled {} into view (now at {:.0},{:.0})", f.desc, f.x, f.y)));
            }
            let vp = b.eval(&s, "[innerWidth, innerHeight]").await?;
            let (vw, vh) = (vp[0].as_f64().unwrap_or(1200.0), vp[1].as_f64().unwrap_or(800.0));
            let (x, y) = if has_target(&spec) {
                let f = find(b, &s, &json!({"ref": spec.get("ref"), "selector": spec.get("selector"), "text": spec.get("text"), "scroll": false})).await?;
                (f.x, f.y)
            } else {
                (a.x.unwrap_or(vw / 2.0), a.y.unwrap_or(vh / 2.0))
            };
            let dy = a.dy.unwrap_or(if a.dx.is_some() { 0.0 } else { vh * 0.85 });
            b.call(&s, "Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": a.dx.unwrap_or(0.0), "deltaY": dy})).await?;
            tokio::time::sleep(Duration::from_millis(350)).await;
            let p = b.eval(&s, "[Math.round(scrollX), Math.round(scrollY), Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0), innerHeight]").await?;
            Ok(ToolOutput::text(format!("scrolled by ({:.0},{dy:.0}); page now at y={} of {} (viewport {})", a.dx.unwrap_or(0.0), p[1], p[2], p[3])))
        }
        "wait" => {
            let s = b.page().await?;
            let max = Duration::from_millis(a.timeout_ms.unwrap_or(10000));
            let q = json!({"selector": a.selector, "text": a.text, "url": a.value.as_deref().or(a.url.as_deref()), "gone": a.gone.unwrap_or(false)});
            let expr = js::fill(js::WAIT, "__Q__", &q);
            let t0 = Instant::now();
            loop {
                // the page may be mid-navigation: re-resolve the session and ignore errors
                let s2 = b.page().await.unwrap_or_else(|_| s.clone());
                if let Ok(Value::Bool(true)) = b.eval_t(&s2, &expr, Duration::from_secs(3)).await {
                    let (url, title) = b.state(&s2).await;
                    return Ok(ToolOutput::text(format!("ok after {} ms: \"{title}\" {url}", t0.elapsed().as_millis())));
                }
                if t0.elapsed() >= max || ctx.cancel.is_cancelled() {
                    return Err(format!("timed out after {} ms waiting for {}", t0.elapsed().as_millis(), q));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        "screenshot" | "shot" => {
            let s = b.page().await?;
            let spec = spec_of(a, true)?;
            let dpr = b.eval(&s, "devicePixelRatio").await?.as_f64().unwrap_or(1.0).max(0.1);
            let full = a.full_page.unwrap_or(false);
            let (params, origin, coord, label) = if has_target(&spec) {
                let f = find(b, &s, &spec).await?;
                let sc = b.eval(&s, "[scrollX, scrollY]").await?;
                let (sx, sy) = (sc[0].as_f64().unwrap_or(0.0), sc[1].as_f64().unwrap_or(0.0));
                let clip = json!({"x": f.l + sx, "y": f.t + sy, "width": f.w.max(1.0), "height": f.h.max(1.0), "scale": 1});
                (json!({"format": "png", "clip": clip, "captureBeyondViewport": true}), (f.l.round() as i32, f.t.round() as i32), "viewport", format!("element {}", f.desc))
            } else if full {
                let m = b.call(&s, "Page.getLayoutMetrics", json!({})).await?;
                let cs = &m["cssContentSize"];
                let (w, h) = (cs["width"].as_f64().unwrap_or(1280.0), cs["height"].as_f64().unwrap_or(800.0).min(16000.0));
                (json!({"format": "png", "clip": {"x": 0, "y": 0, "width": w, "height": h, "scale": 1}, "captureBeyondViewport": true}), (0, 0), "page", "full page".to_string())
            } else {
                (json!({"format": "png"}), (0, 0), "viewport", "viewport".to_string())
            };
            let r = b.call(&s, "Page.captureScreenshot", params).await?;
            let bytes = util::b64_decode(r["data"].as_str().ok_or("no screenshot data")?)?;
            let mut im = img::decode_bytes(&bytes)?;
            if (dpr - 1.0).abs() > 0.01 {
                let (w, h) = (im.width() as f64 / dpr, im.height() as f64 / dpr);
                im = im.resize_exact(w.round().max(1.0) as u32, h.round().max(1.0) as u32, image::imageops::FilterType::Triangle);
            }
            let (url, title) = b.state(&s).await;
            let mut out = ToolOutput::empty();
            out.push_text(format!("\"{}\" {url}", util::one_line(&title, 100)));
            img::view_into(&im, &label, &view_opts(a, ctx, origin, coord)?, &mut out)?;
            Ok(out)
        }
        "text" => {
            let s = b.page().await?;
            let max = a.max_chars.unwrap_or(20000).clamp(200, 500_000);
            let off = a.offset.unwrap_or(0);
            let expr = js::TEXT.replace("__SEL__", &json!(a.selector).to_string()).replace("__OFF__", &off.to_string()).replace("__MAX__", &max.to_string());
            let v = b.eval(&s, &expr).await?;
            let total = v["total"].as_u64().unwrap_or(0) as usize;
            let text = v["text"].as_str().unwrap_or("");
            let end = off + text.chars().count().min(max);
            let mut out = format!("\"{}\" {} · chars {off}..{} of {total}\n", v["title"].as_str().unwrap_or(""), v["url"].as_str().unwrap_or(""), end.min(total));
            out.push_str(text);
            if end < total {
                out.push_str(&format!("\n… more: text {{offset: {end}}}"));
            }
            Ok(ToolOutput::text(out))
        }
        "html" => {
            let s = b.page().await?;
            let html = b.eval(&s, &js::fill(js::HTML, "__SEL__", &json!(a.selector))).await?;
            let html = html.as_str().unwrap_or("");
            let max = a.max_chars.unwrap_or(30000);
            if html.chars().count() <= max {
                return Ok(ToolOutput::text(html.to_string()));
            }
            let dir = ctx.app.paths.out.join("browser");
            let _ = std::fs::create_dir_all(&dir);
            let p = dir.join(format!("page-{}.html", chrono::Local::now().format("%Y%m%d-%H%M%S")));
            std::fs::write(&p, html).map_err(|e| format!("save html: {e}"))?;
            let head: String = html.chars().take(max).collect();
            Ok(ToolOutput::text(format!("{} chars; full HTML saved to {} (read it with fs grep/read)\n{head}\n…", html.chars().count(), p.display())))
        }
        "eval" | "js" => {
            let s = b.page().await?;
            let code = a.js.as_deref().or(a.text.as_deref()).ok_or("eval needs `js`")?;
            let mut r = b
                .call(&s, "Runtime.evaluate", json!({"expression": code, "returnByValue": true, "awaitPromise": true, "userGesture": true, "replMode": true}))
                .await?;
            let illegal_return = r["exceptionDetails"]["exception"]["description"].as_str().is_some_and(|d| d.contains("Illegal return"));
            if illegal_return {
                let wrapped = format!("(async () => {{\n{code}\n}})()");
                r = b.call(&s, "Runtime.evaluate", json!({"expression": wrapped, "returnByValue": true, "awaitPromise": true, "userGesture": true})).await?;
            }
            if let Some(d) = r.get("exceptionDetails") {
                let msg = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("JavaScript error");
                return Ok(ToolOutput::error(first_lines(msg, 8)));
            }
            let res = &r["result"];
            let text = match res.get("value") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                None => res["description"].as_str().or(res["type"].as_str()).unwrap_or("undefined").to_string(),
            };
            Ok(ToolOutput::text(util::clip_middle(&text, a.max_chars.unwrap_or(30000))))
        }
        "console" | "logs" => {
            let s = b.page().await?;
            let lines = b.shared.since(&s, 0);
            let n = lines.len();
            if a.clear_log.unwrap_or(false) {
                b.shared.logs.lock().unwrap().remove(&s);
            }
            if n == 0 {
                return Ok(ToolOutput::text("console is empty for this tab (since nerve attached)"));
            }
            let start = n.saturating_sub(120);
            Ok(ToolOutput::text(format!("{n} console event(s){}:\n{}", if start > 0 { ", last 120" } else { "" }, lines[start..].join("\n"))))
        }
        "upload" => {
            let s = b.page().await?;
            let spec = spec_of(a, true)?;
            if !has_target(&spec) {
                return Err("upload needs ref or selector of the file input".into());
            }
            let paths = a.paths.clone().filter(|p| !p.is_empty()).ok_or("upload needs `paths`")?;
            let base = ctx.app.default_dir.display().to_string();
            let mut files = vec![];
            for p in &paths {
                let full = util::resolve_path(p, Some(&base));
                if !full.is_file() {
                    return Err(format!("not a file: {}", full.display()));
                }
                files.push(full.display().to_string());
            }
            let f = find(b, &s, &spec).await?;
            let r = b.call(&s, "Runtime.evaluate", json!({"expression": "window.__ncur"})).await?;
            let oid = r["result"]["objectId"].as_str().ok_or("element handle unavailable")?.to_string();
            b.call(&s, "DOM.setFileInputFiles", json!({"files": files, "objectId": oid})).await?;
            Ok(ToolOutput::text(format!("attached {} file(s) to {}", files.len(), f.desc)))
        }
        "downloads" => {
            let dir = downloads_dir(ctx);
            let mut s = format!("download folder: {}", dir.display());
            let ev: Vec<String> = b.shared.downloads.lock().unwrap().iter().cloned().collect();
            if !ev.is_empty() {
                s.push_str("\nrecent events:");
                for l in ev.iter().rev().take(20) {
                    s.push_str(&format!("\n  {l}"));
                }
            }
            if let Ok(rd) = std::fs::read_dir(&dir) {
                let mut fs: Vec<_> = rd.flatten().filter_map(|e| Some((e.metadata().ok()?, e.file_name().to_string_lossy().into_owned()))).collect();
                fs.sort_by_key(|(m, _)| std::cmp::Reverse(m.modified().ok()));
                s.push_str(&format!("\nfiles ({}):", fs.len()));
                for (m, n) in fs.iter().take(30) {
                    s.push_str(&format!("\n  {n} ({})", util::fmt_bytes(m.len())));
                }
            }
            Ok(ToolOutput::text(s))
        }
        o => Err(crate::tools::unknown_action(
            "browser",
            o,
            "open, snapshot, click, double_click, right_click, hover, type, key, select, scroll, wait, screenshot, text, html, eval, tabs, tab, close, back, forward, reload, console, upload, downloads, status, launch, quit",
        )),
    }
}
