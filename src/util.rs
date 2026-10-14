use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

pub fn to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Random URL-safe token with `bytes` bytes of entropy.
pub fn rand_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    b64url(&buf)
}

pub fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

pub fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|e| e.to_string())
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    let d = Sha256::digest(data.as_ref());
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Constant-time-ish string comparison.
pub fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_ts(ms_or_s: u64) -> String {
    if ms_or_s == 0 {
        return "never".into();
    }
    // accept both seconds and milliseconds
    let ms = if ms_or_s < 100_000_000_000 { ms_or_s * 1000 } else { ms_or_s };
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ms.to_string())
}

pub fn fmt_dur_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        let s = ms / 1000;
        if s < 3600 { format!("{}m{:02}s", s / 60, s % 60) } else { format!("{}h{:02}m", s / 3600, (s % 3600) / 60) }
    }
}

pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Write via temp file + rename so readers never see a half-written file.
pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", rand::random::<u32>()));
    std::fs::write(&tmp, data)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve a user-supplied path: expands `~`, makes relative paths relative to `base`.
pub fn resolve_path(p: &str, base: Option<&str>) -> PathBuf {
    let p = p.trim();
    let expanded = if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        home_dir().join(rest)
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        let b = base.map(|b| resolve_path(b, None)).unwrap_or_else(home_dir);
        b.join(expanded)
    }
}

/// Keep the first `head` and last `tail` chars of a long string.
pub fn clip_middle(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if max_chars == 0 || n <= max_chars {
        return s.to_string();
    }
    let head = max_chars / 4;
    let tail = max_chars - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}\n…[{} chars omitted]…\n{t}", n - head - tail)
}

pub fn one_line(s: &str, max: usize) -> String {
    let l = s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if l.chars().count() > max { format!("{}…", l.chars().take(max).collect::<String>()) } else { l.to_string() }
}

/// Is the byte slice probably binary (contains NUL in the first 8 KiB)?
pub fn looks_binary(data: &[u8]) -> bool {
    memchr::memchr(0, &data[..data.len().min(8192)]).is_some()
}

pub fn which(exe: &str) -> Option<PathBuf> {
    let p = Path::new(exe);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
        .split(';')
        .map(|s| s.to_ascii_lowercase())
        .collect();
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(exe);
        #[cfg(windows)]
        {
            if cand.extension().is_some() && cand.is_file() {
                return Some(cand);
            }
            for e in &exts {
                let c = dir.join(format!("{exe}{e}"));
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        #[cfg(not(windows))]
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Windows: never flash a console window for spawned children.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn hide_window_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Spawn a process fully detached from us (own process group, no console, outside our job object),
/// so it survives nerve restarts.
pub fn spawn_detached(mut c: std::process::Command) -> std::io::Result<std::process::Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        c.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        match c.spawn() {
            Ok(ch) => Ok(ch),
            Err(_) => {
                c.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
                c.spawn()
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
        c.spawn()
    }
}

/// Windows session id of this process (0 = services session, no desktop). None elsewhere.
pub fn session_id() -> Option<u32> {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
        }
        let mut sid = 0u32;
        let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut sid) };
        (ok != 0).then_some(sid)
    }
    #[cfg(not(windows))]
    None
}

/// Is this process elevated (Windows admin token / Unix root)?
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        // `net session` succeeds only in an elevated context
        let mut c = std::process::Command::new("net");
        c.arg("session").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        hide_window_std(&mut c);
        c.status().map(|s| s.success()).unwrap_or(false)
    }
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
}

/// Kill a process and its whole child tree.
pub fn kill_tree(pid: u32, force: bool) -> bool {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/T", "/PID", &pid.to_string()]);
        if force {
            c.arg("/F");
        }
        hide_window_std(&mut c);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c.status().map(|s| s.success()).unwrap_or(false)
    }
    #[cfg(unix)]
    {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        // children are spawned as their own process-group leaders
        let a = unsafe { libc::kill(-(pid as i32), sig) } == 0;
        let b = unsafe { libc::kill(pid as i32, sig) } == 0;
        a || b
    }
}

pub fn pid_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
    sys.process(p).is_some()
}
pub fn process_name(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
    sys.process(p).map(|x| x.name().to_string_lossy().to_string())
}

/// Write a file, preferring an atomic replace but falling back to an in-place write
/// (Windows refuses to rename over files that other programs hold open).
pub fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    match atomic_write(path, data) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::write(path, data),
    }
}
