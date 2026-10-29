//! Group Policy import and injection.
//!
//! GPOs are *read* from AD (LDAP search of groupPolicyContainer objects and
//! gPLink attributes) and their files are copied from SYSVOL into a local
//! snapshot. Nothing in AD or SYSVOL is ever changed. Each imported GPO
//! becomes a normal package ("gpo-…") that applies the GPO's computer
//! settings directly on the target computers, without linking the GPO:
//! - Administrative Templates / registry policies (Machine\Registry.pol)
//! - Security settings (GptTmpl.inf) through `secedit /configure`
//! - Advanced audit policy (audit.csv) through `auditpol /restore`
//!
//! Group Policy Preferences, scripts, software installation and user settings
//! are listed in the snapshot but not applied.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::ad::{self, AdSettings, Base};
use crate::app::App;
use crate::db::now;
use crate::transport::ps_quote;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpoLink {
    /// DN of the OU / domain the GPO is linked to.
    pub target: String,
    pub enabled: bool,
    pub enforced: bool,
}

/// A GPO as it is in AD, plus local import state.
#[derive(Debug, Clone, Serialize)]
pub struct GpoInfo {
    pub guid: String,
    pub name: String,
    pub path: String,
    pub computer_version: u32,
    pub user_version: u32,
    pub changed: Option<String>,
    pub computer_enabled: bool,
    pub user_enabled: bool,
    pub links: Vec<GpoLink>,
    pub imported: Option<SnapshotMeta>,
}

/// What is stored next to an imported snapshot (meta.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub guid: String,
    pub name: String,
    pub computer_version: u32,
    pub source_path: String,
    pub imported_at: String,
    pub package_id: String,
    pub machine_settings: usize,
    pub user_settings: usize,
    pub has_security: bool,
    pub has_audit: bool,
    /// Files in the GPO that PSCCM does not apply.
    pub unsupported: Vec<String>,
}

// ------------------------------------------------------------ Registry.pol

#[derive(Debug, Clone, PartialEq)]
pub struct PolEntry {
    pub key: String,
    pub name: String,
    pub kind: u32,
    pub data: Vec<u8>,
}

const REG_NONE: u32 = 0;
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_BINARY: u32 = 3;
const REG_DWORD: u32 = 4;
const REG_DWORD_BE: u32 = 5;
const REG_MULTI_SZ: u32 = 7;
const REG_QWORD: u32 = 11;

/// Parse the PReg format used by Registry.pol files.
pub fn parse_pol(b: &[u8]) -> Result<Vec<PolEntry>> {
    if b.len() < 8 || &b[0..4] != b"PReg" {
        bail!("not a Registry.pol file (missing PReg header)");
    }
    if u32::from_le_bytes(b[4..8].try_into().unwrap()) != 1 {
        bail!("unsupported Registry.pol version");
    }
    let mut i = 8;
    let u16_at = |i: usize| -> Result<u16> {
        b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| anyhow!("truncated Registry.pol"))
    };
    let u32_at = |i: usize| -> Result<u32> {
        b.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| anyhow!("truncated Registry.pol"))
    };
    let expect = |i: &mut usize, ch: char| -> Result<()> {
        let got = u16_at(*i)?;
        if got != ch as u16 {
            bail!("malformed Registry.pol at byte {}: expected '{ch}'", *i);
        }
        *i += 2;
        Ok(())
    };
    // A NUL-terminated UTF-16 string, ending at NUL or the ';' delimiter.
    let string = |i: &mut usize| -> Result<String> {
        let mut units = Vec::new();
        loop {
            let u = u16_at(*i)?;
            if u == 0 {
                *i += 2;
                break;
            }
            if u == ';' as u16 {
                break;
            }
            units.push(u);
            *i += 2;
        }
        Ok(String::from_utf16_lossy(&units))
    };
    let mut out = Vec::new();
    while i + 1 < b.len() {
        expect(&mut i, '[')?;
        let key = string(&mut i)?;
        expect(&mut i, ';')?;
        let name = string(&mut i)?;
        expect(&mut i, ';')?;
        let kind = u32_at(i)?;
        i += 4;
        expect(&mut i, ';')?;
        let size = u32_at(i)? as usize;
        i += 4;
        expect(&mut i, ';')?;
        let data = b.get(i..i + size).ok_or_else(|| anyhow!("truncated Registry.pol data"))?.to_vec();
        i += size;
        expect(&mut i, ']')?;
        out.push(PolEntry { key, name, kind, data });
    }
    Ok(out)
}

