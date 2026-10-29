//! Inventory: agentless scans that collect OS and hardware facts, disks and
//! installed software from each computer over WinRM/SSH. The results feed the
//! Inventory page, software reports and dynamic group rules.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info};

use crate::app::{App, WinRmSettings};
use crate::db::{Computer, Disk, InstalledApp, Scan, now};
use crate::engine::{Target, build_transport};
use crate::package::{Os, Shell};
use crate::transport::{LogFn, default_port, detect_os, port_open, wrap_script};

pub const SCAN_PS: &str = include_str!("assets/inventory.ps1");
pub const SCAN_SH: &str = include_str!("assets/inventory.sh");

/// Every output line of the scan scripts that carries data starts with this.
const MARK: &str = "@@inv\t";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySettings {
    /// Scan all computers every N hours (0 = only when started by hand).
    #[serde(default)]
    pub interval_hours: u32,
    /// Computers scanned at the same time.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Re-scan computers after a deployment succeeds on them (only computers
    /// that were scanned before).
    #[serde(default = "yes")]
    pub rescan_after_deploy: bool,
    /// When the last full scan started.
    #[serde(default)]
    pub last_scan: Option<String>,
}

fn default_concurrency() -> usize {
    16
}
fn yes() -> bool {
    true
}

impl Default for InventorySettings {
    fn default() -> Self {
        InventorySettings { interval_hours: 0, concurrency: 16, rescan_after_deploy: true, last_scan: None }
    }
}

/// Progress of the scan started from the console or the scheduler.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanState {
    pub running: bool,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// Outcome of scanning one computer.
#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub computer: String,
    /// "ok", "failed" or "offline"
    pub status: String,
    pub error: Option<String>,
}

/// Parse the scan script output. `unescape` decodes the `\uXXXX` escapes the
/// Windows script uses for non-ASCII text.
pub fn parse_output(lines: &[String], unescape: bool) -> Result<Scan> {
    let mut scan = Scan::default();
    let mut done = false;
    for line in lines {
        let Some(rest) = line.strip_prefix(MARK) else { continue };
        let f: Vec<String> = rest
            .split('\t')
            .map(|s| if unescape { decode_escapes(s.trim()) } else { s.trim().to_string() })
            .collect();
        let get = |i: usize| f.get(i).cloned().unwrap_or_default();
        match f[0].as_str() {
            "fact" if !get(1).is_empty() => {
                scan.facts.insert(get(1), get(2));
            }
            "app" if !get(1).is_empty() => scan.apps.push(InstalledApp { name: get(1), version: get(2), publisher: get(3) }),
            "disk" if !get(1).is_empty() => scan.disks.push(Disk {
                mount: get(1),
                size: get(2).parse().unwrap_or(0),
                free: get(3).parse().unwrap_or(0),
                fs: get(4),
            }),
            "done" => done = true,
            _ => {}
        }
    }
    if !done {
        bail!("the scan did not finish (no end marker in its output)");
    }
    Ok(scan)
}

