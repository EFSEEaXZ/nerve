//! Windows service: SCM glue + supervisor that keeps `nerve serve` running in the user's session.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_LINKED_TOKEN, TokenLinkedToken};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSQueryUserToken, WTSUserName,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess, OpenProcess,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};

use super::{ServiceCmd, ServiceFile};
use crate::config::{Config, Paths};

const NAME: &str = "nerve";
const DISPLAY: &str = "nerve MCP server";
const DESCRIPTION: &str = "nerve — MCP server for full PC control. Supervises `nerve serve` in the user's session.";

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

// ------------------------------------------------------------------ CLI

pub fn cli(cmd: ServiceCmd, paths: Paths) -> anyhow::Result<()> {
    match cmd {
        ServiceCmd::Run => run_dispatcher(paths),
        ServiceCmd::Install { user, no_headless, no_elevate, no_start } => install(&paths, user, !no_headless, !no_elevate, !no_start),
        ServiceCmd::Uninstall => uninstall(),
        ServiceCmd::Start => {
            let svc = open(ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
            svc.start(&[] as &[&OsStr]).context("start service")?;
            wait_state(&svc, ServiceState::Running, 20)?;
            println!("service started");
            Ok(())
        }
        ServiceCmd::Stop => {
            let svc = open(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
            stop_and_wait(&svc)?;
            println!("service stopped");
            Ok(())
        }
        ServiceCmd::Restart => {
            let svc = open(ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
            stop_and_wait(&svc)?;
            svc.start(&[] as &[&OsStr]).context("start service")?;
            wait_state(&svc, ServiceState::Running, 20)?;
            println!("service restarted");
            Ok(())
        }
        ServiceCmd::Status => status(&paths),
    }
}

fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access).context("open service manager (run elevated?)")
}

fn open(access: ServiceAccess) -> anyhow::Result<windows_service::service::Service> {
    manager(ServiceManagerAccess::CONNECT)?.open_service(NAME, access).context("open service `nerve` (installed? elevated?)")
}

fn wait_state(svc: &windows_service::service::Service, want: ServiceState, secs: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let st = svc.query_status()?.current_state;
        if st == want {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("service is {st:?}, expected {want:?}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn stop_and_wait(svc: &windows_service::service::Service) -> anyhow::Result<()> {
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        wait_state(svc, ServiceState::Stopped, 30)?;
    }
    Ok(())
}

fn install(paths: &Paths, user: Option<String>, headless: bool, elevated: bool, start: bool) -> anyhow::Result<()> {
    if !crate::util::is_elevated() {
        bail!("run `nerve service install` from an elevated (administrator) terminal");
    }
    // 1. put the binary at <data>/bin/nerve.exe (the service and the server run from there)
    std::fs::create_dir_all(&paths.bin)?;
    let bin = paths.bin.join("nerve.exe");
    let cur = crate::control::current_exe();
    let same = std::fs::canonicalize(&cur).ok() == std::fs::canonicalize(&bin).ok() && bin.exists();
    if !same {
        let tmp = paths.bin.join("nerve.new.exe");
        std::fs::copy(&cur, &tmp).with_context(|| format!("copy {} -> {}", cur.display(), tmp.display()))?;
        crate::control::swap_binary(&tmp, &bin).map_err(|e| anyhow::anyhow!(e))?;
        println!("installed binary: {}", bin.display());
    }
    // 2. who hosts the server
    let user = user.unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_default());
    if user.is_empty() {
        bail!("cannot determine the user; pass --user NAME");
    }
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(crate::util::home_dir);
    let sf = ServiceFile {
        user: user.clone(),
        domain: std::env::var("USERDOMAIN").unwrap_or_default(),
        profile,
        exe: bin.clone(),
        headless_fallback: headless,
        elevated,
    };
    sf.save(paths)?;
    // 3. register with SCM
    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from(DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: bin.clone(),
        launch_arguments: vec!["--home".into(), paths.data.clone().into_os_string(), "service".into(), "run".into()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let mgr = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
    let svc = match mgr.open_service(NAME, access) {
        Ok(s) => {
            s.change_config(&info).context("update service config")?;
            println!("service `nerve` updated");
            s
        }
        Err(_) => {
            let s = mgr.create_service(&info, access).context("create service")?;
            println!("service `nerve` created");
            s
        }
    };
    let _ = svc.set_description(DESCRIPTION);
    let restart = |s| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(s) };
    svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart(1), restart(2), restart(5)]),
    })
    .context("set recovery actions")?;
    svc.set_failure_actions_on_non_crash_failures(true).context("set recovery flag")?;
    println!("host user: {user} (elevated: {elevated}, headless before logon: {headless})");
    if start {
        if svc.query_status()?.current_state == ServiceState::Running {
            println!("service already running; restarting it to pick up changes");
            stop_and_wait(&svc)?;
        }
        svc.start(&[] as &[&OsStr]).context("start service")?;
        wait_state(&svc, ServiceState::Running, 20)?;
        println!("service started — logs: {}", paths.logs.join("service.log").display());
    }
    Ok(())
}

