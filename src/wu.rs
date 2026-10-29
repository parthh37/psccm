//! Windows Update: agentless scans for missing updates on Windows computers,
//! and the `windows_update` package step that installs them. Both run the
//! Windows Update Agent as SYSTEM on the target, through a one-off scheduled
//! task (the agent refuses to download or install for a WinRM session).
//! Scans feed the Updates page and the "missing updates" group rule.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info};

use crate::app::{App, WinRmSettings};
use crate::db::{Computer, now};
use crate::engine::build_transport;
use crate::inventory::{ScanResult, ScanState, decode_escapes, target_of};
use crate::package::{Os, Shell, WindowsUpdateSpec, render};
use crate::transport::{LogFn, default_port, detect_os, port_open, ps_quote, wrap_script};

const CORE_PS: &str = include_str!("assets/windows-update.ps1");
const RUNNER_PS: &str = include_str!("assets/wu-runner.ps1");

/// Every output line of the scripts that carries data starts with this.
pub const MARK: &str = "@@wu\t";

/// Windows Update classifications, as the scripts name them.
pub const CLASSIFICATIONS: &[&str] = &[
    "Security Updates",
    "Critical Updates",
    "Definition Updates",
    "Update Rollups",
    "Updates",
    "Feature Packs",
    "Service Packs",
    "Tools",
    "Drivers",
    "Upgrades",
];

/// Installed when a step names no classifications.
pub const DEFAULT_CLASSES: &[&str] = &["Security Updates", "Critical Updates", "Definition Updates", "Update Rollups"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// Scan all Windows computers every N hours (0 = only when started by hand).
    #[serde(default)]
    pub interval_hours: u32,
    /// Computers scanned at the same time.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// "default" (WSUS when the computer has one, else Windows Update) or "windows-update".
    #[serde(default = "default_source")]
    pub source: String,
    /// Scan again after a deployment that installs updates.
    #[serde(default = "yes")]
    pub rescan_after_install: bool,
    /// When the last full scan started.
    #[serde(default)]
    pub last_scan: Option<String>,
    /// Updates (KB, or update ID when there is none) that are never installed
    /// by PSCCM and do not count as missing.
    #[serde(default)]
    pub declined: Vec<String>,
}

fn default_concurrency() -> usize {
    16
}
fn default_source() -> String {
    "default".into()
}
fn yes() -> bool {
    true
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings {
            interval_hours: 0,
            concurrency: 16,
            source: default_source(),
            rescan_after_install: true,
            last_scan: None,
            declined: vec![],
        }
    }
}

impl UpdateSettings {
    pub fn load(app: &App) -> UpdateSettings {
        app.db.get_setting("updates").unwrap_or_default()
    }

    fn declined_set(&self) -> HashSet<String> {
        self.declined.iter().map(|k| k.to_ascii_lowercase()).collect()
    }
}

/// An update a computer is missing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MissingUpdate {
    pub update_id: String,
    /// "KB5044284", or empty (drivers, some Defender updates)
    pub kb: String,
    pub title: String,
    pub classification: String,
    /// MSRC severity: Critical, Important, Moderate, Low or empty
    pub severity: String,
    /// Largest download size in bytes
    pub size: u64,
    /// Release date (YYYY-MM-DD)
    pub released: String,
    /// 0 = never needs a restart, 1 = always, 2 = may need one
    pub reboot: u8,
    pub downloaded: bool,
    /// Offered but only installed on request (e.g. optional feature updates).
    pub optional: bool,
}

impl MissingUpdate {
    /// What declines and selections refer to: the KB, or the update ID without one.
    pub fn key(&self) -> String {
        if self.kb.is_empty() { self.update_id.to_ascii_lowercase() } else { self.kb.clone() }
    }

    pub fn is_security(&self) -> bool {
        matches!(self.classification.as_str(), "Security Updates" | "Critical Updates")
    }
}