/// Turn `\u00e9` sequences back into characters.
pub(crate) fn decode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\\u") {
        out.push_str(&rest[..i]);
        let hex = rest.get(i + 2..i + 6);
        match hex.and_then(|h| u32::from_str_radix(h, 16).ok()) {
            Some(code) => {
                // UTF-16 surrogate pairs come as two escapes.
                let mut consumed = 6;
                let ch = if (0xD800..0xDC00).contains(&code) {
                    let low = rest
                        .get(i + 6..i + 12)
                        .filter(|l| l.starts_with("\\u"))
                        .and_then(|l| u32::from_str_radix(&l[2..], 16).ok())
                        .filter(|l| (0xDC00..0xE000).contains(l));
                    match low {
                        Some(low) => {
                            consumed = 12;
                            char::from_u32(0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00))
                        }
                        None => None,
                    }
                } else {
                    char::from_u32(code)
                };
                out.push(ch.unwrap_or('\u{FFFD}'));
                rest = &rest[i + consumed..];
            }
            None => {
                out.push_str("\\u");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Facts whose changes go to the change history. Others (logged-on user,
/// last boot, IP addresses, pending reboot) change all the time.
const TRACKED_FACTS: &[&str] = &[
    "os_name", "os_version", "os_build", "kernel", "arch", "manufacturer", "model", "serial", "cpu", "cpu_count",
    "ram_mb", "domain",
];

/// One difference between two scans of a computer.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// "installed", "removed", "updated" or "changed" (a fact)
    pub kind: &'static str,
    pub name: String,
    pub old: String,
    pub new: String,
}

/// What changed from the previous scan (`old_facts`, `old_apps`) to `new`.
pub fn changes(old_facts: &BTreeMap<String, String>, old_apps: &[InstalledApp], new: &Scan) -> Vec<Change> {
    let mut out = Vec::new();
    for key in TRACKED_FACTS {
        // A value that is missing in one scan is usually a query that failed, not a change.
        if let (Some(o), Some(n)) = (old_facts.get(*key), new.facts.get(*key))
            && !o.is_empty()
            && !n.is_empty()
            && o != n
        {
            out.push(Change { kind: "changed", name: key.to_string(), old: o.clone(), new: n.clone() });
        }
    }
    // A scan that suddenly lists no software at all failed to read it.
    if new.apps.is_empty() && !old_apps.is_empty() {
        return out;
    }
    // Program name (case-insensitive) -> (shown name, versions).
    let index = |apps: &[InstalledApp]| {
        let mut m: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
        for a in apps {
            m.entry(a.name.to_lowercase()).or_insert_with(|| (a.name.clone(), BTreeSet::new())).1.insert(a.version.clone());
        }
        m
    };
    let (old, new) = (index(old_apps), index(&new.apps));
    let none = (String::new(), BTreeSet::new());
    for key in old.keys().chain(new.keys().filter(|k| !old.contains_key(*k))) {
        let (o, n) = (old.get(key).unwrap_or(&none), new.get(key).unwrap_or(&none));
        let name = if n.0.is_empty() { &o.0 } else { &n.0 };
        let gone: Vec<&String> = o.1.difference(&n.1).collect();
        let added: Vec<&String> = n.1.difference(&o.1).collect();
        if gone.len() == 1 && added.len() == 1 {
            out.push(Change { kind: "updated", name: name.clone(), old: gone[0].clone(), new: added[0].clone() });
            continue;
        }
        for v in gone {
            out.push(Change { kind: "removed", name: name.clone(), old: v.clone(), new: String::new() });
        }
        for v in added {
            out.push(Change { kind: "installed", name: name.clone(), old: String::new(), new: v.clone() });
        }
    }
    out
}

pub(crate) fn target_of(c: &Computer) -> Target {
    Target {
        name: c.name.clone(),
        host: c.connect_host().to_string(),
        os: Os::parse(&c.os),
        credential_id: c.credential_id,
        port: c.port,
        in_db: true,
    }
}

/// Scan one computer and store the result.
async fn scan_one(app: &App, c: &Computer, winrm: &WinRmSettings) -> ScanResult {
    let (status, error) = match collect(app, c, winrm).await {
        Ok(Some(scan)) => match app.db.save_inventory(c.id, &scan) {
            Ok(()) => ("ok", None),
            Err(e) => ("failed", Some(format!("saving inventory: {e:#}"))),
        },
        Ok(None) => ("offline", Some("WinRM/SSH port not reachable".to_string())),
        Err(e) => ("failed", Some(format!("{e:#}"))),
    };
    if let Some(e) = &error {
        let _ = app.db.inventory_failed(c.id, status, e);
        if status == "offline" {
            let _ = app.db.set_online(&c.name, false);
        }
    }
    ScanResult { computer: c.name.clone(), status: status.into(), error }
}

/// Run the scan script on a computer. `None` = not reachable.
async fn collect(app: &App, c: &Computer, winrm: &WinRmSettings) -> Result<Option<Scan>> {
    let t = target_of(c);
    let os = match t.os {
        Some(os) => os,
        None => match detect_os(&t.host, winrm.use_ssl).await {
            Some(os) => {
                app.db.set_computer_os(&t.name, os.as_str())?;
                os
            }
            None => return Ok(None),
        },
    };
    let port = t.port.unwrap_or(default_port(os, winrm.use_ssl));
    if !port_open(&t.host, port, Duration::from_secs(5)).await {
        return Ok(None);
    }
    let transport = build_transport(app, &t, os, port, winrm)?;
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let log: LogFn = Arc::new(move |l: &str| {
        let mut v = sink.lock().unwrap();
        if v.len() < 200_000 {
            v.push(l.to_string());
        }
    });
    let shell = Shell::default_for(os);
    let body = if os == Os::Windows { SCAN_PS } else { SCAN_SH };
    let code = transport.run(&wrap_script(shell, body), shell, Duration::from_secs(600), &log).await?;
    let lines = std::mem::take(&mut *lines.lock().unwrap());
    parse_output(&lines, os == Os::Windows).map(Some).map_err(|e| {
        // Show what the computer said instead (e.g. "sudo: a password is required").
        let said: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with(MARK)).collect();
        let tail = said[said.len().saturating_sub(3)..].join(" | ");
        if tail.is_empty() { anyhow!("{e} (exit code {code})") } else { anyhow!("exit code {code}: {tail}") }
    })
}

