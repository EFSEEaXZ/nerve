//! Release updates are pinned to this repository. No user-supplied download URL or embedded token.
use crate::server::AppState;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const REPOSITORY: &str = "EFSEEaXZ/nerve";
const API: &str = "https://api.github.com/repos/EFSEEaXZ/nerve/releases/latest";
const ASSET: &str = "nerve-windows-x86_64.exe";
const MAX_BINARY: usize = 64 * 1024 * 1024;

pub struct Updates {
    state: Mutex<Value>,
    busy: AtomicBool,
}
impl Default for Updates {
    fn default() -> Self {
        Self {
            state: Mutex::new(
                json!({"repository": REPOSITORY, "state": "not_checked", "message": "Updates come from GitHub Releases", "checked_at": null, "latest": null, "available": false}),
            ),
            busy: AtomicBool::new(false),
        }
    }
}
impl Updates {
    pub fn status(&self, automatic: bool) -> Value {
        let mut s = self.state.lock().unwrap().clone();
        s["automatic"] = json!(automatic);
        s["busy"] = json!(self.busy.load(Ordering::Relaxed));
        s
    }
    fn set(&self, state: &str, message: impl Into<String>) {
        let mut s = self.state.lock().unwrap();
        s["state"] = json!(state);
        s["message"] = json!(message.into());
    }
    pub async fn check(&self, app: &AppState) -> Result<bool, String> {
        self.set("checking", "Checking GitHub Releases");
        let result = fetch_release(app).await;
        let mut s = self.state.lock().unwrap();
        s["checked_at"] = json!(crate::util::now_ms());
        match result {
            Ok(release) => {
                let available = release.version > version(env!("CARGO_PKG_VERSION")).unwrap();
                s["latest"] = json!(release.tag);
                s["available"] = json!(available);
                s["state"] = json!(if available { "available" } else { "current" });
                s["message"] = json!(if available {
                    format!("{} is available", release.tag)
                } else {
                    "You are up to date".into()
                });
                Ok(available)
            }
            Err(e) => {
                s["state"] = json!("unavailable");
                s["message"] = json!(e.clone());
                Err(e)
            }
        }
    }
    /// Queue work and return immediately so a slow GitHub download never holds an MCP response.
    pub fn request(app: AppState) -> bool {
        if app
            .updates
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        app.updates
            .set("updating", "Downloading and verifying the GitHub release");
        tokio::spawn(async move {
            match install(&app).await {
                Ok(Some((v, exe))) => {
                    app.updates
                        .set("restarting", format!("Installed {v}; restarting"));
                    app.life.set_successor(exe);
                    app.life.request(
                        crate::control::Quit::Restart,
                        true,
                        Duration::from_millis(1000),
                    );
                }
                Ok(None) => {
                    app.updates.set("current", "You are up to date");
                }
                Err(e) => {
                    app.updates.state.lock().unwrap()["available"] = json!(false);
                    app.updates.set("failed", e.clone());
                    tracing::warn!("GitHub update failed: {e}");
                }
            }
            app.updates.busy.store(false, Ordering::Release);
        });
        true
    }
}

