//! App catalog: common applications kept at their latest version.
//!
//! For each catalog app the console looks up the newest version (GitHub
//! release, JSON API or web page), downloads the installer once into
//! `repository/catalog/<id>/<version>/`, checks its SHA-256 and writes a
//! package `catalog-<id>` that pushes it to the targets (so they need no
//! internet access). A daily check refreshes the added apps and can deploy a
//! new version to chosen groups straight away.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{error, info};

use crate::app::App;
use crate::db::{DeployOptions, InstalledApp, TargetSpec, now};
use crate::engine;
use crate::groups::{cmp_versions, glob, parse_version, version_matches};
use crate::package::{Package, render};
use crate::update::{CURRENT, is_newer};

const BUILTIN: &str = include_str!("../catalog/apps.yaml");
pub const PACKAGE_PREFIX: &str = "catalog-";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogApp {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub homepage: String,
    pub version: VersionSource,
    pub installer: InstallerSource,
    /// Installed-program name (wildcards) for the detect check.
    pub detect: String,
}

/// Where the latest version number comes from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSource {
    /// "owner/repo": the latest GitHub release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// Web page or JSON document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// JSON pointer to the version inside `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<String>,
    /// Capture group 1 is the version (applied to the page or the GitHub tag).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// Where the installer comes from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallerSource {
    /// Regex for the GitHub release asset name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// URL template ({{version}}, {{version_nodots}}).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// JSON pointer to the URL inside the version document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_json: Option<String>,
    /// File name to save as, when the URL does not end in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// SHA256SUMS-style list to verify the download against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_url: Option<String>,
    /// JSON pointer to the SHA-256 inside the version document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_json: Option<String>,
    /// Silent-install arguments (MSI installs already get /qn /norestart).
    #[serde(default)]
    pub args: String,
}

impl CatalogApp {
    pub fn package_id(&self) -> String {
        format!("{PACKAGE_PREFIX}{}", self.id)
    }

    fn validate(&self) -> Result<()> {
        if self.id.is_empty() || !self.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            bail!("id must use only a-z, 0-9, '-' and '_'");
        }
        let v = &self.version;
        match (&v.github, &v.url) {
            (Some(repo), None) => {
                let ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
                if !repo.split_once('/').is_some_and(|(o, r)| ok(o) && ok(r)) {
                    bail!("version.github must be 'owner/repo'");
                }
                if v.json.is_some() {
                    bail!("version.json only works with version.url");
                }
            }
            (None, Some(url)) => {
                require_https(url)?;
                if v.json.is_some() == v.regex.is_some() {
                    bail!("version.url needs exactly one of 'json' or 'regex'");
                }
            }
            _ => bail!("version needs exactly one of 'github' or 'url'"),
        }
        if let Some(r) = &v.regex {
            let re = Regex::new(r).with_context(|| format!("version.regex '{r}'"))?;
            if re.captures_len() < 2 {
                bail!("version.regex needs a capture group: (...)");
            }
        }
        let i = &self.installer;
        if [&i.asset, &i.url, &i.url_json].iter().filter(|x| x.is_some()).count() != 1 {
            bail!("installer needs exactly one of 'asset', 'url' or 'url_json'");
        }
        if let Some(a) = &i.asset {
            if v.github.is_none() {
                bail!("installer.asset needs version.github");
            }
            Regex::new(a).with_context(|| format!("installer.asset '{a}'"))?;
        }
        if (i.url_json.is_some() || i.sha256_json.is_some()) && v.json.is_none() {
            bail!("installer.url_json / sha256_json read the JSON document of version.url + version.json");
        }
        for t in [&i.url, &i.sha256_url].into_iter().flatten() {
            require_https(t)?;
        }
        for (what, s) in [("detect", &self.detect), ("installer.args", &i.args)] {
            if s.contains("{{") || s.contains("}}") || s.chars().any(char::is_control) {
                bail!("{what} must not contain '{{{{', '}}}}' or line breaks");
            }
        }
        if self.detect.trim().is_empty() {
            bail!("detect (installed-program name) is required");
        }
        Ok(())
    }
}

fn require_https(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("'{url}' must be an https:// URL");
    }
    Ok(())
}

fn parse(text: &str) -> Result<Vec<CatalogApp>> {
    if text.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#')) {
        return Ok(Vec::new());
    }
    let list: Vec<CatalogApp> = serde_yaml::from_str(text).context("invalid catalog YAML")?;
    let mut seen = BTreeSet::new();
    for a in &list {
        a.validate().with_context(|| format!("app '{}'", a.id))?;
        if !seen.insert(&a.id) {
            bail!("duplicate app id '{}'", a.id);
        }
    }
    Ok(list)
}