fn uninstall() -> anyhow::Result<()> {
    let svc = open(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE)?;
    stop_and_wait(&svc)?;
    svc.delete().context("delete service")?;
    println!("service removed (run `nerve serve` to start nerve manually)");
    Ok(())
}

fn status(paths: &Paths) -> anyhow::Result<()> {
    match open(ServiceAccess::QUERY_STATUS) {
        Ok(svc) => {
            let st = svc.query_status()?;
            println!("service: {:?}{}", st.current_state, st.process_id.map(|p| format!(" (pid {p})")).unwrap_or_default());
        }
        Err(e) => println!("service: not available ({e})"),
    }
    if let Ok(sf) = ServiceFile::load(paths) {
        println!("host user: {} · elevated {} · headless fallback {}", sf.user, sf.elevated, sf.headless_fallback);
    }
    if let Some(s) = load_serve(paths) {
        println!("server: pid {} · session {} · {}", s.pid, s.session, if crate::util::pid_alive(s.pid) { "alive" } else { "not running" });
    }
    let cfg = Config::load_or_init(paths)?;
    match http(&cfg.server.local_addr, "GET", "/health", None, None) {
        Ok((200, _)) => println!("health: ok (http://{}/mcp)", cfg.server.local_addr),
        Ok((c, _)) => println!("health: HTTP {c}"),
        Err(e) => println!("health: unreachable ({e})"),
    }
    Ok(())
}

// ------------------------------------------------------------------ SCM entry

define_windows_service!(ffi_service_main, service_main);

static PATHS: std::sync::OnceLock<Paths> = std::sync::OnceLock::new();

fn run_dispatcher(paths: Paths) -> anyhow::Result<()> {
    let _ = PATHS.set(paths);
    service_dispatcher::start(NAME, ffi_service_main).context("not started by the service manager (use `nerve service start`)")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    let paths = match PATHS.get().cloned().map(Ok).unwrap_or_else(|| Paths::resolve(None, None)) {
        Ok(p) => p,
        Err(_) => return,
    };
    let log = Log::new(&paths);
    if let Err(e) = run_service(&paths, &log) {
        log.line(&format!("service failed: {e:#}"));
    }
}

enum Msg {
    Stop,
    Wake,
}

