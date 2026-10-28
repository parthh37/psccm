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

    pub fn install(data_dir: &std::path::Path, account: Option<String>, password: Option<String>) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
            .context("opening the service manager (run as administrator)")?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("PSCCM Deployment Console"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()?,
            launch_arguments: vec![
                OsString::from("--data-dir"),
                data_dir.as_os_str().to_owned(),
                OsString::from("service"),
                OsString::from("run"),
            ],
            dependencies: vec![],
            account_name: account.filter(|a| !a.is_empty()).map(OsString::from),
            account_password: password.filter(|p| !p.is_empty()).map(OsString::from),
        };
        let svc = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
        svc.set_description("Agentless software deployment for Windows (WinRM) and Linux (SSH).")?;
        svc.start::<&str>(&[]).context("starting the service")?;
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let svc = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
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
            app.db.fail_interrupted()?;
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
pub use imp::{install, run, uninstall};

#[cfg(not(windows))]
pub fn install(_: &std::path::Path, _: Option<String>, _: Option<String>) -> Result<()> {
    anyhow::bail!("on Linux, run 'psccm serve' from a systemd unit (see README)")
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<()> {
    anyhow::bail!("not a Windows system")
}

#[cfg(not(windows))]
pub fn run(_: PathBuf) -> Result<()> {
    anyhow::bail!("not a Windows system")
}
