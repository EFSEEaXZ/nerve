//! `exec`: run shell commands, argv, or inline code — foreground (streamed progress) or detached jobs.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::jobs::JobMeta;
use crate::mcp::{OrErr, ToolCtx, ToolDef, ToolOutput, tool};
use crate::server::App;
use crate::util::{self, fmt_bytes, fmt_dur_ms, now_ms};

#[derive(Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Shell command line (runs in `shell`).
    command: Option<String>,
    /// Program and arguments, executed directly without a shell (no quoting problems).
    argv: Option<Vec<String>>,
    /// Inline source code to run with `lang`.
    code: Option<String>,
    /// Language for `code`: python (default) | node | powershell | pwsh | bash | sh | cmd.
    lang: Option<String>,
    /// Arguments passed to the `code` script.
    args: Option<Vec<String>>,
    /// Shell for `command`: auto (pwsh/powershell on Windows, bash/sh elsewhere) | powershell | pwsh | cmd | bash | sh | zsh.
    shell: Option<String>,
    /// Working directory.
    cwd: Option<String>,
    /// Extra environment variables.
    env: Option<HashMap<String, String>>,
    /// Text written to stdin (otherwise stdin is closed, so nothing ever hangs on input).
    stdin: Option<String>,
    /// Timeout in ms. 0 = wait forever (default).
    #[serde(alias = "timeout")]
    timeout_ms: Option<u64>,
    /// Start as a detached job and return its id immediately (survives server restarts). Use for servers, watchers, long builds.
    background: Option<bool>,
    /// On timeout, keep the process running as a job instead of killing it.
    keep_on_timeout: Option<bool>,
    /// Inline bytes per stream (0 = unlimited). Longer output is clipped in the middle; the full text is saved to a file.
    max_output: Option<usize>,
    /// Interpreter override for `code` (e.g. a specific python.exe).
    interpreter: Option<String>,
    /// Label for background jobs.
    label: Option<String>,
}

const DESC: &str = "Run a shell command, a program (argv) or inline code (python/node/powershell/bash). \
Returns exit code, duration, stdout and stderr. timeout_ms 0 = wait forever (default). \
background:true starts a detached job (returns job id; manage with the `job` tool). \
Python code auto-uses the nearest .venv. Output beyond max_output is clipped in the middle and saved in full to a file.";

pub fn def() -> ToolDef {
    tool::<ExecArgs, _, _>("exec", "Run command", DESC, json!({"destructiveHint": true, "openWorldHint": true}), run)
}

pub struct Prepared {
    pub program: String,
    pub args: Vec<String>,
    pub raw: Option<String>,
    pub env: HashMap<String, String>,
    pub display: String,
    pub cwd: PathBuf,
    pub temp: Option<PathBuf>,
}

fn auto_shell() -> String {
    static AUTO: OnceLock<String> = OnceLock::new();
    AUTO.get_or_init(|| {
        if cfg!(windows) {
            if util::which("pwsh").is_some() { "pwsh".into() } else { "powershell".into() }
        } else if util::which("bash").is_some() {
            "bash".into()
        } else {
            "sh".into()
        }
    })
    .clone()
}

fn resolve_shell(s: &str) -> String {
    let s = s.trim().to_lowercase();
    if s.is_empty() || s == "auto" { auto_shell() } else { s }
}

fn utf16_b64(s: &str) -> String {
    let bytes: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    util::b64(&bytes)
}