#[derive(Debug)]
struct Release {
    tag: String,
    version: (u64, u64, u64),
    binary: String,
    sums: String,
}
fn version(s: &str) -> Option<(u64, u64, u64)> {
    let p: Vec<&str> = s.strip_prefix('v').unwrap_or(s).split('.').collect();
    if p.len() != 3
        || p.iter()
            .any(|x| x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some((p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?))
}
fn release_from_json(v: &Value) -> Result<Release, String> {
    if v["draft"].as_bool().unwrap_or(true) || v["prerelease"].as_bool().unwrap_or(true) {
        return Err("Only stable published releases are accepted".into());
    }
    let tag = v["tag_name"]
        .as_str()
        .ok_or("GitHub release has no tag")?
        .to_string();
    let ver = version(&tag).ok_or("Release tag must be vMAJOR.MINOR.PATCH")?;
    let prefix = format!("https://github.com/{REPOSITORY}/releases/download/{tag}/");
    let assets = v["assets"]
        .as_array()
        .ok_or("GitHub release has no assets")?;
    let url = |name: &str| -> Result<String, String> {
        let a = assets
            .iter()
            .find(|a| a["name"] == name)
            .ok_or_else(|| format!("Release is missing {name}"))?;
        let u = a["browser_download_url"]
            .as_str()
            .ok_or("Release asset has no URL")?;
        if u != format!("{prefix}{name}") {
            return Err("Release asset URL is outside the pinned repository".into());
        }
        Ok(u.into())
    };
    Ok(Release {
        tag,
        version: ver,
        binary: url(ASSET)?,
        sums: url("SHA256SUMS.txt")?,
    })
}
async fn fetch_release(app: &AppState) -> Result<Release, String> {
    if !cfg!(all(windows, target_arch = "x86_64")) {
        return Err(
            "Release binaries support Windows x86_64; build other targets from source".into(),
        );
    }
    let r = app
        .http
        .get(API)
        .header("User-Agent", concat!("nerve/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("GitHub is unavailable: {e}"))?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(
            "No public release found. The repository must be public and have a published release"
                .into(),
        );
    }
    if !r.status().is_success() {
        return Err(format!(
            "GitHub returned HTTP {}. Try again later",
            r.status()
        ));
    }
    if r.content_length().unwrap_or(0) > 1024 * 1024 {
        return Err("GitHub release metadata is too large".into());
    }
    let bytes = bounded_body(r, 1024 * 1024).await?;
    let v: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("Invalid release metadata: {e}"))?;
    release_from_json(&v)
}
async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Download failed: {e}"))?
    {
        if out.len().saturating_add(chunk.len()) > limit {
            return Err("Download exceeds the size limit".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
fn checksum(sums: &str) -> Result<String, String> {
    for line in sums.lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() == 2
            && p[1].trim_start_matches('*') == ASSET
            && p[0].len() == 64
            && p[0].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Ok(p[0].to_ascii_lowercase());
        }
    }
    Err("SHA256SUMS.txt has no valid checksum for the Windows binary".into())
}
async fn install(app: &AppState) -> Result<Option<(String, std::path::PathBuf)>, String> {
    let release = fetch_release(app).await?;
    {
        let mut s = app.updates.state.lock().unwrap();
        s["latest"] = json!(release.tag);
        s["checked_at"] = json!(crate::util::now_ms());
        s["available"] = json!(release.version > version(env!("CARGO_PKG_VERSION")).unwrap());
    }
    if release.version <= version(env!("CARGO_PKG_VERSION")).unwrap() {
        return Ok(None);
    }
    let r = app
        .http
        .get(&release.sums)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let sums = bounded_body(r, 16 * 1024).await?;
    let expected = checksum(std::str::from_utf8(&sums).map_err(|_| "Invalid checksum encoding")?)?;
    let r = app
        .http
        .get(&release.binary)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let bytes = bounded_body(r, MAX_BINARY).await?;
    if crate::util::sha256_hex(&bytes) != expected {
        return Err("Checksum mismatch. Nothing was installed".into());
    }
    let file = app
        .paths
        .tmp
        .join(format!("github-{}.exe", crate::util::rand_token(6)));
    crate::util::atomic_write(&file, &bytes).map_err(|e| e.to_string())?;
    let f = file.clone();
    let probe = tokio::task::spawn_blocking(move || crate::control::probe_version(&f))
        .await
        .map_err(|e| e.to_string())?;
    let result = match probe {
        Ok(v) if version(v.strip_prefix("nerve ").unwrap_or("")) == Some(release.version) => {
            crate::control::update(&app.http, &file.display().to_string())
                .await
                .map(Some)
        }
        Ok(_) => Err("Release tag and binary version do not match. Nothing was installed".into()),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_file(file);
    result
}

pub async fn run(app: AppState, stop: tokio_util::sync::CancellationToken) {
    if !app.cfg.updates.automatic {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut next = crate::util::now_ms() + 15_000;
    loop {
        tokio::select! {_=tick.tick()=>{},_=stop.cancelled()=>break}
        let now = crate::util::now_ms();
        if now >= next && !app.updates.busy.load(Ordering::Relaxed) {
            let _ = app.updates.check(&app).await;
            next = now + app.cfg.updates.check_interval_secs.max(900) * 1000;
        }
        let available = app.updates.status(true)["available"]
            .as_bool()
            .unwrap_or(false);
        let quiet = app.sessions.summary().iter().all(|s| {
            s["inflight"].as_u64().unwrap_or(0) == 0
                && now.saturating_sub(s["last_seen"].as_u64().unwrap_or(0)) >= 60_000
        });
        if available && quiet {
            Updates::request(app.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"draft":false,"prerelease":false,"tag_name":"v0.8.0","assets":[{"name":ASSET,"browser_download_url":format!("https://github.com/{REPOSITORY}/releases/download/v0.8.0/{ASSET}")},{"name":"SHA256SUMS.txt","browser_download_url":format!("https://github.com/{REPOSITORY}/releases/download/v0.8.0/SHA256SUMS.txt")}]})
    }
    #[test]
    fn stable_versions_only() {
        assert!(version("v0.8.0").unwrap() > version("0.7.1").unwrap());
        for s in ["v0.8.0-beta", "1.2", "v1.2.3/evil", "1.2.3.4"] {
            assert!(version(s).is_none());
        }
    }
    #[test]
    fn pinned_assets() {
        assert!(release_from_json(&fixture()).is_ok());
        let mut v = fixture();
        v["assets"][0]["browser_download_url"] = json!("https://example.org/nerve.exe");
        assert!(release_from_json(&v).is_err());
    }
    #[test]
    fn reject_prerelease_and_missing_assets() {
        let mut v = fixture();
        v["prerelease"] = json!(true);
        assert!(release_from_json(&v).is_err());
        v = fixture();
        v["assets"] = json!([]);
        assert!(release_from_json(&v).is_err());
    }
    #[test]
    fn exact_checksum_filename() {
        let sha = "a".repeat(64);
        assert_eq!(checksum(&format!("{sha}  {ASSET}\n")).unwrap(), sha);
        assert!(checksum(&format!("{sha} other.exe")).is_err());
        assert!(checksum("not-a-hash nerve-windows-x86_64.exe").is_err());
    }
}