fn utf16_string(data: &[u8]) -> String {
    let units: Vec<u16> = data.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

fn type_name(kind: u32) -> &'static str {
    match kind {
        REG_SZ => "String",
        REG_EXPAND_SZ => "ExpandString",
        REG_BINARY => "Binary",
        REG_DWORD | REG_DWORD_BE => "DWord",
        REG_MULTI_SZ => "MultiString",
        REG_QWORD => "QWord",
        REG_NONE => "None",
        _ => "Binary",
    }
}

fn value_json(kind: u32, data: &[u8]) -> serde_json::Value {
    use serde_json::json;
    match kind {
        REG_SZ | REG_EXPAND_SZ => json!(utf16_string(data)),
        REG_DWORD if data.len() >= 4 => json!(i32::from_le_bytes(data[..4].try_into().unwrap())),
        REG_DWORD_BE if data.len() >= 4 => json!(i32::from_be_bytes(data[..4].try_into().unwrap())),
        REG_QWORD if data.len() >= 8 => json!(i64::from_le_bytes(data[..8].try_into().unwrap())),
        REG_MULTI_SZ => {
            let s = utf16_string(data);
            json!(s.split('\0').filter(|p| !p.is_empty()).collect::<Vec<_>>())
        }
        REG_NONE => json!(null),
        _ => json!(B64.encode(data)),
    }
}

/// One registry action, as shown in the UI and applied by the package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegAction {
    /// set | soft | delete | deletevalues | deletekeys | key
    #[serde(rename = "a")]
    pub action: String,
    #[serde(rename = "k")]
    pub key: String,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "t")]
    pub kind: String,
    #[serde(rename = "v")]
    pub value: serde_json::Value,
}

/// Translate Registry.pol entries (including the special "**del." etc.
/// value names) into actions.
pub fn reg_actions(entries: &[PolEntry]) -> Vec<RegAction> {
    let mut out = Vec::new();
    for e in entries {
        let lower = e.name.to_lowercase();
        let mk = |action: &str, name: &str, kind: &str, value: serde_json::Value| RegAction {
            action: action.into(),
            key: e.key.clone(),
            name: name.into(),
            kind: kind.into(),
            value,
        };
        if lower.starts_with("**securekey") {
            continue;
        } else if let Some(n) = e.name.get(6..).filter(|_| lower.starts_with("**del.")) {
            out.push(mk("delete", n, "None", serde_json::Value::Null));
        } else if lower.starts_with("**delvals") {
            out.push(mk("deletevalues", "", "None", serde_json::Value::Null));
        } else if lower.starts_with("**deletevalues") || lower.starts_with("**deletekeys") {
            let action = if lower.starts_with("**deletekeys") { "deletekeys" } else { "delete" };
            for n in utf16_string(&e.data).split(';').map(str::trim).filter(|s| !s.is_empty()) {
                out.push(mk(action, n, "None", serde_json::Value::Null));
            }
        } else if let Some(n) = e.name.get(7..).filter(|_| lower.starts_with("**soft.")) {
            out.push(mk("soft", n, type_name(e.kind), value_json(e.kind, &e.data)));
        } else if e.name.is_empty() && (e.kind == REG_NONE || e.data.is_empty()) {
            out.push(mk("key", "", "None", serde_json::Value::Null));
        } else {
            out.push(mk("set", &e.name, type_name(e.kind), value_json(e.kind, &e.data)));
        }
    }
    out
}

// ---------------------------------------------------------------- listing

fn snapshots_dir(app: &App) -> PathBuf {
    app.data_dir.join("gpos")
}

/// The GUID in canonical form. Only hex digits and dashes survive, so the
/// result is always safe to use as a folder name.
fn clean_guid(g: &str) -> String {
    g.chars().filter(|c| c.is_ascii_hexdigit() || *c == '-').collect::<String>().to_uppercase()
}

