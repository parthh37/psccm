//! Auto-update from GitHub releases.
//!
//! A release carries `psccm-setup-<ver>.exe` (Windows installer),
//! `psccm-x86_64-linux` (Linux binary) and `SHA256SUMS`. Updates are only
//! installed when the downloaded file matches its checksum.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::app::App;
use crate::db::now;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_REPO: &str = match option_env!("PSCCM_UPDATE_REPO") {
    Some(r) => r,
    None => "parthh37/psccm",
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// Look for a new release once a day.
    #[serde(default = "yes")]
    pub auto_check: bool,
    /// Install it automatically when no deployment is running.
    #[serde(default)]
    pub auto_install: bool,
    /// GitHub "owner/repo" to update from.
    #[serde(default = "default_repo")]
    pub repo: String,
    #[serde(default)]
    pub last_check: Option<String>,
    #[serde(default)]
    pub latest: Option<Release>,
}

fn yes() -> bool {
    true
}
fn default_repo() -> String {
    DEFAULT_REPO.into()
}

impl Default for UpdateSettings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
    pub published_at: String,
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub sha256: Option<String>,
    pub newer: bool,
}

/// Numeric version compare: "0.10.1" > "0.9.3". Pre-release suffixes are ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(candidate), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// File name of the release asset for this platform.
fn wanted_asset(name: &str) -> bool {
    if cfg!(windows) {
        name.starts_with("psccm-setup") && name.ends_with(".exe")
    } else {
        name == "psccm-x86_64-linux"
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("psccm/{CURRENT}"))
        .timeout(std::time::Duration::from_secs(300))
        .build()?)
}

pub fn status(app: &App) -> serde_json::Value {
    let s: UpdateSettings = app.db.get_setting("update").unwrap_or_default();
    serde_json::json!({
        "current": CURRENT,
        "settings": s,
        "installed": installed_with_setup(),
        "platform": std::env::consts::OS,
    })
}

/// Running from an Inno Setup install (uninstaller next to the exe)?
fn installed_with_setup() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("unins000.exe").exists()))
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Ask GitHub for the latest release and remember the answer.
pub async fn check(app: &App) -> Result<Release> {
    let mut s: UpdateSettings = app.db.get_setting("update")?;
    let http = client()?;
    let url = format!("https://api.github.com/repos/{}/releases/latest", s.repo);
    let resp = http.get(&url).header("Accept", "application/vnd.github+json").send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no releases published in {} yet", s.repo);
    }
    let gh: GhRelease = resp.error_for_status()?.json().await.context("reading release info")?;
    let asset = gh.assets.iter().find(|a| wanted_asset(&a.name));
    let mut sha256 = None;
    if let (Some(a), Some(sums)) = (asset, gh.assets.iter().find(|a| a.name == "SHA256SUMS")) {
        let text = http.get(&sums.browser_download_url).send().await?.error_for_status()?.text().await?;
        sha256 = text.lines().find_map(|l| {
            let (hash, name) = l.split_once(char::is_whitespace)?;
            (name.trim().trim_start_matches('*') == a.name).then(|| hash.to_lowercase())
        });
    }
    let version = gh.tag_name.trim_start_matches('v').to_string();
    let release = Release {
        newer: is_newer(&version, CURRENT),
        version,
        url: gh.html_url,
        notes: gh.body.unwrap_or_default(),
        published_at: gh.published_at.unwrap_or_default(),
        asset_name: asset.map(|a| a.name.clone()),
        asset_url: asset.map(|a| a.browser_download_url.clone()),
        sha256,
    };
    s.last_check = Some(now());
    s.latest = Some(release.clone());
    app.db.set_setting("update", &s)?;
    Ok(release)
}

/// Download, verify and start installing the latest release. On Windows the
/// installer is launched (it stops and restarts the PSCCM service); on Linux
/// the binary is replaced and the process exits so systemd restarts it.
pub async fn install(app: &App) -> Result<String> {
    let release = check(app).await?;
    if !release.newer {
        bail!("already up to date ({CURRENT})");
    }
    if app.db.list_deployments(50)?.iter().any(|d| d.status == "running") {
        bail!("a deployment is running; try again when it has finished");
    }
    let (name, url) = release
        .asset_name
        .clone()
        .zip(release.asset_url.clone())
        .ok_or_else(|| anyhow!("release {} has no download for this platform", release.version))?;
    let expected = release.sha256.clone().ok_or_else(|| anyhow!("release has no SHA256SUMS entry for {name}; refusing to install"))?;
    let bytes = client()?.get(&url).send().await?.error_for_status()?.bytes().await?;
    let got = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got != expected {
        bail!("checksum mismatch for {name} (expected {expected}, got {got})");
    }
    let dir = app.data_dir.join("updates");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(&name);
    std::fs::write(&file, &bytes)?;
    info!("update {} downloaded and verified: {}", release.version, file.display());
    launch(&file, &dir)?;
    Ok(format!("Installing PSCCM {} — the console restarts in a minute.", release.version))
}

#[cfg(windows)]
fn launch(file: &std::path::Path, dir: &std::path::Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(file)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
        .arg(format!("/LOG={}", dir.join("install.log").display()))
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context("starting the installer")?;
    Ok(())
}

#[cfg(not(windows))]
fn launch(file: &std::path::Path, _dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe()?;
    let staged = exe.with_extension("new");
    std::fs::copy(file, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, &exe).context("replacing the executable (permissions?)")?;
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        info!("exiting to restart into the new version");
        std::process::exit(0);
    });
    Ok(())
}

/// Called by the scheduler: daily check, optional automatic install.
pub async fn tick(app: &App) -> Result<()> {
    let s: UpdateSettings = app.db.get_setting("update")?;
    if !s.auto_check {
        return Ok(());
    }
    let due = s
        .last_check
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_none_or(|t| chrono::Utc::now() - t.with_timezone(&chrono::Utc) > chrono::Duration::hours(24));
    if !due {
        return Ok(());
    }
    let release = match check(app).await {
        Ok(r) => r,
        Err(e) => {
            // Remember the attempt so a missing network doesn't retry every tick.
            let mut s = s;
            s.last_check = Some(now());
            app.db.set_setting("update", &s)?;
            return Err(e);
        }
    };
    if release.newer && s.auto_install {
        info!("auto-installing PSCCM {}", release.version);
        install(app).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn versions() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.9.0"));
        assert!(is_newer("0.2.1", "0.2"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.2.0-rc1", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
    }
}
