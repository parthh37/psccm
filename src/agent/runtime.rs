//! Target side of the agent: a resident process that enrolls with the console,
//! then long-polls it for work. It keeps nothing listening inbound — all
//! traffic is the agent reaching out — which is the whole point for machines
//! with no WinRM or SSH.
//!
//! Installed as the `PSCCMAgent` Windows service (LocalSystem) or a Linux
//! systemd unit; `psccm agent run` also works in the foreground for testing.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use super::terminal::shell::ShellProc;
use super::{CheckinReq, CheckinResp, DiskUsage, EnrollReq, EnrollResp, JobResult, Metrics, RunAs};
#[cfg(windows)]
use super::AGENT_SERVICE_NAME;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// On-disk agent state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    /// Enrollment token, kept only until the first successful enrollment.
    #[serde(default)]
    pub enroll_token: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_key: Option<String>,
    /// PEM certificate (or CA) to trust for the console's HTTPS, for consoles
    /// with a self-signed certificate. Normal certificate checks still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<String>,
    /// Skip HTTPS certificate checks entirely. Anyone on the network path can
    /// then impersonate the console and run commands as SYSTEM; labs only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure_tls: bool,
}

/// `%ProgramData%\PSCCM-Agent` on Windows, `/var/lib/psccm-agent` on Linux.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        match std::env::var_os("ProgramData") {
            Some(pd) => PathBuf::from(pd).join("PSCCM-Agent"),
            None => PathBuf::from(r"C:\ProgramData\PSCCM-Agent"),
        }
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/var/lib/psccm-agent")
    }
}

fn config_path() -> PathBuf {
    data_dir().join("agent.json")
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn save_config(c: &Config) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    restrict_dir(&dir);
    let path = config_path();
    std::fs::write(&path, serde_json::to_string_pretty(c)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// On Linux, make one of the agent's folders private to its owner (mode 700).
/// On Windows the data folder is locked down once, at install
/// ([`protect_data_dir`]), and what is created in it inherits that.
pub fn restrict_dir(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            warn!("restricting {}: {e}", dir.display());
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Windows: make the data folder owned by Administrators and accessible only
/// to SYSTEM and Administrators, replacing whatever ACL it had. This also
/// defeats a folder an ordinary user created in advance to read or edit
/// agent.json.
#[cfg(windows)]
fn protect_data_dir() -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let icacls = |args: &[&str]| -> Result<()> {
        let st = std::process::Command::new("icacls.exe").arg(&dir).args(args).args(["/T", "/C", "/Q"]).status()?;
        if !st.success() {
            bail!("icacls {} failed ({st})", args.join(" "));
        }
        Ok(())
    };
    icacls(&["/setowner", "*S-1-5-32-544"])
        .and_then(|_| icacls(&["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"]))
        .context("locking down the agent data folder (run as administrator)")
}

fn http(cfg: &Config) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder();
    if let Some(pem) = cfg.ca_cert.as_deref().filter(|p| !p.trim().is_empty()) {
        b = b.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).context("agent.json ca_cert")?);
    }
    if cfg.insecure_tls {
        warn!("HTTPS certificate checks are off (insecure_tls); the console can be impersonated");
        b = b.danger_accept_invalid_certs(true);
    }
    Ok(b.user_agent(format!("psccm-agent/{VERSION}"))
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn host_facts() -> (String, String, String) {
    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "unknown".into());
    let os = if cfg!(windows) { "windows" } else { "linux" }.to_string();
    let os_version = sysinfo::System::long_os_version().unwrap_or_default();
    (hostname, os, os_version)
}

// -------------------------------------------------------------- enrollment

/// Ensure the config has agent credentials, enrolling if needed.
async fn ensure_enrolled(client: &reqwest::Client, cfg: &mut Config) -> Result<()> {
    if cfg.agent_id.is_some() && cfg.agent_key.is_some() {
        return Ok(());
    }
    let token = cfg.enroll_token.clone().filter(|t| !t.is_empty()).context("no enrollment token; run 'psccm agent install'")?;
    let (hostname, os, os_version) = host_facts();
    let req = EnrollReq { token, hostname, os, os_version, agent_version: VERSION.to_string(), address: None };
    let url = format!("{}/api/agent/enroll", cfg.server_url.trim_end_matches('/'));
    let resp = client.post(&url).json(&req).send().await.context("contacting the console")?;
    if !resp.status().is_success() {
        let msg = resp.text().await.unwrap_or_default();
        bail!("enrollment rejected: {msg}");
    }
    let er: EnrollResp = resp.json().await?;
    cfg.agent_id = Some(er.agent_id);
    cfg.agent_key = Some(er.agent_key);
    cfg.enroll_token = None; // consumed
    save_config(cfg)?;
    info!("enrolled with {}", cfg.server_url);
    Ok(())
}

// ----------------------------------------------------------------- metrics