/// The built-in catalog plus `data/catalog.yaml`, and any error reading the latter.
pub fn load(app: &App) -> (Vec<CatalogApp>, Vec<String>) {
    let mut list = parse(BUILTIN).expect("built-in catalog is valid");
    let mut errors = Vec::new();
    let custom = custom_file(app);
    if custom.exists() {
        match std::fs::read_to_string(&custom).map_err(anyhow::Error::from).and_then(|t| parse(&t)) {
            Ok(extra) => {
                for a in extra {
                    match list.iter_mut().find(|b| b.id == a.id) {
                        Some(b) => *b = a,
                        None => list.push(a),
                    }
                }
            }
            Err(e) => errors.push(format!("{}: {e:#}", custom.display())),
        }
    }
    (list, errors)
}

pub fn custom_file(app: &App) -> std::path::PathBuf {
    app.data_dir.join("catalog.yaml")
}

// -------------------------------------------------------------------- state

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSettings {
    /// Check daily and download new versions of added apps.
    #[serde(default = "yes")]
    pub auto_check: bool,
    #[serde(default)]
    pub last_check: Option<String>,
    #[serde(default)]
    pub apps: BTreeMap<String, AppState>,
}

fn yes() -> bool {
    true
}

impl Default for CatalogSettings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    /// Kept in PSCCM (package + downloaded installer).
    #[serde(default)]
    pub added: bool,
    /// Newest version seen at the source.
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub checked_at: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// The downloaded installer the package uses.
    #[serde(default)]
    pub current: Option<Download>,
    /// Deploy each new version to these groups.
    #[serde(default)]
    pub deploy_groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub version: String,
    /// Repository-relative path.
    pub file: String,
    pub sha256: String,
    pub size: u64,
    /// Matched a checksum published by the vendor (otherwise HTTPS only).
    pub verified: bool,
    pub downloaded_at: String,
}

/// Serializes read-modify-write of the "catalog" setting.
static STATE: Mutex<()> = Mutex::new(());
/// Apps being downloaded: id -> (bytes so far, total).
static BUSY: Mutex<BTreeMap<String, (u64, Option<u64>)>> = Mutex::new(BTreeMap::new());
static CHECKING: AtomicBool = AtomicBool::new(false);

pub fn settings(app: &App) -> Result<CatalogSettings> {
    app.db.get_setting("catalog")
}

fn modify<R>(app: &App, f: impl FnOnce(&mut CatalogSettings) -> R) -> Result<R> {
    let _g = STATE.lock().unwrap();
    let mut s = settings(app)?;
    let r = f(&mut s);
    app.db.set_setting("catalog", &s)?;
    Ok(r)
}

fn modify_app(app: &App, id: &str, f: impl FnOnce(&mut AppState)) -> Result<()> {
    modify(app, |s| f(s.apps.entry(id.to_string()).or_default()))
}

/// Marks an app as downloading until dropped.
struct Busy(String);

impl Busy {
    fn take(id: &str) -> Result<Busy> {
        let mut b = BUSY.lock().unwrap();
        if b.contains_key(id) {
            bail!("'{id}' is already being downloaded");
        }
        b.insert(id.to_string(), (0, None));
        Ok(Busy(id.to_string()))
    }

    fn progress(&self, done: u64, total: Option<u64>) {
        BUSY.lock().unwrap().insert(self.0.clone(), (done, total));
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.lock().unwrap().remove(&self.0);
    }
}

// ------------------------------------------------------------------ resolve

/// The latest version of an app and where to download it.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub version: String,
    pub url: String,
    /// Safe file name to store the installer as.
    pub file: String,
    /// Published by the vendor, when available.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("psccm/{CURRENT}"))
        .connect_timeout(Duration::from_secs(30))
        .build()?)
}

