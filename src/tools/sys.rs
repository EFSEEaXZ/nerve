//! `sys`: system info, processes, environment, nerve's own status and log.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::mcp::{OrErr, ToolCtx, ToolDef, ToolOutput, blocking, tool};
use crate::util::{self, fmt_bytes, fmt_dur_ms};

#[derive(Deserialize, JsonSchema)]
pub struct SysArgs {
    /// info | ps | proc | kill | env | status | log | restart | update | tunnel_restart
    action: String,
    /// ps/kill: process name filter (case-insensitive substring); env: variable name filter; log: regex filter.
    #[serde(alias = "name", alias = "query")]
    filter: Option<String>,
    /// proc/kill: process id.
    pid: Option<u32>,
    /// kill: several pids.
    pids: Option<Vec<u32>>,
    /// ps: sort by cpu (default) | mem | name | pid | start
    sort: Option<String>,
    /// ps: max rows (default 40); log: lines (default 80).
    limit: Option<usize>,
    /// kill: also kill child processes (default true).
    tree: Option<bool>,
    /// ps: show full command lines.
    full: Option<bool>,
    /// update: optional LOCAL binary path for recovery/development. Omit to update from GitHub Releases; URLs are rejected.
    #[serde(alias = "path", alias = "url")]
    source: Option<String>,
}

const DESC: &str = "System and server. Actions: info (OS, CPU, RAM, disks, GPU, uptime); \
ps {filter?, sort?: cpu|mem|name|pid|start, limit?}; proc {pid} (details: exe, cmdline, cwd, parent, children); \
kill {pid|pids|filter, tree?} (kills the process tree); env {filter?}; \
status (nerve: version, sessions, jobs, tunnel URL, auth); log {limit?, filter?} (nerve's own log); \
restart (restart nerve; jobs and tunnel keep running, clients reconnect transparently); \
update {source?: local recovery path} (default: verified EFSEEaXZ/nerve release, background install); tunnel_restart.";

pub fn def() -> ToolDef {
    tool::<SysArgs, _, _>("sys", "System", DESC, json!({"destructiveHint": true, "openWorldHint": false}), run)
}

async fn run(a: SysArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    match a.action.as_str() {
        "info" => blocking(info).await,
        "ps" | "processes" | "top" => ps(a).await,
        "proc" | "process" => blocking(move || proc_info(a.pid)).await,
        "kill" => blocking(move || kill(a)).await,
        "env" => Ok(env(a.filter.as_deref())),
        "status" => Ok(status(&ctx)),
        "log" | "logs" => {
            let dir = ctx.app.paths.logs.clone();
            blocking(move || log(&dir, a.limit.unwrap_or(80), a.filter.as_deref())).await
        }
        "restart" => {
            ctx.app.life.request(crate::control::Quit::Restart, true, Duration::from_millis(500));
            Ok(ToolOutput::text(format!(
                "nerve is restarting ({}); jobs and the tunnel keep running. Call any tool again in ~2 s.",
                if ctx.app.supervised { "supervised by the service" } else { "unsupervised: a successor process takes over" }
            )))
        }
        "update" | "upgrade" => {
            let Some(src) = a.source else {
                let queued = crate::updates::Updates::request(ctx.app.clone());
                return Ok(ToolOutput::text(if queued { "GitHub release update queued; inspect sys status for progress" } else { "an update is already in progress" }));
            };
            if src.contains("://") { return Err("Updates are pinned to EFSEEaXZ/nerve releases; source accepts only a local recovery path".into()); }
            let (v, exe) = crate::control::update(&ctx.app.http, &src).await?;
            ctx.app.life.set_successor(exe.clone());
            ctx.app.life.request(crate::control::Quit::Restart, true, Duration::from_millis(500));
            Ok(ToolOutput::text(format!("installed {v} at {}; restarting now (jobs and tunnel keep running). Call `sys status` in ~3 s.", exe.display())))
        }
        "tunnel_restart" => {
            ctx.app.tunnel.request_restart();
            Ok(ToolOutput::text("tunnel restart requested"))
        }
        other => Err(super::unknown_action("sys", other, "info, ps, proc, kill, env, status, log, restart, update, tunnel_restart")),
    }
}

fn gpu_info() -> Option<String> {
    let exe = util::which("nvidia-smi")?;
    let mut c = std::process::Command::new(exe);
    c.args(["--query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu,driver_version", "--format=csv,noheader"]);
    util::hide_window_std(&mut c);
    let o = c.output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}


