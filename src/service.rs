//! Running PSCCM as a Windows service ("PSCCM").
//!
//! `psccm service install` registers the service; the service command line is
//! `psccm.exe --data-dir <dir> service run`, and the listen address/port/token
//! come from the "server" setting in the database.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "PSCCM";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub token: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8080
}

impl Default for ServerSettings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::Context;
    use std::ffi::OsString;
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

    /// Where the status icon (psccm-tray) finds the console.
    const REG_KEY: &str = r"HKLM\SOFTWARE\PSCCM";
    const FIREWALL_RULE: &str = "PSCCM Console";

    pub fn install(data_dir: &std::path::Path, account: Option<String>, password: Option<String>) -> Result<()> {
        // The service starts in System32, so a relative folder would land there.
        let data_dir = std::path::absolute(data_dir)?;
        install_named(
            SERVICE_NAME,
            "PSCCM Deployment Console",
            "Agentless software deployment for Windows (WinRM) and Linux (SSH).",
            vec![
                OsString::from("--data-dir"),
                data_dir.into_os_string(),
                OsString::from("service"),
                OsString::from("run"),
            ],
            account,
            password,
        )
    }

    /// Register and start an own-process service with the given launch
    /// arguments. Shared by the console and the agent.
    pub fn install_named(
        name: &str,
        display: &str,
        description: &str,
        launch_arguments: Vec<OsString>,
        account: Option<String>,
        password: Option<String>,
    ) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
            .context("opening the service manager (run as administrator)")?;
        let info = ServiceInfo {
            name: OsString::from(name),
            display_name: OsString::from(display),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()?,
            launch_arguments,
            dependencies: vec![],
            account_name: account.filter(|a| !a.is_empty()).map(OsString::from),
            account_password: password.filter(|p| !p.is_empty()).map(OsString::from),
        };
        let svc = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
        svc.set_description(description)?;
        svc.start::<&str>(&[]).context("starting the service")?;
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        uninstall_named(SERVICE_NAME)?;
        close_firewall();
        let _ = reg(&["delete", REG_KEY, "/f"]);
        Ok(())
    }

    /// Get `account` ready to run the console: CreateService does not grant
    /// "Log on as a service" (services.msc does), and without it the service
    /// fails to start with error 1069. The account must also be able to write
    /// the data folder, which the administrator installing PSCCM created.
    pub fn prepare_account(account: &str, data_dir: &std::path::Path) -> Result<()> {
        if account.eq_ignore_ascii_case("LocalSystem") {
            return Ok(());
        }
        let sid = account_sid(account)?;
        grant_service_logon(&sid).with_context(|| format!("granting {account} the right to log on as a service"))?;
        let st = std::process::Command::new("icacls.exe")
            .arg(data_dir)
            .args(["/grant", &format!("*{}:(OI)(CI)F", sid_string(&sid)?), "/T", "/C", "/Q"])
            .status()?;
        anyhow::ensure!(st.success(), "giving {account} access to {} failed", data_dir.display());
        Ok(())
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn account_sid(account: &str) -> Result<Vec<u8>> {
        use windows_sys::Win32::Security::{LookupAccountNameW, SECURITY_MAX_SID_SIZE};
        // ".\user" is accepted by the service manager but not by LookupAccountName.
        let name = wide(account.strip_prefix(".\\").unwrap_or(account));
        let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut sid_len = sid.len() as u32;
        let mut domain = [0u16; 256];
        let mut domain_len = domain.len() as u32;
        let mut kind = 0;
        // SAFETY: every buffer is valid for the length passed with it.
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if ok == 0 {
            anyhow::bail!("unknown account '{account}': {}", std::io::Error::last_os_error());
        }
        Ok(sid)
    }

    fn sid_string(sid: &[u8]) -> Result<String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        let mut out: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` holds a valid SID; the returned string is LocalAlloc'd and NUL-terminated.
        unsafe {
            if ConvertSidToStringSidW(sid.as_ptr() as *mut _, &mut out) == 0 {
                anyhow::bail!("ConvertSidToStringSid failed: {}", std::io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *out.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
            LocalFree(out as _);
            Ok(s)
        }
    }

    fn grant_service_logon(sid: &[u8]) -> Result<()> {
        use windows_sys::Win32::Security::Authentication::Identity::{
            LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose, LsaNtStatusToWinError,
            LsaOpenPolicy, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
        };
        let check = |status: i32| -> Result<()> {
            if status != 0 {
                // SAFETY: plain status conversion.
                let code = unsafe { LsaNtStatusToWinError(status) };
                anyhow::bail!(std::io::Error::from_raw_os_error(code as i32));
            }
            Ok(())
        };
        let attrs = LSA_OBJECT_ATTRIBUTES::default();
        // SAFETY: zero is the "no handle" value of LSA_HANDLE.
        let mut policy = unsafe { std::mem::zeroed() };
        // SAFETY: attrs and policy outlive the call.
        check(unsafe { LsaOpenPolicy(std::ptr::null(), &attrs, (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32, &mut policy) })?;
        let mut right: Vec<u16> = "SeServiceLogonRight".encode_utf16().collect();
        let len = (right.len() * 2) as u16;
        let name = LSA_UNICODE_STRING { Length: len, MaximumLength: len, Buffer: right.as_mut_ptr() };
        // SAFETY: the policy handle is open, `sid` is a valid SID and `name` points at `right`.
        let status = unsafe { LsaAddAccountRights(policy, sid.as_ptr() as *mut _, &name, 1) };
        // SAFETY: closes the handle opened above.
        unsafe { LsaClose(policy) };
        check(status)
    }

    /// Let other computers reach the console port through Windows Firewall.
    pub fn open_firewall(port: u16) -> Result<()> {
        close_firewall();
        let st = std::process::Command::new("netsh.exe")
            .args(["advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_RULE}"), "dir=in", "action=allow", "protocol=TCP"])
            .arg(format!("localport={port}"))
            .arg(format!("program={}", std::env::current_exe()?.display()))
            .stdout(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(st.success(), "adding the Windows Firewall rule for port {port} failed");
        Ok(())
    }

    fn close_firewall() {
        let _ = std::process::Command::new("netsh.exe")
            .args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// Record where the console listens for the status icon, which runs as
    /// an ordinary user and cannot read the database.
    pub fn publish(s: &ServerSettings, data_dir: &std::path::Path) -> Result<()> {
        let data_dir = std::path::absolute(data_dir)?;
        reg(&["add", REG_KEY, "/v", "Port", "/t", "REG_DWORD", "/d", &s.port.to_string(), "/f"])?;
        reg(&["add", REG_KEY, "/v", "Bind", "/t", "REG_SZ", "/d", &s.bind, "/f"])?;
        reg(&["add", REG_KEY, "/v", "DataDir", "/t", "REG_SZ", "/d", &data_dir.to_string_lossy(), "/f"])
    }

    fn reg(args: &[&str]) -> Result<()> {
        let st = std::process::Command::new("reg.exe")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(st.success(), "reg {} failed", args.join(" "));
        Ok(())
    }

    /// What `psccm service status` prints: service state, where it listens,
    /// whether the console answers and the end of the service log.
    pub fn status_report(s: &ServerSettings, data_dir: &std::path::Path) -> String {
        let mut out = String::new();
        let state = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .and_then(|m| m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
            .and_then(|svc| svc.query_status());
        match state {
            Ok(st) => out += &format!("Service:   {:?} (exit code {:?})\n", st.current_state, st.exit_code),
            Err(_) => out += "Service:   not installed\n",
        }
        out += &format!("Listens:   {}:{} {}\n", s.bind, s.port, if s.token.is_some() { "(token required)" } else { "(this computer only)" });
        let ip: std::net::IpAddr = match s.bind.parse() {
            Ok(ip) if !std::net::IpAddr::is_unspecified(&ip) => ip,
            _ => std::net::Ipv4Addr::LOCALHOST.into(),
        };
        let answers = std::net::TcpStream::connect_timeout(&(ip, s.port).into(), Duration::from_secs(2)).is_ok();
        out += &format!("Console:   {}\n", if answers { "answering" } else { "NOT answering on that port" });
        if !ip.is_loopback() {
            let rule = std::process::Command::new("netsh.exe")
                .args(["advfirewall", "firewall", "show", "rule", &format!("name={FIREWALL_RULE}")])
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|st| st.success());
            out += &format!("Firewall:  {}\n", if rule { "rule 'PSCCM Console' present" } else { "no 'PSCCM Console' rule: other computers may be blocked" });
        }
        if let Ok(log) = std::fs::read_to_string(data_dir.join("psccm.log")) {
            out += &format!("\nLast lines of {}:\n", data_dir.join("psccm.log").display());
            let lines: Vec<&str> = log.lines().collect();
            for l in &lines[lines.len().saturating_sub(15)..] {
                out += l;
                out.push('\n');
            }
        }
        out
    }

    pub fn uninstall_named(name: &str) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let svc = manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
        if svc.query_status()?.current_state != ServiceState::Stopped {
            let _ = svc.stop();
            for _ in 0..30 {
                if svc.query_status()?.current_state == ServiceState::Stopped {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        svc.delete()?;
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    /// Entry point when started by the service control manager.
    pub fn run(data_dir: PathBuf) -> Result<()> {
        let _ = DATA_DIR.set(data_dir);
        service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("not started by the service manager")?;
        Ok(())
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!("service failed: {e:#}");
        }
    }

    fn status(state: ServiceState, code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running { ServiceControlAccept::STOP } else { ServiceControlAccept::empty() },
            exit_code: ServiceExitCode::Win32(code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        }
    }

    fn run_service() -> Result<()> {
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop2 = stop.clone();
        let handle = service_control_handler::register(SERVICE_NAME, move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                stop2.notify_one();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        handle.set_service_status(status(ServiceState::Running, 0))?;
        let data_dir = DATA_DIR.get().cloned().unwrap_or_else(crate::default_data_dir);
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(async move {
            let app = crate::app::Ctx::open(&data_dir)?;
            let s: ServerSettings = app.db.get_setting("server")?;
            // Installs from before the status icon have no registry entry yet
            // (best effort: a non-admin service account cannot write HKLM).
            let _ = publish(&s, &data_dir);
            app.db.fail_interrupted()?;
            app.db.fail_interrupted_jobs()?;
            crate::scheduler::spawn(app.clone());
            let addr: std::net::SocketAddr = format!("{}:{}", s.bind, s.port).parse()?;
            tracing::info!("PSCCM service listening on {addr}");
            crate::web::serve(app, addr, s.token.filter(|t| !t.is_empty()), async move { stop.notified().await }).await
        });
        if let Err(e) = &result {
            tracing::error!("{e:#}");
        }
        handle.set_service_status(status(ServiceState::Stopped, if result.is_ok() { 0 } else { 1 }))?;
        result
    }
}

#[cfg(windows)]
pub use imp::{install, install_named, open_firewall, prepare_account, publish, run, status_report, uninstall, uninstall_named};

#[cfg(not(windows))]
pub fn install(_: &std::path::Path, _: Option<String>, _: Option<String>) -> Result<()> {
    anyhow::bail!("on Linux, run 'psccm serve' from a systemd unit (see README)")
}

#[cfg(not(windows))]
pub fn prepare_account(_: &str, _: &std::path::Path) -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
pub fn open_firewall(_: u16) -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
pub fn publish(_: &ServerSettings, _: &std::path::Path) -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
pub fn status_report(_: &ServerSettings, _: &std::path::Path) -> String {
    "On Linux use: systemctl status psccm\n".into()
}

#[cfg(not(windows))]
#[allow(dead_code)] // kept for API symmetry with the Windows build
pub fn install_named(
    _: &str,
    _: &str,
    _: &str,
    _: Vec<std::ffi::OsString>,
    _: Option<String>,
    _: Option<String>,
) -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
#[allow(dead_code)] // kept for API symmetry with the Windows build
pub fn uninstall_named(_: &str) -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
pub fn run(_: PathBuf) -> Result<()> {
    anyhow::bail!("not a Windows system")
}
