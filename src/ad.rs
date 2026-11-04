//! Read-only Active Directory computer inventory.
//!
//! Only LDAP *search* operations are used; nothing is ever written to AD, so
//! an ordinary domain user (no delegated rights) is enough. (AD restores in
//! `adbackup.rs` run as jobs on a domain controller, never from here.) Two modes:
//! - `ldap`: cross-platform LDAP/LDAPS bind with a stored credential.
//! - `windows`: the console's logged-on Windows account via ADSI (Kerberos,
//!   no password stored). Works with DCs that require LDAP signing.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ldap3::adapters::{Adapter, EntriesOnly, PagedResults};
use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use crate::app::App;
use crate::db::{NewComputer, now};
use crate::transport::ps_quote;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdSettings {
    /// "windows" (current Windows user) or "ldap" (stored credential)
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Domain controller or domain name, e.g. dc01.corp.local. Optional in
    /// windows mode (uses the console's domain).
    #[serde(default)]
    pub server: String,
    /// ldap mode: "ldaps" (636) or "starttls" (389). "none" (plain text) is
    /// refused, since the bind would send the stored password in the clear.
    #[serde(default = "default_security")]
    pub security: String,
    #[serde(default)]
    pub skip_tls_verify: bool,
    /// ldap mode: credential of kind "ldap" (user@domain + password)
    #[serde(default)]
    pub credential_id: Option<i64>,
    /// OUs to search, e.g. "OU=Servers,DC=corp,DC=local". Empty = whole domain.
    #[serde(default)]
    pub base_dns: Vec<String>,
    /// Extra LDAP filter ANDed in, e.g. "(operatingSystem=*Server*)".
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub include_disabled: bool,
    /// Ignore computers that have not logged on to the domain for N days (0 = keep all).
    #[serde(default)]
    pub stale_days: u32,
    /// Tag imported computers with the name of their OU.
    #[serde(default = "yes")]
    pub tag_with_ou: bool,
    /// Re-import automatically every N minutes (0 = off).
    #[serde(default)]
    pub auto_sync_minutes: u32,
    #[serde(default)]
    pub last_sync: Option<String>,
}

fn default_mode() -> String {
    if cfg!(windows) { "windows".into() } else { "ldap".into() }
}
fn default_security() -> String {
    "ldaps".into()
}
fn yes() -> bool {
    true
}

impl Default for AdSettings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdComputer {
    /// FQDN when available, otherwise the CN.
    pub name: String,
    pub cn: String,
    pub dn: String,
    pub ou: String,
    pub os: String,
    pub os_version: String,
    pub description: String,
    pub last_logon: Option<String>,
    pub enabled: bool,
    /// Already in the computer list.
    #[serde(default)]
    pub exists: bool,
    /// AD security groups (names) the computer belongs to.
    #[serde(default)]
    pub ad_groups: Vec<String>,
}

const ATTRS: &[&str] = &[
    "cn",
    "dNSHostName",
    "operatingSystem",
    "operatingSystemVersion",
    "description",
    "lastLogonTimestamp",
    "userAccountControl",
    "memberOf",
];

const ACCOUNT_DISABLED: i64 = 0x2;

fn ldap_filter(s: &AdSettings) -> String {
    let disabled = if s.include_disabled {
        ""
    } else {
        "(!(userAccountControl:1.2.840.113556.1.4.803:=2))"
    };
    let extra = s.filter.trim();
    let extra = if extra.is_empty() || extra.starts_with('(') {
        extra.to_string()
    } else {
        format!("({extra})")
    };
    format!("(&(objectCategory=computer){disabled}{extra})")
}

/// Attributes with binary values. Both search modes return them base64-encoded.
pub const BINARY_ATTRS: &[&str] = &["objectguid", "objectsid", "sidhistory", "logonhours", "thumbnailphoto", "usercertificate"];