/// One entry of a computer's Windows Update history (installs only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: String,
    /// 2 = succeeded, 3 = succeeded with errors, 4 = failed, 5 = aborted
    pub result: u8,
    pub kb: String,
    pub title: String,
}

/// Result of one scan, as parsed from the script's output.
#[derive(Debug, Clone, Default)]
pub struct WuScan {
    /// source, reboot_pending, last_install, os_name, os_build
    pub info: BTreeMap<String, String>,
    pub updates: Vec<MissingUpdate>,
    pub history: Vec<HistoryEntry>,
}

/// The stored scan state of one computer.
#[derive(Debug, Clone, Serialize)]
pub struct WuStatus {
    pub computer_id: i64,
    pub computer: String,
    /// Time of the last successful scan (None = never succeeded).
    pub scanned_at: Option<String>,
    pub last_attempt: String,
    /// "ok", "failed" or "offline" (of the last attempt)
    pub status: String,
    pub error: Option<String>,
    pub info: BTreeMap<String, String>,
    /// Filled only for a single computer.
    pub history: Vec<HistoryEntry>,
}

/// What one install run reported.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InstallResult {
    pub installed: u32,
    pub failed: u32,
    pub reboot: bool,
    pub selected: u32,
}

fn fields(line: &str) -> Option<Vec<String>> {
    line.strip_prefix(MARK).map(|rest| rest.split('\t').map(|s| decode_escapes(s.trim())).collect())
}

/// Parse the scan output.
pub fn parse_scan(lines: &[String]) -> Result<WuScan> {
    let mut scan = WuScan::default();
    let mut done = false;
    for f in lines.iter().filter_map(|l| fields(l)) {
        let get = |i: usize| f.get(i).cloned().unwrap_or_default();
        let num = |i: usize| get(i).parse::<u64>().unwrap_or(0);
        match f[0].as_str() {
            "info" if !get(1).is_empty() => {
                scan.info.insert(get(1), get(2));
            }
            "update" if !get(1).is_empty() => scan.updates.push(MissingUpdate {
                update_id: get(1).to_ascii_lowercase(),
                kb: normalize_id(&get(2)),
                title: get(3),
                classification: get(4),
                severity: get(5),
                size: num(6),
                released: get(7),
                reboot: num(8).min(2) as u8,
                downloaded: get(9) == "1",
                optional: get(10) == "1",
            }),
            "history" if !get(4).is_empty() => scan.history.push(HistoryEntry {
                at: get(1),
                result: num(2).min(255) as u8,
                kb: get(3),
                title: get(4),
            }),
            "done" => done = true,
            _ => {}
        }
    }
    if !done {
        bail!("the update scan did not finish (no end marker in its output)");
    }
    Ok(scan)
}

/// The summary line an install run prints at the end.
pub fn parse_result(line: &str) -> Option<InstallResult> {
    let f = fields(line)?;
    if f.first().map(String::as_str) != Some("result") {
        return None;
    }
    let num = |i: usize| f.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(InstallResult { installed: num(1), failed: num(2), reboot: f.get(3).is_some_and(|s| s == "true"), selected: num(4) })
}

/// "5044284", "kb5044284" -> "KB5044284"; update IDs are lowercased.
pub fn normalize_id(s: &str) -> String {
    let s = s.trim();
    let digits = s.strip_prefix("KB").or_else(|| s.strip_prefix("kb")).or_else(|| s.strip_prefix("Kb")).unwrap_or(s);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        format!("KB{digits}")
    } else {
        s.to_ascii_lowercase()
    }
}

