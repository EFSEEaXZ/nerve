use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub bin: PathBuf,
    pub config: PathBuf,
    pub logs: PathBuf,
    pub jobs: PathBuf,
    pub out: PathBuf,
    pub tmp: PathBuf,
}

impl Paths {
    pub fn resolve(home: Option<PathBuf>, config_override: Option<PathBuf>) -> anyhow::Result<Self> {
        let data = match home.or_else(|| std::env::var_os("NERVE_HOME").map(PathBuf::from)) {
            Some(h) => h,
            None => dirs::data_local_dir().context("no local data dir")?.join("nerve"),
        };
        let config = config_override.unwrap_or_else(|| data.join("config.toml"));
        let p = Paths {
            bin: data.join("bin"),
            logs: data.join("logs"),
            jobs: data.join("jobs"),
            out: data.join("out"),
            tmp: data.join("tmp"),
            config,
            data,
        };
        for d in [&p.data, &p.logs, &p.jobs, &p.out, &p.tmp] {
            std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
        }
        Ok(p)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub updates: UpdatesCfg,
    pub server: ServerCfg,
    pub auth: AuthCfg,
    pub tunnel: TunnelCfg,
    pub exec: ExecCfg,
    pub image: ImageCfg,
    pub output: OutputCfg,
    pub log: LogCfg,
    pub ui: UiCfg,
    pub browser: BrowserCfg,
    pub plugins: PluginsCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrowserCfg {
    /// Chrome/Edge/Chromium executable ("" = auto-detect).
    pub path: String,
    /// user-data-dir of the nerve browser ("" = <data>/browser/profile). Logins persist here.
    pub profile: String,
    /// Start without a window by default.
    pub headless: bool,
    /// Drive an already running browser instead of launching one, e.g. "http://127.0.0.1:9222".
    pub cdp_url: String,
    /// Extra command-line flags for the launched browser.
    pub args: Vec<String>,
    /// Window size for launched browsers, e.g. "1280,900" ("" = browser default).
    pub window_size: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsCfg {
    /// Load plugins from `dir` and hot-reload them on change.
    pub enabled: bool,
    /// Plugin folder ("" = <data>/plugins).
    pub dir: String,
    /// Install/update the plugins shipped with nerve (blender, unreal, SDK).
    pub bundled: bool,
    /// Python interpreter for Python plugins ("" = auto: python, python3, py).
    pub python: String,
}

impl Default for PluginsCfg {
    fn default() -> Self {
        Self { enabled: true, dir: String::new(), bundled: true, python: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatesCfg {
    /// Install stable releases from EFSEEaXZ/nerve after checksum verification and idle time.
    pub automatic: bool,
    /// GitHub release check interval (minimum 900 seconds).
    pub check_interval_secs: u64,
}
impl Default for UpdatesCfg {
    fn default() -> Self { Self { automatic: true, check_interval_secs: 21600 } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiCfg {
    /// Web dashboard on the local listener (http://127.0.0.1:8100/ui).
    pub dashboard: bool,
    /// Tray icon (Windows, interactive session only).
    pub tray: bool,
}

impl Default for UiCfg {
    fn default() -> Self {
        Self { dashboard: true, tray: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    /// Quick tool calls return JSON when accepted; slow calls retain SSE. 0 disables the fast path.
    pub json_grace_ms: u64,
    /// Local listener: no auth, only reachable from this machine.
    pub local_addr: String,
    /// Public listener: OAuth/PIN or API token required. Point the tunnel here. Empty = disabled.
    pub public_addr: String,
    /// Public base URL override (e.g. https://pc.example.com). Empty = auto (tunnel / Host header).
    pub public_url: String,
    /// Extra browser origins allowed on the local listener (DNS-rebinding protection).
    pub allowed_origins: Vec<String>,
    /// Default working directory for relative paths and commands. Empty = home dir.
    pub default_dir: String,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            json_grace_ms: 250,
            local_addr: "127.0.0.1:8100".into(),
            public_addr: "127.0.0.1:8101".into(),
            public_url: String::new(),
            allowed_origins: vec![],
            default_dir: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthCfg {
    pub access_ttl_secs: u64,
    pub refresh_ttl_secs: u64,
    /// Require a token even on the local listener.
    pub local_requires_auth: bool,
    /// Wrong PIN attempts before lockout.
    pub max_pin_failures: u32,
    pub lockout_secs: u64,
}

impl Default for AuthCfg {
    fn default() -> Self {
        Self { access_ttl_secs: 86_400, refresh_ttl_secs: 90 * 86_400, local_requires_auth: false, max_pin_failures: 5, lockout_secs: 900 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelCfg {
    /// none | zrok | cloudflared | ngrok | tailscale | command
    pub provider: String,
    /// zrok v2 CLI (`zrok2`). Empty = <data>/tools/zrok2(.exe), then PATH.
    pub zrok_bin: String,
    /// Reserved zrok name => stable https://<name>.shares.zrok.io. Empty = random name per start.
    pub zrok_name: String,
    pub zrok_namespace: String,
    pub cloudflared_bin: String,
    /// Named-tunnel token (stable domain). Empty = quick tunnel (random trycloudflare.com URL).
    pub cloudflared_token: String,
    pub ngrok_bin: String,
    /// Static ngrok domain, e.g. my-pc.ngrok-free.app
    pub ngrok_domain: String,
    pub ngrok_authtoken: String,
    pub tailscale_bin: String,
    /// Custom tunnel command; `{port}` is replaced with the public port.
    pub command: String,
}

impl Default for TunnelCfg {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            zrok_bin: String::new(),
            zrok_name: String::new(),
            zrok_namespace: "public".into(),
            cloudflared_bin: "cloudflared".into(),
            cloudflared_token: String::new(),
            ngrok_bin: "ngrok".into(),
            ngrok_domain: String::new(),
            ngrok_authtoken: String::new(),
            tailscale_bin: "tailscale".into(),
            command: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecCfg {
    /// 0 = wait forever.
    pub default_timeout_ms: u64,
    /// auto | powershell | pwsh | cmd | bash | sh | zsh
    pub default_shell: String,
    /// Bytes per stream returned inline; the full output is always saved to a file. 0 = unlimited.
    pub inline_output_bytes: usize,
    /// SSE keep-alive interval so tunnels/proxies never cut long calls.
    pub keepalive_secs: u64,
}

impl Default for ExecCfg {
    fn default() -> Self {
        Self { default_timeout_ms: 0, default_shell: "auto".into(), inline_output_bytes: 60_000, keepalive_secs: 15 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageCfg {
    /// Default byte budget of one image result (what the client can inline).
    pub max_bytes: usize,
    pub max_dim: u32,
    /// Never drop JPEG quality below this; shrink dimensions instead.
    pub min_quality: u8,
}

impl Default for ImageCfg {
    fn default() -> Self {
        Self { max_bytes: 30_000, max_dim: 1600, min_quality: 55 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputCfg {
    /// Maximum aggregate text response bytes; larger results are saved in full to disk. 0 disables the guard.
    pub max_text_bytes: usize,
    /// Also return machine-readable `structuredContent` (costs extra context tokens).
    pub structured: bool,
}

impl Default for OutputCfg {
    fn default() -> Self { Self { structured: false, max_text_bytes: 48 * 1024 } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogCfg {
    pub level: String,
    pub keep_days: usize,
}

impl Default for LogCfg {
    fn default() -> Self {
        Self { level: "info".into(), keep_days: 7 }
    }
}

const TEMPLATE: &str = r#"# nerve configuration. Restart nerve after editing.

[server]
# Fast JSON response grace; 0 keeps all accepted tool calls streamed over SSE.
json_grace_ms = 250
# Local listener: no auth, reachable only from this machine (Claude Desktop, Cursor, scripts).
local_addr = "127.0.0.1:8100"
# Public listener: OAuth + PIN (or API token) required. Tunnels point here. "" = disabled.
public_addr = "127.0.0.1:8101"
# Public base URL override, e.g. "https://pc.example.com". Empty = auto-detect.
public_url = ""
# Default dir for relative paths / commands. Empty = home dir.
default_dir = ""

[updates]
# Stable GitHub Releases only; no custom update URL or embedded GitHub token.
automatic = true
check_interval_secs = 21600

[auth]
access_ttl_secs = 86400
refresh_ttl_secs = 7776000
local_requires_auth = false
max_pin_failures = 5
lockout_secs = 900

[tunnel]
# none | zrok | cloudflared | ngrok | tailscale | command
# The tunnel process survives nerve restarts/updates (adopted on start), so the public URL stays up.
provider = "none"
# zrok v2: reserve a name once (`zrok2 create name <name>`) for a stable https://<name>.shares.zrok.io
zrok_bin = ""                 # empty = <data>/tools/zrok2.exe, then PATH
zrok_name = ""
zrok_namespace = "public"
cloudflared_bin = "cloudflared"
# Named tunnel token => stable domain. Empty => quick tunnel with a random *.trycloudflare.com URL.
cloudflared_token = ""
ngrok_bin = "ngrok"
# Static domain (free ngrok accounts get one), e.g. "my-pc.ngrok-free.app"
ngrok_domain = ""
ngrok_authtoken = ""
tailscale_bin = "tailscale"
# Custom command, {port} = public port. Example: "bore local {port} --to bore.pub"
command = ""

[exec]
default_timeout_ms = 0        # 0 = wait forever
default_shell = "auto"        # auto | powershell | pwsh | cmd | bash | sh | zsh
inline_output_bytes = 60000   # per stream; full output is always saved to a file. 0 = unlimited
keepalive_secs = 15

[image]
max_bytes = 30000             # default budget per image result
max_dim = 1600
min_quality = 55

[output]
# Avoid oversized text replies stalling clients; complete results are saved to out/.
max_text_bytes = 49152
structured = false            # also send structuredContent (more tokens)

[log]
level = "info"
keep_days = 7

[ui]
dashboard = true              # http://127.0.0.1:8100/ui (local only)
tray = true                   # Windows tray icon

[browser]
# The `browser` tool drives its own Chrome/Edge profile over CDP (logins you make there persist).
path = ""                     # empty = auto-detect Chrome, Edge, Chromium, Brave
profile = ""                  # empty = <data>/browser/profile
headless = false
cdp_url = ""                  # e.g. "http://127.0.0.1:9222" to drive a browser you started yourself
args = []
window_size = ""

[plugins]
# Folders in `dir` with a plugin.toml add tools; they hot-reload when their files change.
enabled = true
dir = ""                      # empty = <data>/plugins
bundled = true                # install/update bundled plugins (blender, unreal) unless you edited them
python = ""                   # empty = auto-detect
"#;

impl Config {
    pub fn load_or_init(paths: &Paths) -> anyhow::Result<Self> {
        if !paths.config.exists() {
            crate::util::atomic_write(&paths.config, TEMPLATE.as_bytes())?;
        }
        let text = std::fs::read_to_string(&paths.config).with_context(|| format!("read {}", paths.config.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", paths.config.display()))?;
        if let Ok(v) = std::env::var("NERVE_LOCAL_ADDR") {
            cfg.server.local_addr = v;
        }
        if let Ok(v) = std::env::var("NERVE_PUBLIC_ADDR") {
            cfg.server.public_addr = v;
        }
        if let Ok(v) = std::env::var("NERVE_TUNNEL") {
            cfg.tunnel.provider = v;
        }
        Ok(cfg)
    }

    pub fn default_dir(&self) -> PathBuf {
        if self.server.default_dir.trim().is_empty() {
            crate::util::home_dir()
        } else {
            crate::util::resolve_path(&self.server.default_dir, None)
        }
    }
}