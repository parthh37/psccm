//! Active Directory backup and restore.
//!
//! Backups only read:
//! - **Objects**: users, groups, computers, contacts and OUs with their
//!   attributes and group memberships, read with the same LDAP searches as the
//!   AD import (`ad::search`) and kept as JSON in the data folder.
//! - **Group Policy**: `Backup-GPO -All` on a domain controller (or the
//!   console), brought back as a zip, plus every GPO's links.
//! - **System State** of a DC: the built-in package `ad-systemstate-backup`
//!   (wbadmin), scheduled like any package.
//!
//! A restore writes to AD, so it never runs from the console itself. The
//! console builds a plan (a diff against live AD, or a full rebuild into a new
//! domain for disaster recovery), the user confirms it twice (review every
//! item, then type the domain name), and only then does it run as a
//! deployment job on a domain controller with the AD PowerShell module. The
//! job refuses to change anything when the DC is not in the confirmed domain.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ad::{self, AdSettings, Base, Entry};
use crate::app::App;
use crate::db::{DeployOptions, TargetSpec, now};
use crate::gpo::{self, GpoLink};
use crate::package::{Os, Package, Shell};
use crate::transport::{LogFn, default_port, detect_os, port_open, wrap_script};

const RESTORE_PS: &str = include_str!("assets/ad-restore.ps1");
const GPO_BACKUP_PS: &str = include_str!("assets/gpo-backup.ps1");

/// Deployment source of restore jobs; they cannot be re-run without a new plan.
pub const RESTORE_SOURCE: &str = "ad-restore";
pub const SYSTEMSTATE_PACKAGE: &str = "ad-systemstate-backup";
pub const NEW_FOREST_PACKAGE: &str = "ad-ds-new-forest";
/// How long a plan may wait for its two confirmations.
const PLAN_MINUTES: i64 = 30;

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSettings {
    /// Back up AD objects every N hours (0 = only by hand).
    #[serde(default)]
    pub objects_hours: u32,
    /// Back up all GPOs every N hours (0 = only by hand).
    #[serde(default)]
    pub gpo_hours: u32,
    /// Automatic backups of each kind to keep. Backups taken by hand stay until deleted.
    #[serde(default = "default_keep")]
    pub keep: u32,
    /// Computer (a DC) that runs Backup-GPO. Empty = the console itself (needs GPMC).
    #[serde(default)]
    pub gpo_host: String,
    /// Domain controller suggested for restores.
    #[serde(default)]
    pub restore_dc: String,
    #[serde(default)]
    pub last_objects: Option<String>,
    #[serde(default)]
    pub last_gpo: Option<String>,
}

fn default_keep() -> u32 {
    14
}
fn yes() -> bool {
    true
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl Default for BackupSettings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

pub fn settings(app: &App) -> BackupSettings {
    app.db.get_setting("ad_backup").unwrap_or_default()
}

/// Save the editable settings, keeping the "last run" times.
pub fn save_settings(app: &App, mut s: BackupSettings) -> Result<()> {
    let old = settings(app);
    s.last_objects = old.last_objects;
    s.last_gpo = old.last_gpo;
    s.gpo_host = s.gpo_host.trim().to_string();
    s.restore_dc = s.restore_dc.trim().to_string();
    s.keep = s.keep.max(1);
    app.db.set_setting("ad_backup", &s)
}

/// Console-side state: plans waiting for confirmation and the running backup.
#[derive(Default)]
pub struct State {
    pub plans: HashMap<String, RestorePlan>,
    pub running: Option<String>,
}

/// Marks a backup as running; cleared when dropped.
struct Busy<'a>(&'a App);

impl<'a> Busy<'a> {
    fn start(app: &'a App, what: &str) -> Result<Busy<'a>> {
        let mut st = app.ad_backup.lock().unwrap();
        if let Some(r) = &st.running {
            bail!("an AD backup is already running ({r})");
        }
        st.running = Some(what.to_string());
        Ok(Busy(app))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.ad_backup.lock().unwrap().running = None;
    }
}

// ------------------------------------------------------------------ backups

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupMeta {
    pub id: String,
    /// "objects" or "gpo"
    pub kind: String,
    pub created_at: String,
    /// DNS name of the domain, e.g. corp.local
    pub domain: String,
    /// e.g. DC=corp,DC=local
    pub root_dn: String,
    /// "console" or the computer that ran Backup-GPO
    pub source: String,
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub note: String,
    /// Objects by class, or GPOs / links.
    #[serde(default)]
    pub counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One directory object as backed up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdObject {
    pub guid: String,
    pub dn: String,
    /// Relative name (the CN/OU value).
    pub name: String,
    /// user, inetOrgPerson, computer, group, contact, organizationalUnit, foreignSecurityPrincipal
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// isCriticalSystemObject: built-in accounts and groups, DCs. Never re-created.
    #[serde(default, skip_serializing_if = "is_false")]
    pub critical: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<String>,
    /// Writable attributes (lower-case names), as stored in AD.
    #[serde(default)]
    pub attrs: BTreeMap<String, Vec<String>>,
    /// DNs of the groups this object is a member of (not its primary group).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_of: Vec<String>,
}

/// One GPO in a GPO backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpoEntry {
    pub backup_id: String,
    pub guid: String,
    pub name: String,
    #[serde(default)]
    pub links: Vec<GpoLink>,
}

const OBJECT_FILTER: &str = "(|(objectCategory=person)(objectCategory=group)(objectCategory=computer)(objectCategory=organizationalUnit)(objectCategory=foreignSecurityPrincipal))";

const META_ATTRS: &[&str] = &["objectClass", "objectGUID", "objectSid", "name", "whenChanged", "isCriticalSystemObject", "memberOf"];

/// Attributes that are backed up and can be restored.
const DATA_ATTRS: &[&str] = &[
    "description", "displayName", "managedBy", "info", "sAMAccountName", "userPrincipalName", "givenName", "sn",
    "initials", "mail", "telephoneNumber", "mobile", "otherTelephone", "homePhone", "ipPhone", "pager",
    "facsimileTelephoneNumber", "title", "department", "company", "manager", "physicalDeliveryOfficeName",
    "streetAddress", "postOfficeBox", "l", "st", "postalCode", "co", "c", "countryCode", "wWWHomePage", "employeeID",
    "employeeNumber", "employeeType", "homeDirectory", "homeDrive", "scriptPath", "profilePath", "userWorkstations",
    "userAccountControl", "accountExpires", "proxyAddresses", "servicePrincipalName", "groupType", "gPLink",
    "gPOptions", "street", "dNSHostName", "operatingSystem", "operatingSystemVersion", "location",
    "extensionAttribute1", "extensionAttribute2", "extensionAttribute3", "extensionAttribute4", "extensionAttribute5",
    "extensionAttribute6", "extensionAttribute7", "extensionAttribute8", "extensionAttribute9", "extensionAttribute10",
    "extensionAttribute11", "extensionAttribute12", "extensionAttribute13", "extensionAttribute14", "extensionAttribute15",
];

/// Attributes holding DNs of other objects.
const DN_ATTRS: &[&str] = &["manager", "managedby"];
/// Attributes a computer keeps up to date itself: not compared or reverted.
const COMPUTER_SELF: &[&str] = &["operatingsystem", "operatingsystemversion", "dnshostname", "serviceprincipalname"];

/// Most specific class first (a computer is also a user).
const CLASSES: &[(&str, &str)] = &[
    ("computer", "computer"),
    ("group", "group"),
    ("organizationalunit", "organizationalUnit"),
    ("foreignsecurityprincipal", "foreignSecurityPrincipal"),
    ("contact", "contact"),
    ("inetorgperson", "inetOrgPerson"),
    ("user", "user"),
];

fn backups_dir(app: &App) -> PathBuf {
    app.data_dir.join("ad-backups")
}

/// Backup ids are generated here; anything else is refused before it can
/// become part of a path.
fn check_id(id: &str) -> Result<&str> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid backup id");
    }
    Ok(id)
}

fn backup_dir(app: &App, id: &str) -> Result<PathBuf> {
    Ok(backups_dir(app).join(check_id(id)?))
}

fn new_id(app: &App, kind: &str) -> String {
    let base = format!("{}-{kind}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let mut id = base.clone();
    let mut n = 2;
    while backups_dir(app).join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

pub fn read_meta(app: &App, id: &str) -> Result<BackupMeta> {
    let p = backup_dir(app, id)?.join("meta.json");
    let text = std::fs::read_to_string(&p).map_err(|_| anyhow!("backup {id} not found"))?;
    Ok(serde_json::from_str(&text)?)
}

/// All backups, newest first.
pub fn list(app: &App) -> Vec<BackupMeta> {
    let mut v: Vec<BackupMeta> = std::fs::read_dir(backups_dir(app))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_meta(app, &e.file_name().to_string_lossy()).ok())
        .collect();
    v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    v
}

pub fn delete(app: &App, id: &str) -> Result<()> {
    read_meta(app, id)?;
    std::fs::remove_dir_all(backup_dir(app, id)?)?;
    Ok(())
}

pub fn load_objects(app: &App, id: &str) -> Result<(BackupMeta, Vec<AdObject>)> {
    let meta = read_meta(app, id)?;
    if meta.kind != "objects" {
        bail!("backup {id} is not an AD objects backup");
    }
    let bytes = std::fs::read(backup_dir(app, id)?.join("objects.json"))?;
    Ok((meta, serde_json::from_slice(&bytes).context("reading objects.json")?))
}

pub fn load_gpos(app: &App, id: &str) -> Result<(BackupMeta, Vec<GpoEntry>)> {
    let meta = read_meta(app, id)?;
    if meta.kind != "gpo" {
        bail!("backup {id} is not a Group Policy backup");
    }
    let text = std::fs::read_to_string(backup_dir(app, id)?.join("gpos.json"))?;
    Ok((meta, serde_json::from_str(&text)?))
}

fn write_backup(app: &App, mut meta: BackupMeta, files: &[(&str, &[u8])]) -> Result<BackupMeta> {
    let dir = backup_dir(app, &meta.id)?;
    let tmp = dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    meta.size = files.iter().map(|(_, b)| b.len() as u64).sum();
    for (name, bytes) in files {
        std::fs::write(tmp.join(name), bytes)?;
    }
    std::fs::write(tmp.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    std::fs::rename(&tmp, &dir)?;
    Ok(meta)
}

/// Delete the oldest automatic backups of a kind beyond `keep`.
fn prune(app: &App, kind: &str, keep: u32) {
    let autos: Vec<BackupMeta> = list(app).into_iter().filter(|m| m.kind == kind && m.auto).collect();
    for m in autos.into_iter().skip(keep.max(1) as usize) {
        if let Err(e) = delete(app, &m.id) {
            tracing::warn!("removing old AD backup {}: {e:#}", m.id);
        }
    }
}

// --------------------------------------------------------- DN / GUID / SID

/// Split a DN into its RDNs at unescaped commas.
fn rdns(dn: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut escaped) = (0, false);
    for (i, ch) in dn.char_indices() {
        match ch {
            '\\' if !escaped => escaped = true,
            ',' if !escaped => {
                out.push(dn[start..i].trim());
                start = i + 1;
            }
            _ => escaped = false,
        }
    }
    out.push(dn[start..].trim());
    out
}

/// "CN=a,OU=b,DC=x" -> "OU=b,DC=x"
fn parent_of(dn: &str) -> String {
    rdns(dn).into_iter().skip(1).collect::<Vec<_>>().join(",")
}

fn depth(dn: &str) -> usize {
    rdns(dn).len()
}

/// "CN=a,OU=b,DC=corp,DC=local" -> "DC=corp,DC=local"
pub fn root_of(dn: &str) -> String {
    let parts = rdns(dn);
    let first = parts.iter().position(|r| r.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("DC="))).unwrap_or(parts.len());
    parts[first..].join(",")
}

