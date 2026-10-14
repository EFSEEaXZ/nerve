//! `nerve __run`: a tiny detached supervisor for background jobs.
//! It outlives server restarts, writes the child's output to log files and records the exit code.

use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    #[arg(long)]
    pub stdout: PathBuf,
    #[arg(long)]
    pub stderr: PathBuf,
    #[arg(long)]
    pub exit_file: PathBuf,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    #[arg(long)]
    pub stdin_file: Option<PathBuf>,
    /// Windows: raw command-line tail passed verbatim (cmd.exe quoting)
    #[arg(long)]
    pub raw: Option<String>,
    #[arg(last = true, required = true)]
    pub argv: Vec<String>,
}

pub fn run(a: RunArgs) -> i32 {
    let (Ok(out), Ok(err)) = (File::create(&a.stdout), File::create(&a.stderr)) else {
        return 126;
    };
    let mut err_note = err.try_clone().ok();
    let mut cmd = Command::new(&a.argv[0]);
    cmd.args(&a.argv[1..]);
    #[cfg(windows)]
    if let Some(raw) = &a.raw {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(raw);
    }
    #[cfg(not(windows))]
    let _ = &a.raw;
    if let Some(cwd) = &a.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(match a.stdin_file.as_ref().and_then(|p| File::open(p).ok()) {
        Some(f) => Stdio::from(f),
        None => Stdio::null(),
    });
    cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
    crate::util::hide_window_std(&mut cmd);
    let code = match cmd.spawn() {
        Ok(mut child) => match child.wait() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(_) => -1,
        },
        Err(e) => {
            if let Some(f) = err_note.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "nerve: failed to start `{}`: {e}", a.argv[0]);
            }
            127
        }
    };
    let _ = crate::util::atomic_write(&a.exit_file, format!("{code}\n").as_bytes());
    if let Some(p) = &a.stdin_file {
        let _ = std::fs::remove_file(p);
    }
    code
}