async fn get_text(http: &reqwest::Client, url: &str) -> Result<String> {
    require_https(url)?;
    let mut req = http.get(url).timeout(Duration::from_secs(60));
    if url.starts_with("https://api.github.com/") {
        req = req.header("Accept", "application/vnd.github+json");
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if url.starts_with("https://api.github.com/") && (status == 403 || status == 429) {
        bail!("GitHub API rate limit reached (60 requests per hour); try again later");
    }
    if !status.is_success() {
        bail!("GET {url}: HTTP {status}");
    }
    Ok(resp.text().await?)
}

fn capture(pattern: &str, text: &str, what: &str) -> Result<String> {
    let re = Regex::new(pattern)?;
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .ok_or_else(|| anyhow!("version pattern '{pattern}' not found in {what}"))
}

fn pointer_str(doc: &Value, ptr: &str) -> Result<String> {
    match doc.pointer(ptr) {
        Some(Value::String(s)) => Ok(s.trim().to_string()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => bail!("JSON pointer '{ptr}' not found (or not a string)"),
    }
}

/// Versions end up in file paths and PowerShell, so only allow plain ones.
fn check_version(v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.len() <= 64
        && v.chars().any(|c| c.is_ascii_digit())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'));
    if !ok {
        bail!("unexpected version string '{v}'");
    }
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(x) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(x);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Last path segment of a URL, percent-decoded ("Firefox Setup 1.0.msi").
fn url_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    percent_decode(path.rsplit('/').next().unwrap_or(""))
}

/// File name that is safe on disk and in scripts: spaces become '-'.
fn safe_file_name(name: &str) -> Result<String> {
    let clean: String = name
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let ext = clean.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if !matches!(ext.as_str(), "msi" | "exe") {
        bail!("installer '{name}' is not an .msi or .exe (set installer.file to name it)");
    }
    Ok(clean)
}

/// Path of the download relative to the checksum list's folder
/// (".../159.0/SHA256SUMS" + ".../159.0/win64/en-US/a.msi" -> "win64/en-US/a.msi").
fn relative_to_list(list_url: &str, url: &str) -> String {
    let dir = &list_url[..list_url.rfind('/').map_or(0, |i| i + 1)];
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.strip_prefix(dir) {
        Some(rel) if !dir.is_empty() => percent_decode(rel),
        _ => url_file_name(url),
    }
}

/// Find a file's hash in a SHA256SUMS-style list ("<hash>  [*][./]path").
/// `wanted` is matched as the full listed path, or else as a base name that
/// appears exactly once.
fn find_checksum(list: &str, wanted: &str) -> Option<String> {
    let entries: Vec<(&str, &str)> = list
        .lines()
        .filter_map(|l| {
            let (hash, rest) = l.trim().split_once(char::is_whitespace)?;
            let file = rest.trim().trim_start_matches('*').trim_start_matches("./");
            is_sha256(hash).then_some((hash, file))
        })
        .collect();
    if let Some((h, _)) = entries.iter().find(|(_, f)| *f == wanted) {
        return Some(h.to_ascii_lowercase());
    }
    let base = wanted.rsplit('/').next().unwrap_or(wanted);
    let mut same = entries.iter().filter(|(_, f)| f.rsplit('/').next() == Some(base));
    match (same.next(), same.next()) {
        (Some((h, _)), None) => Some(h.to_ascii_lowercase()),
        _ => None,
    }
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub async fn resolve(http: &reqwest::Client, a: &CatalogApp) -> Result<Resolved> {
    let (v, i) = (&a.version, &a.installer);
    let mut doc: Option<Value> = None;
    let mut asset: Option<GhAsset> = None;
    let version = if let Some(repo) = &v.github {
        let text = get_text(http, &format!("https://api.github.com/repos/{repo}/releases/latest")).await?;
        let rel: GhRelease = serde_json::from_str(&text).context("reading GitHub release")?;
        if let Some(pat) = &i.asset {
            let re = Regex::new(pat)?;
            asset = Some(
                rel.assets
                    .into_iter()
                    .find(|x| re.is_match(&x.name))
                    .ok_or_else(|| anyhow!("release {} of {repo} has no file matching '{pat}'", rel.tag_name))?,
            );
        }
        match &v.regex {
            Some(p) => capture(p, &rel.tag_name, &format!("tag '{}'", rel.tag_name))?,
            None => rel.tag_name.trim_start_matches('v').to_string(),
        }
    } else {
        let url = v.url.as_deref().ok_or_else(|| anyhow!("version needs 'github' or 'url'"))?;
        let text = get_text(http, url).await?;
        match (&v.json, &v.regex) {
            (Some(ptr), _) => {
                let d: Value = serde_json::from_str(&text).with_context(|| format!("{url} did not return JSON"))?;
                let s = pointer_str(&d, ptr)?;
                doc = Some(d);
                s
            }
            (None, Some(p)) => capture(p, &text, url)?,
            (None, None) => bail!("version.url needs 'json' or 'regex'"),
        }
    };
    check_version(&version)?;
    let vars: BTreeMap<String, String> = [
        ("version".to_string(), version.clone()),
        ("version_nodots".to_string(), version.replace('.', "")),
    ]
    .into_iter()
    .collect();
    let (url, mut sha256) = if let Some(x) = asset {
        let sha = x.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(|h| h.to_ascii_lowercase());
        (x.browser_download_url, sha)
    } else if let Some(t) = &i.url {
        (render(t, &vars)?, None)
    } else {
        let ptr = i.url_json.as_deref().unwrap_or_default();
        (pointer_str(doc.as_ref().ok_or_else(|| anyhow!("no JSON document"))?, ptr)?, None)
    };
    require_https(&url)?;
    let raw_name = match &i.file {
        Some(f) => render(f, &vars)?,
        None => url_file_name(&url),
    };
    let file = safe_file_name(&raw_name)?;
    if sha256.is_none() {
        if let Some(p) = &i.sha256_json {
            sha256 = Some(pointer_str(doc.as_ref().ok_or_else(|| anyhow!("no JSON document"))?, p)?.to_ascii_lowercase());
        } else if let Some(t) = &i.sha256_url {
            let list_url = render(t, &vars)?;
            let list = get_text(http, &list_url).await?;
            let wanted = relative_to_list(&list_url, &url);
            sha256 = Some(find_checksum(&list, &wanted).ok_or_else(|| anyhow!("{wanted} is not listed in {list_url}"))?);
        }
    }
    if let Some(h) = &sha256
        && !is_sha256(h) {
            bail!("source published an invalid SHA-256 '{h}'");
        }
    Ok(Resolved { version, url, file, sha256 })
}

// ----------------------------------------------------------------- download

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Stream `url` to `dest`, returning (sha256, size). Fails on a checksum mismatch.
async fn download(http: &reqwest::Client, busy: &Busy, url: &str, dest: &Path, expected: Option<&str>) -> Result<(String, u64)> {
    let mut resp = http.get(url).send().await.with_context(|| format!("GET {url}"))?.error_for_status()?;
    if resp.url().scheme() != "https" {
        bail!("download of {url} was redirected to a non-HTTPS address");
    }
    let total = resp.content_length();
    let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap().to_string_lossy()));
    let mut f = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut n = 0u64;
    let result: Result<()> = async {
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(120), resp.chunk()).await.context("download stalled")?? {
            hasher.update(&chunk);
            f.write_all(&chunk)?;
            n += chunk.len() as u64;
            busy.progress(n, total);
        }
        f.flush()?;
        Ok(())
    }
    .await;
    drop(f);
    let got = hex(&hasher.finalize());
    let check = result.and_then(|_| {
        if n == 0 {
            bail!("{url} returned an empty file");
        }
        if let Some(e) = expected
            && !e.eq_ignore_ascii_case(&got) {
                bail!("SHA-256 mismatch for {url}: expected {e}, got {got}");
            }
        Ok(())
    });
    if let Err(e) = check {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest)?;
    Ok((got, n))
}