/// Computers to scan: the named ones (all when `None`).
fn select(app: &App, names: Option<&[String]>) -> Result<Vec<Computer>> {
    let mut all = app.db.list_computers()?;
    if let Some(names) = names {
        let set: HashSet<String> = names.iter().map(|n| n.trim().to_ascii_lowercase()).collect();
        all.retain(|c| set.contains(&c.name.to_ascii_lowercase()));
    }
    Ok(all)
}

/// Scan computers and wait for the results. `track` reports progress in the
/// console's scan state.
pub async fn run(app: &App, computers: Vec<Computer>, track: bool) -> Vec<ScanResult> {
    let winrm = app.winrm_settings();
    let settings: InventorySettings = app.db.get_setting("inventory").unwrap_or_default();
    let results: Vec<ScanResult> = futures::stream::iter(computers)
        .map(|c| {
            let app = app.clone();
            let winrm = winrm.clone();
            async move {
                let r = scan_one(&app, &c, &winrm).await;
                if track {
                    let mut st = app.scan.lock().unwrap();
                    st.done += 1;
                    if r.status != "ok" {
                        st.failed += 1;
                    }
                }
                r
            }
        })
        .buffer_unordered(settings.concurrency.clamp(1, 128))
        .collect()
        .await;
    if track {
        let mut st = app.scan.lock().unwrap();
        st.running = false;
        st.finished_at = Some(now());
    }
    results
}

/// Start a scan in the background (all computers when `names` is `None`).
/// Returns how many computers it covers.
pub fn start(app: &App, names: Option<Vec<String>>) -> Result<usize> {
    let computers = select(app, names.as_deref())?;
    if computers.is_empty() {
        bail!("no computers to scan");
    }
    let n = computers.len();
    {
        let mut st = app.scan.lock().unwrap();
        if st.running {
            bail!("an inventory scan is already running");
        }
        *st = ScanState { running: true, total: n, started_at: Some(now()), ..Default::default() };
    }
    if names.is_none() {
        let mut s: InventorySettings = app.db.get_setting("inventory")?;
        s.last_scan = Some(now());
        app.db.set_setting("inventory", &s)?;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let results = run(&app, computers, true).await;
        let bad = results.iter().filter(|r| r.status != "ok").count();
        info!("inventory scan: {} computers, {bad} not scanned", results.len());
    });
    Ok(n)
}

/// Called by the scheduler: start the periodic full scan when it is due.
pub fn tick(app: &App) -> Result<()> {
    let s: InventorySettings = app.db.get_setting("inventory")?;
    if s.interval_hours == 0 || app.scan.lock().unwrap().running {
        return Ok(());
    }
    let last = s.last_scan.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
    if last.is_some_and(|t| Utc::now() - t < ChronoDuration::hours(s.interval_hours as i64)) {
        return Ok(());
    }
    match start(app, None) {
        Ok(n) => info!("scheduled inventory scan of {n} computers"),
        Err(e) => {
            error!("scheduled inventory scan: {e:#}");
            // Don't retry every 20 seconds.
            let mut s = s;
            s.last_scan = Some(now());
            app.db.set_setting("inventory", &s)?;
        }
    }
    Ok(())
}

/// After a deployment, re-scan the computers it succeeded on, so software
/// rules and reports see the change. Only computers scanned before are
/// included, so nobody gets scanned without having opted in.
pub fn after_deployment(app: &App, succeeded: Vec<String>) {
    let run_it = || -> Result<Vec<Computer>> {
        let s: InventorySettings = app.db.get_setting("inventory")?;
        if !s.rescan_after_deploy || succeeded.is_empty() {
            return Ok(vec![]);
        }
        let scanned: HashSet<i64> = app.db.list_inventory(false)?.into_iter().map(|i| i.computer_id).collect();
        Ok(select(app, Some(&succeeded))?.into_iter().filter(|c| scanned.contains(&c.id)).collect())
    };
    match run_it() {
        Ok(list) if !list.is_empty() => {
            let app = app.clone();
            tokio::spawn(async move {
                run(&app, list, false).await;
            });
        }
        Ok(_) => {}
        Err(e) => error!("re-scan after deployment: {e:#}"),
    }
}