/// (program, args, raw-arg, temp file)
fn shell_invocation(app: &App, shell: &str, command: &str) -> Result<(String, Vec<String>, Option<String>, Option<PathBuf>), String> {
    match shell {
        "powershell" | "pwsh" => {
            let script = format!(
                "$ProgressPreference='SilentlyContinue'\ntry {{ [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 }} catch {{}}\n$OutputEncoding=[System.Text.Encoding]::UTF8\n& {{\n{command}\n}}\n$__ok=$?\nif (-not $__ok) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 1 }} }}\nexit 0\n"
            );
            let mut args: Vec<String> = vec!["-NoProfile".into(), "-NonInteractive".into(), "-OutputFormat".into(), "Text".into()];
            if cfg!(windows) {
                args.push("-ExecutionPolicy".into());
                args.push("Bypass".into());
            }
            let enc = utf16_b64(&script);
            if enc.len() < 30_000 {
                args.push("-EncodedCommand".into());
                args.push(enc);
                Ok((shell.to_string(), args, None, None))
            } else {
                // too long for a command line: run from a temp .ps1 (UTF-8 BOM so Windows PowerShell reads it right)
                let f = app.paths.tmp.join(format!("nerve-{}.ps1", util::rand_token(6)));
                let mut data = vec![0xEF, 0xBB, 0xBF];
                data.extend_from_slice(script.as_bytes());
                std::fs::write(&f, data).ctx("write temp script")?;
                args.push("-File".into());
                args.push(f.display().to_string());
                Ok((shell.to_string(), args, None, Some(f)))
            }
        }
        "cmd" => {
            if !cfg!(windows) {
                return Err("cmd is only available on Windows".into());
            }
            Ok(("cmd".into(), vec![], Some(format!("/d /s /c \"chcp 65001>nul & {command}\"")), None))
        }
        "bash" | "sh" | "zsh" | "fish" | "dash" | "ksh" => Ok((shell.to_string(), vec!["-c".into(), command.to_string()], None, None)),
        other => {
            if util::which(other).is_some() {
                Ok((other.to_string(), vec!["-c".into(), command.to_string()], None, None))
            } else {
                Err(format!("unknown shell `{other}` (use auto, powershell, pwsh, cmd, bash, sh, zsh)"))
            }
        }
    }
}

pub fn resolve_python(explicit: Option<&str>, cwd: &Path) -> (String, Vec<String>) {
    if let Some(e) = explicit.filter(|s| !s.trim().is_empty()) {
        return (e.to_string(), vec![]);
    }
    if let Ok(e) = std::env::var("NERVE_PYTHON") {
        if !e.is_empty() {
            return (e, vec![]);
        }
    }
    let mut dir = Some(cwd);
    for _ in 0..5 {
        let Some(d) = dir else { break };
        for v in [".venv", "venv", "env"] {
            let p = if cfg!(windows) { d.join(v).join("Scripts").join("python.exe") } else { d.join(v).join("bin").join("python") };
            if p.is_file() {
                return (p.display().to_string(), vec![]);
            }
        }
        dir = d.parent();
    }
    if cfg!(windows) {
        if util::which("py").is_some() {
            return ("py".into(), vec!["-3".into()]);
        }
    } else if util::which("python3").is_some() {
        return ("python3".into(), vec![]);
    }
    ("python".into(), vec![])
}

fn prepare(a: &ExecArgs, app: &App) -> Result<Prepared, String> {
    let cwd = match a.cwd.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(c) => util::resolve_path(c, Some(&app.default_dir.display().to_string())),
        None => app.default_dir.clone(),
    };
    if !cwd.is_dir() {
        return Err(format!("cwd does not exist: {}", cwd.display()));
    }
    let mut env = a.env.clone().unwrap_or_default();
    if let Some(argv) = a.argv.as_ref().filter(|v| !v.is_empty()) {
        return Ok(Prepared {
            program: argv[0].clone(),
            args: argv[1..].to_vec(),
            raw: None,
            display: argv.join(" "),
            env,
            cwd,
            temp: None,
        });
    }
    if let Some(code) = &a.code {
        let lang = a.lang.as_deref().unwrap_or("python").trim().to_lowercase();
        let script_args = a.args.clone().unwrap_or_default();
        let tag = util::rand_token(6);
        return match lang.as_str() {
            "python" | "py" | "python3" => {
                let (interp, mut pre) = resolve_python(a.interpreter.as_deref(), &cwd);
                let f = app.paths.tmp.join(format!("nerve-{tag}.py"));
                std::fs::write(&f, code).ctx("write temp script")?;
                pre.push("-u".into());
                pre.push(f.display().to_string());
                pre.extend(script_args);
                for (k, v) in [("PYTHONUTF8", "1"), ("PYTHONIOENCODING", "utf-8"), ("PYTHONUNBUFFERED", "1"), ("PYTHONDONTWRITEBYTECODE", "1")] {
                    env.entry(k.into()).or_insert_with(|| v.into());
                }
                Ok(Prepared { display: format!("{interp} <inline python>"), program: interp, args: pre, raw: None, env, cwd, temp: Some(f) })
            }
            "node" | "js" | "javascript" | "mjs" => {
                let f = app.paths.tmp.join(format!("nerve-{tag}.mjs"));
                std::fs::write(&f, code).ctx("write temp script")?;
                let mut args = vec![f.display().to_string()];
                args.extend(script_args);
                let prog = a.interpreter.clone().unwrap_or_else(|| "node".into());
                Ok(Prepared { display: format!("{prog} <inline js>"), program: prog, args, raw: None, env, cwd, temp: Some(f) })
            }
            "powershell" | "pwsh" | "ps" | "ps1" | "bash" | "sh" | "zsh" | "cmd" => {
                let shell = match lang.as_str() {
                    "ps" | "ps1" => resolve_shell("auto").replace("bash", "pwsh").replace("sh", "pwsh"),
                    l => l.to_string(),
                };
                let (program, args, raw, temp) = shell_invocation(app, &shell, code)?;
                Ok(Prepared { display: format!("{shell} <inline>"), program, args, raw, env, cwd, temp })
            }
            other => Err(format!("unsupported lang `{other}`")),
        };
    }
    if let Some(command) = a.command.as_ref().filter(|c| !c.trim().is_empty()) {
        let shell = resolve_shell(a.shell.as_deref().unwrap_or(&app.cfg.exec.default_shell));
        let (program, args, raw, temp) = shell_invocation(app, &shell, command)?;
        return Ok(Prepared { display: command.clone(), program, args, raw, env, cwd, temp });
    }
    Err("provide `command`, `argv` or `code`".into())
}