/// One directory object: its DN and attribute values (names lower-cased).
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub dn: String,
    pub attrs: HashMap<String, Vec<String>>,
}

impl Entry {
    pub fn first(&self, attr: &str) -> Option<String> {
        self.attrs.get(&attr.to_lowercase()).and_then(|v| v.first().cloned()).filter(|v| !v.is_empty())
    }

    pub fn all(&self, attr: &str) -> Vec<String> {
        self.attrs.get(&attr.to_lowercase()).cloned().unwrap_or_default()
    }
}

/// Where a search starts.
pub enum Base<'a> {
    /// The OUs in the AD settings (or the whole domain when none are set).
    Configured,
    /// The whole domain.
    Root,
    /// A container under the domain root, e.g. "CN=Policies,CN=System".
    UnderRoot(&'a str),
}

/// Common name of a DN: "CN=Web Servers,OU=Groups,DC=x" -> "Web Servers".
pub fn cn_of(dn: &str) -> String {
    let first = dn.split(',').next().unwrap_or(dn);
    first.split_once('=').map(|(_, v)| v.trim().to_string()).unwrap_or_else(|| first.to_string())
}

fn make(e: Entry) -> AdComputer {
    let cn = e.first("cn").unwrap_or_default();
    let dns = e.first("dNSHostName").unwrap_or_default();
    let uac: i64 = e.first("userAccountControl").and_then(|v| v.parse().ok()).unwrap_or(0);
    let last_logon = e
        .first("lastLogonTimestamp")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .and_then(filetime_to_rfc3339);
    let ou = e.dn.split_once(',').map(|(_, rest)| rest.to_string()).unwrap_or_default();
    let mut ad_groups: Vec<String> = e.all("memberOf").iter().map(|g| cn_of(g)).collect();
    ad_groups.sort_by_key(|g| g.to_lowercase());
    AdComputer {
        name: if dns.is_empty() { cn.clone() } else { dns },
        cn,
        ou,
        os: e.first("operatingSystem").unwrap_or_default(),
        os_version: e.first("operatingSystemVersion").unwrap_or_default(),
        description: e.first("description").unwrap_or_default(),
        last_logon,
        enabled: uac & ACCOUNT_DISABLED == 0,
        exists: false,
        ad_groups,
        dn: e.dn,
    }
}

/// Windows FILETIME (100 ns ticks since 1601) -> RFC 3339.
fn filetime_to_rfc3339(ft: i64) -> Option<String> {
    let secs = ft / 10_000_000 - 11_644_473_600;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// First OU name in a container path: "OU=Web,OU=Servers,DC=x" -> "web".
pub fn ou_tag(ou: &str) -> Option<String> {
    let first = ou.split(',').next()?.trim();
    let (k, v) = first.split_once('=')?;
    if !k.eq_ignore_ascii_case("OU") {
        return None;
    }
    Some(v.trim().to_lowercase().replace(' ', "-"))
}

/// Query AD and return the matching computers (nothing is imported).
pub async fn fetch(app: &App, s: &AdSettings) -> Result<Vec<AdComputer>> {
    let mut entries = search(app, s, Base::Configured, &ldap_filter(s), ATTRS).await?;
    // An OU and one below it can both be configured; keep each computer once.
    let mut seen = std::collections::HashSet::new();
    entries.retain(|e| seen.insert(e.dn.to_lowercase()));
    let mut list: Vec<AdComputer> = entries.into_iter().map(make).collect();
    if s.stale_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(s.stale_days as i64);
        list.retain(|c| {
            c.last_logon
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|t| t > cutoff)
        });
    }
    let existing: std::collections::HashSet<String> =
        app.db.list_computers()?.into_iter().map(|c| c.name.to_lowercase()).collect();
    for c in &mut list {
        c.exists = existing.contains(&c.name.to_lowercase());
    }
    list.sort_by_key(|a| a.name.to_lowercase());
    Ok(list)
}