/// A comma/space separated list of KBs or update IDs.
pub fn parse_ids(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in s.split([',', ';', ' ', '\n', '\r', '\t']).filter(|p| !p.trim().is_empty()).map(normalize_id) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Comma-separated classification names (any case) -> the scripts' names.
/// Empty = the defaults; "*" or "all" = everything.
pub fn parse_classes(s: &str) -> Result<Vec<String>> {
    let parts: Vec<&str> = s.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Ok(DEFAULT_CLASSES.iter().map(|c| c.to_string()).collect());
    }
    if parts.iter().any(|p| *p == "*" || p.eq_ignore_ascii_case("all")) {
        return Ok(vec!["*".into()]);
    }
    parts
        .iter()
        .map(|p| {
            // "Security", "security updates" and "Security Updates" all work.
            CLASSIFICATIONS
                .iter()
                .find(|c| c.eq_ignore_ascii_case(p) || c.split(' ').next().is_some_and(|w| w.eq_ignore_ascii_case(p)))
                .map(|c| c.to_string())
                .ok_or_else(|| anyhow!("unknown update classification '{p}' (use: {})", CLASSIFICATIONS.join(", ")))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reboot {
    IfRequired,
    Never,
    Always,
}

/// A `windows_update` step with its variables filled in.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub classes: Vec<String>,
    pub kbs: Vec<String>,
    pub exclude: Vec<String>,
    pub reboot: Reboot,
    pub passes: u32,
    pub source: String,
}

impl InstallPlan {
    /// `declined` (from the console settings) is added to the exclusions,
    /// except for updates the step names explicitly.
    pub fn new(spec: &WindowsUpdateSpec, vars: &BTreeMap<String, String>, declined: &[String]) -> Result<InstallPlan> {
        let r = |s: &str| render(s, vars).map(|v| v.trim().to_string());
        let kbs = parse_ids(&r(&spec.kbs)?);
        let mut exclude = parse_ids(&r(&spec.exclude)?);
        for d in declined.iter().map(|d| normalize_id(d)) {
            if !exclude.contains(&d) && !kbs.contains(&d) {
                exclude.push(d);
            }
        }
        let reboot = match r(&spec.reboot)?.to_ascii_lowercase().as_str() {
            "" | "if-required" | "if_required" | "ifrequired" | "yes" => Reboot::IfRequired,
            "never" | "no" => Reboot::Never,
            "always" => Reboot::Always,
            other => bail!("windows_update reboot must be if-required, never or always, not '{other}'"),
        };
        let passes = match r(&spec.passes)?.as_str() {
            "" => 2,
            p => p.parse::<u32>().ok().filter(|n| (1..=5).contains(n)).ok_or_else(|| anyhow!("windows_update passes must be 1 to 5, not '{p}'"))?,
        };
        let source = match r(&spec.source)?.to_ascii_lowercase().as_str() {
            "" | "default" => "default".to_string(),
            "windows-update" | "windowsupdate" | "microsoft" => "windows-update".to_string(),
            other => bail!("windows_update source must be default or windows-update, not '{other}'"),
        };
        Ok(InstallPlan { classes: parse_classes(&r(&spec.classifications)?)?, kbs, exclude, reboot, passes, source })
    }

    pub fn describe(&self) -> String {
        let what = if !self.kbs.is_empty() {
            self.kbs.join(", ")
        } else if self.classes == ["*"] {
            "all classifications".to_string()
        } else {
            self.classes.join(", ")
        };
        let mut s = format!("installing {what}");
        if !self.exclude.is_empty() {
            s += &format!("; never {}", self.exclude.join(", "));
        }
        s
    }

    pub fn script(&self) -> String {
        script("install", &self.source, &self.classes, &self.kbs, &self.exclude)
    }
}

/// The PowerShell that runs the Windows Update script as SYSTEM on the target.
pub fn script(mode: &str, source: &str, classes: &[String], kbs: &[String], exclude: &[String]) -> String {
    let arr = |v: &[String]| format!("@({})", v.iter().map(|s| ps_quote(s)).collect::<Vec<_>>().join(", "));
    let core = format!(
        "$WuMode = {}\n$WuSource = {}\n$WuClasses = {}\n$WuKbs = {}\n$WuExclude = {}\n{CORE_PS}",
        ps_quote(mode),
        ps_quote(source),
        arr(classes),
        arr(kbs),
        arr(exclude),
    );
    let task = if mode == "install" { "Windows Update install" } else { "Windows Update scan" };
    format!(
        "$WuMode = {}\n$WuTask = {}\n$WuScript = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))\n{RUNNER_PS}",
        ps_quote(mode),
        ps_quote(task),
        B64.encode(core.as_bytes()),
    )
}

// ------------------------------------------------------------------- scans

/// Scan one computer and store the result.
async fn scan_one(app: &App, c: &Computer, winrm: &WinRmSettings, source: &str) -> ScanResult {
    let (status, error) = match collect(app, c, winrm, source).await {
        Ok(Some(scan)) => match app.db.save_wu_scan(c.id, &scan) {
            Ok(()) => ("ok", None),
            Err(e) => ("failed", Some(format!("saving update scan: {e:#}"))),
        },
        Ok(None) => ("offline", Some("WinRM port not reachable".to_string())),
        Err(e) => ("failed", Some(format!("{e:#}"))),
    };
    if let Some(e) = &error {
        let _ = app.db.wu_failed(c.id, status, e);
        if status == "offline" {
            let _ = app.db.set_online(&c.name, false);
        }
    }
    ScanResult { computer: c.name.clone(), status: status.into(), error }
}

/// Run the scan on a computer. `None` = not reachable.
async fn collect(app: &App, c: &Computer, winrm: &WinRmSettings, source: &str) -> Result<Option<WuScan>> {
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
    if os != Os::Windows {
        bail!("not a Windows computer");
    }
    let port = t.port.unwrap_or(default_port(os, winrm.use_ssl));
    if !port_open(&t.host, port, Duration::from_secs(5)).await {
        return Ok(None);
    }
    let transport = build_transport(app, &t, os, port, winrm)?;
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let log: LogFn = Arc::new(move |l: &str| {
        let mut v = sink.lock().unwrap();
        if v.len() < 100_000 {
            v.push(l.to_string());
        }
    });
    let body = script("scan", source, &[], &[], &[]);
    // A first search on a computer that has not looked for updates in a while can take long.
    let code = transport
        .run(&wrap_script(Shell::Powershell, &body), Shell::Powershell, Duration::from_secs(2400), &log)
        .await?;
    let lines = std::mem::take(&mut *lines.lock().unwrap());
    parse_scan(&lines).map(Some).map_err(|e| {
        let said: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with(MARK)).collect();
        let tail = said[said.len().saturating_sub(3)..].join(" | ");
        if tail.is_empty() { anyhow!("{e} (exit code {code})") } else { anyhow!("exit code {code}: {tail}") }
    })
}

/// Windows computers (and ones of unknown OS) to scan: the named ones, or all.
fn select(app: &App, names: Option<&[String]>) -> Result<Vec<Computer>> {
    let mut all = app.db.list_computers()?;
    all.retain(|c| c.os != "linux");
    if let Some(names) = names {
        let set: HashSet<String> = names.iter().map(|n| n.trim().to_ascii_lowercase()).collect();
        all.retain(|c| set.contains(&c.name.to_ascii_lowercase()));
    }
    Ok(all)
}

/// Scan computers and wait for the results. `track` reports progress in the
/// console's update scan state.
pub async fn run(app: &App, computers: Vec<Computer>, track: bool) -> Vec<ScanResult> {
    let winrm = app.winrm_settings();
    let settings = UpdateSettings::load(app);
    let results: Vec<ScanResult> = futures::stream::iter(computers)
        .map(|c| {
            let (app, winrm, source) = (app.clone(), winrm.clone(), settings.source.clone());
            async move {
                let r = scan_one(&app, &c, &winrm, &source).await;
                if track {
                    let mut st = app.wu_scan.lock().unwrap();
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
        let mut st = app.wu_scan.lock().unwrap();
        st.running = false;
        st.finished_at = Some(now());
    }
    results
}

/// Start a scan in the background (all Windows computers when `names` is
/// `None`). Returns how many computers it covers.
pub fn start(app: &App, names: Option<Vec<String>>) -> Result<usize> {
    let computers = select(app, names.as_deref())?;
    if computers.is_empty() {
        bail!("no Windows computers to scan");
    }
    let n = computers.len();
    {
        let mut st = app.wu_scan.lock().unwrap();
        if st.running {
            bail!("an update scan is already running");
        }
        *st = ScanState { running: true, total: n, started_at: Some(now()), ..Default::default() };
    }
    if names.is_none() {
        let mut s = UpdateSettings::load(app);
        s.last_scan = Some(now());
        app.db.set_setting("updates", &s)?;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let results = run(&app, computers, true).await;
        let bad = results.iter().filter(|r| r.status != "ok").count();
        info!("update scan: {} computers, {bad} not scanned", results.len());
    });
    Ok(n)
}

/// Called by the scheduler: start the periodic full scan when it is due.
pub fn tick(app: &App) -> Result<()> {
    let s: UpdateSettings = app.db.get_setting("updates")?;
    if s.interval_hours == 0 || app.wu_scan.lock().unwrap().running {
        return Ok(());
    }
    let last = s.last_scan.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
    if last.is_some_and(|t| Utc::now() - t < ChronoDuration::hours(s.interval_hours as i64)) {
        return Ok(());
    }
    match start(app, None) {
        Ok(n) => info!("scheduled update scan of {n} computers"),
        Err(e) => {
            error!("scheduled update scan: {e:#}");
            let mut s = s;
            s.last_scan = Some(now());
            app.db.set_setting("updates", &s)?;
        }
    }
    Ok(())
}

/// After a deployment that installed updates, scan the computers it reached
/// again so the Updates page shows what is still missing.
pub fn after_deployment(app: &App, reached: Vec<String>) {
    if reached.is_empty() || !UpdateSettings::load(app).rescan_after_install {
        return;
    }
    match select(app, Some(&reached)) {
        Ok(list) if !list.is_empty() => {
            let app = app.clone();
            tokio::spawn(async move {
                run(&app, list, false).await;
            });
        }
        Ok(_) => {}
        Err(e) => error!("update scan after deployment: {e:#}"),
    }
}

/// Scan named computers (all Windows computers when empty) and wait; used by the CLI.
pub async fn scan_now(app: &App, names: &[String]) -> Result<Vec<ScanResult>> {
    let computers = select(app, (!names.is_empty()).then_some(names)).context("loading computers")?;
    if computers.is_empty() {
        bail!("no matching Windows computers in the computer list");
    }
    Ok(run(app, computers, false).await)
}

// ----------------------------------------------------------------- reports

/// Missing-update counts of one computer, for group rules.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UpdateSummary {
    /// Missing updates that are neither declined nor optional.
    pub missing: u32,
    /// Of those, security and critical updates.
    pub security: u32,
}

/// Summaries by computer id of every successfully scanned computer.
pub fn summaries(app_db: &crate::db::Db) -> Result<HashMap<i64, UpdateSummary>> {
    let declined = app_db.get_setting::<UpdateSettings>("updates")?.declined_set();
    let mut out: HashMap<i64, UpdateSummary> =
        app_db.list_wu_status()?.into_iter().filter(|s| s.scanned_at.is_some()).map(|s| (s.computer_id, UpdateSummary::default())).collect();
    for (id, u) in app_db.wu_missing()? {
        if u.optional || declined.contains(&u.key().to_ascii_lowercase()) {
            continue;
        }
        if let Some(s) = out.get_mut(&id) {
            s.missing += 1;
            if u.is_security() {
                s.security += 1;
            }
        }
    }
    Ok(out)
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "Critical" => 0,
        "Important" => 1,
        "Moderate" => 2,
        "Low" => 3,
        _ => 4,
    }
}

/// Everything the Updates page shows: per computer and per update.
pub fn report(app: &App) -> Result<Value> {
    let settings = UpdateSettings::load(app);
    let declined = settings.declined_set();
    let status = app.db.list_wu_status()?;
    let names: HashMap<i64, String> = status.iter().map(|s| (s.computer_id, s.computer.clone())).collect();
    #[derive(Default)]
    struct Counts {
        missing: u32,
        security: u32,
        optional: u32,
        declined: u32,
    }
    let mut counts: HashMap<i64, Counts> = HashMap::new();
    // Update key -> (update, computers)
    let mut updates: BTreeMap<String, (MissingUpdate, Vec<String>)> = BTreeMap::new();
    for (id, u) in app.db.wu_missing()? {
        let key = u.key();
        let is_declined = declined.contains(&key.to_ascii_lowercase());
        let c = counts.entry(id).or_default();
        if is_declined {
            c.declined += 1;
        } else if u.optional {
            c.optional += 1;
        } else {
            c.missing += 1;
            if u.is_security() {
                c.security += 1;
            }
        }
        let e = updates.entry(key).or_insert_with(|| (u.clone(), Vec::new()));
        if let Some(n) = names.get(&id) {
            e.1.push(n.clone());
        }
    }
    let computers: Vec<Value> = status
        .iter()
        .map(|s| {
            let c = counts.get(&s.computer_id);
            let n = |f: fn(&Counts) -> u32| c.map(f).unwrap_or(0);
            json!({
                "computer": s.computer,
                "scanned_at": s.scanned_at,
                "last_attempt": s.last_attempt,
                "status": s.status,
                "error": s.error,
                "info": s.info,
                "missing": n(|c| c.missing),
                "security": n(|c| c.security),
                "optional": n(|c| c.optional),
                "declined": n(|c| c.declined),
            })
        })
        .collect();
    let mut list: Vec<(MissingUpdate, Vec<String>)> = updates.into_values().collect();
    list.sort_by(|(a, ac), (b, bc)| {
        (!a.is_security(), severity_rank(&a.severity), std::cmp::Reverse(ac.len()), &a.title)
            .cmp(&(!b.is_security(), severity_rank(&b.severity), std::cmp::Reverse(bc.len()), &b.title))
    });
    let updates: Vec<Value> = list
        .into_iter()
        .map(|(u, mut computers)| {
            computers.sort_by_key(|c| c.to_ascii_lowercase());
            let key = u.key();
            json!({
                "key": key,
                "declined": declined.contains(&key.to_ascii_lowercase()),
                "security": u.is_security(),
                "update": u,
                "computers": computers,
            })
        })
        .collect();
    let scan = app.wu_scan.lock().unwrap().clone();
    Ok(json!({
        "computers": computers,
        "updates": updates,
        "scan": scan,
        "settings": settings,
        "classifications": CLASSIFICATIONS,
        "default_classifications": DEFAULT_CLASSES,
    }))
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
            "Downloading\n@@wu\tinfo\tsource\tWSUS http://wsus:8530\n@@wu\tinfo\treboot_pending\tfalse\n\
             @@wu\tupdate\t0B3C-AA\t5044284\t2024-10 Cumulative Update for Windows 11 (KB5044284)\tSecurity Updates\tImportant\t812345678\t2024-10-08\t1\t0\t0\n\
             @@wu\tupdate\tdddd-1111\t\tIntel - Display - 31.0.101.5590\tDrivers\t\t1234\t2024-06-01\t0\t1\t1\n\
             @@wu\thistory\t2024-09-11T02:10:00Z\t2\tKB5043076\tCaf\\u00e9 update (KB5043076)\n\
             Found 2 missing update(s)\n@@wu\tdone\t",
        );
        let s = parse_scan(&out).unwrap();
        assert_eq!(s.info["source"], "WSUS http://wsus:8530");
        assert_eq!(s.updates.len(), 2);
        let cu = &s.updates[0];
        assert_eq!((cu.kb.as_str(), cu.update_id.as_str(), cu.size, cu.reboot), ("KB5044284", "0b3c-aa", 812_345_678, 1));
        assert!(cu.is_security() && !cu.downloaded && !cu.optional);
        assert_eq!(cu.key(), "KB5044284");
        let drv = &s.updates[1];
        assert_eq!((drv.key().as_str(), drv.downloaded, drv.optional), ("dddd-1111", true, true));
        assert_eq!(s.history[0].title, "Café update (KB5043076)");
        assert_eq!(s.history[0].result, 2);
        assert!(parse_scan(&lines("@@wu\tinfo\tsource\tx")).is_err(), "no done marker");
    }

    #[test]
    fn parses_install_result() {
        assert_eq!(
            parse_result("@@wu\tresult\t3\t1\ttrue\t4"),
            Some(InstallResult { installed: 3, failed: 1, reboot: true, selected: 4 })
        );
        assert_eq!(parse_result("@@wu\tupdate\tx"), None);
        assert_eq!(parse_result("Installed 3"), None);
    }

    #[test]
    fn normalizes_ids_and_classes() {
        assert_eq!(parse_ids("5044284, kb5043076;KB5044284 ABCD-ef"), vec!["KB5044284", "KB5043076", "abcd-ef"]);
        assert_eq!(parse_classes("").unwrap(), DEFAULT_CLASSES);
        assert_eq!(parse_classes("security, critical updates,DRIVERS").unwrap(), ["Security Updates", "Critical Updates", "Drivers"]);
        assert_eq!(parse_classes("Security, *").unwrap(), ["*"]);
        assert!(parse_classes("Patches").is_err());
    }

    #[test]
    fn install_plan_from_step() {
        let vars: BTreeMap<String, String> =
            [("classes", "Security"), ("only", ""), ("reboot", "never")].into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        let spec = WindowsUpdateSpec {
            classifications: "{{classes}}".into(),
            kbs: "{{only}}".into(),
            exclude: "5034441".into(),
            reboot: "{{reboot}}".into(),
            ..Default::default()
        };
        let p = InstallPlan::new(&spec, &vars, &["KB5034441".into(), "kb5001716".into()]).unwrap();
        assert_eq!(p.classes, ["Security Updates"]);
        assert_eq!(p.exclude, ["KB5034441", "KB5001716"]);
        assert_eq!((p.reboot, p.passes, p.source.as_str()), (Reboot::Never, 2, "default"));
        // A declined update the step names explicitly is installed.
        let spec = WindowsUpdateSpec { kbs: "KB5001716".into(), ..Default::default() };
        let p = InstallPlan::new(&spec, &vars, &["KB5001716".into()]).unwrap();
        assert!(p.exclude.is_empty());
        assert_eq!(p.reboot, Reboot::IfRequired);
        let bad = WindowsUpdateSpec { passes: "9".into(), ..Default::default() };
        assert!(InstallPlan::new(&bad, &vars, &[]).is_err());
    }

    /// The generated scripts parse with the real PowerShell parser, and the
    /// core script decoded from the runner does too (Windows only).
    #[test]
    fn scripts_parse() {
        if !cfg!(windows) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("psccm-wu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = InstallPlan::new(&WindowsUpdateSpec::default(), &BTreeMap::new(), &["KB1".into()]).unwrap();
        let files = [
            ("runner.ps1", wrap_script(Shell::Powershell, &plan.script())),
            ("core.ps1", format!("$WuMode = 'scan'\n{CORE_PS}")),
        ];
        let mut errors = Vec::new();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            let cmd = format!(
                "$e = $null; [void][Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$e); foreach ($x in $e) {{ \"{name}:$($x.Extent.StartLineNumber): $($x.Message)\" }}",
                path.display()
            );
            let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &cmd]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !text.is_empty() {
                errors.push(text);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(errors.is_empty(), "Windows Update script syntax errors:\n{}", errors.join("\n"));
    }
}