fn std_command(p: &Prepared) -> std::process::Command {
    let mut c = std::process::Command::new(&p.program);
    c.args(&p.args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(raw) = &p.raw {
            c.raw_arg(raw);
        }
    }
    c.current_dir(&p.cwd);
    c.envs(&p.env);
    c
}

fn spawn_err(p: &Prepared, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("program not found: `{}` (is it installed and on PATH?)", p.program)
    } else {
        format!("failed to start `{}`: {e}", p.program)
    }
}

// ---------------------------------------------------------------- output collection

struct Collector {
    limit: usize,
    all: Vec<u8>,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    spill: Option<std::fs::File>,
    spill_path: PathBuf,
    spilled: bool,
}

impl Collector {
    fn new(limit: usize, spill_path: PathBuf) -> Self {
        Self { limit, all: Vec::new(), head: Vec::new(), tail: VecDeque::new(), total: 0, spill: None, spill_path, spilled: false }
    }
    fn tail_cap(&self) -> usize {
        self.limit - self.limit / 4
    }
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if self.spilled {
            if let Some(f) = self.spill.as_mut() {
                let _ = f.write_all(chunk);
            }
            if self.limit > 0 {
                self.tail.extend(chunk.iter().copied());
                let cap = self.tail_cap();
                if self.tail.len() > cap {
                    let excess = self.tail.len() - cap;
                    self.tail.drain(..excess);
                }
            } else {
                self.all.extend_from_slice(chunk);
            }
            return;
        }
        self.all.extend_from_slice(chunk);
        if self.limit > 0 && self.all.len() > self.limit {
            self.start_spill(true);
        }
    }
    /// Start writing everything to the spill file. `clip`: switch to head/tail mode.
    fn start_spill(&mut self, clip: bool) {
        if self.spilled {
            return;
        }
        if let Ok(mut f) = std::fs::File::create(&self.spill_path) {
            let _ = f.write_all(&self.all);
            self.spill = Some(f);
        }
        self.spilled = true;
        if clip && self.limit > 0 {
            let head_len = (self.limit / 4).min(self.all.len());
            self.head = self.all[..head_len].to_vec();
            let start = self.all.len().saturating_sub(self.tail_cap()).max(head_len);
            self.tail = self.all[start..].iter().copied().collect();
            self.all = Vec::new();
        }
    }
    fn render(&self) -> String {
        if self.head.is_empty() && self.tail.is_empty() {
            return String::from_utf8_lossy(&self.all).into_owned();
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let omitted = self.total.saturating_sub((self.head.len() + tail.len()) as u64);
        format!(
            "{}\n…[{} omitted — full output: {}]…\n{}",
            String::from_utf8_lossy(&self.head),
            fmt_bytes(omitted),
            self.spill_path.display(),
            String::from_utf8_lossy(&tail)
        )
    }
    fn last_line(&self) -> String {
        let bytes: Vec<u8> = if !self.tail.is_empty() {
            let n = self.tail.len();
            self.tail.iter().skip(n.saturating_sub(512)).copied().collect()
        } else {
            let n = self.all.len();
            self.all[n.saturating_sub(512)..].to_vec()
        };
        util::one_line(&String::from_utf8_lossy(&bytes), 160)
    }
}

async fn pump<R: AsyncRead + Unpin>(mut r: R, c: Arc<Mutex<Collector>>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => c.lock().unwrap().push(&buf[..n]),
        }
    }
}