/// "DC=corp,DC=local" -> "corp.local"
pub fn dns_of(root: &str) -> String {
    rdns(root).iter().filter_map(|r| r.split_once('=')).map(|(_, v)| v.trim().to_lowercase()).collect::<Vec<_>>().join(".")
}

/// "corp.local" -> "DC=corp,DC=local"
pub fn root_for(domain: &str) -> String {
    domain.trim().trim_end_matches('.').split('.').map(|l| format!("DC={l}")).collect::<Vec<_>>().join(",")
}

fn ends_with_ci(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.is_char_boundary(s.len() - suffix.len()) && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// Move a DN from one domain to another: the `from` root becomes `to`.
fn rewrite_dn(dn: &str, from: &str, to: &str) -> String {
    if from.eq_ignore_ascii_case(to) || !ends_with_ci(dn, from) {
        return dn.to_string();
    }
    format!("{}{to}", &dn[..dn.len() - from.len()])
}

fn b64(s: &str) -> Option<Vec<u8>> {
    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    B64.decode(s.trim()).ok()
}

/// objectGUID bytes (little-endian fields) -> "6f9619ff-8b86-d011-b42d-00c04fc964ff" form.
fn guid_string(b: &[u8]) -> Option<String> {
    let b: &[u8; 16] = b.try_into().ok()?;
    Some(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10..].iter().map(|x| format!("{x:02x}")).collect::<String>()
    ))
}

/// objectSid bytes -> "S-1-5-21-…"
fn sid_string(b: &[u8]) -> Option<String> {
    if b.len() < 8 || b[0] != 1 {
        return None;
    }
    let count = b[1] as usize;
    if b.len() < 8 + 4 * count {
        return None;
    }
    let auth = b[2..8].iter().fold(0u64, |a, x| (a << 8) | *x as u64);
    let mut s = format!("S-1-{auth}");
    for i in 0..count {
        let o = 8 + 4 * i;
        s += &format!("-{}", u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]));
    }
    Some(s)
}

fn to_object(e: Entry) -> Option<AdObject> {
    let classes: Vec<String> = e.all("objectClass").iter().map(|c| c.to_lowercase()).collect();
    let class = CLASSES.iter().find(|(k, _)| classes.iter().any(|c| c == k)).map(|(_, v)| v.to_string())?;
    let guid = e.first("objectGUID").and_then(|v| b64(&v)).and_then(|b| guid_string(&b))?;
    let mut attrs = BTreeMap::new();
    for a in DATA_ATTRS {
        let v = e.all(a);
        if !v.is_empty() {
            attrs.insert(a.to_lowercase(), v);
        }
    }
    Some(AdObject {
        guid,
        name: e.first("name").unwrap_or_else(|| ad::cn_of(&e.dn)),
        class,
        sid: e.first("objectSid").and_then(|v| b64(&v)).and_then(|b| sid_string(&b)),
        critical: e.first("isCriticalSystemObject").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        changed: e.first("whenChanged").map(|t| gpo::generalized_time(&t)),
        attrs,
        member_of: e.all("memberOf"),
        dn: e.dn,
    })
}

/// Read every user, group, computer, contact, OU and foreign security
/// principal in the domain (read-only searches).
pub async fn capture(app: &App) -> Result<Vec<AdObject>> {
    let s: AdSettings = app.db.get_setting("ad")?;
    let attrs: Vec<&str> = META_ATTRS.iter().chain(DATA_ATTRS).copied().collect();
    let entries = ad::search(app, &s, Base::Root, OBJECT_FILTER, &attrs).await.context("reading Active Directory")?;
    let mut seen = HashSet::new();
    let mut list: Vec<AdObject> = entries.into_iter().filter_map(to_object).filter(|o| seen.insert(o.guid.clone())).collect();
    list.sort_by(|a, b| depth(&a.dn).cmp(&depth(&b.dn)).then_with(|| a.dn.to_lowercase().cmp(&b.dn.to_lowercase())));
    Ok(list)
}

/// Back up AD objects now.
pub async fn backup_objects(app: &App, auto: bool, note: &str) -> Result<BackupMeta> {
    let _busy = Busy::start(app, "AD objects")?;
    let list = capture(app).await?;
    let first = list.iter().find(|o| o.class != "foreignSecurityPrincipal").ok_or_else(|| anyhow!("AD returned no objects; check Settings → Active Directory"))?;
    let root = root_of(&first.dn);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for o in &list {
        *counts.entry(o.class.clone()).or_default() += 1;
    }
    let s: AdSettings = app.db.get_setting("ad")?;
    let meta = BackupMeta {
        id: new_id(app, "objects"),
        kind: "objects".into(),
        created_at: now(),
        domain: dns_of(&root),
        root_dn: root,
        source: if s.server.trim().is_empty() { "console".into() } else { s.server.trim().to_string() },
        auto,
        note: note.to_string(),
        counts,
        size: 0,
        warnings: vec![],
    };
    let meta = write_backup(app, meta, &[("objects.json", &serde_json::to_vec(&list)?)])?;
    tracing::info!("AD backup {}: {} objects", meta.id, list.len());
    Ok(meta)
}

/// Back up every GPO now (Backup-GPO on the configured host).
pub async fn backup_gpos(app: &App, auto: bool, note: &str) -> Result<BackupMeta> {
    let _busy = Busy::start(app, "Group Policy")?;
    let host = settings(app).gpo_host;
    let lines = if host.is_empty() {
        if !cfg!(windows) {
            bail!("choose a domain controller to run the GPO backup on (the console is not on Windows)");
        }
        ad::run_ps_timeout(GPO_BACKUP_PS, &[], 3600).await.context("Backup-GPO on the console")?.lines().map(String::from).collect()
    } else {
        let (code, lines) = run_remote(app, &host, GPO_BACKUP_PS, 3600).await?;
        if code != 0 {
            bail!("Backup-GPO on {host} failed (exit code {code}): {}", tail(&lines));
        }
        lines
    };
    let parsed = parse_gpo_backup(&lines)?;
    let mut warnings = vec![];
    // Links are read from AD by the console, like the Group Policy page does.
    let links: HashMap<String, Vec<GpoLink>> = match gpo::list(app).await {
        Ok(list) => list.into_iter().map(|g| (g.guid.to_lowercase(), g.links)).collect(),
        Err(e) => {
            warnings.push(format!("GPO links were not saved (reading AD failed: {e:#})"));
            HashMap::new()
        }
    };
    let gpos: Vec<GpoEntry> = parsed
        .gpos
        .into_iter()
        .map(|(backup_id, guid, name)| {
            let links = links.get(&guid.to_lowercase()).cloned().unwrap_or_default();
            GpoEntry { backup_id, guid, name, links }
        })
        .collect();
    let root = root_for(&parsed.domain);
    let counts = BTreeMap::from([("gpos".to_string(), gpos.len()), ("links".to_string(), gpos.iter().map(|g| g.links.len()).sum())]);
    let meta = BackupMeta {
        id: new_id(app, "gpo"),
        kind: "gpo".into(),
        created_at: now(),
        domain: parsed.domain.to_lowercase(),
        root_dn: root,
        source: if host.is_empty() { "console".into() } else { host },
        auto,
        note: note.to_string(),
        counts,
        size: 0,
        warnings,
    };
    let meta = write_backup(app, meta, &[("gpos.zip", &parsed.zip), ("gpos.json", serde_json::to_string_pretty(&gpos)?.as_bytes())])?;
    tracing::info!("GPO backup {}: {} GPOs", meta.id, gpos.len());
    Ok(meta)
}

#[derive(Debug)]
struct ParsedGpoBackup {
    domain: String,
    gpos: Vec<(String, String, String)>,
    zip: Vec<u8>,
}

fn parse_gpo_backup(lines: &[String]) -> Result<ParsedGpoBackup> {
    let (mut gpos, mut b64s, mut sha, mut domain, mut done) = (vec![], String::new(), None, String::new(), false);
    for l in lines {
        let l = l.trim_end_matches(['\r', '\n']);
        let mut f = l.split('\t');
        match f.next() {
            Some("@@gpo") => {
                let (Some(id), Some(guid)) = (f.next(), f.next()) else { continue };
                let name = f.collect::<Vec<_>>().join(" ");
                let clean = |s: &str| s.trim().trim_matches(['{', '}']).to_lowercase();
                gpos.push((clean(id), clean(guid), name.trim().to_string()));
            }
            Some("@@domain") => domain = f.next().unwrap_or("").trim().to_string(),
            Some("@@zip") => b64s.push_str(f.next().unwrap_or("").trim()),
            Some("@@sha") => sha = f.next().map(|s| s.trim().to_lowercase()),
            Some("@@done") => done = true,
            _ => {}
        }
    }
    if !done {
        bail!("the GPO backup did not finish: {}", tail(lines));
    }
    if gpos.is_empty() {
        bail!("Backup-GPO returned no GPOs");
    }
    if domain.is_empty() || !domain.contains('.') {
        bail!("the GPO backup did not report its domain");
    }
    let zip = b64(&b64s).ok_or_else(|| anyhow!("the GPO backup data is damaged (base64)"))?;
    let got: String = Sha256::digest(&zip).iter().map(|b| format!("{b:02x}")).collect();
    if sha.as_deref() != Some(got.as_str()) {
        bail!("the GPO backup data is damaged (checksum mismatch)");
    }
    Ok(ParsedGpoBackup { domain, gpos, zip })
}

fn tail(lines: &[String]) -> String {
    let said: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with("@@")).collect();
    let t = said[said.len().saturating_sub(3)..].join(" | ");
    if t.is_empty() { "no output".into() } else { t }
}

