//! `nerve service …` — run nerve as an OS service.
//!
//! Windows: a LocalSystem service that only *supervises*. It starts `nerve serve` inside the
//! logged-on user's desktop session with the user's elevated token (so desktop automation, user
//! env, HOME, credentials all just work), falls back to a headless instance before logon, restarts
//! the server on crash/hang/`exit 75`, hands over when the user logs on/off and updates itself
//! when the binary is replaced.

use std::path::PathBuf;

use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::config::Paths;

#[cfg(windows)]
mod win;

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Install (or update) and start the service. Run from an elevated terminal.
    Install {
        /// Windows user whose desktop session hosts nerve (default: current user)
        #[arg(long)]
        user: Option<String>,
        /// Don't run a headless instance while that user is logged off
        #[arg(long)]
        no_headless: bool,
        /// Run the server with the user's normal (non-elevated) token
        #[arg(long)]
        no_elevate: bool,
        /// Install only, don't start
        #[arg(long)]
        no_start: bool,
    },
    /// Stop and remove the service
    Uninstall,
    /// Start the service
    Start,
    /// Stop the service (and the server it supervises)
    Stop,
    /// Stop + start
    Restart,
    /// Show service and server state
    Status,
    /// Internal: entry point used by the service manager
    #[command(hide = true)]
    Run,
}

/// Written by `service install`; read by the service (which runs as SYSTEM and can't guess these).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServiceFile {
    pub user: String,
    #[serde(default)]
    pub domain: String,
    pub profile: PathBuf,
    pub exe: PathBuf,
    #[serde(default = "yes")]
    pub headless_fallback: bool,
    #[serde(default = "yes")]
    pub elevated: bool,
}

fn yes() -> bool {
    true
}

impl ServiceFile {
    pub fn path(paths: &Paths) -> PathBuf {
        paths.data.join("service.json")
    }
    pub fn load(paths: &Paths) -> anyhow::Result<Self> {
        let p = Self::path(paths);
        let s = std::fs::read(&p).map_err(|e| anyhow::anyhow!("read {}: {e} (run `nerve service install`)", p.display()))?;
        Ok(serde_json::from_slice(&s)?)
    }
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        crate::util::atomic_write(&Self::path(paths), serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}

pub fn cli(cmd: ServiceCmd, paths: Paths) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        win::cli(cmd, paths)
    }
    #[cfg(not(windows))]
    {
        let _ = (cmd, paths);
        anyhow::bail!(
            "`nerve service` is Windows-only for now. On Linux use a systemd user unit, e.g.\n\
             [Service]\nExecStart=%h/.local/bin/nerve serve\nRestart=always\nRestartForceExitStatus=75\n"
        )
    }
}