/// One OU or container in the OU browser.
#[derive(Debug, Clone, Serialize)]
pub struct OuNode {
    pub dn: String,
    pub name: String,
    /// DN of the node above, `None` for the domain.
    pub parent: Option<String>,
    /// "domain", "ou" or "container" (e.g. CN=Computers).
    pub kind: String,
    pub description: String,
    /// Computers directly in this OU that the import settings would include.
    pub computers: usize,
    /// The same, counting every OU below too.
    pub total: usize,
    /// Computers in the computer list that came from this OU or below.
    pub known: usize,
}

/// The whole domain's OU tree, with computer counts. Only searches.
pub async fn ous(app: &App, s: &AdSettings) -> Result<Vec<OuNode>> {
    let ous = search(app, s, Base::Root, "(objectCategory=organizationalUnit)", &["name", "description"]).await?;
    let computers = search(app, s, Base::Root, &ldap_filter(s), &["cn"]).await?;
    let mut known: HashMap<String, usize> = HashMap::new();
    for c in app.db.list_computers()? {
        if let Some(ou) = c.ad_ou.filter(|o| !o.is_empty()) {
            *known.entry(ou.to_lowercase()).or_default() += 1;
        }
    }
    Ok(ou_tree(ous, computers.iter().map(|e| e.dn.as_str()), &known))
}

/// The DN one level up: "CN=a,OU=b,DC=x" -> "OU=b,DC=x". An escaped `\,`
/// stays part of its RDN.
fn parent_dn(dn: &str) -> Option<&str> {
    let mut escaped = false;
    for (i, ch) in dn.char_indices() {
        match ch {
            '\\' if !escaped => escaped = true,
            ',' if !escaped => return Some(dn[i + 1..].trim_start()).filter(|p| !p.is_empty()),
            _ => escaped = false,
        }
    }
    None
}

fn is_domain_root(dn: &str) -> bool {
    dn.trim_start().get(..3).is_some_and(|p| p.eq_ignore_ascii_case("DC="))
}

/// Build the tree from OU entries and computer DNs. Containers that hold
/// computers but are not OUs (CN=Computers) are added, as is the domain.
fn ou_tree<'a>(ous: Vec<Entry>, computer_dns: impl IntoIterator<Item = &'a str>, known: &HashMap<String, usize>) -> Vec<OuNode> {
    fn ensure(nodes: &mut HashMap<String, OuNode>, dn: &str) {
        if nodes.contains_key(&dn.to_lowercase()) {
            return;
        }
        let (kind, name) = if is_domain_root(dn) {
            let name = dn.split(',').filter_map(|r| r.split_once('=')).map(|(_, v)| v.trim()).collect::<Vec<_>>().join(".");
            ("domain", name)
        } else if dn.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("OU=")) {
            ("ou", cn_of(dn))
        } else {
            ("container", cn_of(dn))
        };
        let parent = if kind == "domain" { None } else { parent_dn(dn) };
        nodes.insert(
            dn.to_lowercase(),
            OuNode {
                dn: dn.to_string(),
                name,
                parent: parent.map(str::to_string),
                kind: kind.into(),
                description: String::new(),
                computers: 0,
                total: 0,
                known: 0,
            },
        );
        if let Some(p) = parent {
            ensure(nodes, p);
        }
    }
    /// Add `n` to the node at `key` and every node above it.
    fn add_up(nodes: &mut HashMap<String, OuNode>, key: &str, n: usize, field: fn(&mut OuNode) -> &mut usize) {
        let mut key = Some(key.to_string());
        while let Some(k) = key {
            let Some(node) = nodes.get_mut(&k) else { break };
            *field(node) += n;
            key = node.parent.as_ref().map(|p| p.to_lowercase());
        }
    }

    let mut nodes: HashMap<String, OuNode> = HashMap::new();
    for e in &ous {
        ensure(&mut nodes, &e.dn);
        let node = nodes.get_mut(&e.dn.to_lowercase()).expect("just added");
        node.name = e.first("name").unwrap_or_else(|| cn_of(&e.dn));
        node.description = e.first("description").unwrap_or_default();
    }
    for dn in computer_dns {
        let Some(parent) = parent_dn(dn) else { continue };
        ensure(&mut nodes, parent);
        nodes.get_mut(&parent.to_lowercase()).expect("just added").computers += 1;
        add_up(&mut nodes, &parent.to_lowercase(), 1, |n| &mut n.total);
    }
    for (ou, n) in known {
        add_up(&mut nodes, ou, *n, |n| &mut n.known);
    }
    // Parents keep the spelling of the parent's own DN.
    let spelled: HashMap<String, String> = nodes.iter().map(|(k, n)| (k.clone(), n.dn.clone())).collect();
    let mut list: Vec<OuNode> = nodes.into_values().collect();
    for n in &mut list {
        if let Some(p) = n.parent.as_ref().and_then(|p| spelled.get(&p.to_lowercase())) {
            n.parent = Some(p.clone());
        }
    }
    list.sort_by_key(|n| (n.name.to_lowercase(), n.dn.to_lowercase()));
    list
}