/// gPCFileSysPath must be `\\<domain>\SYSVOL\<domain>\Policies\{<guid>}` for
/// this GPO. Anyone allowed to edit a GPO can change that attribute, and the
/// console reads from (and authenticates to) whatever host it names.
fn check_sysvol_path(path: &str, guid: &str) -> Result<()> {
    let parts: Vec<&str> = path.strip_prefix(r"\\").unwrap_or("").split('\\').collect();
    let ok = matches!(parts.as_slice(), [host, sysvol, domain, policies, g]
        if host.eq_ignore_ascii_case(domain)
            && sysvol.eq_ignore_ascii_case("SYSVOL")
            && policies.eq_ignore_ascii_case("Policies")
            && g.len() == 38
            && clean_guid(g) == guid
            && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    if !ok {
        bail!("GPO {guid} has an unexpected SYSVOL path ({path}); refusing to read it");
    }
    Ok(())
}

/// Parse a gPLink value: "[LDAP://cn={GUID},cn=policies,...;0][...]".
pub fn parse_gplink(v: &str) -> Vec<(String, u32)> {
    v.split('[')
        .filter_map(|part| {
            let part = part.trim_end_matches(']');
            let (dn, opt) = part.rsplit_once(';')?;
            let guid = dn.split('{').nth(1)?.split('}').next()?;
            Some((clean_guid(guid), opt.trim().parse().unwrap_or(0)))
        })
        .collect()
}

/// GPOs in the domain (read-only LDAP search) with their links.
pub async fn list(app: &App) -> Result<Vec<GpoInfo>> {
    let s: AdSettings = app.db.get_setting("ad")?;
    let entries = ad::search(
        app,
        &s,
        Base::UnderRoot("CN=Policies,CN=System"),
        "(objectClass=groupPolicyContainer)",
        &["displayName", "cn", "gPCFileSysPath", "versionNumber", "whenChanged", "flags"],
    )
    .await?;
    let link_entries = ad::search(app, &s, Base::Root, "(gPLink=*)", &["gPLink"]).await.unwrap_or_default();
    let mut out: Vec<GpoInfo> = entries
        .into_iter()
        .map(|e| {
            let guid = clean_guid(&e.first("cn").unwrap_or_default());
            let version: u32 = e.first("versionNumber").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) as u32;
            let flags: u32 = e.first("flags").and_then(|v| v.parse().ok()).unwrap_or(0);
            let links = link_entries
                .iter()
                .flat_map(|l| {
                    parse_gplink(&l.first("gPLink").unwrap_or_default())
                        .into_iter()
                        .filter(|(g, _)| *g == guid)
                        .map(|(_, opt)| GpoLink { target: l.dn.clone(), enabled: opt & 1 == 0, enforced: opt & 2 != 0 })
                        .collect::<Vec<_>>()
                })
                .collect();
            GpoInfo {
                name: e.first("displayName").unwrap_or_else(|| guid.clone()),
                path: e.first("gPCFileSysPath").unwrap_or_default(),
                computer_version: version & 0xFFFF,
                user_version: version >> 16,
                changed: e.first("whenChanged").map(|t| generalized_time(&t)),
                computer_enabled: flags & 2 == 0,
                user_enabled: flags & 1 == 0,
                links,
                imported: read_meta(app, &guid).ok(),
                guid,
            }
        })
        .collect();
    out.sort_by_key(|g| g.name.to_lowercase());
    Ok(out)
}

/// "20261028123456.0Z" -> RFC 3339.
fn generalized_time(t: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(t.get(..14).unwrap_or(t), "%Y%m%d%H%M%S")
        .map(|d| d.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|_| t.to_string())
}

pub fn read_meta(app: &App, guid: &str) -> Result<SnapshotMeta> {
    let p = snapshots_dir(app).join(clean_guid(guid)).join("meta.json");
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

/// Locally imported snapshots (works without AD access).
pub fn list_imported(app: &App) -> Vec<SnapshotMeta> {
    let mut v: Vec<SnapshotMeta> = std::fs::read_dir(snapshots_dir(app))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_meta(app, &e.file_name().to_string_lossy()).ok())
        .collect();
    v.sort_by_key(|m| m.name.to_lowercase());
    v
}

// ---------------------------------------------------------------- import