fn collect_metrics(sys: &mut sysinfo::System) -> Metrics {
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let processes = sys.refresh_processes(sysinfo::ProcessesToUpdate::All, false) as u32;
    let disks = sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .map(|d| DiskUsage {
            mount: d.mount_point().to_string_lossy().trim_end_matches('\\').to_string(),
            total: d.total_space(),
            available: d.available_space(),
        })
        .collect();
    Metrics {
        cpu_pct: sys.global_cpu_usage(),
        mem_used: sys.used_memory(),
        mem_total: sys.total_memory(),
        uptime_secs: sysinfo::System::uptime(),
        disks,
        processes,
    }
}

// --------------------------------------------------------------- check-in

fn auth_header(cfg: &Config) -> String {
    format!("Agent {}:{}", cfg.agent_id.as_deref().unwrap_or(""), cfg.agent_key.as_deref().unwrap_or(""))
}

/// One check-in: report metrics + terminal output, run returned jobs, apply
/// terminal control. Returns the poll interval to wait before the next one.
async fn checkin(
    client: &reqwest::Client,
    cfg: &Config,
    sys: &mut sysinfo::System,
    sessions: &mut HashMap<String, ShellProc>,
    want_metrics: bool,
) -> Result<u64> {
    // Drain terminal output; drop sessions whose shell has exited.
    let mut terminal = Vec::new();
    sessions.retain(|sid, proc| {
        let (data, closed) = proc.take_output();
        if !data.is_empty() || closed {
            terminal.push(super::TerminalChunk { session_id: sid.clone(), data, closed });
        }
        !closed
    });

    let req = CheckinReq {
        metrics: want_metrics.then(|| collect_metrics(sys)),
        agent_version: VERSION.to_string(),
        terminal,
    };
    let url = format!("{}/api/agent/checkin", cfg.server_url.trim_end_matches('/'));
    let resp = client.post(&url).header("Authorization", auth_header(cfg)).json(&req).send().await?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("unauthorized (the agent may have been removed from the console)");
    }
    let resp: CheckinResp = resp.error_for_status()?.json().await?;

    for ctl in &resp.terminal {
        apply_terminal(sessions, ctl);
    }
    for job in resp.jobs {
        let client = client.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let result = super::jobs::run_job(&job).await;
            if let Err(e) = post_result(&client, &cfg, &result).await {
                warn!("posting job {} result failed: {e:#}", job.id);
            }
        });
    }
    Ok(resp.poll_ms)
}

fn apply_terminal(sessions: &mut HashMap<String, ShellProc>, ctl: &super::TerminalCtl) {
    match ctl.action.as_str() {
        "open" => {
            let shell = ctl.shell.as_deref().unwrap_or("");
            let run_as = ctl.run_as.clone().unwrap_or_default();
            match ShellProc::spawn(shell, &run_as) {
                Ok(proc) => {
                    sessions.insert(ctl.session_id.clone(), proc);
                    info!("terminal {} opened ({})", ctl.session_id, RunAs::label(&run_as));
                }
                Err(e) => {
                    // Report the failure back as output so the browser sees it.
                    let proc = ShellProc::failed(format!("cannot open shell: {e:#}\n"));
                    sessions.insert(ctl.session_id.clone(), proc);
                }
            }
        }
        "input" => {
            if let Some(p) = sessions.get(&ctl.session_id) {
                p.write_stdin(&ctl.data);
            }
        }
        "close" => {
            if let Some(p) = sessions.remove(&ctl.session_id) {
                p.close();
            }
        }
        _ => {}
    }
}

async fn post_result(client: &reqwest::Client, cfg: &Config, result: &JobResult) -> Result<()> {
    let url = format!("{}/api/agent/job-result", cfg.server_url.trim_end_matches('/'));
    client
        .post(&url)
        .header("Authorization", auth_header(cfg))
        .json(result)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// The agent main loop: enroll (retrying) then check in forever.
async fn serve_loop(stop: impl std::future::Future<Output = ()>) {
    let mut cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("no agent config ({e:#}); run 'psccm agent install' first");
            return;
        }
    };
    let client = match http(&cfg) {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return;
        }
    };
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();
    let mut sessions: HashMap<String, ShellProc> = HashMap::new();
    let mut last_metrics = Instant::now() - Duration::from_secs(3600);
    const METRICS_EVERY: Duration = Duration::from_secs(60);

    let run = async {
        loop {
            if let Err(e) = ensure_enrolled(&client, &mut cfg).await {
                warn!("enroll: {e:#}");
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
            let want_metrics = last_metrics.elapsed() >= METRICS_EVERY;
            let poll = match checkin(&client, &cfg, &mut sys, &mut sessions, want_metrics).await {
                Ok(poll) => {
                    if want_metrics {
                        last_metrics = Instant::now();
                    }
                    poll
                }
                Err(e) => {
                    warn!("check-in: {e:#}");
                    5_000
                }
            };
            tokio::time::sleep(Duration::from_millis(poll)).await;
        }
    };
    tokio::select! {
        _ = run => {}
        _ = stop => { info!("agent stopping"); }
    }
}

// --------------------------------------------------------------- CLI glue