/// Run a PowerShell script on a Windows computer from the computer list and
/// return its exit code and output lines.
async fn run_remote(app: &App, computer: &str, body: &str, secs: u64) -> Result<(i32, Vec<String>)> {
    let spec = TargetSpec { computers: vec![computer.to_string()], ..Default::default() };
    let t = crate::engine::resolve_targets(app, &spec)?.into_iter().next().ok_or_else(|| anyhow!("computer {computer} not found"))?;
    let winrm = app.winrm_settings();
    let os = match t.os {
        Some(os) => os,
        None => detect_os(&t.host, winrm.use_ssl).await.ok_or_else(|| anyhow!("{computer} is not reachable"))?,
    };
    if os != Os::Windows {
        bail!("{computer} is not a Windows computer");
    }
    let port = t.port.unwrap_or(default_port(os, winrm.use_ssl));
    if !port_open(&t.host, port, Duration::from_secs(5)).await {
        bail!("{computer}: WinRM port {port} is not reachable");
    }
    let transport = crate::engine::build_transport(app, &t, os, port, &winrm)?;
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let log: LogFn = Arc::new(move |l: &str| sink.lock().unwrap().push(l.to_string()));
    let code = transport
        .run(&wrap_script(Shell::Powershell, body), Shell::Powershell, Duration::from_secs(secs), &log)
        .await
        .with_context(|| format!("running on {computer}"))?;
    let lines = std::mem::take(&mut *lines.lock().unwrap());
    Ok((code, lines))
}

/// System State backups a DC knows about (`wbadmin get versions`).
pub async fn systemstate_versions(app: &App, dc: &str, target: &str) -> Result<String> {
    let target = target.trim();
    let arg = if target.is_empty() { String::new() } else { format!(" \"-backupTarget:{}\"", target.replace('"', "")) };
    let body = format!("& wbadmin.exe get versions{arg}\nexit 0");
    let (_, lines) = run_remote(app, dc, &body, 300).await?;
    Ok(lines.join("\n"))
}

/// Called by the scheduler: start automatic backups that are due.
pub fn tick(app: &App) {
    let s = settings(app);
    let due = |hours: u32, last: &Option<String>| {
        hours > 0
            && last
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|t| chrono::Utc::now() - t.with_timezone(&chrono::Utc) >= chrono::Duration::hours(hours as i64))
    };
    let objects = due(s.objects_hours, &s.last_objects);
    let gpos = due(s.gpo_hours, &s.last_gpo);
    if (!objects && !gpos) || app.ad_backup.lock().unwrap().running.is_some() {
        return;
    }
    // Recorded first, so a failing backup is not retried every 20 seconds.
    let mut s2 = s.clone();
    if objects {
        s2.last_objects = Some(now());
    }
    if gpos {
        s2.last_gpo = Some(now());
    }
    if let Err(e) = app.db.set_setting("ad_backup", &s2) {
        tracing::error!("AD backup settings: {e:#}");
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        if objects {
            match backup_objects(&app, true, "").await {
                Ok(_) => prune(&app, "objects", s.keep),
                Err(e) => tracing::error!("automatic AD objects backup: {e:#}"),
            }
        }
        if gpos {
            match backup_gpos(&app, true, "").await {
                Ok(_) => prune(&app, "gpo", s.keep),
                Err(e) => tracing::error!("automatic GPO backup: {e:#}"),
            }
        }
    });
}