/// Quote a string as a YAML scalar.
fn yq(s: &str) -> String {
    serde_yaml::to_string(s).unwrap_or_default().trim_end().to_string()
}

/// The package that installs the downloaded version.
pub fn package_yaml(a: &CatalogApp, d: &Download) -> String {
    let name = a.detect.replace('\'', "''");
    let v = &d.version;
    let check = format!("if (Test-AppInstalled -Name '{name}' -MinVersion '{v}') {{ exit 0 }}");
    let desc = format!(
        "{} {v} from the app catalog. {} Computers with this version or newer are skipped.",
        a.name, a.description
    );
    format!(
        "# Generated by the PSCCM app catalog. It is rewritten when a new version is
# downloaded, so changes here are lost: use Duplicate to make your own copy.
id: {id}
name: {name_y}
version: {ver}
category: App catalog
description: {desc}
platforms: [windows]
detect:
  windows: |
    {check}
    exit 1
steps:
  - name: {step}
    install:
      file: {file}
      args: {args}
      sha256: {sha}
verify:
  windows: |
    {check}
    Get-InstalledApp -Name '{name}' | Format-Table -AutoSize | Out-String
    exit 1
",
        id = a.package_id(),
        name_y = yq(&a.name),
        ver = yq(v),
        desc = yq(&desc),
        step = yq(&format!("Install {} {v}", a.name)),
        file = yq(&d.file),
        args = yq(&a.installer.args),
        sha = yq(&d.sha256),
    )
}

fn write_package(app: &App, a: &CatalogApp, d: &Download) -> Result<()> {
    let yaml = package_yaml(a, d);
    Package::from_yaml(&yaml).context("generated package is invalid")?;
    let path = app.packages_dir().join(format!("{}.yaml", a.package_id()));
    std::fs::write(&path, yaml)?;
    // A package deleted in the console is renamed *.disabled; the app is still added.
    let _ = std::fs::remove_file(path.with_extension("yaml.disabled"));
    Ok(())
}