/// Read-only LDAP search in the configured mode. Only search operations
/// exist in this module; nothing can modify the directory.
pub async fn search(app: &App, s: &AdSettings, base: Base<'_>, filter: &str, attrs: &[&str]) -> Result<Vec<Entry>> {
    match s.mode.as_str() {
        "windows" => search_adsi(s, base, filter, attrs).await,
        "ldap" => search_ldap(app, s, base, filter, attrs).await,
        m => bail!("unknown AD mode '{m}'"),
    }
}

async fn search_ldap(app: &App, s: &AdSettings, base: Base<'_>, filter: &str, attrs: &[&str]) -> Result<Vec<Entry>> {
    if s.server.trim().is_empty() {
        bail!("set the domain controller / domain name for LDAP mode");
    }
    let cred_id = s.credential_id.ok_or_else(|| anyhow!("choose an LDAP credential for AD"))?;
    let cred = app.db.get_credential(cred_id)?.ok_or_else(|| anyhow!("AD credential no longer exists"))?;
    let password = app.db.credential_secret(cred_id)?.unwrap_or_default();

    let server = s.server.trim();
    let url = if server.starts_with("ldap://") || server.starts_with("ldaps://") {
        server.to_string()
    } else if s.security == "ldaps" {
        format!("ldaps://{server}:636")
    } else {
        format!("ldap://{server}:389")
    };
    // A simple bind sends the password as is: never over an unencrypted connection.
    if url.starts_with("ldap://") && s.security != "starttls" {
        bail!("refusing to send the AD password unencrypted: choose LDAPS or StartTLS, or use Windows mode (no password stored)");
    }
    let settings = LdapConnSettings::new()
        .set_conn_timeout(Duration::from_secs(15))
        .set_starttls(s.security == "starttls")
        .set_no_tls_verify(s.skip_tls_verify);
    let (conn, mut ldap) = LdapConnAsync::with_settings(settings, &url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    ldap3::drive!(conn);
    ldap.simple_bind(&cred.username, &password)
        .await?
        .success()
        .map_err(|e| anyhow!("AD bind as {} failed: {e}", cred.username))?;

    let configured: Vec<String> = s.base_dns.iter().filter(|b| !b.trim().is_empty()).cloned().collect();
    let bases = if matches!(base, Base::Configured) && !configured.is_empty() {
        configured
    } else {
        let (rs, _) = ldap
            .search("", Scope::Base, "(objectClass=*)", vec!["defaultNamingContext"])
            .await?
            .success()?;
        let root = rs
            .into_iter()
            .next()
            .map(SearchEntry::construct)
            .and_then(|e| e.attrs.get("defaultNamingContext").and_then(|v| v.first().cloned()))
            .ok_or_else(|| anyhow!("could not read defaultNamingContext from rootDSE"))?;
        match base {
            Base::UnderRoot(rel) => vec![format!("{rel},{root}")],
            _ => vec![root],
        }
    };

    let mut out = Vec::new();
    for base in bases {
        let adapters: Vec<Box<dyn Adapter<_, _>>> =
            vec![Box::new(EntriesOnly::new()), Box::new(PagedResults::new(500))];
        let mut stream = ldap
            .streaming_search_with(adapters, &base, Scope::Subtree, filter, attrs.to_vec())
            .await
            .with_context(|| format!("searching {base}"))?;
        while let Some(entry) = stream.next().await? {
            let e = SearchEntry::construct(entry);
            let mut attrs: HashMap<String, Vec<String>> = e
                .attrs
                .into_iter()
                .map(|(k, v)| {
                    let k = k.to_lowercase();
                    // A binary value that happens to be valid UTF-8 lands here;
                    // that conversion was lossless, so its bytes are intact.
                    let v = if BINARY_ATTRS.contains(&k.as_str()) { v.iter().map(|s| B64.encode(s.as_bytes())).collect() } else { v };
                    (k, v)
                })
                .collect();
            for (k, v) in e.bin_attrs {
                attrs.entry(k.to_lowercase()).or_default().extend(v.iter().map(|b| B64.encode(b)));
            }
            out.push(Entry { dn: e.dn, attrs });
        }
        stream.finish().await.success().with_context(|| format!("searching {base}"))?;
    }
    let _ = ldap.unbind().await;
    Ok(out)
}

#[derive(Deserialize)]
struct AdsiRow {
    dn: String,
    #[serde(flatten)]
    attrs: HashMap<String, serde_json::Value>,
}

async fn search_adsi(s: &AdSettings, base: Base<'_>, filter: &str, attrs: &[&str]) -> Result<Vec<Entry>> {
    if !cfg!(windows) {
        bail!("AD mode 'windows' needs the console on a domain-joined Windows machine; use 'ldap'");
    }
    let server = s.server.trim().trim_start_matches("ldap://").trim_start_matches("ldaps://");
    let prefix = if server.is_empty() { "LDAP://".to_string() } else { format!("LDAP://{server}/") };
    let root = format!("([adsi]{}).defaultNamingContext", ps_quote(&format!("{prefix}RootDSE")));
    let configured: Vec<String> = s.base_dns.iter().filter(|b| !b.trim().is_empty()).map(|b| format!("{prefix}{b}")).collect();
    let roots = match base {
        Base::Configured if !configured.is_empty() => {
            format!("@({})", configured.iter().map(|b| ps_quote(b)).collect::<Vec<_>>().join(","))
        }
        Base::UnderRoot(rel) => format!("@({}+{}+','+{root})", ps_quote(&prefix), ps_quote(rel)),
        _ => format!("@({}+{root})", ps_quote(&prefix)),
    };
    let props = attrs.iter().map(|a| ps_quote(a)).collect::<Vec<_>>().join(",");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
trap {{ [Console]::Error.WriteLine($_.Exception.InnerException.Message + ' ' + $_.Exception.Message); exit 1 }}
$props = @({props})
$rows = foreach ($root in {roots}) {{
    $searcher = New-Object DirectoryServices.DirectorySearcher([adsi]$root, {filter})
    $searcher.PageSize = 500
    foreach ($p in $props + 'distinguishedName') {{ [void]$searcher.PropertiesToLoad.Add($p) }}
    foreach ($r in $searcher.FindAll()) {{
        $o = @{{ dn = [string]$r.Properties['distinguishedname'][0] }}
        foreach ($p in $props) {{
            $vals = @($r.Properties[$p.ToLower()] | ForEach-Object {{
                if ($_ -is [byte[]]) {{ [Convert]::ToBase64String($_) }}
                elseif ($_ -is [datetime]) {{ $_.ToUniversalTime().ToString('yyyyMMddHHmmss.0Z') }} else {{ [string]$_ }}
            }})
            # Leave out empty attributes: a whole-domain backup asks for many.
            if ($vals.Count) {{ $o[$p.ToLower()] = $vals }}
        }}
        [pscustomobject]$o
    }}
}}
ConvertTo-Json -InputObject @($rows) -Compress -Depth 3
"#,
        filter = ps_quote(filter),
    );
    // A whole-domain read for a backup can take a while in a large domain.
    let stdout = run_ps_timeout(&script, &[], 1800).await.map_err(|e| {
        let err = format!("{e:#}");
        let hint = if err.contains("0x80005000") || err.contains("0x8007054B") || err.contains("domain") {
            " — is this PC joined to the domain? If not, enter the domain controller and use LDAP mode."
        } else {
            ""
        };
        anyhow!("AD query failed: {err}{hint}")
    })?;
    let json = stdout.trim().trim_start_matches('\u{feff}');
    let rows: Vec<AdsiRow> = serde_json::from_str(json)
        .with_context(|| format!("unexpected output from AD query: {}", json.chars().take(300).collect::<String>()))?;
    Ok(rows
        .into_iter()
        .map(|r| Entry {
            dn: r.dn,
            attrs: r.attrs.into_iter().map(|(k, v)| (k.to_lowercase(), json_strings(v))).collect(),
        })
        .collect())
}