fn info() -> Result<ToolOutput, String> {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250)));
    sys.refresh_cpu_usage();
    let cpus = sys.cpus();
    let brand = cpus.first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    let mut s = format!(
        "host: {}  user: {}{}\nos: {} ({}, {})\nuptime: {}\ncpu: {brand} · {} logical cores · {:.0}% busy\nram: {} used of {} ({} free) · swap {} / {}\n",
        System::host_name().unwrap_or_default(),
        whoami(),
        if util::is_elevated() { " (elevated/admin)" } else { "" },
        System::long_os_version().unwrap_or_else(|| std::env::consts::OS.into()),
        System::kernel_version().unwrap_or_default(),
        std::env::consts::ARCH,
        fmt_dur_ms(System::uptime() * 1000),
        cpus.len(),
        sys.global_cpu_usage(),
        fmt_bytes(sys.used_memory()),
        fmt_bytes(sys.total_memory()),
        fmt_bytes(sys.available_memory()),
        fmt_bytes(sys.used_swap()),
        fmt_bytes(sys.total_swap()),
    );
    if let Some(g) = gpu_info() {
        s.push_str(&format!("gpu: {g}\n"));
    }
    s.push_str("disks:\n");
    let disks = Disks::new_with_refreshed_list();
    let mut dj = vec![];
    for d in disks.list() {
        let (t, f) = (d.total_space(), d.available_space());
        if t == 0 {
            continue;
        }
        s.push_str(&format!(
            "  {} {} — {} free of {} ({:.0}% used){}\n",
            d.mount_point().display(),
            d.file_system().to_string_lossy(),
            fmt_bytes(f),
            fmt_bytes(t),
            (t - f) as f64 * 100.0 / t as f64,
            if d.is_removable() { ", removable" } else { "" }
        ));
        dj.push(json!({"mount": d.mount_point(), "fs": d.file_system().to_string_lossy(), "total": t, "free": f}));
    }
    Ok(ToolOutput::text(s.trim_end().to_string()).with_structured(json!({
        "host": System::host_name(), "os": System::long_os_version(), "cpu": brand, "cores": cpus.len(),
        "ram_total": sys.total_memory(), "ram_used": sys.used_memory(), "disks": dj,
    })))
}

fn whoami() -> String {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default()
}

fn proc_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::OnlyIfNotSet).with_cmd(UpdateKind::OnlyIfNotSet)
}

async fn ps(a: SysArgs) -> Result<ToolOutput, String> {
    blocking(move || {
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind());
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(300)));
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cpu().with_memory());
        let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
        let f = a.filter.as_deref().map(str::to_lowercase);
        let mut rows: Vec<&sysinfo::Process> = sys
            .processes()
            .values()
            .filter(|p| p.thread_kind().is_none())
            .filter(|p| match &f {
                None => true,
                Some(f) => {
                    p.name().to_string_lossy().to_lowercase().contains(f)
                        || p.pid().as_u32().to_string() == *f
                        || p.cmd().iter().any(|c| c.to_string_lossy().to_lowercase().contains(f))
                }
            })
            .collect();
        match a.sort.as_deref().unwrap_or("cpu") {
            "mem" | "memory" | "ram" => rows.sort_by(|x, y| y.memory().cmp(&x.memory())),
            "name" => rows.sort_by_key(|p| p.name().to_string_lossy().to_lowercase()),
            "pid" => rows.sort_by_key(|p| p.pid().as_u32()),
            "start" | "new" | "newest" => rows.sort_by(|x, y| y.start_time().cmp(&x.start_time())),
            _ => rows.sort_by(|x, y| y.cpu_usage().total_cmp(&x.cpu_usage()).then(y.memory().cmp(&x.memory()))),
        }
        let total = rows.len();
        let lim = a.limit.unwrap_or(40).max(1);
        let full = a.full.unwrap_or(false);
        let mut s = format!("{total} processes{}\n   PID   PPID   CPU%       MEM  NAME / COMMAND\n", f.as_ref().map(|f| format!(" matching `{f}`")).unwrap_or_default());
        let mut arr = vec![];
        for p in rows.iter().take(lim) {
            let cmd: Vec<String> = p.cmd().iter().map(|c| c.to_string_lossy().to_string()).collect();
            let cmdl = cmd.join(" ");
            let cmdl = if full { cmdl } else { util::clip_middle(&cmdl, 110).replace('\n', " ") };
            let name = p.name().to_string_lossy().to_string();
            s.push_str(&format!(
                "{:>6} {:>6} {:>6.1} {:>9}  {}{}\n",
                p.pid().as_u32(),
                p.parent().map(|x| x.as_u32().to_string()).unwrap_or_default(),
                p.cpu_usage() / ncpu,
                fmt_bytes(p.memory()),
                name,
                if cmdl.is_empty() { String::new() } else { format!("  {cmdl}") }
            ));
            arr.push(json!({"pid": p.pid().as_u32(), "name": name, "cpu": p.cpu_usage() / ncpu, "mem": p.memory(), "cmd": cmd}));
        }
        if total > lim {
            s.push_str(&format!("… {} more (raise limit or use filter)\n", total - lim));
        }
        Ok(ToolOutput::text(s.trim_end().to_string()).with_structured(json!({"processes": arr, "total": total})))
    })
    .await
}

