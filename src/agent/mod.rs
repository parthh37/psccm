//! The PSCCM agent: a small resident process that connects **out** to the
//! console over HTTP and long-polls for work, so machines with no inbound WinRM
//! or SSH can still be managed. Because the agent runs locally (as LocalSystem
//! on Windows), it does everything with local PowerShell/CIM and Win32 calls —
//! event-log capture, CPU/disk analytics, Task Scheduler, firewall, local users
//! and an interactive shell — with nothing opened inbound on the target.
//!
//! This module holds the wire protocol shared by both sides. The console side
//! lives in [`server`] and [`terminal`]; the target side in [`runtime`],
//! [`jobs`] and [`terminal`].

pub mod jobs;
pub mod runtime;
pub mod server;
pub mod terminal;

use serde::{Deserialize, Serialize};

/// Windows service name for the agent (the console service is `PSCCM`).
#[cfg(windows)]
pub const AGENT_SERVICE_NAME: &str = "PSCCMAgent";

/// How long the console holds a check-in open waiting for work (seconds).
pub const CHECKIN_HOLD_SECS: u64 = 25;

/// An agent is considered offline if it has not checked in for this long.
pub const STALE_SECS: i64 = 150;

// ----------------------------------------------------------------- enrollment

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollReq {
    /// The console's shared enrollment token.
    pub token: String,
    pub hostname: String,
    /// "windows" or "linux".
    pub os: String,
    #[serde(default)]
    pub os_version: String,
    #[serde(default)]
    pub agent_version: String,
    /// Address the agent reached the console from is filled in by the server;
    /// the agent may send its own primary address for the computer record.
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResp {
    pub agent_id: String,
    pub agent_key: String,
}

// -------------------------------------------------------------------- metrics

/// A point-in-time resource snapshot pushed on every check-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    /// Overall CPU utilisation, 0–100.
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub uptime_secs: u64,
    #[serde(default)]
    pub disks: Vec<DiskUsage>,
    /// Number of running processes (handy fleet-health signal).
    #[serde(default)]
    pub processes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskUsage {
    /// Drive letter / mount point, e.g. `C:` or `/`.
    pub mount: String,
    pub total: u64,
    pub available: u64,
}

impl DiskUsage {
    pub fn used_pct(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            100.0 * (self.total - self.available) as f32 / self.total as f32
        }
    }
}

// ------------------------------------------------------------------- check-in

/// Terminal output (or a close notice) the agent streams up to the console.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalChunk {
    pub session_id: String,
    /// UTF-8 (lossy) output; empty when `closed`.
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub closed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckinReq {
    #[serde(default)]
    pub metrics: Option<Metrics>,
    #[serde(default)]
    pub agent_version: String,
    /// Output produced since the last check-in for any open terminal sessions.
    #[serde(default)]
    pub terminal: Vec<TerminalChunk>,
}

/// A unit of work the agent should run. `params` is interpreted per `kind`
/// (see [`jobs`]); the console stores it opaquely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A terminal control message the console sends down to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalCtl {
    pub session_id: String,
    /// "open", "input" or "close".
    pub action: String,
    /// For "open": the shell to launch.
    #[serde(default)]
    pub shell: Option<String>,
    /// For "open": how to run it (SYSTEM / logged-on user / password runas).
    #[serde(default)]
    pub run_as: Option<RunAs>,
    /// For "input": bytes to write to the shell's stdin (UTF-8).
    #[serde(default)]
    pub data: String,
}

/// How a command or terminal should be run on the target.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunAs {
    /// Local or domain account, e.g. `svc`, `.\\admin`, `CORP\\jo`. Empty / none
    /// means the agent's own identity (LocalSystem).
    #[serde(default)]
    pub user: Option<String>,
    /// Password for `CreateProcessWithLogonW`. Omitted when `impersonate` is set.
    #[serde(default)]
    pub password: Option<String>,
    /// Impersonate the (first) interactively logged-on user's token instead of
    /// using a password. Requires the agent to run as LocalSystem.
    #[serde(default)]
    pub impersonate: bool,
}

impl RunAs {
    /// A run-as that changes identity from the agent's own.
    pub fn is_elevated_identity(&self) -> bool {
        self.impersonate || self.user.as_deref().is_some_and(|u| !u.is_empty())
    }
    /// A short human label for logs/UI (never the password).
    pub fn label(&self) -> String {
        if self.impersonate {
            "logged-on user".into()
        } else {
            match self.user.as_deref().filter(|u| !u.is_empty()) {
                Some(u) => u.to_string(),
                None => "SYSTEM".into(),
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckinResp {
    #[serde(default)]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub terminal: Vec<TerminalCtl>,
    /// How long the agent should wait before the next check-in.
    pub poll_ms: u64,
}

// --------------------------------------------------------------- job results

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: i64,
    /// "success" or "failed".
    pub status: String,
    /// Human-readable log / command output.
    #[serde(default)]
    pub output: String,
    /// Structured result (event rows, task list, firewall rules, users…).
    #[serde(default)]
    pub data: serde_json::Value,
}

/// Interval until the agent's next check-in. Short while there is live work
/// (an open terminal or queued jobs), long otherwise so idle agents are cheap.
pub fn next_poll_ms(terminal_active: bool, has_jobs: bool) -> u64 {
    if terminal_active {
        250
    } else if has_jobs {
        800
    } else {
        // Long-poll already waited CHECKIN_HOLD_SECS; return promptly.
        1_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_scales_with_work() {
        assert_eq!(next_poll_ms(true, true), 250);
        assert_eq!(next_poll_ms(true, false), 250);
        assert_eq!(next_poll_ms(false, true), 800);
        assert_eq!(next_poll_ms(false, false), 1_000);
    }

    #[test]
    fn disk_used_pct() {
        let d = DiskUsage { mount: "C:".into(), total: 100, available: 25 };
        assert_eq!(d.used_pct(), 75.0);
        let empty = DiskUsage { mount: "D:".into(), total: 0, available: 0 };
        assert_eq!(empty.used_pct(), 0.0);
    }

    #[test]
    fn protocol_round_trips() {
        let req = CheckinReq {
            metrics: Some(Metrics { cpu_pct: 12.5, mem_total: 16, mem_used: 8, ..Default::default() }),
            agent_version: "0.3.0".into(),
            terminal: vec![TerminalChunk { session_id: "s1".into(), data: "hi".into(), closed: false }],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: CheckinReq = serde_json::from_str(&json).unwrap();
        assert_eq!(back.metrics.unwrap().cpu_pct, 12.5);
        assert_eq!(back.terminal[0].data, "hi");
    }

    #[test]
    fn run_as_label_hides_password() {
        let ra = RunAs { user: Some("CORP\\jo".into()), password: Some("secret".into()), impersonate: false };
        assert_eq!(ra.label(), "CORP\\jo");
        assert!(ra.is_elevated_identity());
        assert_eq!(RunAs::default().label(), "SYSTEM");
        let imp = RunAs { impersonate: true, ..Default::default() };
        assert_eq!(imp.label(), "logged-on user");
    }
}