// --------------------------------------------------------------------- diff

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Member {
    pub guid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttrChange {
    pub attr: String,
    pub backup: Vec<String>,
    pub live: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffItem {
    pub guid: String,
    pub class: String,
    pub name: String,
    /// DN in the backup.
    pub dn: String,
    /// DN in AD now (None = deleted).
    pub live_dn: Option<String>,
    /// "deleted", "changed" or "new" (only in AD; never restored or deleted)
    pub status: String,
    pub moved: bool,
    pub renamed: bool,
    pub changes: Vec<AttrChange>,
    /// Group members in the backup that are missing now.
    pub add_members: Vec<Member>,
    /// Group members now that were not in the backup.
    pub remove_members: Vec<Member>,
    pub critical: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diff {
    pub backup: BackupMeta,
    pub compared_at: String,
    pub items: Vec<DiffItem>,
    pub unchanged: usize,
    pub live_count: usize,
}

/// Group GUID -> member GUIDs, from every object's memberOf (no range limits).
fn memberships(objs: &[AdObject]) -> HashMap<String, BTreeSet<String>> {
    let groups: HashMap<String, &str> =
        objs.iter().filter(|o| o.class == "group").map(|o| (o.dn.to_lowercase(), o.guid.as_str())).collect();
    let mut out: HashMap<String, BTreeSet<String>> = HashMap::new();
    for o in objs {
        for g in &o.member_of {
            if let Some(gid) = groups.get(&g.to_lowercase()) {
                out.entry(gid.to_string()).or_default().insert(o.guid.clone());
            }
        }
    }
    out
}

fn compared_attrs(o: &AdObject) -> impl Iterator<Item = (&String, &Vec<String>)> {
    o.attrs.iter().filter(move |(k, _)| !(o.class == "computer" && COMPUTER_SELF.contains(&k.as_str())))
}

fn same_values(attr: &str, a: &[String], b: &[String]) -> bool {
    let norm = |v: &[String]| -> BTreeSet<String> {
        v.iter().map(|s| if DN_ATTRS.contains(&attr) { s.to_lowercase() } else { s.clone() }).collect()
    };
    norm(a) == norm(b)
}

fn attr_changes(b: &AdObject, l: &AdObject) -> Vec<AttrChange> {
    let empty = vec![];
    let mut keys: BTreeSet<&String> = compared_attrs(b).map(|(k, _)| k).collect();
    keys.extend(compared_attrs(l).map(|(k, _)| k));
    keys.into_iter()
        .filter_map(|k| {
            let bv = b.attrs.get(k).unwrap_or(&empty);
            let lv = l.attrs.get(k).unwrap_or(&empty);
            (!same_values(k, bv, lv)).then(|| AttrChange { attr: k.clone(), backup: bv.clone(), live: lv.clone() })
        })
        .collect()
}

/// Compare a backup with AD as it is now.
pub fn compare(meta: &BackupMeta, backup: &[AdObject], live: &[AdObject]) -> Diff {
    let live_by: HashMap<&str, &AdObject> = live.iter().map(|o| (o.guid.as_str(), o)).collect();
    let backup_ids: HashSet<&str> = backup.iter().map(|o| o.guid.as_str()).collect();
    let names: HashMap<&str, &str> = backup.iter().chain(live).map(|o| (o.guid.as_str(), o.name.as_str())).collect();
    let member = |g: &String| Member { guid: g.clone(), name: names.get(g.as_str()).copied().unwrap_or(g).to_string() };
    let (bmem, lmem) = (memberships(backup), memberships(live));
    let none = BTreeSet::new();
    let mut items = vec![];
    let mut unchanged = 0;
    for b in backup {
        let Some(l) = live_by.get(b.guid.as_str()) else {
            if b.class != "foreignSecurityPrincipal" {
                items.push(DiffItem {
                    guid: b.guid.clone(),
                    class: b.class.clone(),
                    name: b.name.clone(),
                    dn: b.dn.clone(),
                    live_dn: None,
                    status: "deleted".into(),
                    moved: false,
                    renamed: false,
                    changes: vec![],
                    add_members: vec![],
                    remove_members: vec![],
                    critical: b.critical,
                });
            }
            continue;
        };
        let changes = attr_changes(b, l);
        let moved = !parent_of(&b.dn).eq_ignore_ascii_case(&parent_of(&l.dn));
        let renamed = b.name != l.name;
        let (mut add, mut remove) = (vec![], vec![]);
        if b.class == "group" {
            let (bm, lm) = (bmem.get(&b.guid).unwrap_or(&none), lmem.get(&b.guid).unwrap_or(&none));
            // Members deleted from AD come back with their own restore.
            add = bm.difference(lm).filter(|g| live_by.contains_key(g.as_str())).map(member).collect();
            remove = lm.difference(bm).map(member).collect();
        }
        if changes.is_empty() && !moved && !renamed && add.is_empty() && remove.is_empty() {
            unchanged += 1;
            continue;
        }
        items.push(DiffItem {
            guid: b.guid.clone(),
            class: b.class.clone(),
            name: b.name.clone(),
            dn: b.dn.clone(),
            live_dn: Some(l.dn.clone()),
            status: "changed".into(),
            moved,
            renamed,
            changes,
            add_members: add,
            remove_members: remove,
            critical: b.critical,
        });
    }
    for l in live.iter().filter(|l| !backup_ids.contains(l.guid.as_str()) && l.class != "foreignSecurityPrincipal") {
        items.push(DiffItem {
            guid: l.guid.clone(),
            class: l.class.clone(),
            name: l.name.clone(),
            dn: l.dn.clone(),
            live_dn: Some(l.dn.clone()),
            status: "new".into(),
            moved: false,
            renamed: false,
            changes: vec![],
            add_members: vec![],
            remove_members: vec![],
            critical: l.critical,
        });
    }
    Diff { backup: meta.clone(), compared_at: now(), items, unchanged, live_count: live.len() }
}

/// Read live AD and compare it with an objects backup.
pub async fn compare_live(app: &App, id: &str) -> Result<Diff> {
    let (meta, backup) = load_objects(app, id)?;
    let live = capture(app).await?;
    check_same_domain(&meta, &live)?;
    Ok(compare(&meta, &backup, &live))
}

fn check_same_domain(meta: &BackupMeta, live: &[AdObject]) -> Result<()> {
    let live_root = live.iter().find(|o| o.class != "foreignSecurityPrincipal").map(|o| root_of(&o.dn)).unwrap_or_default();
    if !live_root.eq_ignore_ascii_case(&meta.root_dn) {
        bail!(
            "the AD settings point at {} but the backup is of {}; to restore into another domain, use Disaster recovery (rebuild)",
            if live_root.is_empty() { "an empty directory".to_string() } else { dns_of(&live_root) },
            meta.domain
        );
    }
    Ok(())
}

// --------------------------------------------------------------------- plan

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Revert: re-create a deleted object that is not in the AD Recycle Bin any more.
    #[serde(default = "yes")]
    pub recreate_missing: bool,
    /// Revert: remove group members that were added after the backup.
    #[serde(default)]
    pub remove_added_members: bool,
    /// Restore the links of restored GPOs.
    #[serde(default = "yes")]
    pub restore_links: bool,
    /// Rebuild: overwrite the attributes of objects that already exist.
    #[serde(default = "yes")]
    pub update_existing: bool,
    /// Revert: back up the current state before the restore starts.
    #[serde(default = "yes")]
    pub backup_first: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanRequest {
    /// "revert" (same domain, from a comparison) or "rebuild" (disaster recovery, any domain)
    pub mode: String,
    /// Domain controller (from the computer list) that runs the restore.
    pub dc: String,
    /// Rebuild: DNS name of the domain the DC is in.
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub objects_backup: Option<String>,
    #[serde(default)]
    pub gpo_backup: Option<String>,
    /// Object GUIDs to restore (None = everything that differs / everything).
    #[serde(default)]
    pub objects: Option<Vec<String>>,
    /// GPO GUIDs to restore (None = all).
    #[serde(default)]
    pub gpos: Option<Vec<String>>,
    #[serde(default)]
    pub options: PlanOptions,
}

/// One line of "what will be done", shown for the first confirmation.
#[derive(Debug, Clone, Serialize)]
pub struct PlanStep {
    pub action: String,
    pub class: String,
    pub name: String,
    pub target: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestorePlan {
    pub id: String,
    pub created_at: String,
    pub expires_at: String,
    pub mode: String,
    pub dc: String,
    /// The domain written to, and the confirmation phrase.
    pub domain: String,
    pub root_dn: String,
    pub source_domain: String,
    pub objects_backup: Option<BackupMeta>,
    pub gpo_backup: Option<BackupMeta>,
    pub options: PlanOptions,
    pub steps: Vec<PlanStep>,
    pub summary: BTreeMap<String, usize>,
    pub warnings: Vec<String>,
    /// What the backup has but this plan leaves alone, and why.
    pub left_alone: Vec<String>,
    /// Accounts may be created: the final step asks for an initial password.
    pub creates_accounts: bool,
    pub reviewed_at: Option<String>,
    /// JSON Lines for the restore script: header, then operations.
    #[serde(skip)]
    pub payload: Vec<String>,
}

fn short(v: &[String]) -> String {
    let s = v.join("; ");
    if s.is_empty() {
        return "(empty)".into();
    }
    let s: String = s.chars().take(60).collect();
    if s.len() < v.join("; ").len() { format!("{s}…") } else { s }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

fn is_account(class: &str) -> bool {
    matches!(class, "user" | "inetOrgPerson" | "computer")
}

/// Groups (guid, name) of the backup an object belongs to.
fn groups_of(o: &AdObject, groups: &HashMap<String, &AdObject>) -> Vec<Member> {
    o.member_of
        .iter()
        .filter_map(|g| groups.get(&g.to_lowercase()))
        .map(|g| Member { guid: g.guid.clone(), name: g.name.clone() })
        .collect()
}

struct Builder {
    steps: Vec<PlanStep>,
    ops: Vec<Value>,
    left_alone: Vec<String>,
    creates_accounts: bool,
    recreates: bool,
    computers: bool,
    /// Shown with the warnings.
    notes: Vec<String>,
}

impl Builder {
    fn step(&mut self, action: &str, class: &str, name: &str, target: &str, detail: String) {
        self.steps.push(PlanStep { action: action.into(), class: class.into(), name: name.into(), target: target.into(), detail });
    }
}

const PASSWORD_NOTE: &str = "password: the initial password from the final step, or a random one with the account disabled";

fn plan_revert(b: &mut Builder, opts: &PlanOptions, backup: &[AdObject], diff: &Diff, only: Option<&HashSet<String>>) {
    let by_guid: HashMap<&str, &AdObject> = backup.iter().map(|o| (o.guid.as_str(), o)).collect();
    let groups: HashMap<String, &AdObject> = backup.iter().filter(|o| o.class == "group").map(|o| (o.dn.to_lowercase(), o)).collect();
    let picked = |i: &&DiffItem| i.status != "new" && only.is_none_or(|s| s.contains(&i.guid));
    let mut items: Vec<&DiffItem> = diff.items.iter().filter(picked).collect();
    items.sort_by_key(|i| depth(&i.dn));
    let new_count = diff.items.iter().filter(|i| i.status == "new").count();
    if new_count > 0 {
        b.left_alone.push(format!("{} in AD now that {} not in the backup: PSCCM never deletes anything", plural(new_count, "object"), if new_count == 1 { "is" } else { "are" }));
    }
    // 1. Deleted objects, parents first.
    for i in items.iter().filter(|i| i.status == "deleted") {
        let Some(o) = by_guid.get(i.guid.as_str()) else { continue };
        if o.critical {
            b.left_alone.push(format!("{} '{}' is a critical system object and is not restored", o.class, o.name));
            continue;
        }
        let groups = groups_of(o, &groups);
        let mut detail = "from the AD Recycle Bin (same SID, password and history)".to_string();
        if opts.recreate_missing {
            detail += "; if it is not there any more: re-create it from the backup (new SID";
            if is_account(&o.class) {
                b.creates_accounts |= o.class != "computer";
                b.computers |= o.class == "computer";
                detail += if o.class == "computer" { "; the computer must re-join the domain" } else { "; " };
                if o.class != "computer" {
                    detail += PASSWORD_NOTE;
                }
            }
            detail += ")";
            b.recreates = true;
        } else {
            detail += "; fails if it is not there any more";
        }
        if !groups.is_empty() {
            detail += &format!(", and back into {}", plural(groups.len(), "group"));
        }
        b.step("Restore deleted", &o.class, &o.name, &o.dn, detail);
        b.ops.push(json!({
            "op": "undelete", "guid": o.guid, "class": o.class, "name": o.name, "parent": parent_of(&o.dn),
            "attrs": o.attrs, "recreate": opts.recreate_missing, "groups": groups,
        }));
    }
    // 2. Moved or renamed objects.
    for i in items.iter().filter(|i| i.moved || i.renamed) {
        let what = match (i.moved, i.renamed) {
            (true, true) => "Move and rename back",
            (true, false) => "Move back",
            _ => "Rename back",
        };
        let now_dn = i.live_dn.clone().unwrap_or_default();
        b.step(what, &i.class, &i.name, &i.dn, format!("now {now_dn}"));
        b.ops.push(json!({ "op": "move", "guid": i.guid, "class": i.class, "name": i.name, "parent": parent_of(&i.dn) }));
    }
    // 3. Attributes.
    for i in items.iter().filter(|i| !i.changes.is_empty()) {
        let replace: BTreeMap<&str, &Vec<String>> =
            i.changes.iter().filter(|c| !c.backup.is_empty()).map(|c| (c.attr.as_str(), &c.backup)).collect();
        let clear: Vec<&str> = i.changes.iter().filter(|c| c.backup.is_empty()).map(|c| c.attr.as_str()).collect();
        let detail = i.changes.iter().map(|c| format!("{}: {} → {}", c.attr, short(&c.live), short(&c.backup))).collect::<Vec<_>>().join(" · ");
        b.step("Revert attributes", &i.class, &i.name, i.live_dn.as_deref().unwrap_or(&i.dn), detail);
        b.ops.push(json!({ "op": "set", "guid": i.guid, "class": i.class, "name": i.name, "replace": replace, "clear": clear }));
    }
    // 4. Group members.
    let mut kept = 0;
    for i in items.iter().filter(|i| !i.add_members.is_empty() || !i.remove_members.is_empty()) {
        let remove: &[Member] = if opts.remove_added_members { &i.remove_members } else { &[] };
        kept += i.remove_members.len() - remove.len();
        if i.add_members.is_empty() && remove.is_empty() {
            continue;
        }
        let names = |m: &[Member]| m.iter().take(8).map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ") + if m.len() > 8 { ", …" } else { "" };
        let mut parts = vec![];
        if !i.add_members.is_empty() {
            parts.push(format!("add {} ({})", plural(i.add_members.len(), "member"), names(&i.add_members)));
        }
        if !remove.is_empty() {
            parts.push(format!("remove {} ({})", plural(remove.len(), "member"), names(remove)));
        }
        b.step("Group members", &i.class, &i.name, i.live_dn.as_deref().unwrap_or(&i.dn), parts.join("; "));
        b.ops.push(json!({ "op": "members", "guid": i.guid, "name": i.name, "add": i.add_members, "remove": remove }));
    }
    if kept > 0 {
        b.left_alone.push(format!("{} added to groups after the backup stay (\"remove members added since\" is off)", plural(kept, "member")));
    }
}

fn plan_rebuild(b: &mut Builder, opts: &PlanOptions, backup: &[AdObject], only: Option<&HashSet<String>>, from: (&str, &str), to: (&str, &str)) {
    let (from_root, from_domain) = from;
    let (to_root, to_domain) = to;
    let dn = |d: &str| rewrite_dn(d, from_root, to_root);
    let mut chosen: HashSet<&str> =
        backup.iter().filter(|o| o.class != "foreignSecurityPrincipal" && only.is_none_or(|s| s.contains(&o.guid))).map(|o| o.guid.as_str()).collect();
    // The OUs above every chosen object come along, or it has nowhere to go.
    let ous: HashMap<String, &AdObject> = backup.iter().filter(|o| o.class == "organizationalUnit").map(|o| (o.dn.to_lowercase(), o)).collect();
    for o in backup.iter().filter(|o| chosen.contains(o.guid.as_str())).collect::<Vec<_>>() {
        let mut p = parent_of(&o.dn);
        while !p.is_empty() {
            if let Some(ou) = ous.get(&p.to_lowercase()) {
                chosen.insert(ou.guid.as_str());
            }
            p = parent_of(&p);
        }
    }
    let list: Vec<&AdObject> = backup.iter().filter(|o| chosen.contains(o.guid.as_str())).collect();
    let critical = list.iter().filter(|o| o.critical).count();
    if critical > 0 {
        b.left_alone.push(format!(
            "{} (Administrator, Domain Admins, domain controllers, …) are not created: the domain has its own; their group memberships are restored",
            plural(critical, "built-in object")
        ));
    }
    if !from_root.eq_ignore_ascii_case(to_root) {
        b.notes.push(format!("DNs, user logon names (@{from_domain}) and DNS host names move from {from_domain} to {to_domain}"));
    }
    let update = if opts.update_existing { "create it, or overwrite its attributes if it exists" } else { "create it if it is missing; an existing one is left unchanged" };
    for o in list.iter().filter(|o| !o.critical) {
        let mut attrs = o.attrs.clone();
        attrs.remove("gplink");
        for a in DN_ATTRS {
            attrs.remove(*a);
        }
        if o.class == "computer" {
            attrs.remove("serviceprincipalname");
            if let Some(v) = attrs.get_mut("dnshostname") {
                for h in v.iter_mut() {
                    if ends_with_ci(h, &format!(".{from_domain}")) {
                        *h = format!("{}.{to_domain}", &h[..h.len() - from_domain.len() - 1]);
                    }
                }
            }
        }
        if let Some(v) = attrs.get_mut("userprincipalname") {
            for u in v.iter_mut() {
                if ends_with_ci(u, &format!("@{from_domain}")) {
                    *u = format!("{}@{to_domain}", &u[..u.len() - from_domain.len() - 1]);
                }
            }
        }
        let mut detail = update.to_string();
        if is_account(&o.class) {
            b.creates_accounts |= o.class != "computer";
            b.computers |= o.class == "computer";
            detail += if o.class == "computer" { " (new SID; the computer must join this domain)" } else { " (new SID; " };
            if o.class != "computer" {
                detail += PASSWORD_NOTE;
                detail += ")";
            }
        }
        let target = dn(&o.dn);
        b.step("Create", &o.class, &o.name, &target, detail);
        b.ops.push(json!({ "op": "ensure", "class": o.class, "name": o.name, "dn": target, "parent": dn(&parent_of(&o.dn)), "attrs": attrs }));
    }
    // References to other objects, once they all exist.
    for o in list.iter().filter(|o| !o.critical) {
        let refs: BTreeMap<&str, Vec<String>> = DN_ATTRS
            .iter()
            .filter_map(|a| o.attrs.get(*a).map(|v| (*a, v.iter().map(|d| dn(d)).collect::<Vec<_>>())))
            .collect();
        if refs.is_empty() {
            continue;
        }
        let what = refs.keys().map(|k| if *k == "manager" { "manager" } else { "managed by" }).collect::<Vec<_>>().join(", ");
        b.step("Set references", &o.class, &o.name, &dn(&o.dn), format!("{what}: {}", refs.values().flatten().map(|d| ad::cn_of(d)).collect::<Vec<_>>().join(", ")));
        b.ops.push(json!({ "op": "refs", "dn": dn(&o.dn), "name": o.name, "replace": refs }));
    }
    // Group members by DN (other domains' members are left out).
    let by_dn: HashMap<String, &AdObject> = backup.iter().map(|o| (o.dn.to_lowercase(), o)).collect();
    let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for o in backup.iter().filter(|o| o.class != "foreignSecurityPrincipal") {
        for g in &o.member_of {
            if by_dn.get(&g.to_lowercase()).is_some_and(|g| chosen.contains(g.guid.as_str())) {
                members.entry(g.to_lowercase()).or_default().push(dn(&o.dn));
            }
        }
    }
    let fsp = backup.iter().filter(|o| o.class == "foreignSecurityPrincipal" && !o.member_of.is_empty()).count();
    if fsp > 0 {
        b.left_alone.push(format!("{} from other domains/forests in groups are not added", plural(fsp, "member")));
    }
    for (g, add) in members {
        let g = by_dn[&g];
        b.step("Group members", "group", &g.name, &dn(&g.dn), format!("add {} if they exist", plural(add.len(), "member")));
        b.ops.push(json!({ "op": "members_dn", "dn": dn(&g.dn), "name": g.name, "add": add }));
    }
}

fn plan_gpos(b: &mut Builder, opts: &PlanOptions, mode: &str, meta: &BackupMeta, gpos: &[GpoEntry], only: Option<&HashSet<String>>, to: (&str, &str)) {
    let (to_root, to_domain) = to;
    for g in gpos.iter().filter(|g| only.is_none_or(|s| s.contains(&g.guid.to_lowercase()))) {
        let links: Vec<GpoLink> = if opts.restore_links {
            g.links.iter().map(|l| GpoLink { target: rewrite_dn(&l.target, &meta.root_dn, to_root), ..l.clone() }).collect()
        } else {
            vec![]
        };
        let mut detail = if mode == "revert" {
            format!("Restore-GPO: settings and permissions as of {} UTC; a GPO deleted since is re-created (new GUID)", meta.created_at.get(..16).unwrap_or(&meta.created_at).replace('T', " "))
        } else {
            format!("Import-GPO into {to_domain}: settings only (permissions, WMI filters and delegation are not imported)")
        };
        if !links.is_empty() {
            detail += &format!("; link to {}", links.iter().map(|l| target_label(&l.target)).collect::<Vec<_>>().join(", "));
        }
        b.step(if mode == "revert" { "Restore GPO" } else { "Import GPO" }, "gpo", &g.name, &format!("{{{}}}", g.guid), detail);
        b.ops.push(json!({ "op": "gpo", "backup_id": g.backup_id, "guid": g.guid, "name": g.name, "links": links }));
    }
    if !opts.restore_links && gpos.iter().any(|g| !g.links.is_empty()) {
        b.left_alone.push("GPO links are not restored (option off)".into());
    }
}

/// "OU=Sales,DC=x" -> "Sales"; the domain root -> its DNS name.
fn target_label(dn: &str) -> String {
    if root_of(dn).eq_ignore_ascii_case(dn.trim()) { dns_of(dn) } else { ad::cn_of(dn) }
}

fn valid_domain(d: &str) -> bool {
    d.contains('.')
        && d.len() <= 253
        && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !l.starts_with('-'))
}

/// Build a restore plan and keep it for the two confirmations. Nothing is
/// changed anywhere.
pub async fn plan(app: &App, req: PlanRequest) -> Result<RestorePlan> {
    let mode = req.mode.as_str();
    if !matches!(mode, "revert" | "rebuild") {
        bail!("mode must be revert or rebuild");
    }
    let dc = app
        .db
        .list_computers()?
        .into_iter()
        .find(|c| c.name.eq_ignore_ascii_case(req.dc.trim()))
        .ok_or_else(|| anyhow!("choose the domain controller to run the restore on (from the computer list)"))?;
    if dc.os == "linux" {
        bail!("{} is a Linux computer; choose a domain controller", dc.name);
    }
    let objects = req.objects_backup.as_deref().filter(|s| !s.is_empty()).map(|id| load_objects(app, id)).transpose()?;
    let gpos = req.gpo_backup.as_deref().filter(|s| !s.is_empty()).map(|id| load_gpos(app, id)).transpose()?;
    let source = objects.as_ref().map(|(m, _)| m).or(gpos.as_ref().map(|(m, _)| m)).ok_or_else(|| anyhow!("choose a backup to restore"))?.clone();
    if let (Some((a, _)), Some((b, _))) = (&objects, &gpos)
        && !a.domain.eq_ignore_ascii_case(&b.domain)
    {
        bail!("the objects backup is of {} but the GPO backup is of {}", a.domain, b.domain);
    }
    let domain = if mode == "revert" { source.domain.clone() } else { req.domain.trim().trim_end_matches('.').to_lowercase() };
    if !valid_domain(&domain) {
        bail!("enter the DNS name of the domain the DC is in, e.g. corp.local");
    }
    let root = if mode == "revert" { source.root_dn.clone() } else { root_for(&domain) };
    let only_objects = req.objects.map(|v| v.into_iter().map(|g| g.to_lowercase()).collect::<HashSet<_>>());
    let only_gpos = req.gpos.map(|v| v.into_iter().map(|g| g.trim_matches(['{', '}']).to_lowercase()).collect::<HashSet<_>>());
    let opts = req.options;
    let mut b = Builder { steps: vec![], ops: vec![], left_alone: vec![], creates_accounts: false, recreates: false, computers: false, notes: vec![] };

    if let Some((meta, list)) = &objects {
        if mode == "revert" {
            let live = capture(app).await?;
            check_same_domain(meta, &live)?;
            let diff = compare(meta, list, &live);
            plan_revert(&mut b, &opts, list, &diff, only_objects.as_ref());
        } else {
            plan_rebuild(&mut b, &opts, list, only_objects.as_ref(), (&meta.root_dn, &meta.domain), (&root, &domain));
        }
    }
    if let Some((meta, list)) = &gpos {
        plan_gpos(&mut b, &opts, mode, meta, list, only_gpos.as_ref(), (&root, &domain));
    }
    if b.ops.is_empty() {
        bail!("nothing to restore: {}", if mode == "revert" { "the chosen items match AD as it is now" } else { "nothing was chosen" });
    }

    let mut warnings = vec![format!("This writes to the Active Directory domain {domain} through {} and cannot be undone automatically.", dc.name)];
    warnings.append(&mut b.notes);
    if mode == "revert" && opts.backup_first && objects.is_some() {
        warnings.push("Before the restore starts, PSCCM backs up AD as it is now, so you can go back.".into());
    }
    if b.recreates || mode == "rebuild" {
        warnings.push("Re-created users and groups get a new SID: permissions on files, shares and other systems that referred to the old ones do not apply to them.".into());
    }
    if b.creates_accounts {
        warnings.push("Passwords are never in a backup. Accounts PSCCM creates get the initial password you enter in the final step (they must change it at next logon); without one they get a random password and stay disabled.".into());
    }
    if b.computers {
        warnings.push("Re-created computer accounts don't match the computers' own passwords: those computers must (re-)join the domain.".into());
    }
    if mode == "rebuild" {
        warnings.push("Not in a backup: trusts, DNS zones, sites, fine-grained password policies, WMI filters and GPO permissions.".into());
    }
    if b.steps.iter().any(|s| s.action.starts_with("Move")) {
        warnings.push("Objects protected from accidental deletion cannot be moved; such moves fail and are listed in the job log.".into());
    }
    let mut summary: BTreeMap<String, usize> = BTreeMap::new();
    for s in &b.steps {
        *summary.entry(s.action.clone()).or_default() += 1;
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let header = json!({
        "plan_id": id, "mode": mode, "expect_domain": domain, "target_root": root,
        "options": { "update_existing": opts.update_existing, "restore_links": opts.restore_links },
    });
    let mut payload = vec![header.to_string()];
    payload.extend(b.ops.iter().map(|o| o.to_string()));
    let created = chrono::Utc::now();
    let plan = RestorePlan {
        id: id.clone(),
        created_at: created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        expires_at: (created + chrono::Duration::minutes(PLAN_MINUTES)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        mode: mode.into(),
        dc: dc.name,
        domain,
        root_dn: root,
        source_domain: source.domain.clone(),
        objects_backup: objects.map(|(m, _)| m),
        gpo_backup: gpos.map(|(m, _)| m),
        options: opts,
        steps: b.steps,
        summary,
        warnings,
        left_alone: b.left_alone,
        creates_accounts: b.creates_accounts,
        reviewed_at: None,
        payload,
    };
    let mut st = app.ad_backup.lock().unwrap();
    purge_expired(&mut st);
    st.plans.insert(id, plan.clone());
    Ok(plan)
}

fn purge_expired(st: &mut State) {
    let now = chrono::Utc::now();
    st.plans.retain(|_, p| chrono::DateTime::parse_from_rfc3339(&p.expires_at).is_ok_and(|t| t > now));
}

pub fn get_plan(app: &App, id: &str) -> Result<RestorePlan> {
    let mut st = app.ad_backup.lock().unwrap();
    purge_expired(&mut st);
    st.plans.get(id).cloned().ok_or_else(|| anyhow!("restore plan not found or expired; build it again"))
}

pub fn cancel_plan(app: &App, id: &str) {
    app.ad_backup.lock().unwrap().plans.remove(id);
}

/// Confirmation 1 of 2: the user reviewed every item.
pub fn review(app: &App, id: &str) -> Result<RestorePlan> {
    let mut st = app.ad_backup.lock().unwrap();
    purge_expired(&mut st);
    let p = st.plans.get_mut(id).ok_or_else(|| anyhow!("restore plan not found or expired; build it again"))?;
    p.reviewed_at = Some(now());
    Ok(p.clone())
}

/// Confirmation 2 of 2: the domain name typed by the user. Creates the
/// restore job (the caller starts it) and returns its deployment id. A plan
/// runs at most once.
pub async fn execute(app: &App, id: &str, confirm: &str, initial_password: &str) -> Result<(String, PathBuf)> {
    let plan = {
        let mut st = app.ad_backup.lock().unwrap();
        purge_expired(&mut st);
        let p = st.plans.get(id).ok_or_else(|| anyhow!("restore plan not found or expired; build it again"))?;
        if p.reviewed_at.is_none() {
            bail!("confirm the plan first (step 1 of 2: review what will be restored)");
        }
        if !confirm.trim().eq_ignore_ascii_case(&p.domain) {
            bail!("type the domain name {} to confirm (step 2 of 2)", p.domain);
        }
        st.plans.remove(id).unwrap()
    };
    if plan.mode == "revert"
        && plan.options.backup_first
        && plan.objects_backup.is_some()
        && let Err(e) = backup_objects(app, false, &format!("before restore {}", &plan.id[..8])).await
    {
        // Nothing changed: the plan can still be confirmed again.
        app.ad_backup.lock().unwrap().plans.insert(plan.id.clone(), plan);
        return Err(e.context("backing up the current state before the restore failed; nothing was changed"));
    }
    let rel = format!("ad-restore/{}", plan.id);
    let dir = app.repo_dir().join(&rel);
    clean_staged(app);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("plan.jsonl"), plan.payload.join("\n"))?;
    if let Some(g) = &plan.gpo_backup {
        std::fs::copy(backup_dir(app, &g.id)?.join("gpos.zip"), dir.join("gpos.zip"))?;
    }
    let pkg = restore_package(&plan, &rel)?;
    let vars = BTreeMap::from([("initial_password".to_string(), initial_password.to_string())]);
    let targets = TargetSpec { computers: vec![plan.dc.clone()], ..Default::default() };
    let options = DeployOptions { concurrency: 1, use_detect: false, ..Default::default() };
    let dep = crate::engine::create_deployment(app, &pkg, vars, targets, options, RESTORE_SOURCE, None)?;
    tracing::warn!("AD restore {} confirmed: {} on {} ({} operations)", plan.id, plan.domain, plan.dc, plan.payload.len() - 1);
    Ok((dep, dir))
}

/// Run a confirmed restore job in the background; its staged files (the plan
/// and GPO backup copies) are removed when it ends.
pub fn start(app: &App, dep: String, staged: PathBuf) {
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::engine::run_deployment(&app, &dep).await {
            tracing::error!("AD restore {dep} failed: {e:#}");
            let _ = app.db.set_deployment_status(&dep, "failed");
        }
        let _ = std::fs::remove_dir_all(&staged);
    });
}

/// Staged restore files older than a week.
fn clean_staged(app: &App) {
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(7 * 86400);
    for e in std::fs::read_dir(app.repo_dir().join("ad-restore")).into_iter().flatten().flatten() {
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn restore_package(plan: &RestorePlan, rel: &str) -> Result<Package> {
    let mut steps = vec![json!({ "name": "Upload the restore plan", "copy": { "src": format!("{rel}/plan.jsonl"), "dest": "{{workdir}}\\plan.jsonl" } })];
    let zip = if plan.gpo_backup.is_some() {
        steps.push(json!({ "name": "Upload the GPO backup", "copy": { "src": format!("{rel}/gpos.zip"), "dest": "{{workdir}}\\gpos.zip" } }));
        "Join-Path {{workdir|ps}} 'gpos.zip'"
    } else {
        "''"
    };
    let script = format!(
        "$planFile = Join-Path {{{{workdir|ps}}}} 'plan.jsonl'\n$zipFile = {zip}\n$initialPassword = {{{{initial_password|ps}}}}\n{RESTORE_PS}"
    );
    steps.push(json!({ "name": format!("Restore ({} operations)", plan.payload.len() - 1), "timeout": 14400, "script": script }));
    let pkg: Package = serde_json::from_value(json!({
        "id": "ad-restore",
        "name": format!("AD restore ({}) into {}", plan.mode, plan.domain),
        "category": "Active Directory",
        "description": format!("Restore plan {} confirmed {}: {}", plan.id, now(), plan.summary.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ")),
        "platforms": ["windows"],
        "variables": { "initial_password": { "default": "", "secret": true, "description": "Initial password of accounts the restore creates" } },
        "steps": steps,
    }))?;
    pkg.validate()?;
    Ok(pkg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(guid: &str, dn: &str, class: &str, attrs: &[(&str, &[&str])], member_of: &[&str]) -> AdObject {
        AdObject {
            guid: guid.into(),
            dn: dn.into(),
            name: ad::cn_of(dn),
            class: class.into(),
            sid: None,
            critical: false,
            changed: None,
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect(),
            member_of: member_of.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn meta() -> BackupMeta {
        BackupMeta {
            id: "20261104-120000-objects".into(),
            kind: "objects".into(),
            created_at: "2026-11-04T12:00:00Z".into(),
            domain: "corp.local".into(),
            root_dn: "DC=corp,DC=local".into(),
            source: "console".into(),
            auto: false,
            note: String::new(),
            counts: BTreeMap::new(),
            size: 0,
            warnings: vec![],
        }
    }

    #[test]
    fn dn_helpers() {
        assert_eq!(parent_of(r"CN=Smith\, John,OU=Users,DC=corp,DC=local"), "OU=Users,DC=corp,DC=local");
        assert_eq!(root_of("CN=a,OU=b,DC=corp,DC=local"), "DC=corp,DC=local");
        assert_eq!(dns_of("DC=Corp,DC=local"), "corp.local");
        assert_eq!(root_for("new.example.com"), "DC=new,DC=example,DC=com");
        assert_eq!(rewrite_dn("CN=a,OU=b,dc=corp,dc=local", "DC=corp,DC=local", "DC=new,DC=lan"), "CN=a,OU=b,DC=new,DC=lan");
        assert_eq!(rewrite_dn("CN=x,DC=other", "DC=corp,DC=local", "DC=new,DC=lan"), "CN=x,DC=other");
        assert!(valid_domain("corp.local") && !valid_domain("corp") && !valid_domain("a..b") && !valid_domain("x.-y"));
        assert!(check_id("20261104-120000-objects").is_ok() && check_id("../x").is_err() && check_id("").is_err());
    }

    #[test]
    fn guid_and_sid() {
        let g = [0xff, 0x19, 0x96, 0x6f, 0x86, 0x8b, 0x11, 0xd0, 0xb4, 0x2d, 0x00, 0xc0, 0x4f, 0xc9, 0x64, 0xff];
        assert_eq!(guid_string(&g).unwrap(), "6f9619ff-8b86-d011-b42d-00c04fc964ff");
        assert!(guid_string(&g[..15]).is_none());
        // S-1-5-21-1-2-3-500
        let mut s = vec![1u8, 5, 0, 0, 0, 0, 0, 5];
        for n in [21u32, 1, 2, 3, 500] {
            s.extend(n.to_le_bytes());
        }
        assert_eq!(sid_string(&s).unwrap(), "S-1-5-21-1-2-3-500");
        assert!(sid_string(&s[..10]).is_none());
    }

    #[test]
    fn entry_to_object() {
        use base64::{Engine, engine::general_purpose::STANDARD as B64};
        let mut attrs = HashMap::new();
        let mut put = |k: &str, v: &[&str]| attrs.insert(k.to_string(), v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        put("objectclass", &["top", "person", "organizationalPerson", "user", "computer"]);
        put("objectguid", &[&B64.encode([1u8; 16])]);
        put("name", &["PC01"]);
        put("iscriticalsystemobject", &["True"]);
        put("description", &["front desk"]);
        put("memberof", &["CN=G1,DC=corp,DC=local"]);
        let o = to_object(Entry { dn: "CN=PC01,OU=PCs,DC=corp,DC=local".into(), attrs }).unwrap();
        assert_eq!((o.class.as_str(), o.critical, o.name.as_str()), ("computer", true, "PC01"));
        assert_eq!(o.guid, "01010101-0101-0101-0101-010101010101");
        assert_eq!(o.attrs["description"], ["front desk"]);
        assert_eq!(o.member_of, ["CN=G1,DC=corp,DC=local"]);
        assert!(to_object(Entry { dn: "CN=x".into(), ..Default::default() }).is_none());
    }

    fn sample() -> (Vec<AdObject>, Vec<AdObject>) {
        let backup = vec![
            obj("ou1", "OU=Staff,DC=corp,DC=local", "organizationalUnit", &[("description", &["People"])], &[]),
            obj("g1", "CN=Sales,OU=Staff,DC=corp,DC=local", "group", &[("samaccountname", &["Sales"]), ("grouptype", &["-2147483646"])], &[]),
            obj("u1", "CN=Ann,OU=Staff,DC=corp,DC=local", "user", &[("title", &["Boss"]), ("manager", &["CN=Bob,OU=Staff,DC=corp,DC=local"])], &["CN=Sales,OU=Staff,DC=corp,DC=local"]),
            obj("u2", "CN=Bob,OU=Staff,DC=corp,DC=local", "user", &[("userprincipalname", &["bob@corp.local"])], &["CN=Sales,OU=Staff,DC=corp,DC=local"]),
            obj("u3", "CN=Cid,OU=Staff,DC=corp,DC=local", "user", &[], &["CN=Sales,OU=Staff,DC=corp,DC=local"]),
            obj("c1", "CN=PC1,OU=Staff,DC=corp,DC=local", "computer", &[("operatingsystemversion", &["10.0 (19045)"])], &[]),
        ];
        let live = vec![
            obj("ou1", "OU=Staff,DC=corp,DC=local", "organizationalUnit", &[("description", &["People"])], &[]),
            obj("g1", "CN=Sales,OU=Staff,DC=corp,DC=local", "group", &[("samaccountname", &["Sales"]), ("grouptype", &["-2147483646"])], &[]),
            // Ann: title changed, moved out of Staff, manager DN case changed only.
            obj("u1", "CN=Ann,DC=corp,DC=local", "user", &[("title", &["Clerk"]), ("department", &["X"]), ("manager", &["cn=bob,ou=staff,dc=corp,dc=local"])], &["CN=Sales,OU=Staff,DC=corp,DC=local"]),
            // Bob removed from Sales.
            obj("u2", "CN=Bob,OU=Staff,DC=corp,DC=local", "user", &[("userprincipalname", &["bob@corp.local"])], &[]),
            // Cid deleted; Dan new and in Sales.
            obj("u4", "CN=Dan,OU=Staff,DC=corp,DC=local", "user", &[], &["CN=Sales,OU=Staff,DC=corp,DC=local"]),
            // A computer updating its own OS version is not a change.
            obj("c1", "CN=PC1,OU=Staff,DC=corp,DC=local", "computer", &[("operatingsystemversion", &["10.0 (22631)"])], &[]),
        ];
        (backup, live)
    }

    #[test]
    fn compares_with_live() {
        let (backup, live) = sample();
        let d = compare(&meta(), &backup, &live);
        let get = |g: &str| d.items.iter().find(|i| i.guid == g).unwrap();
        assert_eq!(d.unchanged, 3, "{:#?}", d.items); // ou1, u2, c1
        let ann = get("u1");
        assert!(ann.moved && !ann.renamed);
        let attrs: Vec<&str> = ann.changes.iter().map(|c| c.attr.as_str()).collect();
        assert_eq!(attrs, ["department", "title"]);
        assert_eq!(get("u3").status, "deleted");
        assert_eq!(get("u4").status, "new");
        let sales = get("g1");
        // Bob is missing; Cid is deleted (comes back with its own restore); Dan was added.
        assert_eq!(sales.add_members, [Member { guid: "u2".into(), name: "Bob".into() }]);
        assert_eq!(sales.remove_members, [Member { guid: "u4".into(), name: "Dan".into() }]);
    }

    fn builder() -> Builder {
        Builder { steps: vec![], ops: vec![], left_alone: vec![], creates_accounts: false, recreates: false, computers: false, notes: vec![] }
    }

    #[test]
    fn revert_plan() {
        let (backup, live) = sample();
        let d = compare(&meta(), &backup, &live);
        let mut b = builder();
        plan_revert(&mut b, &PlanOptions::default(), &backup, &d, None);
        let ops: Vec<&str> = b.ops.iter().map(|o| o["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["undelete", "move", "set", "members"]);
        let undelete = &b.ops[0];
        assert_eq!(undelete["guid"], "u3");
        assert_eq!(undelete["parent"], "OU=Staff,DC=corp,DC=local");
        assert_eq!(undelete["groups"][0]["guid"], "g1");
        assert_eq!(b.ops[2]["replace"]["title"][0], "Boss");
        assert_eq!(b.ops[2]["clear"][0], "department");
        // Removing Dan is off by default.
        assert_eq!(b.ops[3]["remove"].as_array().unwrap().len(), 0);
        assert!(b.left_alone.iter().any(|l| l.contains("1 member added")), "{:?}", b.left_alone);
        assert!(b.left_alone.iter().any(|l| l.contains("never deletes")));
        assert!(b.creates_accounts);

        // Only Ann, with member removal on.
        let mut b = builder();
        let only = HashSet::from(["u1".to_string(), "g1".to_string()]);
        let opts = PlanOptions { remove_added_members: true, ..Default::default() };
        plan_revert(&mut b, &opts, &backup, &d, Some(&only));
        let ops: Vec<&str> = b.ops.iter().map(|o| o["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["move", "set", "members"]);
        assert_eq!(b.ops[2]["remove"][0]["guid"], "u4");
    }

    #[test]
    fn rebuild_plan_into_new_domain() {
        let (backup, _) = sample();
        let mut b = builder();
        let only = HashSet::from(["u2".to_string()]);
        plan_rebuild(&mut b, &PlanOptions::default(), &backup, Some(&only), ("DC=corp,DC=local", "corp.local"), ("DC=new,DC=lan", "new.lan"));
        // Bob plus the OU above him.
        let ensure: Vec<(&str, &str)> =
            b.ops.iter().filter(|o| o["op"] == "ensure").map(|o| (o["dn"].as_str().unwrap(), o["parent"].as_str().unwrap())).collect();
        assert_eq!(ensure, [("OU=Staff,DC=new,DC=lan", "DC=new,DC=lan"), ("CN=Bob,OU=Staff,DC=new,DC=lan", "OU=Staff,DC=new,DC=lan")]);
        let bob = b.ops.iter().find(|o| o["name"] == "Bob").unwrap();
        assert_eq!(bob["attrs"]["userprincipalname"][0], "bob@new.lan");

        let mut b = builder();
        plan_rebuild(&mut b, &PlanOptions::default(), &backup, None, ("DC=corp,DC=local", "corp.local"), ("DC=new,DC=lan", "new.lan"));
        let refs = b.ops.iter().find(|o| o["op"] == "refs").unwrap();
        assert_eq!(refs["replace"]["manager"][0], "CN=Bob,OU=Staff,DC=new,DC=lan");
        let ann = b.ops.iter().find(|o| o["op"] == "ensure" && o["name"] == "Ann").unwrap();
        assert!(ann["attrs"].get("manager").is_none(), "manager is set after every object exists");
        let members = b.ops.iter().find(|o| o["op"] == "members_dn").unwrap();
        assert_eq!(members["add"].as_array().unwrap().len(), 3);
        assert!(b.creates_accounts && b.computers);
    }

    #[test]
    fn gpo_backup_output() {
        use base64::{Engine, engine::general_purpose::STANDARD as B64};
        let zip = b"PK\x03\x04 not really a zip".to_vec();
        let enc = B64.encode(&zip);
        let lines: Vec<String> = [
            "Backing up".to_string(),
            "@@domain\tcorp.local".into(),
            "@@gpo\t{AAAA-1}\t{31B2F340-016D-11D2-945F-00C04FB984F9}\tDefault Domain Policy".into(),
            format!("@@zip\t{}", &enc[..8]),
            format!("@@zip\t{}", &enc[8..]),
            format!("@@sha\t{}", Sha256::digest(&zip).iter().map(|b| format!("{b:02X}")).collect::<String>()),
            "@@done".into(),
        ]
        .into();
        let p = parse_gpo_backup(&lines).unwrap();
        assert_eq!(p.domain, "corp.local");
        assert_eq!(p.gpos, [("aaaa-1".into(), "31b2f340-016d-11d2-945f-00c04fb984f9".into(), "Default Domain Policy".into())]);
        assert_eq!(p.zip, zip);
        let mut bad = lines.clone();
        bad[4] = "@@zip\tAAAA".into();
        assert!(parse_gpo_backup(&bad).unwrap_err().to_string().contains("damaged"));
        assert!(parse_gpo_backup(&lines[..6]).is_err(), "no done marker");
    }

    #[test]
    fn restore_package_is_valid() {
        let plan = RestorePlan {
            id: "0123456789abcdef".into(),
            created_at: now(),
            expires_at: now(),
            mode: "rebuild".into(),
            dc: "dc01".into(),
            domain: "new.lan".into(),
            root_dn: "DC=new,DC=lan".into(),
            source_domain: "corp.local".into(),
            objects_backup: Some(meta()),
            gpo_backup: Some(BackupMeta { kind: "gpo".into(), ..meta() }),
            options: PlanOptions::default(),
            steps: vec![],
            summary: BTreeMap::from([("Create".to_string(), 2)]),
            warnings: vec![],
            left_alone: vec![],
            creates_accounts: true,
            reviewed_at: None,
            payload: vec!["{}".into(), "{}".into()],
        };
        let pkg = restore_package(&plan, "ad-restore/0123456789abcdef").unwrap();
        assert_eq!(pkg.steps.len(), 3);
        assert!(pkg.variables["initial_password"].secret);
        // The script renders: no stray {{ }} in the embedded PowerShell.
        let vars = BTreeMap::from([("workdir".to_string(), r"C:\Windows\Temp\psccm-x".to_string()), ("initial_password".to_string(), "it's".to_string())]);
        let script = crate::package::render(pkg.steps[2].script.as_deref().unwrap(), &vars).unwrap();
        assert!(script.starts_with("$planFile = Join-Path 'C:\\Windows\\Temp\\psccm-x' 'plan.jsonl'"));
        assert!(script.contains("$initialPassword = 'it''s'"));
    }

    /// A stand-in for the AD and GroupPolicy cmdlets (functions win over
    /// cmdlets), so the real restore script runs against an in-memory domain.
    #[cfg(windows)]
    const FAKE_AD: &str = r#"
$global:AD = [ordered]@{}
$global:N = 0
$global:Gpos = @{}
function Import-Module { param([Parameter(ValueFromRemainingArguments = $true)]$rest) }
function Get-ADDomain { param($Server) [pscustomobject]@{ DNSRoot = $fakeDomain; DistinguishedName = $fakeRoot } }
function Get-ADDomainController { param($Server, [switch]$Discover) [pscustomobject]@{ HostName = 'dc01.' + $fakeDomain } }
function Get-ADOptionalFeature { param($Filter, $Server) [pscustomobject]@{ EnabledScopes = @('on') } }
function Find-Fake($id) {
    $id = [string]$id
    foreach ($o in $global:AD.Values) { if ($o.dn -eq $id -or $o.guid -eq $id) { return $o } }
    return $null
}
function Add-Fake($dn, $class, $guid = $null, [switch]$deleted) {
    $global:N++
    if (-not $guid) { $guid = 'new-{0:d4}' -f $global:N }
    $name = ($dn -replace '^[^=]+=', '') -replace '(?<!\\),.*$', ''
    $o = @{ dn = $dn; guid = $guid; name = $name; class = $class; attrs = @{}; deleted = [bool]$deleted }
    $global:AD[$guid] = $o
    return $o
}
function MemberOf-Fake($dn) { @($global:AD.Values | Where-Object { @($_.attrs['member']) -contains $dn } | ForEach-Object { $_.dn }) }
function Get-ADObject { param($Identity, [switch]$IncludeDeletedObjects, $Properties, $Server)
    $o = Find-Fake $Identity
    if (-not $o -or ($o.deleted -and -not $IncludeDeletedObjects)) { throw "Cannot find an object with identity: '$Identity'" }
    [pscustomobject]@{ DistinguishedName = $o.dn; ObjectGUID = $o.guid; Name = $o.name; isDeleted = $o.deleted; memberOf = (MemberOf-Fake $o.dn) }
}
function New-ADObject { param($Name, $Type, $Path, $OtherAttributes, [switch]$PassThru, $Server)
    if (-not (Find-Fake $Path)) { throw "no parent $Path" }
    $dn = $(if ($Type -eq 'organizationalUnit') { 'OU=' } else { 'CN=' }) + $Name + ',' + $Path
    if (Find-Fake $dn) { throw "already exists: $dn" }
    $o = Add-Fake $dn $Type
    if ($OtherAttributes) { foreach ($k in $OtherAttributes.Keys) { $o.attrs[$k.ToLower()] = $OtherAttributes[$k] } }
    $o.attrs['useraccountcontrol'] = 546
    [pscustomobject]@{ DistinguishedName = $o.dn; ObjectGUID = $o.guid }
}
function Set-ADObject { param($Identity, $Replace, $Add, $Remove, $Clear, $Server)
    $o = Find-Fake $Identity
    if (-not $o) { throw "Cannot find $Identity" }
    if ($Replace) { foreach ($k in $Replace.Keys) { $o.attrs[$k.ToLower()] = $Replace[$k] } }
    if ($Add) { foreach ($k in $Add.Keys) { $o.attrs[$k.ToLower()] = @(@($o.attrs[$k.ToLower()]) + @($Add[$k]) | Where-Object { $_ }) } }
    if ($Remove) { foreach ($k in $Remove.Keys) { $o.attrs[$k.ToLower()] = @(@($o.attrs[$k.ToLower()]) | Where-Object { $_ -notin @($Remove[$k]) }) } }
    foreach ($k in @($Clear)) { if ($k) { $o.attrs.Remove($k.ToLower()) } }
}
function Set-ADAccountPassword { param($Identity, [switch]$Reset, $NewPassword, $Server) (Find-Fake $Identity).attrs['pw'] = 'set' }
function Restore-ADObject { param($Identity, $NewName, $TargetPath, $Server)
    $o = Find-Fake $Identity; $o.deleted = $false; $o.dn = 'CN=' + $NewName + ',' + $TargetPath; $o.name = $NewName
}
function Move-ADObject { param($Identity, $TargetPath, $Server) $o = Find-Fake $Identity; $o.dn = ($o.dn -replace '(?<!\\),.*$', '') + ',' + $TargetPath }
function Rename-ADObject { param($Identity, $NewName, $Server) $o = Find-Fake $Identity; $o.dn = ($o.dn -replace '^([^=]+=)(?:\\.|[^,\\])*', ('$1' + $NewName)); $o.name = $NewName }
function Get-GPO { param($Guid, $Server) if (-not $global:Gpos.ContainsKey([string]$Guid)) { throw 'GPO not found' } }
function Restore-GPO { param($BackupId, $Path, $Server, [switch]$Confirm) Write-Output "restored $BackupId" | Out-Null; [pscustomobject]@{ Id = 'gpo-restored' } }
function Import-GPO { param($BackupId, $Path, $TargetName, [switch]$CreateIfNeeded, $Server) [pscustomobject]@{ Id = 'gpo-' + $TargetName } }
function Get-GPInheritance { param($Target, $Server) [pscustomobject]@{ GpoLinks = @() } }
function New-GPLink { param($Guid, $Target, $LinkEnabled, $Enforced, $Server) $global:Links += , "$Guid>$Target>$LinkEnabled>$Enforced" }
$global:Links = @()
"#;

    /// Write the plan, run fake AD setup + the real restore script, then `check`.
    #[cfg(windows)]
    fn run_restore(payload: &[String], domain: &str, setup: &str, check: &str) -> String {
        let dir = std::env::temp_dir().join(format!("psccm-adrestore-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("gpo")).unwrap();
        std::fs::write(dir.join("plan.jsonl"), payload.join("\n")).unwrap();
        std::fs::write(dir.join("gpo").join("bkupInfo.xml"), "x").unwrap();
        let q = |p: &std::path::Path| crate::transport::ps_quote(&p.to_string_lossy());
        // The restore script ends with exit; run it as a child script so the checks still run.
        std::fs::write(dir.join("restore.ps1"), [b"\xEF\xBB\xBF".as_slice(), RESTORE_PS.as_bytes()].concat()).unwrap();
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n$fakeDomain = '{domain}'\n$fakeRoot = '{root}'\n{FAKE_AD}\n{setup}\n\
             Compress-Archive -Path (Join-Path {gpo} '*') -DestinationPath {zip}\n\
             $planFile = {plan}\n$zipFile = {zip}\n$initialPassword = ''\n\
             try {{ & {restore} }} catch {{ Write-Output \"THROWN: $_\" }}\n{check}\n",
            root = root_for(domain),
            gpo = q(&dir.join("gpo")),
            zip = q(&dir.join("gpos.zip")),
            plan = q(&dir.join("plan.jsonl")),
            restore = q(&dir.join("restore.ps1")),
        );
        std::fs::write(dir.join("run.ps1"), [b"\xEF\xBB\xBF".as_slice(), script.as_bytes()].concat()).unwrap();
        let out = std::process::Command::new("powershell.exe")
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(dir.join("run.ps1"))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    fn payload(b: &Builder, mode: &str, domain: &str, opts: &PlanOptions) -> Vec<String> {
        let header = json!({
            "plan_id": "test", "mode": mode, "expect_domain": domain, "target_root": root_for(domain),
            "options": { "update_existing": opts.update_existing, "restore_links": opts.restore_links },
        });
        std::iter::once(header.to_string()).chain(b.ops.iter().map(|o| o.to_string())).collect()
    }

    #[cfg(windows)]
    #[test]
    fn restore_script_rebuilds_into_new_domain() {
        let (backup, _) = sample();
        let opts = PlanOptions::default();
        let mut b = builder();
        plan_rebuild(&mut b, &opts, &backup, None, ("DC=corp,DC=local", "corp.local"), ("DC=new,DC=lan", "new.lan"));
        let gpos = vec![GpoEntry {
            backup_id: "bk1".into(),
            guid: "g-ddp".into(),
            name: "Desktop".into(),
            links: vec![GpoLink { target: "OU=Staff,DC=corp,DC=local".into(), enabled: true, enforced: true }],
        }];
        plan_gpos(&mut b, &opts, "rebuild", &meta(), &gpos, None, ("DC=new,DC=lan", "new.lan"));
        let setup = "[void](Add-Fake 'DC=new,DC=lan' 'domainDNS')";
        let check = r#"
$ann = Find-Fake 'CN=Ann,OU=Staff,DC=new,DC=lan'
$sales = Find-Fake 'CN=Sales,OU=Staff,DC=new,DC=lan'
"CHECK manager=$($ann.attrs['manager']) uac=$($ann.attrs['useraccountcontrol']) pw=$($ann.attrs['pw']) title=$($ann.attrs['title'])"
"CHECK members=$(@($sales.attrs['member']).Count) gt=$($sales.attrs['grouptype'])"
"CHECK upn=$((Find-Fake 'CN=Bob,OU=Staff,DC=new,DC=lan').attrs['userprincipalname'])"
"CHECK links=$($global:Links -join ';')"
"#;
        let out = run_restore(&payload(&b, "rebuild", "new.lan", &opts), "new.lan", setup, check);
        assert!(out.contains("failed") && out.contains(" 0 failed"), "{out}");
        assert!(out.contains("CHECK manager=CN=Bob,OU=Staff,DC=new,DC=lan uac=514 pw=set title=Boss"), "{out}");
        assert!(out.contains("CHECK members=3 gt=-2147483646"), "{out}");
        assert!(out.contains("CHECK upn=bob@new.lan"), "{out}");
        assert!(out.contains("CHECK links=gpo-Desktop>OU=Staff,DC=new,DC=lan>Yes>Yes"), "{out}");

        // The same plan against a DC of another domain changes nothing.
        let out = run_restore(&payload(&b, "rebuild", "new.lan", &opts), "corp.local", "[void](Add-Fake 'DC=corp,DC=local' 'domainDNS')", "\"CHECK count=$($global:AD.Count)\"");
        assert!(out.contains("Nothing was changed") && out.contains("CHECK count=1"), "{out}");
    }

    #[cfg(windows)]
    #[test]
    fn restore_script_reverts_changes() {
        let (backup, live) = sample();
        let d = compare(&meta(), &backup, &live);
        let opts = PlanOptions { remove_added_members: true, ..Default::default() };
        let mut b = builder();
        plan_revert(&mut b, &opts, &backup, &d, None);
        // AD as in `live`, with Cid in the Recycle Bin.
        let setup = r#"
[void](Add-Fake 'DC=corp,DC=local' 'domainDNS')
[void](Add-Fake 'OU=Staff,DC=corp,DC=local' 'organizationalUnit' 'ou1')
$g = Add-Fake 'CN=Sales,OU=Staff,DC=corp,DC=local' 'group' 'g1'
$a = Add-Fake 'CN=Ann,DC=corp,DC=local' 'user' 'u1'; $a.attrs['title'] = 'Clerk'; $a.attrs['department'] = 'X'
[void](Add-Fake 'CN=Bob,OU=Staff,DC=corp,DC=local' 'user' 'u2')
[void](Add-Fake 'CN=Cid,CN=Deleted Objects,DC=corp,DC=local' 'user' 'u3' -deleted)
[void](Add-Fake 'CN=Dan,OU=Staff,DC=corp,DC=local' 'user' 'u4')
$g.attrs['member'] = @('CN=Ann,DC=corp,DC=local', 'CN=Dan,OU=Staff,DC=corp,DC=local')
"#;
        let check = r#"
$ann = Find-Fake 'u1'
"CHECK ann=$($ann.dn) title=$($ann.attrs['title']) dept=$($ann.attrs.ContainsKey('department'))"
"CHECK cid=$((Find-Fake 'u3').dn) deleted=$((Find-Fake 'u3').deleted)"
"CHECK members=$((@((Find-Fake 'g1').attrs['member']) | Sort-Object) -join '|')"
"#;
        let out = run_restore(&payload(&b, "revert", "corp.local", &opts), "corp.local", setup, check);
        assert!(out.contains(" 0 failed"), "{out}");
        assert!(out.contains("from the Recycle Bin, same SID"), "{out}");
        assert!(out.contains("CHECK ann=CN=Ann,OU=Staff,DC=corp,DC=local title=Boss dept=False"), "{out}");
        assert!(out.contains("CHECK cid=CN=Cid,OU=Staff,DC=corp,DC=local deleted=False"), "{out}");
        // Bob and Cid are back in Sales and Dan is out. (Ann's member DN is stale only
        // because this fake does not update links on a move, as AD does.)
        let members = out.lines().find(|l| l.starts_with("CHECK members=")).unwrap_or_default();
        assert!(members.contains("CN=Bob,OU=Staff") && members.contains("CN=Cid,OU=Staff") && !members.contains("Dan"), "{out}");
    }

    /// The PowerShell scripts parse (Windows only).
    #[cfg(windows)]
    #[test]
    fn scripts_parse() {
        for (name, body) in [("ad-restore.ps1", RESTORE_PS), ("gpo-backup.ps1", GPO_BACKUP_PS)] {
            let check = "$e = $null; [void][System.Management.Automation.Language.Parser]::ParseInput([Console]::In.ReadToEnd(), [ref]$null, [ref]$e); $e | ForEach-Object { $_.Message + ' at line ' + $_.Extent.StartLineNumber }";
            let mut child = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", check])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            let errors = String::from_utf8_lossy(&out.stdout);
            assert!(errors.trim().is_empty(), "{name}: {errors}");
        }
    }
}