fn clean_stderr(s: String) -> String {
    if !s.trim_start().starts_with("#< CLIXML") {
        return s;
    }
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?s)<S S="Error">(.*?)</S>"#).unwrap());
    let mut out = String::new();
    for c in re.captures_iter(&s) {
        out.push_str(&c[1]);
    }
    out.replace("_x000D__x000A_", "\n")
        .replace("_x000A_", "\n")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn format_result(code: Option<i32>, status: &str, dur_ms: u64, so: &str, se: &str, extra: &str) -> ToolOutput {
    let mut s = format!("exit={} · {}", code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()), fmt_dur_ms(dur_ms));
    if status != "exited" {
        s.push_str(&format!(" · {status}"));
    }
    if !extra.is_empty() {
        s.push_str(" · ");
        s.push_str(extra);
    }
    let (so_t, se_t) = (so.trim_end(), se.trim_end());
    if !so_t.is_empty() {
        s.push('\n');
        s.push_str(so_t);
    }
    if !se_t.is_empty() {
        s.push_str("\n[stderr]\n");
        s.push_str(se_t);
    }
    if so_t.is_empty() && se_t.is_empty() {
        s.push_str("\n(no output)");
    }
    ToolOutput::text(s)
        .with_structured(json!({"exit_code": code, "status": status, "duration_ms": dur_ms, "stdout": so, "stderr": se}))
        .error_if(code != Some(0))
}

enum Outcome {
    Exit(Option<i32>),
    Timeout,
    Cancel,
}

async fn foreground(ctx: &ToolCtx, p: &Prepared, a: &ExecArgs, timeout: Option<Duration>) -> Result<ToolOutput, String> {
    let app = &ctx.app;
    let limit = a.max_output.unwrap_or(app.cfg.exec.inline_output_bytes);
    let mut sc = std_command(p);
    util::hide_window_std(&mut sc);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        sc.process_group(0);
    }
    let mut cmd = tokio::process::Command::from(sc);
    cmd.stdin(if a.stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let t0 = Instant::now();
    let mut child = cmd.spawn().map_err(|e| spawn_err(p, e))?;
    let pid = child.id().unwrap_or(0);
    if let (Some(s), Some(mut si)) = (a.stdin.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            let _ = si.write_all(s.as_bytes()).await;
            let _ = si.shutdown().await;
        });
    }
    let tag = util::rand_token(6);
    let oc = Arc::new(Mutex::new(Collector::new(limit, app.paths.out.join(format!("exec-{tag}.out.log")))));
    let ec = Arc::new(Mutex::new(Collector::new(limit, app.paths.out.join(format!("exec-{tag}.err.log")))));
    let po = tokio::spawn(pump(child.stdout.take().expect("stdout"), oc.clone()));
    let pe = tokio::spawn(pump(child.stderr.take().expect("stderr"), ec.clone()));
    let (abort_o, abort_e) = (po.abort_handle(), pe.abort_handle());

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let sleep = async {
        match timeout {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(sleep);
    let progress = ctx.progress.enabled();
    let outcome = loop {
        tokio::select! {
            st = child.wait() => break Outcome::Exit(st.ok().and_then(|s| s.code())),
            _ = &mut sleep => break Outcome::Timeout,
            _ = ctx.cancel.cancelled() => break Outcome::Cancel,
            _ = tick.tick(), if progress => {
                let line = {
                    let o = oc.lock().unwrap().last_line();
                    if o.is_empty() { ec.lock().unwrap().last_line() } else { o }
                };
                ctx.progress.send(format!("{} · {line}", fmt_dur_ms(t0.elapsed().as_millis() as u64)), None);
            }
        }
    };
    let drain = |ms: u64| async move {
        // grandchildren may keep the pipes open forever — never wait on them indefinitely
        let drained = tokio::time::timeout(Duration::from_millis(ms), async {
            let _ = po.await;
            let _ = pe.await;
        }).await.is_ok();
        abort_o.abort();
        abort_e.abort();
        drained
    };
    match outcome {
        Outcome::Exit(code) => {
            let drained = drain(250).await;
            let dur = t0.elapsed().as_millis() as u64;
            let (so, se) = (oc.lock().unwrap().render(), clean_stderr(ec.lock().unwrap().render()));
            Ok(format_result(code, "exited", dur, &so, &se, if drained { "" } else { "output pipes still open after parent exit; capture stopped (use background for services)" }))
        }
        Outcome::Timeout if a.keep_on_timeout.unwrap_or(false) => {
            let id = app.jobs.new_id();
            let (_, _, exit_file) = app.jobs.paths(&id);
            let (so, se, out_path, err_path) = {
                let mut o = oc.lock().unwrap();
                let mut e = ec.lock().unwrap();
                let r = (o.render(), clean_stderr(e.render()));
                o.start_spill(false);
                e.start_spill(false);
                (r.0, r.1, o.spill_path.clone(), e.spill_path.clone())
            };
            app.jobs.insert(JobMeta {
                id: id.clone(),
                pid,
                command: p.display.clone(),
                label: a.label.clone().unwrap_or_default(),
                cwd: p.cwd.display().to_string(),
                started: now_ms() - t0.elapsed().as_millis() as u64,
                stdout: out_path,
                stderr: err_path,
                exit_file: exit_file.clone(),
                finished: None,
                exit_code: None,
                killed: false,
                kind: "attached".into(),
            });
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
                drain(3000).await;
                let _ = util::atomic_write(&exit_file, format!("{code}\n").as_bytes());
            });
            let dur = t0.elapsed().as_millis() as u64;
            Ok(format_result(None, "timed_out", dur, &so, &se, &format!("still running as job {id} (pid {pid})")).error_if(false))
        }
        Outcome::Timeout => {
            util::kill_tree(pid, true);
            let _ = child.kill().await;
            drain(1000).await;
            let dur = t0.elapsed().as_millis() as u64;
            let (so, se) = (oc.lock().unwrap().render(), clean_stderr(ec.lock().unwrap().render()));
            Ok(format_result(None, "timed_out", dur, &so, &se, "killed (use keep_on_timeout or background)"))
        }
        Outcome::Cancel => {
            util::kill_tree(pid, true);
            let _ = child.kill().await;
            drain(0).await;
            Err("cancelled by client; process tree killed".into())
        }
    }
}