/// Delete downloaded versions of `id` other than `keep`.
fn prune(app: &App, id: &str, keep: &[&str]) {
    let dir = app.repo_dir().join("catalog").join(id);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let rel = format!("catalog/{id}/{}/", e.file_name().to_string_lossy());
        if e.path().is_dir() && !keep.iter().any(|k| k.starts_with(&rel)) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn find(app: &App, id: &str) -> Result<CatalogApp> {
    load(app).0.into_iter().find(|a| a.id == id).ok_or_else(|| anyhow!("app '{id}' not found in the catalog"))
}

// ------------------------------------------------------------------ actions

/// Look up the latest version of an app without downloading it.
pub async fn lookup(app: &App, a: &CatalogApp) -> Result<String> {
    let r = resolve(&client()?, a).await;
    modify_app(app, &a.id, |st| {
        st.checked_at = Some(now());
        match &r {
            Ok(r) => {
                st.latest = Some(r.version.clone());
                st.error = None;
            }
            Err(e) => st.error = Some(format!("{e:#}")),
        }
    })?;
    Ok(r?.version)
}

/// Make sure the newest version of an added app is downloaded and its
/// package written. With `deploy`, a newly downloaded version is deployed to
/// the app's groups.
pub async fn refresh(app: &App, id: &str, deploy: bool) -> Result<String> {
    let a = find(app, id)?;
    let busy = Busy::take(id)?;
    refresh_with(app, &a, busy, deploy).await
}

async fn refresh_with(app: &App, a: &CatalogApp, busy: Busy, deploy: bool) -> Result<String> {
    let id = a.id.as_str();
    let r = refresh_inner(app, a, &busy, deploy).await;
    let checked = now();
    modify_app(app, id, |st| {
        st.checked_at = Some(checked);
        st.error = r.as_ref().err().map(|e| format!("{e:#}"));
    })?;
    r
}

async fn refresh_inner(app: &App, a: &CatalogApp, busy: &Busy, deploy: bool) -> Result<String> {
    let http = client()?;
    let r = resolve(&http, a).await?;
    modify_app(app, &a.id, |st| st.latest = Some(r.version.clone()))?;
    let previous = settings(app)?.apps.get(&a.id).and_then(|s| s.current.clone());
    let have = previous.clone().filter(|d| app.repo_file(&d.file).is_ok());
    if let Some(d) = &have
        && !is_newer(&r.version, &d.version) {
            write_package(app, a, d)?;
            return Ok(format!("{} {} is up to date", a.name, d.version));
        }

    let ver_dir: String = r.version.chars().map(|c| if c == '+' { '_' } else { c }).collect();
    let rel = format!("catalog/{}/{ver_dir}/{}", a.id, r.file);
    let dest = app.repo_dir().join(&rel);
    std::fs::create_dir_all(dest.parent().unwrap())?;
    info!("catalog: downloading {} {} from {}", a.name, r.version, r.url);
    let (sha256, size) = download(&http, busy, &r.url, &dest, r.sha256.as_deref()).await?;
    let d = Download {
        version: r.version.clone(),
        file: rel,
        sha256,
        size,
        verified: r.sha256.is_some(),
        downloaded_at: now(),
    };
    write_package(app, a, &d)?;
    let groups = modify(app, |s| {
        let st = s.apps.entry(a.id.clone()).or_default();
        st.added = true;
        st.current = Some(d.clone());
        st.deploy_groups.clone()
    })?;
    let mut keep = vec![d.file.as_str()];
    keep.extend(have.as_ref().map(|p| p.file.as_str()));
    prune(app, &a.id, &keep);
    let mut msg = format!("Downloaded {} {} ({:.1} MB)", a.name, d.version, size as f64 / 1_048_576.0);
    if deploy && !groups.is_empty() {
        match deploy_to_groups(app, a, &d, groups) {
            Ok(id) => msg += &format!("; deploying to its groups ({})", &id[..8]),
            Err(e) => msg += &format!("; auto-deploy failed: {e:#}"),
        }
    }
    info!("catalog: {msg}");
    Ok(msg)
}

fn deploy_to_groups(app: &App, a: &CatalogApp, d: &Download, groups: Vec<String>) -> Result<String> {
    let lib = app.library()?;
    let pkg = lib.packages.get(&a.package_id()).ok_or_else(|| anyhow!("package '{}' not found", a.package_id()))?;
    let targets = TargetSpec { groups, ..Default::default() };
    let source = format!("catalog: {} {}", a.name, d.version);
    let id = engine::create_deployment(app, pkg, BTreeMap::new(), targets, DeployOptions::default(), &source, None)?;
    engine::start(app, id.clone());
    Ok(id)
}

/// Add an app: download its latest installer and create its package.
pub async fn add(app: &App, id: &str) -> Result<String> {
    find(app, id)?;
    modify_app(app, id, |st| st.added = true)?;
    refresh(app, id, false).await
}

/// Remove an app's package and downloaded installers.
pub fn remove(app: &App, id: &str) -> Result<()> {
    if BUSY.lock().unwrap().contains_key(id) {
        bail!("'{id}' is being downloaded; try again when it has finished");
    }
    let pkg = app.packages_dir().join(format!("{PACKAGE_PREFIX}{id}.yaml"));
    let _ = std::fs::remove_file(&pkg);
    let _ = std::fs::remove_file(pkg.with_extension("yaml.disabled"));
    let dir = app.repo_dir().join("catalog").join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
    }
    modify_app(app, id, |st| {
        st.added = false;
        st.current = None;
        st.deploy_groups.clear();
        st.error = None;
    })
}