fn copy_dir(src: &Path, dst: &Path, budget: &mut u64) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to, budget)?;
        } else {
            let len = e.metadata()?.len();
            if len > *budget {
                bail!("GPO folder is larger than 50 MB; refusing to copy");
            }
            *budget -= len;
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Copy the GPO folder from SYSVOL (read-only access).
async fn fetch_sysvol(app: &App, s: &AdSettings, path: &str, dest: &Path) -> Result<()> {
    if !cfg!(windows) {
        bail!("importing GPOs needs the console on Windows (SYSVOL is an SMB share)");
    }
    // A console that is not domain-joined may not resolve the domain name in
    // gPCFileSysPath; use the configured domain controller instead.
    let mut path = path.to_string();
    if s.mode == "ldap" && !s.server.trim().is_empty() {
        let server = s.server.trim().trim_start_matches("ldaps://").trim_start_matches("ldap://");
        let server = server.split(':').next().unwrap_or(server);
        if let Some(rest) = path.strip_prefix(r"\\").and_then(|p| p.split_once('\\')).map(|(_, r)| r.to_string()) {
            path = format!(r"\\{server}\{rest}");
        }
    }
    let mut budget = 50 * 1024 * 1024;
    let direct = copy_dir(Path::new(&path), dest, &mut budget);
    if direct.is_ok() || s.mode != "ldap" {
        return direct.with_context(|| format!("copying {path}"));
    }
    // Retry with the AD credential (console not in the domain).
    let cred_id = s.credential_id.ok_or_else(|| anyhow!("no AD credential to read SYSVOL"))?;
    let cred = app.db.get_credential(cred_id)?.ok_or_else(|| anyhow!("AD credential missing"))?;
    let pw = app.db.credential_secret(cred_id)?.unwrap_or_default();
    let share = path.trim_start_matches('\\').splitn(3, '\\').take(2).collect::<Vec<_>>().join("\\");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
trap {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}
$sec = ConvertTo-SecureString $env:PSCCM_PASS -AsPlainText -Force
$cred = New-Object System.Management.Automation.PSCredential($env:PSCCM_USER, $sec)
New-PSDrive -Name PSCCMSYSVOL -PSProvider FileSystem -Root {share} -Credential $cred | Out-Null
try {{
    New-Item -ItemType Directory -Force -Path {dest} | Out-Null
    Copy-Item -Path (Join-Path {src} '*') -Destination {dest} -Recurse -Force
}} finally {{ Remove-PSDrive PSCCMSYSVOL -ErrorAction SilentlyContinue }}
"#,
        share = ps_quote(&format!(r"\\{share}")),
        src = ps_quote(&path),
        dest = ps_quote(&dest.to_string_lossy()),
    );
    ad::run_ps(&script, &[("PSCCM_USER", &cred.username), ("PSCCM_PASS", &pw)])
        .await
        .with_context(|| format!("copying {path} as {}", cred.username))?;
    Ok(())
}

fn find_ci(dir: &Path, rel: &[&str]) -> Option<PathBuf> {
    let mut cur = dir.to_path_buf();
    for part in rel {
        let next = std::fs::read_dir(&cur)
            .ok()?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))?;
        cur = next.path();
    }
    Some(cur)
}

/// Import (or re-import) a GPO: snapshot from SYSVOL and (re)generate its package.
pub async fn import(app: &App, guid: &str) -> Result<SnapshotMeta> {
    let guid = clean_guid(guid);
    let s: AdSettings = app.db.get_setting("ad")?;
    let gpo = list(app).await?.into_iter().find(|g| g.guid == guid).ok_or_else(|| anyhow!("GPO {guid} not found in AD"))?;
    if gpo.path.is_empty() {
        bail!("GPO {} has no SYSVOL path", gpo.name);
    }
    check_sysvol_path(&gpo.path, &guid)?;
    let dir = snapshots_dir(app).join(&guid);
    let tmp = dir.join("files.new");
    let _ = std::fs::remove_dir_all(&tmp);
    fetch_sysvol(app, &s, &gpo.path, &tmp).await?;
    finish_import(app, &guid, &gpo.name, gpo.computer_version, &gpo.path)
}

/// Import from a folder: a SYSVOL-style GPO folder (with Machine\) or a GPMC
/// / Backup-GPO backup ({GUID}\DomainSysvol\GPO\Machine). Works without AD.
pub fn import_folder(app: &App, src: &Path, name: Option<String>) -> Result<SnapshotMeta> {
    let gpo_root = if find_ci(src, &["Machine"]).is_some() || find_ci(src, &["User"]).is_some() {
        src.to_path_buf()
    } else if let Some(p) = find_ci(src, &["DomainSysvol", "GPO"]) {
        p
    } else {
        bail!("{} is not a GPO folder (no Machine\\ or DomainSysvol\\GPO\\ inside)", src.display());
    };
    let folder = src.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let looks_guid = folder.trim_matches(|c| c == '{' || c == '}').len() == 36 && folder.contains('-');
    let guid = if looks_guid { clean_guid(&folder) } else { uuid::Uuid::new_v4().to_string().to_uppercase() };
    let backup_name = std::fs::read(src.join("bkupInfo.xml"))
        .ok()
        .map(|b| decode_text(&b))
        .and_then(|x| x.split("<GPODisplayName><![CDATA[").nth(1).and_then(|r| r.split("]]>").next()).map(String::from));
    let name = name.filter(|n| !n.trim().is_empty()).or(backup_name).unwrap_or_else(|| folder.clone());
    let version = find_ci(&gpo_root, &["GPT.INI"])
        .and_then(|p| std::fs::read(p).ok())
        .map(|b| decode_text(&b))
        .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("Version=").and_then(|v| v.trim().parse::<u32>().ok())))
        .unwrap_or(0)
        & 0xFFFF;
    let dir = snapshots_dir(app).join(&guid);
    let tmp = dir.join("files.new");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut budget = 50 * 1024 * 1024;
    copy_dir(&gpo_root, &tmp, &mut budget)?;
    finish_import(app, &guid, &name, version, &src.to_string_lossy())
}