/// Scan named computers (all when empty) and wait; used by the CLI.
pub async fn scan_now(app: &App, names: &[String]) -> Result<Vec<ScanResult>> {
    let computers = select(app, (!names.is_empty()).then_some(names)).context("loading computers")?;
    if computers.is_empty() {
        bail!("no matching computers in the computer list");
    }
    Ok(run(app, computers, false).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn parses_scan_output() {
        let out = lines(
            "noise before\n@@inv\tfact\tos_name\tWindows Server 2022 Datacenter\n@@inv\tfact\tram_mb\t16384\n\
             @@inv\tdisk\tC:\t127434190848\t52428800000\tNTFS\n@@inv\tapp\tGoogle Chrome\t130.0.6723.92\tGoogle LLC\n\
             @@inv\tapp\tCaf\\u00e9 Tool \\u005c x\t1.0\t\n@@inv\tdone",
        );
        let s = parse_output(&out, true).unwrap();
        assert_eq!(s.facts["os_name"], "Windows Server 2022 Datacenter");
        assert_eq!(s.facts["ram_mb"], "16384");
        assert_eq!(s.disks[0].mount, "C:");
        assert_eq!(s.disks[0].free, 52_428_800_000);
        assert_eq!(s.apps.len(), 2);
        assert_eq!(s.apps[0].publisher, "Google LLC");
        assert_eq!(s.apps[1].name, "Café Tool \\ x");
        assert!(parse_output(&lines("@@inv\tfact\tos_name\tx"), false).is_err(), "no done marker");
    }

    fn app(name: &str, version: &str) -> InstalledApp {
        InstalledApp { name: name.into(), version: version.into(), publisher: String::new() }
    }

    #[test]
    fn finds_changes_between_scans() {
        let old_facts: BTreeMap<String, String> =
            [("os_build", "22631.4317"), ("ram_mb", "16384"), ("user", "CORP\\bob"), ("serial", "ABC")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let old_apps = vec![
            app("Google Chrome", "129.0.1"),
            app("7-Zip 24.08 (x64)", "24.08"),
            app("Microsoft Visual C++ 2015-2022 Redistributable", "14.38"),
            app("Microsoft Visual C++ 2015-2022 Redistributable", "14.36"),
        ];
        let mut new = Scan::default();
        for (k, v) in [("os_build", "22631.4460"), ("ram_mb", "16384"), ("user", "CORP\\alice")] {
            new.facts.insert(k.into(), v.into());
        }
        new.apps = vec![
            app("google chrome", "130.0.2"),
            app("Microsoft Visual C++ 2015-2022 Redistributable", "14.40"),
            app("Notepad++ (64-bit x64)", "8.7"),
        ];
        let got: Vec<(&str, String, String, String)> =
            changes(&old_facts, &old_apps, &new).into_iter().map(|c| (c.kind, c.name, c.old, c.new)).collect();
        let want = |k: &'static str, n: &str, o: &str, v: &str| (k, n.to_string(), o.to_string(), v.to_string());
        assert_eq!(
            got,
            vec![
                // The user changed too, but is not tracked; the serial is missing, not changed.
                want("changed", "os_build", "22631.4317", "22631.4460"),
                want("removed", "7-Zip 24.08 (x64)", "24.08", ""),
                want("updated", "google chrome", "129.0.1", "130.0.2"),
                want("removed", "Microsoft Visual C++ 2015-2022 Redistributable", "14.36", ""),
                want("removed", "Microsoft Visual C++ 2015-2022 Redistributable", "14.38", ""),
                want("installed", "Microsoft Visual C++ 2015-2022 Redistributable", "", "14.40"),
                want("installed", "Notepad++ (64-bit x64)", "", "8.7"),
            ]
        );
        // A scan without any software is a failed read, not "everything removed".
        new.apps.clear();
        assert_eq!(changes(&old_facts, &old_apps, &new).len(), 1);
    }

    #[test]
    fn decodes_escapes() {
        assert_eq!(decode_escapes("a\\u00e9b"), "aéb");
        assert_eq!(decode_escapes("\\ud83d\\ude00"), "😀");
        assert_eq!(decode_escapes("C:\\users"), "C:\\users");
        assert_eq!(decode_escapes("end\\u"), "end\\u");
    }

    /// Both scan scripts parse with the real PowerShell and bash parsers
    /// (skipped where the interpreter is missing).
    #[test]
    fn scan_scripts_parse() {
        let dir = std::env::temp_dir().join(format!("psccm-inv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ps = dir.join("inventory.ps1");
        let sh = dir.join("inventory.sh");
        std::fs::write(&ps, wrap_script(Shell::Powershell, SCAN_PS)).unwrap();
        std::fs::write(&sh, wrap_script(Shell::Bash, SCAN_SH)).unwrap();
        let mut errors = Vec::new();
        let bash = std::process::Command::new("bash")
            .args(["-c", "echo ok"])
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok");
        if bash {
            let out = std::process::Command::new("bash").arg("-n").arg(&sh).output().unwrap();
            if !out.status.success() {
                errors.push(String::from_utf8_lossy(&out.stderr).to_string());
            }
        }
        if cfg!(windows) {
            let cmd = format!(
                "$e = $null; [void][Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$e); foreach ($x in $e) {{ \"$($x.Extent.StartLineNumber): $($x.Message)\" }}",
                ps.display()
            );
            let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &cmd]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !text.is_empty() {
                errors.push(text);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(errors.is_empty(), "scan script syntax errors:\n{}", errors.join("\n"));
    }
}