pub fn set_groups(app: &App, id: &str, groups: Vec<String>) -> Result<()> {
    find(app, id)?;
    let groups: Vec<String> = groups.into_iter().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect();
    modify_app(app, id, |st| st.deploy_groups = groups)
}

pub fn set_auto_check(app: &App, on: bool) -> Result<()> {
    modify(app, |s| s.auto_check = on)
}

/// Look up every app; download new versions of the added ones.
pub async fn check_all(app: &App, deploy: bool) -> Vec<(String, Result<String>)> {
    let _ = modify(app, |s| s.last_check = Some(now()));
    let added: BTreeSet<String> = settings(app)
        .map(|s| s.apps.into_iter().filter(|(_, a)| a.added).map(|(k, _)| k).collect())
        .unwrap_or_default();
    let jobs = load(app).0.into_iter().map(|a| {
        let app = app.clone();
        let is_added = added.contains(&a.id);
        async move {
            let r = if is_added { refresh(&app, &a.id, deploy).await } else { lookup(&app, &a).await.map(|v| format!("latest is {v}")) };
            (a.id, r)
        }
    });
    futures::stream::iter(jobs).buffer_unordered(4).collect().await
}

/// Run [`check_all`] in the background (no-op when one is running).
pub fn start_check(app: &App, deploy: bool) {
    if CHECKING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        for (id, r) in check_all(&app, deploy).await {
            if let Err(e) = r {
                error!("catalog: {id}: {e:#}");
            }
        }
        CHECKING.store(false, Ordering::SeqCst);
    });
}

/// Run [`add`] or [`refresh`] in the background. Unknown or busy apps fail
/// right away; the app shows as downloading as soon as this returns.
pub fn start_refresh(app: &App, id: &str, adding: bool) -> Result<()> {
    let a = find(app, id)?;
    let busy = Busy::take(id)?;
    if adding {
        modify_app(app, id, |st| st.added = true)?;
    }
    let app = app.clone();
    tokio::spawn(async move {
        // Adding never auto-deploys; a later new version does.
        if let Err(e) = refresh_with(&app, &a, busy, !adding).await {
            error!("catalog: {}: {e:#}", a.id);
        }
    });
    Ok(())
}

/// Called by the scheduler: the daily check.
pub fn tick(app: &App) {
    let Ok(s) = settings(app) else { return };
    if !s.auto_check || !s.apps.values().any(|a| a.added) {
        return;
    }
    let due = s
        .last_check
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_none_or(|t| chrono::Utc::now() - t.with_timezone(&chrono::Utc) > chrono::Duration::hours(24));
    if due {
        info!("catalog: daily check");
        start_check(app, true);
    }
}

/// Everything the console page shows.
pub fn overview(app: &App) -> Result<Value> {
    let (list, errors) = load(app);
    let s = settings(app)?;
    let busy = BUSY.lock().unwrap().clone();
    let apps: Vec<Value> = list
        .iter()
        .map(|a| {
            let st = s.apps.get(&a.id).cloned().unwrap_or_default();
            let update_available = st.added
                && match (&st.latest, &st.current) {
                    (Some(l), Some(c)) => is_newer(l, &c.version),
                    _ => false,
                };
            json!({
                "id": a.id, "name": a.name, "category": a.category, "description": a.description,
                "homepage": a.homepage, "package_id": a.package_id(),
                "added": st.added, "latest": st.latest, "checked_at": st.checked_at, "error": st.error,
                "current": st.current, "deploy_groups": st.deploy_groups, "update_available": update_available,
                "busy": busy.get(&a.id).map(|(done, total)| json!({ "done": done, "total": total })),
            })
        })
        .collect();
    Ok(json!({
        "auto_check": s.auto_check,
        "last_check": s.last_check,
        "checking": CHECKING.load(Ordering::SeqCst),
        "custom_file": custom_file(app),
        "errors": errors,
        "apps": apps,
    }))
}

/// One catalog app as found in the inventory.
#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub computer: String,
    /// Highest installed version that matches the app's detect name.
    pub version: String,
}