/// Move the freshly copied files.new into place, write meta.json and the package.
fn finish_import(app: &App, guid: &str, name: &str, version: u32, source: &str) -> Result<SnapshotMeta> {
    // The name comes from AD or a backup file and ends up in a YAML package.
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let name = name.as_str();
    let dir = snapshots_dir(app).join(guid);
    let files = dir.join("files");
    let _ = std::fs::remove_dir_all(&files);
    std::fs::rename(dir.join("files.new"), &files)?;

    let snap = load_files(&files)?;
    let package_id = package_id_for(name, guid);
    let meta = SnapshotMeta {
        guid: guid.to_string(),
        name: name.to_string(),
        computer_version: version,
        source_path: source.to_string(),
        imported_at: now(),
        package_id: package_id.clone(),
        machine_settings: snap.machine.len(),
        user_settings: snap.user_count,
        has_security: snap.security.is_some(),
        has_audit: snap.audit.is_some(),
        unsupported: snap.unsupported.clone(),
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    let yaml = package_yaml(&meta, &snap);
    crate::package::Package::from_yaml(&yaml).context("generated GPO package is invalid")?;
    std::fs::write(app.packages_dir().join(format!("{package_id}.yaml")), yaml)?;
    Ok(meta)
}

pub fn delete(app: &App, guid: &str) -> Result<()> {
    let meta = read_meta(app, guid)?;
    let _ = std::fs::remove_file(app.packages_dir().join(format!("{}.yaml", meta.package_id)));
    std::fs::remove_dir_all(snapshots_dir(app).join(clean_guid(guid)))?;
    Ok(())
}

/// Parsed content of a snapshot.
#[derive(Debug, Default, Serialize)]
pub struct Snapshot {
    pub machine: Vec<RegAction>,
    pub user_count: usize,
    #[serde(skip)]
    pub security_raw: Option<Vec<u8>>,
    pub security: Option<String>,
    pub audit: Option<String>,
    pub unsupported: Vec<String>,
}

pub fn load_files(files: &Path) -> Result<Snapshot> {
    let mut snap = Snapshot::default();
    if let Some(p) = find_ci(files, &["Machine", "Registry.pol"]) {
        snap.machine = reg_actions(&parse_pol(&std::fs::read(&p)?).context("Machine\\Registry.pol")?);
    }
    if let Some(p) = find_ci(files, &["User", "Registry.pol"]) {
        snap.user_count = parse_pol(&std::fs::read(&p)?).map(|v| v.len()).unwrap_or(0);
    }
    if let Some(p) = find_ci(files, &["Machine", "Microsoft", "Windows NT", "SecEdit", "GptTmpl.inf"]) {
        let raw = std::fs::read(&p)?;
        snap.security = Some(decode_text(&raw));
        snap.security_raw = Some(raw);
    }
    if let Some(p) = find_ci(files, &["Machine", "Microsoft", "Windows NT", "Audit", "audit.csv"]) {
        snap.audit = Some(decode_text(&std::fs::read(&p)?));
    }
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
                continue;
            }
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('/', "\\");
            let l = rel.to_lowercase();
            let handled = l == "gpt.ini"
                || l.ends_with("comment.cmtx")
                || l == "machine\\registry.pol"
                || l == "machine\\microsoft\\windows nt\\secedit\\gpttmpl.inf"
                || l == "machine\\microsoft\\windows nt\\audit\\audit.csv";
            if !handled {
                out.push(rel);
            }
        }
    }
    walk(files, files, &mut snap.unsupported);
    snap.unsupported.sort();
    Ok(snap)
}

/// Decode a UTF-16 (with BOM) or UTF-8 text file.
fn decode_text(b: &[u8]) -> String {
    if b.starts_with(&[0xFF, 0xFE]) {
        utf16_string(&b[2..])
    } else {
        String::from_utf8_lossy(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b)).to_string()
    }
}

pub fn read_snapshot(app: &App, guid: &str) -> Result<(SnapshotMeta, Snapshot)> {
    let meta = read_meta(app, guid)?;
    let snap = load_files(&snapshots_dir(app).join(clean_guid(guid)).join("files"))?;
    Ok((meta, snap))
}

fn package_id_for(name: &str, guid: &str) -> String {
    let mut slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    format!("gpo-{}-{}", slug.trim_end_matches('-'), guid.get(..8).unwrap_or(guid).to_lowercase())
}