fn run_service(paths: &Paths, log: &Log) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let handler = move |ctl| match ctl {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            let _ = tx.send(Msg::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = tx.send(Msg::Wake);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let handle = service_control_handler::register(NAME, handler)?;
    let set = |state: ServiceState, code: u32, wait: u64| {
        let _ = handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(wait),
            process_id: None,
        });
    };
    set(ServiceState::Running, 0, 0);

    let mut sup = match Supervisor::new(paths, log) {
        Ok(s) => s,
        Err(e) => {
            log.line(&format!("cannot start supervisor: {e:#}"));
            set(ServiceState::Stopped, 1, 0);
            return Err(e);
        }
    };
    log.line(&format!("service started (pid {}, v{}), host user {}", std::process::id(), env!("CARGO_PKG_VERSION"), sup.sf.user));
    sup.adopt_or_take_over();

    let mut self_update = false;
    let mut n = 0u64;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        sup.tick();
        n += 1;
        if n % 5 == 0 && sup.binary_changed() {
            log.line("binary was updated: restarting the service (server keeps running)");
            self_update = true;
            break;
        }
    }
    if self_update {
        // non-zero exit => SCM recovery action restarts us with the new binary; the server is adopted.
        // Belt and braces: a detached helper starts the service in case recovery doesn't kick in.
        let mut c = std::process::Command::new("cmd.exe");
        c.args(["/d", "/c", "ping -n 6 127.0.0.1 >nul & sc.exe start nerve >nul"]);
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let _ = crate::util::spawn_detached(c);
        set(ServiceState::Stopped, 1067, 0);
        return Ok(());
    }
    set(ServiceState::StopPending, 0, 20);
    log.line("service stopping");
    sup.stop_child(false);
    set(ServiceState::Stopped, 0, 0);
    Ok(())
}

// ------------------------------------------------------------------ supervisor

struct Log {
    path: PathBuf,
}

impl Log {
    fn new(paths: &Paths) -> Self {
        Self { path: paths.logs.join("service.log") }
    }
    fn line(&self, s: &str) {
        if std::fs::metadata(&self.path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{} {s}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ServeFile {
    pid: u32,
    session: u32,
    started: u64,
}

fn load_serve(paths: &Paths) -> Option<ServeFile> {
    serde_json::from_slice(&std::fs::read(paths.data.join("serve.json")).ok()?).ok()
}

struct Child {
    pid: u32,
    handle: HANDLE,
    session: u32,
    started: Instant,
}

impl Drop for Child {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Child {
    fn exit_code(&self) -> Option<u32> {
        unsafe {
            if WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 {
                let mut code = 0u32;
                GetExitCodeProcess(self.handle, &mut code);
                Some(code)
            } else {
                None
            }
        }
    }
    fn wait(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.handle, ms) == WAIT_OBJECT_0 }
    }
    fn terminate(&self) {
        unsafe { TerminateProcess(self.handle, 1) };
    }
}

fn open_process(pid: u32) -> Option<HANDLE> {
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, 0, pid) };
    (!h.is_null()).then_some(h)
}

fn process_image(h: HANDLE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

fn session_of(pid: u32) -> Option<u32> {
    let mut s = 0u32;
    (unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
}

fn file_stamp(p: &Path) -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()?))
}

struct Supervisor<'a> {
    paths: Paths,
    sf: ServiceFile,
    local_addr: String,
    key: String,
    log: &'a Log,
    child: Option<Child>,
    next_start: Instant,
    backoff: u64,
    health_fails: u32,
    last_health: Instant,
    stamp: Option<(u64, SystemTime)>,
    last_error: String,
}

impl<'a> Supervisor<'a> {
    fn new(paths: &Paths, log: &'a Log) -> anyhow::Result<Self> {
        let sf = ServiceFile::load(paths)?;
        let cfg = Config::load_or_init(paths)?;
        Ok(Self {
            stamp: file_stamp(&sf.exe),
            paths: paths.clone(),
            local_addr: cfg.server.local_addr.clone(),
            key: crate::control::control_key(paths),
            log,
            sf,
            child: None,
            next_start: Instant::now(),
            backoff: 1,
            health_fails: 0,
            last_health: Instant::now(),
            last_error: String::new(),
        })
    }

    fn binary_changed(&self) -> bool {
        let now = file_stamp(&self.sf.exe);
        now.is_some() && now != self.stamp && self.child.is_some()
    }