/// Split scanned Windows computers that have app `a` into up to date and
/// outdated, using the same rule as the package's detect check: a computer is
/// up to date when any matching program has `target` or newer.
pub fn compare_installed(a: &CatalogApp, target: &str, inventory: &[(String, Vec<InstalledApp>)]) -> (Vec<Installed>, Vec<Installed>) {
    let (mut current, mut outdated) = (Vec::new(), Vec::new());
    for (computer, apps) in inventory {
        let matching: Vec<&InstalledApp> = apps.iter().filter(|x| glob(&a.detect, &x.name)).collect();
        let Some(best) = matching.iter().max_by(|x, y| cmp_versions(&parse_version(&x.version), &parse_version(&y.version)))
        else {
            continue;
        };
        let entry = Installed { computer: computer.clone(), version: best.version.clone() };
        if matching.iter().any(|x| version_matches(&format!(">={target}"), &x.version)) {
            current.push(entry);
        } else {
            outdated.push(entry);
        }
    }
    (current, outdated)
}

/// Catalog apps installed on scanned Windows computers and which of those
/// computers have an older version than the latest one.
pub fn outdated(app: &App) -> Result<Value> {
    let (list, _) = load(app);
    let s = settings(app)?;
    let windows: BTreeSet<i64> = app.db.list_computers()?.into_iter().filter(|c| c.os == "windows").map(|c| c.id).collect();
    let inventory: Vec<(String, Vec<InstalledApp>)> = app
        .db
        .list_inventory(true)?
        .into_iter()
        .filter(|i| i.scanned_at.is_some() && windows.contains(&i.computer_id))
        .map(|i| (i.computer, i.apps))
        .collect();
    let mut apps: Vec<Value> = Vec::new();
    for a in &list {
        let st = s.apps.get(&a.id).cloned().unwrap_or_default();
        let package_version = st.current.as_ref().map(|c| c.version.clone());
        // Without a looked-up latest version, compare with the downloaded one.
        let target = st.latest.clone().or_else(|| package_version.clone());
        let (current, outdated) = match &target {
            Some(t) => compare_installed(a, t, &inventory),
            // With no known version nothing can be called outdated.
            None => {
                let (c, o) = compare_installed(a, "0", &inventory);
                ([c, o].concat(), vec![])
            }
        };
        if current.is_empty() && outdated.is_empty() {
            continue;
        }
        // The package would install an older version than the latest one.
        let package_behind = matches!((&st.latest, &package_version), (Some(l), Some(p)) if is_newer(l, p));
        apps.push(json!({
            "id": a.id, "name": a.name, "detect": a.detect, "package_id": a.package_id(), "added": st.added,
            "latest": st.latest, "checked_at": st.checked_at, "package_version": package_version, "package_behind": package_behind,
            "installed": current.len() + outdated.len(), "current": current, "outdated": outdated,
        }));
    }
    apps.sort_by_key(|v| (std::cmp::Reverse(v["outdated"].as_array().map_or(0, Vec::len)), v["name"].as_str().unwrap_or("").to_lowercase()));
    Ok(json!({ "last_check": s.last_check, "scanned": inventory.len(), "apps": apps }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_installed_versions() {
        let a: CatalogApp = parse("- id: x\n  name: X\n  detect: Python 3.* (64-bit)\n  version: { github: o/r }\n  installer: { asset: x }\n")
            .unwrap()
            .remove(0);
        let app = |n: &str, v: &str| InstalledApp { name: n.into(), version: v.into(), publisher: String::new() };
        let inv = vec![
            ("old".to_string(), vec![app("Python 3.11.4 (64-bit)", "3.11.4150.0")]),
            // Two side by side: the newer one makes it up to date.
            ("both".to_string(), vec![app("Python 3.11.4 (64-bit)", "3.11.4150.0"), app("Python 3.13.1 (64-bit)", "3.13.1150.0")]),
            ("none".to_string(), vec![app("Python Launcher", "3.13.1150.0")]),
        ];
        let (current, outdated) = compare_installed(&a, "3.13.1", &inv);
        assert_eq!(current.iter().map(|i| (i.computer.as_str(), i.version.as_str())).collect::<Vec<_>>(), [("both", "3.13.1150.0")]);
        assert_eq!(outdated.iter().map(|i| (i.computer.as_str(), i.version.as_str())).collect::<Vec<_>>(), [("old", "3.11.4150.0")]);
    }

    fn fake_download(a: &CatalogApp) -> Download {
        Download {
            version: "1.2.3".into(),
            file: format!("catalog/{}/1.2.3/setup.msi", a.id),
            sha256: "ab".repeat(32),
            size: 1,
            verified: true,
            downloaded_at: now(),
        }
    }

    #[test]
    fn builtin_catalog_makes_valid_packages() {
        let list = parse(BUILTIN).unwrap();
        assert!(list.len() >= 10);
        let lib = std::collections::HashMap::new();
        for a in &list {
            let p = Package::from_yaml(&package_yaml(a, &fake_download(a))).unwrap_or_else(|e| panic!("{}: {e:#}", a.id));
            assert_eq!(p.id, format!("catalog-{}", a.id));
            let mut vars = BTreeMap::new();
            p.plan(crate::package::Os::Windows, &lib, &mut vars).unwrap();
        }
    }

    #[test]
    fn quotes_in_names_stay_quoted() {
        let mut a = parse(BUILTIN).unwrap().remove(0);
        a.name = "Bob's \"Tool\": #1".into();
        a.detect = "Bob's Tool*".into();
        let yaml = package_yaml(&a, &fake_download(&a));
        let p = Package::from_yaml(&yaml).unwrap();
        assert_eq!(p.name, a.name);
        assert!(p.detect.windows.unwrap().contains("-Name 'Bob''s Tool*'"));
    }

    #[test]
    fn rejects_bad_definitions_and_versions() {
        let base = "- id: x\n  name: X\n  detect: X\n";
        let bad = [
            "  version: { url: 'http://example.com', regex: '(\\d+)' }\n  installer: { url: 'https://e/x.msi' }\n",
            "  version: { url: 'https://example.com', regex: '\\d+' }\n  installer: { url: 'https://e/x.msi' }\n",
            "  version: { url: 'https://example.com', regex: '(\\d+)' }\n  installer: { asset: 'x' }\n",
            "  version: { github: 'nope' }\n  installer: { asset: 'x' }\n",
        ];
        for b in bad {
            assert!(parse(&format!("{base}{b}")).is_err(), "{b}");
        }
        assert!(parse(&format!("{base}  version: {{ github: a/b }}\n  installer: {{ asset: 'x' }}\n")).is_ok());
        assert!(check_version("24.09").is_ok());
        assert!(check_version("21.0.12+101.0.LTS").is_ok());
        for v in ["", "abc", "1.0'; Remove-Item", "1.0 {{host}}", "1/../2"] {
            assert!(check_version(v).is_err(), "{v}");
        }
    }

    #[test]
    fn file_names_and_checksums() {
        let url = "https://cdn/pub/firefox/releases/159.0/win64/en-US/Firefox%20Setup%20159.0.msi";
        assert_eq!(url_file_name(url), "Firefox Setup 159.0.msi");
        assert_eq!(safe_file_name("Firefox Setup 159.0.msi").unwrap(), "Firefox-Setup-159.0.msi");
        assert!(safe_file_name("stable").is_err());
        let h = "f6996d56e47c65d781faeb65679a021899baa5876c0a548bc79d083ea41c1e46";
        let rel = relative_to_list("https://cdn/pub/firefox/releases/159.0/SHA256SUMS", url);
        assert_eq!(rel, "win64/en-US/Firefox Setup 159.0.msi");
        // The same file name for every language: only the full path picks the right one.
        let list = format!("{}  win64/de/Firefox Setup 159.0.msi\n{h}  win64/en-US/Firefox Setup 159.0.msi\n", "0".repeat(64));
        assert_eq!(find_checksum(&list, &rel).as_deref(), Some(h));
        assert_eq!(find_checksum(&list, "Firefox Setup 159.0.msi"), None);
        assert_eq!(find_checksum(&format!("{h} *w64/putty.msi"), "putty.msi").as_deref(), Some(h));
        assert_eq!(find_checksum(&format!("{h}  node-v1.msi"), "node-v1.msi").as_deref(), Some(h));
        assert_eq!(find_checksum("zz  a.msi", "a.msi"), None);
    }

    #[test]
    fn node_lts_pattern() {
        let a = parse(BUILTIN).unwrap().into_iter().find(|a| a.id == "nodejs-lts").unwrap();
        let tab = "version\tdate\tfiles\tnpm\tv8\tuv\tzlib\topenssl\tmodules\tlts\tsecurity\n\
            v27.0.0\t2026-10-28\tsrc\t11\t14\t1\t1\t3\t150\t-\t-\n\
            v26.13.0\t2026-10-28\tsrc,win-x64-msi\t11.20.0\t14.6\t1.52.1\t1.3\t3.5.9\t147\tLithium\t-\n";
        assert_eq!(capture(a.version.regex.as_deref().unwrap(), tab, "t").unwrap(), "26.13.0");
    }

    /// Resolves every built-in app against the real sources (network):
    /// `cargo test catalog_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn catalog_live() {
        let http = client().unwrap();
        let mut failed = Vec::new();
        for a in parse(BUILTIN).unwrap() {
            match resolve(&http, &a).await {
                Ok(r) => println!("{:<18} {:<20} {} {}", a.id, r.version, r.file, r.sha256.as_deref().map(|_| "sha256").unwrap_or("-")),
                Err(e) => failed.push(format!("{}: {e:#}", a.id)),
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }
}