const REGISTRY_SCRIPT: &str = r#"$entries = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($payload)) | ConvertFrom-Json
$remove = $mode -eq 'remove'
$set = 0; $removed = 0
foreach ($e in @($entries)) {
    $path = 'Registry::HKEY_LOCAL_MACHINE\' + $e.k
    if ($e.a -eq 'set' -or $e.a -eq 'soft') {
        if ($remove) {
            if (Test-Path -LiteralPath $path) { Remove-ItemProperty -LiteralPath $path -Name $e.n -ErrorAction SilentlyContinue; $removed++ }
            continue
        }
        if (-not (Test-Path -LiteralPath $path)) { New-Item -Path $path -Force | Out-Null }
        if ($e.a -eq 'soft' -and $null -ne (Get-ItemProperty -LiteralPath $path -Name $e.n -ErrorAction SilentlyContinue)) { continue }
        if ($e.t -eq 'DWord') { $val = [int]$e.v }
        elseif ($e.t -eq 'QWord') { $val = [long]$e.v }
        elseif ($e.t -eq 'Binary') { $val = [byte[]][Convert]::FromBase64String([string]$e.v) }
        elseif ($e.t -eq 'MultiString') { $val = [string[]]@($e.v) }
        elseif ($e.t -eq 'None') { $val = [byte[]]@(); $e.t = 'None' }
        else { $val = [string]$e.v }
        New-ItemProperty -LiteralPath $path -Name $e.n -PropertyType $e.t -Value $val -Force | Out-Null
        Write-Output "set    $($e.k)\$($e.n) = $($e.v)"
        $set++
    } elseif ($remove) {
        continue
    } elseif ($e.a -eq 'key') {
        if (-not (Test-Path -LiteralPath $path)) { New-Item -Path $path -Force | Out-Null }
    } elseif ($e.a -eq 'delete') {
        if (Test-Path -LiteralPath $path) { Remove-ItemProperty -LiteralPath $path -Name $e.n -ErrorAction SilentlyContinue }
        Write-Output "delete $($e.k)\$($e.n)"; $removed++
    } elseif ($e.a -eq 'deletevalues') {
        if (Test-Path -LiteralPath $path) {
            (Get-Item -LiteralPath $path).GetValueNames() | ForEach-Object { Remove-ItemProperty -LiteralPath $path -Name $_ -ErrorAction SilentlyContinue }
        }
        Write-Output "clear  $($e.k)"; $removed++
    } elseif ($e.a -eq 'deletekeys') {
        $sub = $path + '\' + $e.n
        if (Test-Path -LiteralPath $sub) { Remove-Item -LiteralPath $sub -Recurse -Force }
        Write-Output "delkey $($e.k)\$($e.n)"; $removed++
    }
}
Write-Output "Registry policies: $set set, $removed removed"
"#;