    /// On service start: adopt our previous server (service restart/update), or ask a foreign
    /// instance (e.g. started by hand) to hand over the port.
    fn adopt_or_take_over(&mut self) {
        if let Some(s) = load_serve(&self.paths) {
            if let Some(h) = open_process(s.pid) {
                let img = process_image(h).map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
                if img.contains("nerve") {
                    let session = session_of(s.pid).unwrap_or(s.session);
                    self.log.line(&format!("adopted running server pid {} (session {session})", s.pid));
                    self.child = Some(Child { pid: s.pid, handle: h, session, started: Instant::now() });
                    return;
                }
                unsafe { CloseHandle(h) };
            }
        }
        // something else serves the port?
        if let Ok((200, body)) = http(&self.local_addr, "GET", "/health", None, None) {
            let pid = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["pid"].as_u64()).unwrap_or(0) as u32;
            self.log.line(&format!("another nerve (pid {pid}) is serving {}; asking it to hand over", self.local_addr));
            let _ = http(&self.local_addr, "POST", "/_nerve/control", Some(r#"{"action":"shutdown","keep_tunnel":true}"#), Some(&self.key));
            let h = if pid != 0 { open_process(pid) } else { None };
            let gone = match h {
                Some(h) => {
                    let ok = unsafe { WaitForSingleObject(h, 10_000) } == WAIT_OBJECT_0;
                    if !ok {
                        self.log.line(&format!("pid {pid} did not exit; terminating it"));
                        unsafe { TerminateProcess(h, 1) };
                        unsafe { WaitForSingleObject(h, 5000) };
                    }
                    unsafe { CloseHandle(h) };
                    true
                }
                None => false,
            };
            if !gone {
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }

    fn desired_session(&self) -> Option<u32> {
        find_user_session(&self.sf.user)
    }

    fn tick(&mut self) {
        // 1. reap
        if let Some(code) = self.child.as_ref().and_then(|c| c.exit_code()) {
            let c = self.child.take().unwrap();
            let up = c.started.elapsed();
            if code == crate::control::RESTART_EXIT_CODE as u32 {
                self.log.line(&format!("server pid {} asked for a restart", c.pid));
                self.backoff = 1;
                self.next_start = Instant::now();
            } else {
                if up > Duration::from_secs(60) {
                    self.backoff = 1;
                }
                self.log.line(&format!("server pid {} exited with code {code} after {}s; restarting in {}s", c.pid, up.as_secs(), self.backoff));
                self.next_start = Instant::now() + Duration::from_secs(self.backoff);
                self.backoff = (self.backoff * 2).min(30);
            }
        }
        let desired = self.desired_session();
        let target = match desired {
            Some(s) => Some(s),
            None if self.sf.headless_fallback => Some(0),
            None => None,
        };
        // 2. hand over when the user logs on / off
        if let Some(c) = &self.child {
            if target != Some(c.session) && c.started.elapsed() > Duration::from_secs(3) {
                self.log.line(&format!("session change: server in session {} -> {:?}", c.session, target));
                self.stop_child(true);
                self.next_start = Instant::now();
            }
        }
        // 3. start
        if self.child.is_none() && Instant::now() >= self.next_start {
            if let Some(t) = target {
                match self.spawn(t) {
                    Ok(c) => {
                        self.log.line(&format!("started server pid {} in session {}", c.pid, c.session));
                        let sf = ServeFile { pid: c.pid, session: c.session, started: crate::util::now_ms() };
                        let _ = crate::util::atomic_write(&self.paths.data.join("serve.json"), serde_json::to_string(&sf).unwrap_or_default().as_bytes());
                        self.child = Some(c);
                        self.health_fails = 0;
                        self.last_health = Instant::now();
                        self.last_error.clear();
                    }
                    Err(e) => {
                        let msg = format!("cannot start server in session {t}: {e:#}");
                        if msg != self.last_error {
                            self.log.line(&msg);
                            self.last_error = msg;
                        }
                        self.next_start = Instant::now() + Duration::from_secs(self.backoff);
                        self.backoff = (self.backoff * 2).min(30);
                    }
                }
            }
        }
        // 4. watchdog: restart a hung server
        if let Some(c) = &self.child {
            if self.last_health.elapsed() >= Duration::from_secs(10) && c.started.elapsed() > Duration::from_secs(30) {
                self.last_health = Instant::now();
                match http(&self.local_addr, "GET", "/health", None, None) {
                    Ok((200, _)) => self.health_fails = 0,
                    _ => {
                        self.health_fails += 1;
                        if self.health_fails >= 6 {
                            self.log.line(&format!("server pid {} unresponsive for 60s; killing it", c.pid));
                            crate::util::kill_tree(c.pid, true);
                            c.terminate();
                            self.health_fails = 0;
                        }
                    }
                }
            }
        }
    }

    /// Graceful stop via the control endpoint, then terminate.
    fn stop_child(&mut self, keep_tunnel: bool) {
        let Some(c) = self.child.take() else { return };
        let body = format!(r#"{{"action":"shutdown","keep_tunnel":{keep_tunnel}}}"#);
        let _ = http(&self.local_addr, "POST", "/_nerve/control", Some(&body), Some(&self.key));
        if !c.wait(15_000) {
            self.log.line(&format!("server pid {} did not stop in 15s; terminating", c.pid));
            c.terminate();
            c.wait(5000);
        }
        if !keep_tunnel {
            let _ = std::fs::remove_file(self.paths.data.join("serve.json"));
        }
    }

    fn spawn(&self, session: u32) -> anyhow::Result<Child> {
        let exe = &self.sf.exe;
        let mut extra: Vec<(String, String)> =
            vec![("NERVE_HOME".into(), self.paths.data.to_string_lossy().into()), ("NERVE_SUPERVISED".into(), "1".into())];
        if session == 0 {
            // headless (SYSTEM) instance: point HOME-ish variables at the user's profile so tools
            // (zrok, git, …) find their config; config/data dir comes from NERVE_HOME
            let prof = self.sf.profile.to_string_lossy().to_string();
            extra.push(("USERPROFILE".into(), prof.clone()));
            extra.push(("HOME".into(), prof.clone()));
            extra.push(("APPDATA".into(), format!(r"{prof}\AppData\Roaming")));
            extra.push(("LOCALAPPDATA".into(), format!(r"{prof}\AppData\Local")));
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--home").arg(&self.paths.data).arg("serve").current_dir(&self.sf.profile);
            for (k, v) in &extra {
                cmd.env(k, v);
            }
            cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            let child = cmd.spawn().with_context(|| format!("spawn {}", exe.display()))?;
            let pid = child.id();
            drop(child);
            let handle = open_process(pid).context("open spawned process")?;
            return Ok(Child { pid, handle, session: 0, started: Instant::now() });
        }
        let cmdline = format!("\"{}\" --home \"{}\" serve", exe.display(), self.paths.data.display());
        let (pid, handle) = unsafe { spawn_as_user(session, self.sf.elevated, exe, &cmdline, &self.sf.profile, &extra)? };
        Ok(Child { pid, handle, session, started: Instant::now() })
    }
}

/// Session where `user` is logged on (active console/RDP preferred, then disconnected).
fn find_user_session(user: &str) -> Option<u32> {
    let mut best: Option<(u32, bool)> = None;
    unsafe {
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) == 0 {
            return None;
        }
        let list = std::slice::from_raw_parts(info, count as usize);
        for s in list {
            if s.SessionId == 0 || (s.State != WTSActive && s.State != WTSDisconnected) {
                continue;
            }
            let mut buf: *mut u16 = std::ptr::null_mut();
            let mut bytes = 0u32;
            if WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, s.SessionId, WTSUserName, &mut buf, &mut bytes) == 0 || buf.is_null() {
                continue;
            }
            let len = (0..).take_while(|&i| *buf.add(i) != 0).count();
            let name = String::from_utf16_lossy(std::slice::from_raw_parts(buf, len));
            WTSFreeMemory(buf as _);
            if name.eq_ignore_ascii_case(user) {
                let active = s.State == WTSActive;
                if best.is_none_or(|(_, a)| active && !a) {
                    best = Some((s.SessionId, active));
                }
            }
        }
        WTSFreeMemory(info as _);
    }
    best.map(|(id, _)| id)
}

/// CreateProcessAsUser in `session` with the user's (optionally elevated/linked) token.
unsafe fn spawn_as_user(session: u32, elevated: bool, exe: &Path, cmdline: &str, cwd: &Path, extra: &[(String, String)]) -> anyhow::Result<(u32, HANDLE)> {
    unsafe {
        let mut user_tok: HANDLE = std::ptr::null_mut();
        if WTSQueryUserToken(session, &mut user_tok) == 0 {
            bail!("WTSQueryUserToken({session}): {}", std::io::Error::last_os_error());
        }
        let mut tok = user_tok;
        let mut linked: TOKEN_LINKED_TOKEN = std::mem::zeroed();
        let mut ret = 0u32;
        let mut linked_ok = false;
        if elevated
            && GetTokenInformation(user_tok, TokenLinkedToken, &mut linked as *mut _ as _, std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32, &mut ret) != 0
            && !linked.LinkedToken.is_null()
        {
            tok = linked.LinkedToken;
            linked_ok = true;
        }
        // environment: the user's own block + our variables
        let mut envp: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut vars: Vec<String> = vec![];
        if CreateEnvironmentBlock(&mut envp, tok, 0) != 0 && !envp.is_null() {
            let mut p = envp as *const u16;
            loop {
                let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                if len == 0 {
                    break;
                }
                vars.push(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                p = p.add(len + 1);
            }
            DestroyEnvironmentBlock(envp);
        }
        for (k, v) in extra {
            let pref = format!("{}=", k.to_lowercase());
            vars.retain(|e| !e.to_lowercase().starts_with(&pref));
            vars.push(format!("{k}={v}"));
        }
        vars.sort_by_key(|a| a.to_lowercase());
        let mut block: Vec<u16> = vec![];
        for v in &vars {
            block.extend(OsStr::new(v).encode_wide());
            block.push(0);
        }
        block.push(0);

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut desktop = wide("winsta0\\default");
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let app = wide(exe);
        let mut cmd = wide(cmdline);
        let dir = wide(cwd);
        let ok = CreateProcessAsUserW(
            tok,
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            block.as_ptr() as _,
            if cwd.is_dir() { dir.as_ptr() } else { std::ptr::null() },
            &si,
            &mut pi,
        );
        let err = std::io::Error::last_os_error();
        if linked_ok {
            CloseHandle(linked.LinkedToken);
        }
        CloseHandle(user_tok);
        if ok == 0 {
            bail!("CreateProcessAsUser: {err}");
        }
        CloseHandle(pi.hThread);
        Ok((pi.dwProcessId, pi.hProcess))
    }
}

/// Minimal blocking HTTP/1.1 client for the loopback control channel.
fn http(addr: &str, method: &str, path: &str, body: Option<&str>, key: Option<&str>) -> anyhow::Result<(u16, String)> {
    let sa: SocketAddr = addr.parse()?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(4)))?;
    s.set_write_timeout(Some(Duration::from_secs(4)))?;
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nUser-Agent: nerve-service\r\n");
    if let Some(k) = key {
        req.push_str(&format!("x-nerve-key: {k}\r\n"));
    }
    if method == "POST" {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    let code = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
    Ok((code, body))
}
