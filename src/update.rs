//! Auto-update from GitHub releases.
//!
//! A release carries `psccm-setup-<ver>.exe` (Windows installer),
//! `psccm-x86_64-linux` (Linux binary), `SHA256SUMS` and `SHA256SUMS.sig`.
//! Updates are only installed when `SHA256SUMS.sig` is a valid signature by
//! the release key built into this binary and the download matches its
//! checksum, so a release published by anyone else (a stolen GitHub token, a
//! changed `repo` setting) is refused.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, VerifyingKey};
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

/// Public key (base64) that release signatures are checked against. Building
/// with `PSCCM_UPDATE_PUBKEY` set (e.g. for a fork) replaces the default.
pub const UPDATE_PUBKEY: &str = match option_env!("PSCCM_UPDATE_PUBKEY") {
    Some(k) if !k.is_empty() => k,
    _ => RELEASE_PUBKEY,
};
/// Public half of the `PSCCM_SIGNING_KEY` secret the release workflow signs
/// with (README, "Release signing"). Empty: this build installs no updates.
const RELEASE_PUBKEY: &str = "";

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
    /// Why the release's signature could not be verified (it is not installed).
    #[serde(default)]
    pub signature_error: Option<String>,
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
    let named = |n: &str| gh.assets.iter().find(|a| a.name == n);
    let (mut sha256, mut signature_error) = (None, None);
    if let (Some(a), Some(sums)) = (asset, named("SHA256SUMS")) {
        let sums = http.get(&sums.browser_download_url).send().await?.error_for_status()?.bytes().await?;
        let sig = match named("SHA256SUMS.sig") {
            Some(s) => http.get(&s.browser_download_url).send().await?.error_for_status()?.text().await?,
            None => String::new(),
        };
        match verify_sums(UPDATE_PUBKEY, &gh.tag_name, &sums, &sig) {
            Ok(()) => sha256 = sum_for(&String::from_utf8_lossy(&sums), &a.name),
            Err(e) => signature_error = Some(format!("{e:#}")),
        }
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
        signature_error,
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
    if let Some(e) = &release.signature_error {
        bail!("refusing to install {}: {e}", release.version);
    }
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

/// What the release workflow signs: a line naming the tag, then SHA256SUMS.
/// Including the tag stops an old signed release from being re-published
/// under a newer tag to downgrade installs.
fn signed_message(tag: &str, sums: &[u8]) -> Vec<u8> {
    [format!("psccm-release {tag}\n").as_bytes(), sums].concat()
}

/// Check `sig` (the base64 contents of SHA256SUMS.sig) over a release's tag
/// and SHA256SUMS with the base64 ed25519 `pubkey`, given either as the raw
/// 32 bytes or as the body of `openssl pkey -pubout`.
fn verify_sums(pubkey: &str, tag: &str, sums: &[u8], sig: &str) -> Result<()> {
    if pubkey.is_empty() {
        bail!("this build has no release signing key, so it cannot verify updates; install new versions by hand");
    }
    // An SPKI public key is a fixed 12-byte header followed by the raw key.
    const SPKI: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    let key = B64.decode(pubkey.trim()).context("decoding the release signing key")?;
    let raw: &[u8; 32] = key
        .strip_prefix(&SPKI[..])
        .unwrap_or(&key)
        .try_into()
        .map_err(|_| anyhow!("the release signing key is not an ed25519 key"))?;
    let key = VerifyingKey::from_bytes(raw).map_err(|_| anyhow!("the release signing key is not a valid ed25519 key"))?;
    if sig.trim().is_empty() {
        bail!("the release is not signed (no SHA256SUMS.sig)");
    }
    let sig = B64.decode(sig.trim()).ok().and_then(|b| Signature::from_slice(&b).ok());
    let sig = sig.ok_or_else(|| anyhow!("SHA256SUMS.sig is not an ed25519 signature"))?;
    key.verify_strict(&signed_message(tag, sums), &sig)
        .map_err(|_| anyhow!("SHA256SUMS.sig does not match: the release was not signed with the PSCCM release key"))
}

/// The hash for `name` in a SHA256SUMS list.
fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_lowercase())
    })
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
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const SUMS: &[u8] = b"abc123  psccm-setup-1.2.3.exe\ndef456  psccm-x86_64-linux\n";

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[7; 32]);
        let pk = B64.encode(sk.verifying_key().as_bytes());
        (sk, pk)
    }

    fn sign(sk: &SigningKey, tag: &str, sums: &[u8]) -> String {
        B64.encode(sk.sign(&signed_message(tag, sums)).to_bytes())
    }

    #[test]
    fn valid_signature() {
        let (sk, pk) = keypair();
        verify_sums(&pk, "v1.2.3", SUMS, &sign(&sk, "v1.2.3", SUMS)).unwrap();
        // With the trailing newline `base64` writes in the workflow.
        verify_sums(&pk, "v1.2.3", SUMS, &format!("{}\n", sign(&sk, "v1.2.3", SUMS))).unwrap();
    }

    #[test]
    fn tampered_release() {
        let (sk, pk) = keypair();
        let sig = sign(&sk, "v1.2.3", SUMS);
        let evil = b"000000  psccm-setup-1.2.3.exe\ndef456  psccm-x86_64-linux\n";
        assert!(verify_sums(&pk, "v1.2.3", evil, &sig).is_err());
        // A signed old release re-published under a newer tag.
        assert!(verify_sums(&pk, "v9.9.9", SUMS, &sig).is_err());
        let mut bytes = B64.decode(&sig).unwrap();
        bytes[10] ^= 1;
        assert!(verify_sums(&pk, "v1.2.3", SUMS, &B64.encode(bytes)).is_err());
    }

    #[test]
    fn wrong_key() {
        let (_, pk) = keypair();
        let other = SigningKey::from_bytes(&[9; 32]);
        let err = verify_sums(&pk, "v1.2.3", SUMS, &sign(&other, "v1.2.3", SUMS)).unwrap_err();
        assert!(err.to_string().contains("not signed with the PSCCM release key"), "{err}");
    }

    #[test]
    fn missing_or_garbage_signature() {
        let (_, pk) = keypair();
        let err = verify_sums(&pk, "v1.2.3", SUMS, "").unwrap_err();
        assert!(err.to_string().contains("not signed"), "{err}");
        assert!(verify_sums(&pk, "v1.2.3", SUMS, "not base64!").is_err());
        assert!(verify_sums(&pk, "v1.2.3", SUMS, &B64.encode([1u8; 32])).is_err());
    }

    #[test]
    fn no_key_built_in() {
        let (sk, _) = keypair();
        let err = verify_sums("", "v1.2.3", SUMS, &sign(&sk, "v1.2.3", SUMS)).unwrap_err();
        assert!(err.to_string().contains("no release signing key"), "{err}");
        assert!(verify_sums("AAAA", "v1.2.3", SUMS, &sign(&sk, "v1.2.3", SUMS)).is_err());
    }

    /// A signature made the way the release workflow makes it, with
    /// `openssl pkeyutl -sign -rawin` (throwaway key).
    #[test]
    fn openssl_signature() {
        let sums = b"abc123  psccm-x86_64-linux\n";
        let sig = "tKQQojm1XDEG4lr+XLdGgBGTnLaGwuc+zx5aqTn55cgEI+PYkyD9buuFxZ28XdXS3sWvcs/QK9249V4WDPPzAA==";
        let raw = "kKQlq0qEUvIIuM/KfH2MMNNN1HEIkHRojCyX9IZC9RQ=";
        let spki = "MCowBQYDK2VwAyEAkKQlq0qEUvIIuM/KfH2MMNNN1HEIkHRojCyX9IZC9RQ=";
        verify_sums(raw, "v1.2.3", sums, sig).unwrap();
        verify_sums(spki, "v1.2.3", sums, sig).unwrap();
        assert!(verify_sums(spki, "v1.2.4", sums, sig).is_err());
    }

    #[test]
    fn sums_lookup() {
        let text = "ABC123  psccm-setup-1.2.3.exe\ndef456 *psccm-x86_64-linux\n";
        assert_eq!(sum_for(text, "psccm-setup-1.2.3.exe").as_deref(), Some("abc123"));
        assert_eq!(sum_for(text, "psccm-x86_64-linux").as_deref(), Some("def456"));
        assert_eq!(sum_for(text, "psccm"), None);
    }

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