fn proc_info(pid: Option<u32>) -> Result<ToolOutput, String> {
    let pid = pid.ok_or("missing required parameter `pid`")?;
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything().without_environ().without_tasks(),
    );
    let p = sys.process(Pid::from_u32(pid)).ok_or_else(|| format!("no process with pid {pid}"))?;
    let children: Vec<String> = sys
        .processes()
        .values()
        .filter(|c| c.parent() == Some(p.pid()) && c.thread_kind().is_none())
        .map(|c| format!("{} {}", c.pid().as_u32(), c.name().to_string_lossy()))
        .collect();
    let parent = p.parent().and_then(|pp| sys.process(pp)).map(|pp| format!("{} {}", pp.pid().as_u32(), pp.name().to_string_lossy()));
    let cmd: Vec<String> = p.cmd().iter().map(|c| c.to_string_lossy().to_string()).collect();
    let s = format!(
        "pid {pid}: {}\nstatus: {}  started: {} ({} ago)\nexe: {}\ncwd: {}\ncmd: {}\nmemory: {} (virtual {})\nparent: {}\nchildren ({}): {}",
        p.name().to_string_lossy(),
        p.status(),
        util::fmt_ts(p.start_time()),
        fmt_dur_ms(p.run_time() * 1000),
        p.exe().map(|e| e.display().to_string()).unwrap_or_else(|| "?".into()),
        p.cwd().map(|e| e.display().to_string()).unwrap_or_else(|| "?".into()),
        cmd.join(" "),
        fmt_bytes(p.memory()),
        fmt_bytes(p.virtual_memory()),
        parent.unwrap_or_else(|| "-".into()),
        children.len(),
        if children.is_empty() { "-".into() } else { children.join(", ") },
    );
    Ok(ToolOutput::text(s))
}

fn kill(a: SysArgs) -> Result<ToolOutput, String> {
    let me = std::process::id();
    let mut targets: Vec<(u32, String)> = vec![];
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    for pid in a.pid.into_iter().chain(a.pids.into_iter().flatten()) {
        let name = sys.process(Pid::from_u32(pid)).map(|p| p.name().to_string_lossy().to_string()).unwrap_or_else(|| "?".into());
        targets.push((pid, name));
    }
    if let Some(f) = &a.filter {
        let f = f.to_lowercase();
        let f = f.strip_suffix(".exe").unwrap_or(&f).to_string();
        for p in sys.processes().values().filter(|p| p.thread_kind().is_none()) {
            let n = p.name().to_string_lossy().to_lowercase();
            let n = n.strip_suffix(".exe").unwrap_or(&n);
            if n == f {
                targets.push((p.pid().as_u32(), p.name().to_string_lossy().to_string()));
            }
        }
        if targets.is_empty() {
            return Err(format!("no process named `{f}` (exact name match; see ps)"));
        }
    }
    if targets.is_empty() {
        return Err("give pid, pids or filter (exact process name)".into());
    }
    let tree = a.tree.unwrap_or(true);
    let mut lines = vec![];
    let mut failed = 0;
    for (pid, name) in targets {
        if pid == me {
            lines.push(format!("skipped {pid} ({name}): that is nerve itself (use the service controls to restart)"));
            continue;
        }
        let ok = if tree {
            util::kill_tree(pid, true)
        } else {
            sys.process(Pid::from_u32(pid)).map(|p| p.kill()).unwrap_or(false)
        };
        std::thread::sleep(Duration::from_millis(150));
        let alive = util::pid_alive(pid);
        if alive {
            failed += 1;
        }
        lines.push(format!(
            "{} {pid} ({name}){}",
            if alive { "FAILED to kill" } else { "killed" },
            if alive && !ok { ": access denied or protected process" } else { "" }
        ));
    }
    Ok(ToolOutput::text(lines.join("\n")).error_if(failed > 0))
}