async fn background(ctx: &ToolCtx, p: &Prepared, a: &ExecArgs) -> Result<ToolOutput, String> {
    let app = &ctx.app;
    let id = app.jobs.new_id();
    let (out, err, exit_file) = app.jobs.paths(&id);
    let exe = std::env::current_exe().ctx("locate nerve executable")?;
    let mut c = std::process::Command::new(&exe);
    c.arg("__run").arg("--stdout").arg(&out).arg("--stderr").arg(&err).arg("--exit-file").arg(&exit_file).arg("--cwd").arg(&p.cwd);
    if let Some(s) = &a.stdin {
        let f = app.jobs.dir().join(format!("{id}.stdin"));
        std::fs::write(&f, s).ctx("write stdin file")?;
        c.arg("--stdin-file").arg(f);
    }
    if let Some(raw) = &p.raw {
        c.arg("--raw").arg(raw);
    }
    c.arg("--").arg(&p.program).args(&p.args);
    c.envs(&p.env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = util::spawn_detached(c).map_err(|e| format!("failed to start job runner: {e}"))?;
    let pid = child.id();
    #[cfg(unix)]
    std::thread::spawn(move || {
        let mut ch = child;
        let _ = ch.wait();
    });
    #[cfg(not(unix))]
    drop(child);
    app.jobs.insert(JobMeta {
        id: id.clone(),
        pid,
        command: p.display.clone(),
        label: a.label.clone().unwrap_or_default(),
        cwd: p.cwd.display().to_string(),
        started: now_ms(),
        stdout: out.clone(),
        stderr: err.clone(),
        exit_file,
        finished: None,
        exit_code: None,
        killed: false,
        kind: "detached".into(),
    });
    tracing::info!("job {id} started (pid {pid}): {}", util::one_line(&p.display, 120));
    Ok(ToolOutput::text(format!(
        "started job {id} (pid {pid})\nstdout: {}\nstderr: {}\nnext: job {{\"action\":\"logs\",\"id\":\"{id}\"}} · {{\"action\":\"wait\"}} · {{\"action\":\"kill\"}}",
        out.display(),
        err.display()
    ))
    .with_structured(json!({"job_id": id, "pid": pid, "stdout": out, "stderr": err})))
}

async fn run(a: ExecArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let p = prepare(&a, &ctx.app)?;
    if a.background.unwrap_or(false) {
        return background(&ctx, &p, &a).await;
    }
    let tms = a.timeout_ms.unwrap_or(ctx.app.cfg.exec.default_timeout_ms);
    let timeout = (tms > 0).then(|| Duration::from_millis(tms.max(100)));
    let r = foreground(&ctx, &p, &a, timeout).await;
    if let Some(t) = &p.temp {
        let t = t.clone();
        // give interpreters a moment to finish reading the file if the process was kept alive
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = std::fs::remove_file(t);
        });
    }
    r
}