fn package_yaml(meta: &SnapshotMeta, snap: &Snapshot) -> String {
    let indent = |s: &str| s.lines().map(|l| format!("      {l}")).collect::<Vec<_>>().join("\n");
    let payload = B64.encode(serde_json::to_vec(&snap.machine).unwrap());
    let mut parts = vec![];
    if !snap.machine.is_empty() {
        parts.push(format!("{} registry settings", snap.machine.len()));
    }
    if snap.security.is_some() {
        parts.push("security policy".to_string());
    }
    if snap.audit.is_some() {
        parts.push("audit policy".to_string());
    }
    let what = if parts.is_empty() { "no supported computer settings".to_string() } else { parts.join(", ") };
    let mut y = format!(
        r#"# Generated by PSCCM from GPO "{name}" {{{guid}}} - re-import the GPO to refresh.
id: {id}
name: "GPO: {name_q}"
version: "{ver}"
category: Group Policy
description: >-
  Applies the computer settings of GPO "{name_q}" ({what}) directly on the selected
  computers, without linking the GPO in Active Directory. Imported {at}.
  mode=remove deletes the registry values this GPO sets (security/audit settings are
  not reverted).
platforms: [windows]
variables:
  mode:
    default: apply
    options: [apply, remove]
    description: apply the settings, or remove the registry values again
  refresh:
    default: "no"
    options: ["no", "yes"]
    description: Run gpupdate /target:computer afterwards
steps:
"#,
        name = meta.name.replace(|c: char| c == '"' || c.is_control(), "'"),
        name_q = meta.name.replace(|c: char| c == '"' || c == '\\' || c.is_control(), "'"),
        guid = meta.guid,
        id = meta.package_id,
        ver = meta.computer_version,
        at = &meta.imported_at[..10],
    );
    if !snap.machine.is_empty() {
        y += &format!(
            "  - name: Registry policies ({} settings)\n    script: |\n      $mode = '{{{{mode}}}}'\n      $payload = '{payload}'\n{}\n",
            snap.machine.len(),
            indent(REGISTRY_SCRIPT)
        );
    }
    if let Some(raw) = &snap.security_raw {
        let script = format!(
            "$dir = Join-Path $env:windir ('Temp\\psccm-gpo-' + [guid]::NewGuid().ToString('N'))\nNew-Item -ItemType Directory -Path $dir | Out-Null\n$inf = Join-Path $dir 'gpo.inf'\n$db = Join-Path $dir 'gpo.sdb'\n$log = Join-Path $dir 'secedit.log'\nRemove-Item $db, $log -ErrorAction SilentlyContinue\n[IO.File]::WriteAllBytes($inf, [Convert]::FromBase64String('{}'))\n& secedit.exe /configure /db $db /cfg $inf /log $log /quiet\n$code = $LASTEXITCODE\nGet-Content $log -Tail 20 -ErrorAction SilentlyContinue\nRemove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue\nexit $code",
            B64.encode(raw)
        );
        y += &format!(
            "  - name: Security policy (secedit)\n    when: |\n      if ('{{{{mode}}}}' -eq 'apply') {{ exit 0 }}\n      exit 1\n    success_codes: [0, 3]\n    script: |\n{}\n",
            indent(&script)
        );
    }
    if let Some(audit) = &snap.audit {
        let script = format!(
            "$f = Join-Path $env:windir 'Temp\\psccm-audit.csv'\n[IO.File]::WriteAllBytes($f, [Convert]::FromBase64String('{}'))\n& auditpol.exe /restore /file:$f\n$code = $LASTEXITCODE\nRemove-Item $f -ErrorAction SilentlyContinue\nexit $code",
            B64.encode(audit.as_bytes())
        );
        y += &format!(
            "  - name: Audit policy (auditpol)\n    when: |\n      if ('{{{{mode}}}}' -eq 'apply') {{ exit 0 }}\n      exit 1\n    script: |\n{}\n",
            indent(&script)
        );
    }
    y += "  - name: Refresh Group Policy\n    when: |\n      if ('{{refresh}}' -eq 'yes') { exit 0 }\n      exit 1\n    script: gpupdate.exe /target:computer /force\n";
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol_entry(key: &str, name: &str, kind: u32, data: &[u8]) -> Vec<u8> {
        let w = |s: &str| s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>();
        let mut v = w("[");
        v.extend(w(key));
        v.extend([0, 0]);
        v.extend(w(";"));
        v.extend(w(name));
        v.extend([0, 0]);
        v.extend(w(";"));
        v.extend(kind.to_le_bytes());
        v.extend(w(";"));
        v.extend((data.len() as u32).to_le_bytes());
        v.extend(w(";"));
        v.extend(data);
        v.extend(w("]"));
        v
    }

    fn sz(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn parses_registry_pol() {
        let mut b = b"PReg\x01\x00\x00\x00".to_vec();
        let k = r"Software\Policies\Microsoft\Windows\WindowsUpdate\AU";
        b.extend(pol_entry(k, "NoAutoUpdate", REG_DWORD, &1u32.to_le_bytes()));
        b.extend(pol_entry(k, "Server", REG_SZ, &sz("http://wsus:8530")));
        b.extend(pol_entry(k, "**del.Old", REG_SZ, &sz(" ")));
        b.extend(pol_entry(k, "**delvals.", REG_SZ, &sz(" ")));
        b.extend(pol_entry(k, "**DeleteValues", REG_SZ, &sz("A;B;")));
        b.extend(pol_entry(k, "**soft.Soft", REG_DWORD, &0xFFFF_FFFFu32.to_le_bytes()));
        b.extend(pol_entry(k, "", REG_NONE, &[]));
        b.extend(pol_entry(k, "Multi", REG_MULTI_SZ, &sz("a\0b\0")));
        let entries = parse_pol(&b).unwrap();
        assert_eq!(entries.len(), 8);
        let a = reg_actions(&entries);
        let summary: Vec<(String, String, serde_json::Value)> =
            a.iter().map(|x| (x.action.clone(), x.name.clone(), x.value.clone())).collect();
        use serde_json::json;
        assert_eq!(summary[0], ("set".into(), "NoAutoUpdate".into(), json!(1)));
        assert_eq!(summary[1], ("set".into(), "Server".into(), json!("http://wsus:8530")));
        assert_eq!(summary[2], ("delete".into(), "Old".into(), json!(null)));
        assert_eq!(summary[3].0, "deletevalues");
        assert_eq!((summary[4].1.as_str(), summary[5].1.as_str()), ("A", "B"));
        assert_eq!(summary[6], ("soft".into(), "Soft".into(), json!(-1)));
        assert_eq!(summary[7].0, "key");
        assert_eq!(summary[8], ("set".into(), "Multi".into(), json!(["a", "b"])));
        assert!(parse_pol(b"nope").is_err());
    }

    /// Runs the real apply/remove script against a throwaway HKCU key.
    #[cfg(windows)]
    #[test]
    fn registry_script_applies_and_removes() {
        let key = format!(r"Software\PSCCM-Test-{}", std::process::id());
        let mut b = b"PReg\x01\x00\x00\x00".to_vec();
        b.extend(pol_entry(&key, "Dw", REG_DWORD, &0xFFFF_FFFEu32.to_le_bytes()));
        b.extend(pol_entry(&key, "Sz", REG_SZ, &sz("hello {world}")));
        b.extend(pol_entry(&key, "Multi", REG_MULTI_SZ, &sz("a\0b\0")));
        b.extend(pol_entry(&key, "Bin", REG_BINARY, &[1, 2, 3]));
        b.extend(pol_entry(&format!(r"{key}\Sub"), "", REG_NONE, &[]));
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::process::Command::new("reg.exe").args(["delete", &format!(r"HKCU\{}", self.0), "/f"]).output();
            }
        }
        let _cleanup = Cleanup(key.clone());
        let actions = reg_actions(&parse_pol(&b).unwrap());
        let payload = B64.encode(serde_json::to_vec(&actions).unwrap());
        let body = REGISTRY_SCRIPT.replace(r"Registry::HKEY_LOCAL_MACHINE\", r"Registry::HKEY_CURRENT_USER\");
        let check = format!(
            r#"$p = 'HKCU:\{key}'
$v = Get-ItemProperty $p
"$($v.Dw)|$($v.Sz)|$($v.Multi -join ',')|$($v.Bin -join ',')|$(Test-Path "$p\Sub")""#
        );
        let run = |mode: &str, extra: &str| {
            let script = format!("$ErrorActionPreference='Stop'\n$mode='{mode}'\n$payload='{payload}'\n{body}\n{extra}");
            let out = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let out = run("apply", &check);
        assert!(out.contains("4 set"), "{out}");
        assert!(out.contains("4294967294|hello {world}|a,b|1,2,3|True"), "{out}");
        let out = run("remove", &format!("(Get-Item 'HKCU:\\{key}').GetValueNames().Count"));
        assert!(out.trim().ends_with('0'), "values left after remove: {out}");
    }

    #[test]
    fn gplink_and_ids() {
        let v = "[LDAP://cn={31B2F340-016D-11D2-945F-00C04FB984F9},cn=policies,cn=system,DC=corp,DC=local;0][LDAP://cn={6AC1786C-016F-11D2-945F-00C04fB984F9},cn=policies,cn=system,DC=corp,DC=local;2]";
        let l = parse_gplink(v);
        assert_eq!(l, vec![("31B2F340-016D-11D2-945F-00C04FB984F9".to_string(), 0), ("6AC1786C-016F-11D2-945F-00C04FB984F9".to_string(), 2)]);
        assert_eq!(package_id_for("Default Domain Policy!", "31B2F340-016D"), "gpo-default-domain-policy-31b2f340");
    }

    #[test]
    fn generated_package_is_valid() {
        let snap = Snapshot {
            machine: reg_actions(&[PolEntry { key: r"Software\Policies\X".into(), name: "A".into(), kind: REG_DWORD, data: 1u32.to_le_bytes().to_vec() }]),
            security_raw: Some(b"[Unicode]\r\nUnicode=yes\r\n".to_vec()),
            security: Some("x".into()),
            audit: Some("Machine Name,Policy Target\n".into()),
            ..Default::default()
        };
        let meta = SnapshotMeta {
            guid: "31B2F340-016D-11D2-945F-00C04FB984F9".into(),
            name: "Test \"GPO\"".into(),
            computer_version: 3,
            source_path: String::new(),
            imported_at: now(),
            package_id: "gpo-test-31b2f340".into(),
            machine_settings: 1,
            user_settings: 0,
            has_security: true,
            has_audit: true,
            unsupported: vec![],
        };
        let p = crate::package::Package::from_yaml(&package_yaml(&meta, &snap)).unwrap();
        assert_eq!(p.steps.len(), 4);
    }
}
