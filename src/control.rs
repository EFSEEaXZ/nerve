//! Lifecycle: graceful stop/restart, self-update (swap the running binary) and the control key
//! used by the supervisor (Windows service) to talk to the server.
//!
//! Restart semantics: background jobs are detached processes and the tunnel is adopted by the next
//! instance, so a restart/update costs ~1 s of listener downtime and nothing else.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Paths;

/// Exit code that asks the supervisor to start the server again immediately.
pub const RESTART_EXIT_CODE: i32 = 75;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quit {
    Stop = 1,
    Restart = 2,
}

pub struct Lifecycle {
    pub token: CancellationToken,
    reason: AtomicU8,
    keep_tunnel: AtomicBool,
    successor: Mutex<Option<PathBuf>>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: AtomicU8::new(0),
            keep_tunnel: AtomicBool::new(false),
            successor: Mutex::new(None),
        }
    }
}

impl Lifecycle {
    /// Ask the server to exit after `delay` (lets the current HTTP response flush first).
    pub fn request(&self, reason: Quit, keep_tunnel: bool, delay: Duration) {
        self.reason.store(reason as u8, Ordering::SeqCst);
        self.keep_tunnel.store(keep_tunnel, Ordering::SeqCst);
        let t = self.token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            t.cancel();
        });
    }
    pub fn reason(&self) -> Option<Quit> {
        match self.reason.load(Ordering::SeqCst) {
            1 => Some(Quit::Stop),
            2 => Some(Quit::Restart),
            _ => None,
        }
    }
    pub fn keep_tunnel(&self) -> bool {
        self.keep_tunnel.load(Ordering::SeqCst)
    }
    pub fn set_successor(&self, exe: PathBuf) {
        *self.successor.lock().unwrap() = Some(exe);
    }
    pub fn successor(&self) -> Option<PathBuf> {
        self.successor.lock().unwrap().clone()
    }
}

/// Started by the nerve service (which restarts us on exit code 75).
pub fn supervised() -> bool {
    std::env::var("NERVE_SUPERVISED").map(|v| v == "1").unwrap_or(false)
}

/// Shared secret between the supervisor and the server (`<data>/control.key`).
pub fn control_key(paths: &Paths) -> String {
    let p = paths.data.join("control.key");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let s = s.trim().to_string();
        if s.len() >= 16 {
            return s;
        }
    }
    let k = crate::util::rand_token(24);
    let _ = crate::util::atomic_write(&p, k.as_bytes());
    k
}

/// Path of the running binary (without the `\\?\` prefix).
pub fn current_exe() -> PathBuf {
    let p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nerve"));
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// Delete leftovers of previous updates (`nerve.old-*`, `nerve.new*`) that are no longer running.
pub fn cleanup_old_binaries(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        if n.starts_with("nerve.old-") || n.starts_with("nerve.new") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Run `<exe> --version` and return its output (proves the new binary starts on this machine).
pub fn probe_version(exe: &Path) -> Result<String, String> {
    let mut c = std::process::Command::new(exe);
    c.arg("--version").stdin(std::process::Stdio::null());
    crate::util::hide_window_std(&mut c);
    let mut child = c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return Err(format!("{} --version did not finish in 15 s", exe.display()));
            }
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !s.to_lowercase().starts_with("nerve") {
        return Err(format!("{} does not look like nerve (`--version` printed {s:?})", exe.display()));
    }
    Ok(s)
}

/// Replace `target` with `new_file` while `target` may be running.
/// Windows can't overwrite a running exe but can rename it, so: target -> nerve.old-<ts>, new -> target.
pub fn swap_binary(new_file: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let dir = target.parent().ok_or("bad target path")?;
        let old = dir.join(format!("nerve.old-{}.exe", crate::util::now_ms()));
        if target.exists() {
            std::fs::rename(target, &old).map_err(|e| format!("rename running binary: {e}"))?;
        }
        if let Err(e) = std::fs::rename(new_file, target) {
            let _ = std::fs::rename(&old, target);
            return Err(format!("move new binary into place: {e}"));
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(new_file, std::fs::Permissions::from_mode(0o755));
        std::fs::rename(new_file, target).map_err(|e| format!("move new binary into place: {e}"))
    }
}

/// Fetch (URL) or copy (path) a new binary next to the running one, verify it, swap it in.
/// Returns (new version string, installed path). The caller schedules the restart.
pub async fn update(http: &reqwest::Client, source: &str) -> Result<(String, PathBuf), String> {
    update_to(http, source, current_exe()).await
}

pub async fn update_to(http: &reqwest::Client, source: &str, target: PathBuf) -> Result<(String, PathBuf), String> {
    let dir = target.parent().map(Path::to_path_buf).ok_or("cannot locate the running binary")?;
    let staged = dir.join(format!("nerve.new{}", std::env::consts::EXE_SUFFIX));
    let src = source.trim();
    if src.starts_with("http://") || src.starts_with("https://") {
        let resp = http.get(src).timeout(Duration::from_secs(600)).send().await.map_err(|e| format!("download: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("download: HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("download: {e}"))?;
        std::fs::write(&staged, &bytes).map_err(|e| format!("write {}: {e}", staged.display()))?;
    } else {
        let p = crate::util::resolve_path(src, None);
        if !p.is_file() {
            return Err(format!("{} is not a file", p.display()));
        }
        if same_file(&p, &target) {
            return Err("source is the running binary itself".into());
        }
        std::fs::copy(&p, &staged).map_err(|e| format!("copy {} -> {}: {e}", p.display(), staged.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let probe = {
        let s = staged.clone();
        tokio::task::spawn_blocking(move || probe_version(&s)).await.map_err(|e| e.to_string())?
    };
    let version = match probe {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    };
    swap_binary(&staged, &target)?;
    tracing::info!("update: installed {version} at {}", target.display());
    Ok((version, target))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Unsupervised restart: start a successor that waits for this process to exit, then takes over.
pub fn spawn_successor(exe: &Path, paths: &Paths) -> std::io::Result<u32> {
    let mut c = std::process::Command::new(exe);
    c.arg("--home").arg(&paths.data).arg("--config").arg(&paths.config).arg("serve").arg("--wait-pid").arg(std::process::id().to_string());
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let child = crate::util::spawn_detached(c)?;
    Ok(child.id())
}

/// Used by `serve --wait-pid`: wait (max 30 s) for the previous instance to exit.
pub fn wait_for_pid(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while crate::util::pid_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}
