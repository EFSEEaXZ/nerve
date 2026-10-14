//! nerve — fast, extensible MCP server for full PC control.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod auth;
mod config;
mod control;
mod dashboard;
mod jobs;
mod mcp;
mod plugins;
mod runner;
mod server;
mod service;
mod tools;
#[cfg(windows)]
mod tray;
mod tunnel;
mod util;
mod updates;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nerve", version, about = "nerve — fast, extensible MCP server for full PC control")]
struct Cli {
    /// Path to config.toml (default: <data dir>/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Data directory (default: %LOCALAPPDATA%\nerve or ~/.local/share/nerve; env NERVE_HOME)
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (default)
    Serve {
        /// Wait for this process (the previous instance) to exit first
        #[arg(long, hide = true)]
        wait_pid: Option<u32>,
    },
    /// Install as a service (autostart, self-restart, runs in your desktop session)
    Service {
        #[command(subcommand)]
        cmd: service::ServiceCmd,
    },
    /// Replace the installed nerve binary and restart the running server (jobs and tunnel keep running)
    Update {
        /// Optional local recovery binary. Omit to update from the pinned GitHub release.
        source: Option<String>,
    },
    /// Manage the OAuth PIN required for remote clients
    Pin {
        #[command(subcommand)]
        cmd: PinCmd,
    },
    /// Manage static API tokens (for clients without OAuth)
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Manage registered OAuth clients
    Clients {
        #[command(subcommand)]
        cmd: ClientsCmd,
    },
    /// Print data/config/log paths
    Paths,
    /// Internal: detached job runner
    #[command(hide = true, name = "__run")]
    Run(runner::RunArgs),
}

#[derive(Subcommand)]
enum PinCmd {
    /// Set or change the PIN (prompts if not given)
    Set { pin: Option<String> },
    /// Remove the PIN (remote OAuth logins become impossible)
    Clear,
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Create a token (printed once)
    Create { name: String },
    /// List tokens
    List,
    /// Revoke a token by id
    Revoke { id: String },
}

#[derive(Subcommand)]
enum ClientsCmd {
    /// List OAuth clients
    List,
    /// Revoke one client and all its tokens
    Revoke { id: String },
    /// Revoke every client and token
    RevokeAll,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::Run(args)) = cli.cmd {
        std::process::exit(runner::run(args));
    }
    let paths = config::Paths::resolve(cli.home.clone(), cli.config.clone())?;
    match cli.cmd.unwrap_or(Cmd::Serve { wait_pid: None }) {
        Cmd::Serve { wait_pid } => {
            if let Some(pid) = wait_pid {
                control::wait_for_pid(pid);
            }
            let cfg = config::Config::load_or_init(&paths)?;
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let code = rt.block_on(server::run(cfg, paths))?;
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
            std::process::exit(code);
        }
        Cmd::Service { cmd } => service::cli(cmd, paths),
        Cmd::Update { source } => {
            let cfg = config::Config::load_or_init(&paths)?;
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(async {
                let http = reqwest::Client::new();
                if let Some(src) = source {
                    anyhow::ensure!(!src.contains("://"), "updates are pinned to EFSEEaXZ/nerve; only a local recovery path is allowed");
                    let target = service::ServiceFile::load(&paths).map(|s| s.exe).unwrap_or_else(|_| paths.bin.join(format!("nerve{}", std::env::consts::EXE_SUFFIX)));
                    let (v, exe) = control::update_to(&http, &src, target).await.map_err(|e| anyhow::anyhow!(e))?;
                    println!("installed {v} -> {}", exe.display());
                    let r = http.post(format!("http://{}/_nerve/control", cfg.server.local_addr)).header("x-nerve-key", control::control_key(&paths)).json(&serde_json::json!({"action":"restart"})).timeout(std::time::Duration::from_secs(5)).send().await;
                    match r { Ok(r) if r.status().is_success() => println!("restarting"), _ => println!("start/restart nerve to use the installed binary") }
                } else {
                    http.post(format!("http://{}/_nerve/control", cfg.server.local_addr)).header("x-nerve-key", control::control_key(&paths)).json(&serde_json::json!({"action":"update"})).timeout(std::time::Duration::from_secs(5)).send().await?.error_for_status()?;
                    println!("GitHub release update queued; progress is in the dashboard Settings");
                }
                anyhow::Ok(())
            })
        }
        Cmd::Paths => {
            println!("data:   {}", paths.data.display());
            println!("config: {}", paths.config.display());
            println!("logs:   {}", paths.logs.display());
            println!("jobs:   {}", paths.jobs.display());
            Ok(())
        }
        Cmd::Pin { cmd } => {
            let store = auth::Store::open(&paths)?;
            match cmd {
                PinCmd::Set { pin } => {
                    let pin = match pin {
                        Some(p) => p,
                        None => {
                            let a = rpassword::prompt_password("New PIN: ")?;
                            let b = rpassword::prompt_password("Repeat PIN: ")?;
                            anyhow::ensure!(a == b, "PINs do not match");
                            a
                        }
                    };
                    anyhow::ensure!(pin.chars().count() >= 4, "PIN must be at least 4 characters");
                    store.set_pin(Some(&pin))?;
                    println!("PIN saved.");
                }
                PinCmd::Clear => {
                    store.set_pin(None)?;
                    println!("PIN removed.");
                }
            }
            Ok(())
        }
        Cmd::Token { cmd } => {
            let store = auth::Store::open(&paths)?;
            match cmd {
                TokenCmd::Create { name } => {
                    let (id, token) = store.create_api_token(&name)?;
                    println!("id:    {id}\ntoken: {token}\n(shown once — use as `Authorization: Bearer <token>`)");
                }
                TokenCmd::List => {
                    for t in store.list_api_tokens() {
                        println!("{}  {}  created {}  last used {}", t.id, t.name, util::fmt_ts(t.created), util::fmt_ts(t.last_used));
                    }
                }
                TokenCmd::Revoke { id } => {
                    println!("{}", if store.revoke_api_token(&id)? { "revoked" } else { "not found" });
                }
            }
            Ok(())
        }
        Cmd::Clients { cmd } => {
            let store = auth::Store::open(&paths)?;
            match cmd {
                ClientsCmd::List => {
                    for c in store.list_clients() {
                        println!(
                            "{}  {}  redirect={}  created {}  last used {}",
                            c.client_id,
                            c.name,
                            c.redirect_uris.join(","),
                            util::fmt_ts(c.created),
                            util::fmt_ts(c.last_used)
                        );
                    }
                }
                ClientsCmd::Revoke { id } => {
                    println!("{}", if store.revoke_client(&id)? { "revoked" } else { "not found" });
                }
                ClientsCmd::RevokeAll => {
                    let n = store.revoke_all_clients()?;
                    println!("revoked {n} client(s)");
                }
            }
            Ok(())
        }
        Cmd::Run(_) => unreachable!(),
    }
}