fn json_strings(v: serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Array(a) => a.into_iter().flat_map(json_strings).collect(),
        serde_json::Value::String(s) => vec![s],
        serde_json::Value::Null => vec![],
        other => vec![other.to_string()],
    }
}

/// Run a short PowerShell script on the console and return its stdout. A
/// non-zero exit becomes an error with the (cleaned) stderr text.
pub(crate) async fn run_ps(script: &str, env: &[(&str, &str)]) -> Result<String> {
    run_ps_timeout(script, env, 300).await
}

/// [`run_ps`] with a timeout in seconds.
pub(crate) async fn run_ps_timeout(script: &str, env: &[(&str, &str)], secs: u64) -> Result<String> {
    let mut cmd = tokio::process::Command::new("powershell.exe");
    // Started from PowerShell 7, 5.1 would inherit its module path and fail to load its own modules.
    cmd.env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-OutputFormat", "Text", "-EncodedCommand"])
        .arg(encode_command(script))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.spawn().context("starting powershell.exe")?.wait_with_output();
    let out = tokio::time::timeout(Duration::from_secs(secs), out)
        .await
        .map_err(|_| anyhow!("PowerShell timed out"))??;
    if !out.status.success() {
        bail!("{}", strip_clixml(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Remove the CLIXML progress/error records PowerShell writes to stderr.
fn strip_clixml(s: &str) -> String {
    let mut out = s.replace("#< CLIXML", "");
    while let (Some(a), Some(b)) = (out.find("<Objs"), out.find("</Objs>")) {
        if b < a {
            break;
        }
        out.replace_range(a..b + "</Objs>".len(), "");
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Base64 of the UTF-16LE script, as `powershell -EncodedCommand` expects.
fn encode_command(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    B64.encode(utf16)
}

/// Turn AD entries into computers (insert new, refresh existing).
pub fn import(app: &App, s: &AdSettings, list: &[AdComputer]) -> Result<usize> {
    let mut n = 0;
    for c in list {
        if !crate::db::valid_computer_name(&c.name) {
            tracing::warn!("skipping AD computer with an invalid name: {:?}", c.name);
            continue;
        }
        let os_lower = c.os.to_lowercase();
        let os = if os_lower.contains("windows") {
            Some("windows".to_string())
        } else if !os_lower.is_empty() {
            Some("linux".to_string())
        } else {
            None
        };
        let tags = if s.tag_with_ou {
            let mut tags = app.db.get_computer(&c.name)?.map(|e| e.tags).unwrap_or_default();
            tags.extend(ou_tag(&c.ou));
            Some(tags)
        } else {
            None
        };
        let detail = format!("{} {}", c.os, c.os_version).trim().to_string();
        app.db.upsert_computer(&NewComputer {
            name: c.name.clone(),
            os,
            os_detail: Some(detail).filter(|d| !d.is_empty()),
            source: Some("ad".into()),
            ad_ou: Some(c.ou.clone()),
            description: Some(c.description.clone()).filter(|d| !d.is_empty()),
            tags,
            last_logon: c.last_logon.clone(),
            ad_groups: Some(c.ad_groups.clone()),
            ..Default::default()
        })?;
        n += 1;
    }
    Ok(n)
}

/// Fetch and import everything (CLI and auto-sync).
pub async fn sync(app: &App) -> Result<usize> {
    let mut s: AdSettings = app.db.get_setting("ad")?;
    let list = fetch(app, &s).await?;
    let n = import(app, &s, &list)?;
    s.last_sync = Some(now());
    app.db.set_setting("ad", &s)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_tags() {
        let mut s = AdSettings::default();
        assert_eq!(
            ldap_filter(&s),
            "(&(objectCategory=computer)(!(userAccountControl:1.2.840.113556.1.4.803:=2)))"
        );
        s.include_disabled = true;
        s.filter = "operatingSystem=*Server*".into();
        assert_eq!(ldap_filter(&s), "(&(objectCategory=computer)(operatingSystem=*Server*))");
        assert_eq!(ou_tag("OU=Web Servers,OU=HQ,DC=corp,DC=local").as_deref(), Some("web-servers"));
        assert_eq!(ou_tag("CN=Computers,DC=corp,DC=local"), None);
        // 2024-01-01T00:00:00Z
        assert_eq!(filetime_to_rfc3339(133_485_408_000_000_000).as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn builds_ou_tree() {
        assert_eq!(parent_dn("CN=web01,OU=Web,DC=corp,DC=local"), Some("OU=Web,DC=corp,DC=local"));
        assert_eq!(parent_dn(r"OU=Smith\, John,OU=Users,DC=x"), Some("OU=Users,DC=x"));
        assert_eq!(parent_dn("DC=local"), None);

        let ou = |dn: &str, name: &str| Entry {
            dn: dn.into(),
            attrs: HashMap::from([("name".to_string(), vec![name.to_string()])]),
        };
        let ous = vec![
            ou("OU=Servers,DC=corp,DC=local", "Servers"),
            ou("OU=Web,OU=Servers,DC=corp,DC=local", "Web"),
            ou("OU=Empty,DC=corp,DC=local", "Empty"),
        ];
        let computers = [
            "CN=web01,OU=Web,OU=Servers,DC=corp,DC=local",
            "CN=web02,ou=web,ou=servers,dc=corp,dc=local",
            "CN=db01,OU=Servers,DC=corp,DC=local",
            "CN=pc01,CN=Computers,DC=corp,DC=local",
        ];
        let known = HashMap::from([("ou=web,ou=servers,dc=corp,dc=local".to_string(), 1)]);
        let tree = ou_tree(ous, computers, &known);
        let get = |name: &str| tree.iter().find(|n| n.name == name).unwrap();

        let domain = get("corp.local");
        assert_eq!((domain.kind.as_str(), domain.parent.as_deref(), domain.total, domain.known), ("domain", None, 4, 1));
        let servers = get("Servers");
        assert_eq!((servers.computers, servers.total, servers.known), (1, 3, 1));
        let web = get("Web");
        assert_eq!((web.computers, web.total, web.parent.as_deref()), (2, 2, Some("OU=Servers,DC=corp,DC=local")));
        let cn = get("Computers");
        assert_eq!((cn.kind.as_str(), cn.total), ("container", 1));
        assert_eq!(get("Empty").total, 0);
        assert_eq!(tree.len(), 5);
    }
}