fn env(filter: Option<&str>) -> ToolOutput {
    let f = filter.map(str::to_lowercase);
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().to_string(), v.to_string_lossy().to_string()))
        .filter(|(k, _)| f.as_ref().is_none_or(|f| k.to_lowercase().contains(f)))
        .collect();
    vars.sort_by_key(|(k, _)| k.to_lowercase());
    let mut s = String::new();
    for (k, v) in &vars {
        if k.eq_ignore_ascii_case("path") || k.eq_ignore_ascii_case("pathext") || k.eq_ignore_ascii_case("psmodulepath") {
            s.push_str(&format!("{k}:\n"));
            for p in std::env::split_paths(v) {
                s.push_str(&format!("  {}\n", p.display()));
            }
        } else {
            s.push_str(&format!("{k}={}\n", util::clip_middle(v, 500)));
        }
    }
    if vars.is_empty() {
        s = "no matching variables".into();
    }
    ToolOutput::text(s.trim_end().to_string())
}

fn status(ctx: &ToolCtx) -> ToolOutput {
    let app = &ctx.app;
    let st = crate::server::status_json(app);
    let t = app.tunnel.status();
    let (clients, tokens) = app.store.counts();
    let mut s = format!(
        "nerve {} · pid {} · up {}{}\nhost: {} ({})\nlocal: http://{}/mcp\npublic: {} (listening {})\ntunnel: {} · {}{}{}\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        fmt_dur_ms(app.started.elapsed().as_millis() as u64),
        format!(
            "{}{}{}",
            if app.supervised { " · service" } else { "" },
            if app.elevated { " · admin" } else { "" },
            util::session_id().map(|s| format!(" · session {s}")).unwrap_or_default()
        ),
        app.hostname,
        std::env::consts::OS,
        app.cfg.server.local_addr,
        st["public_url"].as_str().map(|u| format!("{}/mcp", u.trim_end_matches('/'))).unwrap_or_else(|| "-".into()),
        app.cfg.server.public_addr,
        t.provider,
        t.state,
        if t.restarts > 0 { format!(" (restarted {}x)", t.restarts) } else { String::new() },
        if t.adopted { " (adopted)" } else { "" },
    );
    s.push_str(&format!(
        "auth: PIN {} · {clients} OAuth clients · {tokens} API tokens\njobs running: {}\ntools: {}\ndata: {}\ndashboard: http://{}/ui\nsessions ({}):\n",
        if app.store.has_pin() { "set" } else { "NOT SET (public access disabled until `nerve pin set`)" },
        app.jobs.running_count(),
        app.registry.names().join(", "),
        app.paths.data.display(),
        app.cfg.server.local_addr,
        app.sessions.count(),
    ));
    for x in app.sessions.summary().iter().take(10) {
        s.push_str(&format!(
            "  {} {} · seen {} ago · {} in flight\n",
            x["client"].as_str().unwrap_or("?"),
            x["protocol"].as_str().unwrap_or(""),
            fmt_dur_ms(util::now_ms().saturating_sub(x["last_seen"].as_u64().unwrap_or(0))),
            x["inflight"]
        ));
    }
    let updates = app.updates.status(app.cfg.updates.automatic);
    s.push_str(&format!("updates: {} · {}\n", crate::updates::REPOSITORY, updates["message"].as_str().unwrap_or("not checked")));
    ToolOutput::text(s.trim_end().to_string()).with_structured(st)
}

fn log(dir: &std::path::Path, n: usize, filter: Option<&str>) -> Result<ToolOutput, String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ctx(format!("read {}", dir.display()))?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("nerve"))
        .collect();
    files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let f = files.last().ok_or("no log file yet")?.path();
    let text = if let Some(pat) = filter {
        let re = regex::Regex::new(pat).ctx("bad filter regex")?;
        let all = std::fs::read_to_string(&f).ctx("read log")?;
        let hits: Vec<&str> = all.lines().filter(|l| re.is_match(l)).collect();
        hits[hits.len().saturating_sub(n)..].join("\n")
    } else {
        crate::jobs::tail_lines(&f, n, 200_000).ctx("read log")?.0
    };
    Ok(ToolOutput::text(format!("{}\n{}", f.display(), text)))
}