/// `psccm agent install --server <url> --token <tok>`: store the config and
/// register the service so the agent starts now and on boot.
pub fn install(server: &str, token: &str, ca_cert: Option<&std::path::Path>, insecure_tls: bool) -> Result<()> {
    let server_url = server.trim().trim_end_matches('/').to_string();
    if server_url.is_empty() {
        bail!("--server is required (e.g. https://console.corp.local:8080)");
    }
    if !server_url.starts_with("https://") && !server_url.starts_with("http://") {
        bail!("--server must start with https:// or http://");
    }
    if server_url.starts_with("http://") {
        eprintln!(
            "warning: {server_url} is plain HTTP, so the agent key, commands and run-as passwords cross the \
             network unencrypted. Put the console behind HTTPS unless this network is trusted."
        );
    }
    let ca_cert = match ca_cert {
        Some(p) => {
            let pem = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            reqwest::Certificate::from_pem(pem.as_bytes()).context("--ca-cert is not a PEM certificate")?;
            Some(pem)
        }
        None => None,
    };
    let cfg = Config {
        server_url,
        enroll_token: Some(token.trim().to_string()).filter(|t| !t.is_empty()),
        ca_cert,
        insecure_tls,
        ..Default::default()
    };
    #[cfg(windows)]
    protect_data_dir()?;
    save_config(&cfg)?;
    install_service()?;
    println!("PSCCM agent installed; it will enroll with {} shortly.", cfg.server_url);
    Ok(())
}

pub fn uninstall() -> Result<()> {
    uninstall_service()?;
    let _ = std::fs::remove_file(config_path());
    println!("PSCCM agent removed.");
    Ok(())
}

/// `psccm agent enroll`: enroll once, in the foreground (for testing).
pub async fn enroll_now() -> Result<()> {
    let mut cfg = load_config()?;
    let client = http(&cfg)?;
    ensure_enrolled(&client, &mut cfg).await?;
    println!("Enrolled as agent {}", cfg.agent_id.unwrap_or_default());
    Ok(())
}

pub fn status() -> Result<()> {
    match load_config() {
        Ok(c) => {
            println!("Server:   {}", c.server_url);
            println!("Enrolled: {}", if c.agent_id.is_some() { "yes" } else { "no" });
            if let Some(id) = c.agent_id {
                println!("Agent id: {id}");
            }
            println!("Data dir: {}", data_dir().display());
        }
        Err(_) => println!("No agent config at {} (run 'psccm agent install').", config_path().display()),
    }
    Ok(())
}

#[cfg(windows)]
fn install_service() -> Result<()> {
    crate::service::install_named(
        AGENT_SERVICE_NAME,
        "PSCCM Agent",
        "PSCCM management agent: connects out to the console for deployment, analytics and remote administration.",
        vec![std::ffi::OsString::from("agent"), std::ffi::OsString::from("run")],
        None, // LocalSystem
        None,
    )
}

#[cfg(windows)]
fn uninstall_service() -> Result<()> {
    crate::service::uninstall_named(AGENT_SERVICE_NAME)
}

#[cfg(not(windows))]
fn install_service() -> Result<()> {
    let exe = std::env::current_exe()?;
    let unit = format!(
        "[Unit]\nDescription=PSCCM Agent\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=simple\nExecStart={} agent run\nRestart=always\nRestartSec=10\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        exe.display()
    );
    std::fs::write("/etc/systemd/system/psccm-agent.service", unit)
        .context("writing the systemd unit (run as root)")?;
    let _ = std::process::Command::new("systemctl").arg("daemon-reload").status();
    let _ = std::process::Command::new("systemctl").args(["enable", "--now", "psccm-agent"]).status();
    Ok(())
}

#[cfg(not(windows))]
fn uninstall_service() -> Result<()> {
    let _ = std::process::Command::new("systemctl").args(["disable", "--now", "psccm-agent"]).status();
    let _ = std::fs::remove_file("/etc/systemd/system/psccm-agent.service");
    let _ = std::process::Command::new("systemctl").arg("daemon-reload").status();
    Ok(())
}

/// `psccm agent run`: as a Windows service when started by the SCM, otherwise
/// in the foreground.
pub fn run() -> Result<()> {
    #[cfg(windows)]
    {
        if winsvc::try_dispatch()? {
            return Ok(());
        }
    }
    // Foreground: run until Ctrl-C.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve_loop(async {
        let _ = tokio::signal::ctrl_c().await;
    }));
    Ok(())
}

#[cfg(windows)]
mod winsvc {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    define_windows_service!(ffi_agent_main, agent_main);

    /// Returns `Ok(true)` when started by the service manager (and the service
    /// ran to completion), `Ok(false)` when not launched as a service.
    pub fn try_dispatch() -> Result<bool> {
        match service_dispatcher::start(AGENT_SERVICE_NAME, ffi_agent_main) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn agent_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            error!("agent service failed: {e:#}");
        }
    }

    fn status(state: ServiceState, accept: ServiceControlAccept) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        }
    }

    fn run_service() -> Result<()> {
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop2 = stop.clone();
        let handle = service_control_handler::register(AGENT_SERVICE_NAME, move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                stop2.notify_one();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN))?;
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(serve_loop(async move { stop.notified().await }));
        handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
        Ok(())
    }
}
