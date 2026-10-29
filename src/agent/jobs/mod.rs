//! Target-side job execution. The agent runs everything **locally** — because
//! it already lives on the box (as LocalSystem on Windows) it can read every
//! event log, enumerate scheduled tasks, change the firewall and manage local
//! users through ordinary PowerShell/CIM, with nothing opened inbound.
//!
//! Each job is dispatched by `kind`; `params` is the JSON the console queued.
//! Results come back as [`JobResult`] with human output and, for inventory
//! jobs, structured `data` the console caches for its UI.

pub mod winproc;

use anyhow::{Result, bail};
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;

use crate::agent::{Job, JobResult, RunAs};
use crate::transport::ps_quote;

/// Run a job to completion, turning any error into a failed result.
pub async fn run_job(job: &Job) -> JobResult {
    let (status, output, data) = match dispatch(job).await {
        Ok((out, data)) => ("success", out, data),
        Err(e) => ("failed", format!("{e:#}"), serde_json::Value::Null),
    };
    JobResult { job_id: job.id, status: status.into(), output, data }
}

async fn dispatch(job: &Job) -> Result<(String, serde_json::Value)> {
    match job.kind.as_str() {
        "command" => command(job).await,
        "eventlog" => eventlog(job).await,
        "tasks_list" => run_ps_json(TASKS_LIST_PS).await.map(|d| ("ok".into(), d)),
        "task_control" => task_control(job).await,
        "firewall_list" => run_ps_json(FIREWALL_LIST_PS).await.map(|d| ("ok".into(), d)),
        "firewall_set" => firewall_set(job).await,
        "users_list" => run_ps_json(USERS_LIST_PS).await.map(|d| ("ok".into(), d)),
        "user_control" => user_control(job).await,
        other => bail!("unknown job kind '{other}'"),
    }
}

fn params<T: for<'de> Deserialize<'de>>(job: &Job) -> Result<T> {
    Ok(serde_json::from_value(job.params.clone())?)
}

// -------------------------------------------------------------- command job

#[derive(Deserialize)]
struct CommandParams {
    #[serde(default)]
    shell: Option<String>,
    script: String,
    #[serde(default)]
    run_as: Option<RunAs>,
}

async fn command(job: &Job) -> Result<(String, serde_json::Value)> {
    let p: CommandParams = params(job)?;
    let shell = p.shell.as_deref().unwrap_or(if cfg!(windows) { "powershell" } else { "bash" });
    let run_as = p.run_as.unwrap_or_default();
    let (code, output) = run_shell_capture(shell, &p.script, &run_as).await?;
    let label = run_as.label();
    let header = if run_as.is_elevated_identity() { format!("(as {label})\n") } else { String::new() };
    if code != 0 {
        bail!("{header}{output}\nexit code {code}");
    }
    Ok((format!("{header}{output}"), serde_json::json!({ "exit_code": code })))
}

// ------------------------------------------------------------- event log job

#[derive(Deserialize)]
struct EventLogParams {
    #[serde(default = "default_log")]
    log: String,
    /// WinEvent level 1=Critical 2=Error 3=Warning 4=Information 5=Verbose.
    #[serde(default)]
    level: Option<u8>,
    #[serde(default)]
    since_hours: Option<u32>,
    #[serde(default)]
    event_ids: Vec<u32>,
    #[serde(default = "default_max")]
    max: u32,
}

fn default_log() -> String {
    "System".into()
}
fn default_max() -> u32 {
    200
}

async fn eventlog(job: &Job) -> Result<(String, serde_json::Value)> {
    if !cfg!(windows) {
        bail!("event log capture is only available on Windows targets");
    }
    let p: EventLogParams = params(job)?;
    let mut filter = format!("LogName = {}", ps_quote(&p.log));
    if let Some(l) = p.level {
        filter += &format!("; Level = {}", l.clamp(1, 5));
    }
    if let Some(h) = p.since_hours.filter(|h| *h > 0) {
        filter += &format!("; StartTime = (Get-Date).AddHours(-{h})");
    }
    if !p.event_ids.is_empty() {
        let ids: Vec<String> = p.event_ids.iter().map(|i| i.to_string()).collect();
        filter += &format!("; Id = @({})", ids.join(","));
    }
    let max = p.max.clamp(1, 5000);
    let script = format!(
        r#"$ErrorActionPreference='Stop'
try {{
  $ev = Get-WinEvent -FilterHashtable @{{ {filter} }} -MaxEvents {max} -ErrorAction Stop
}} catch {{
  if ($_.Exception.Message -match 'No events were found') {{ '[]'; return }} else {{ throw }}
}}
$ev | Select-Object @{{n='time';e={{$_.TimeCreated.ToString('o')}}}}, Id,
  @{{n='level';e={{$_.LevelDisplayName}}}}, @{{n='provider';e={{$_.ProviderName}}}},
  @{{n='message';e={{$_.Message}}}} | ConvertTo-Json -Depth 3"#
    );
    let data = run_ps_json(&script).await?;
    let n = data.as_array().map(|a| a.len()).unwrap_or(if data.is_null() { 0 } else { 1 });
    Ok((format!("collected {n} event(s) from {}", p.log), data))
}

// --------------------------------------------------------- scheduled tasks

const TASKS_LIST_PS: &str = r#"$ErrorActionPreference='Stop'
Get-ScheduledTask | ForEach-Object {
  $info = $null; try { $info = $_ | Get-ScheduledTaskInfo } catch {}
  [pscustomobject]@{
    name = $_.TaskName; path = $_.TaskPath; state = "$($_.State)";
    author = $_.Author; description = $_.Description;
    last_run = if ($info) { "$($info.LastRunTime)" } else { '' };
    next_run = if ($info) { "$($info.NextRunTime)" } else { '' };
    last_result = if ($info) { $info.LastTaskResult } else { $null }
  }
} | ConvertTo-Json -Depth 3"#;

#[derive(Deserialize)]
struct TaskControlParams {
    /// run | enable | disable | delete
    action: String,
    path: String,
    name: String,
}

async fn task_control(job: &Job) -> Result<(String, serde_json::Value)> {
    if !cfg!(windows) {
        bail!("Task Scheduler control is only available on Windows targets");
    }
    let p: TaskControlParams = params(job)?;
    let (path, name) = (ps_quote(&p.path), ps_quote(&p.name));
    let cmd = match p.action.as_str() {
        "run" => format!("Start-ScheduledTask -TaskPath {path} -TaskName {name}"),
        "enable" => format!("Enable-ScheduledTask -TaskPath {path} -TaskName {name}"),
        "disable" => format!("Disable-ScheduledTask -TaskPath {path} -TaskName {name}"),
        "delete" => format!("Unregister-ScheduledTask -TaskPath {path} -TaskName {name} -Confirm:$false"),
        a => bail!("unknown task action '{a}'"),
    };
    let (code, out) = run_powershell(&format!("$ErrorActionPreference='Stop'\n{cmd}\nWrite-Output 'ok'"), &[]).await?;
    if code != 0 {
        bail!("{out}");
    }
    Ok((format!("task {} {}", p.name, p.action), serde_json::Value::Null))
}

// ------------------------------------------------------------------ firewall

const FIREWALL_LIST_PS: &str = r#"$ErrorActionPreference='Stop'
Get-NetFirewallRule | Select-Object -First 1000 |
  Select-Object Name, DisplayName, @{n='enabled';e={[bool]$_.Enabled}},
    @{n='direction';e={"$($_.Direction)"}}, @{n='action';e={"$($_.Action)"}},
    @{n='profile';e={"$($_.Profile)"}}, @{n='group';e={$_.Group}} |
  ConvertTo-Json -Depth 3"#;

#[derive(Deserialize)]
struct FirewallSetParams {
    /// new | delete | enable | disable
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    fw_action: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    local_port: Option<String>,
    #[serde(default)]
    remote_address: Option<String>,
    #[serde(default)]
    program: Option<String>,
}

async fn firewall_set(job: &Job) -> Result<(String, serde_json::Value)> {
    if !cfg!(windows) {
        bail!("firewall management is only available on Windows targets");
    }
    let p: FirewallSetParams = params(job)?;
    let selector = || -> Result<String> {
        if let Some(n) = p.name.as_deref().filter(|s| !s.is_empty()) {
            Ok(format!("-Name {}", ps_quote(n)))
        } else if let Some(d) = p.display_name.as_deref().filter(|s| !s.is_empty()) {
            Ok(format!("-DisplayName {}", ps_quote(d)))
        } else {
            bail!("a rule name or display name is required");
        }
    };
    let cmd = match p.action.as_str() {
        "delete" => format!("Remove-NetFirewallRule {}", selector()?),
        "enable" => format!("Enable-NetFirewallRule {}", selector()?),
        "disable" => format!("Disable-NetFirewallRule {}", selector()?),
        "new" | "create" => {
            let display = p.display_name.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("a display name is required for a new rule"))?;
            let mut c = format!("New-NetFirewallRule -DisplayName {}", ps_quote(display));
            c += &format!(" -Direction {}", if p.direction.as_deref() == Some("Outbound") { "Outbound" } else { "Inbound" });
            c += &format!(" -Action {}", if p.fw_action.as_deref() == Some("Block") { "Block" } else { "Allow" });
            if let Some(pr) = p.protocol.as_deref().filter(|s| !s.is_empty()) {
                c += &format!(" -Protocol {}", ps_quote(pr));
            }
            if let Some(port) = p.local_port.as_deref().filter(|s| !s.is_empty()) {
                c += &format!(" -LocalPort {}", ps_quote(port));
            }
            if let Some(addr) = p.remote_address.as_deref().filter(|s| !s.is_empty()) {
                c += &format!(" -RemoteAddress {}", ps_quote(addr));
            }
            if let Some(prog) = p.program.as_deref().filter(|s| !s.is_empty()) {
                c += &format!(" -Program {}", ps_quote(prog));
            }
            c
        }
        a => bail!("unknown firewall action '{a}'"),
    };
    let (code, out) = run_powershell(&format!("$ErrorActionPreference='Stop'\n{cmd} | Out-Null\nWrite-Output 'ok'"), &[]).await?;
    if code != 0 {
        bail!("{out}");
    }
    Ok((format!("firewall rule {}", p.action), serde_json::Value::Null))
}

// -------------------------------------------------------- local users/groups

const USERS_LIST_PS: &str = r#"$ErrorActionPreference='Stop'
$users = Get-LocalUser | Select-Object Name, @{n='enabled';e={[bool]$_.Enabled}},
  @{n='full_name';e={$_.FullName}}, Description,
  @{n='last_logon';e={ if ($_.LastLogon) { "$($_.LastLogon)" } else { '' } }}
$groups = Get-LocalGroup | ForEach-Object {
  $m = @(); try { $m = Get-LocalGroupMember -Group $_.Name -ErrorAction Stop | ForEach-Object { "$($_.Name)" } } catch {}
  [pscustomobject]@{ name = $_.Name; description = $_.Description; members = @($m) }
}
[pscustomobject]@{ users = @($users); groups = @($groups) } | ConvertTo-Json -Depth 4"#;

#[derive(Deserialize)]
struct UserControlParams {
    /// create | delete | enable | disable | setpassword | addgroup | removegroup
    action: String,
    user: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    group: Option<String>,
}

async fn user_control(job: &Job) -> Result<(String, serde_json::Value)> {
    if !cfg!(windows) {
        bail!("local user management is only available on Windows targets");
    }
    let p: UserControlParams = params(job)?;
    let user = ps_quote(&p.user);
    // The password is passed through the environment, never the script text or
    // the process arguments, so it does not land in logs.
    let mut env: Vec<(&str, String)> = Vec::new();
    let secure = "(ConvertTo-SecureString $env:PSCCM_PW -AsPlainText -Force)";
    let cmd = match p.action.as_str() {
        "create" => {
            let full = p.full_name.as_deref().unwrap_or("");
            match p.password.as_deref().filter(|s| !s.is_empty()) {
                Some(pw) => {
                    env.push(("PSCCM_PW", pw.to_string()));
                    format!("New-LocalUser -Name {user} -Password {secure} -FullName {} -AccountNeverExpires", ps_quote(full))
                }
                None => format!("New-LocalUser -Name {user} -NoPassword -FullName {}", ps_quote(full)),
            }
        }
        "delete" => format!("Remove-LocalUser -Name {user}"),
        "enable" => format!("Enable-LocalUser -Name {user}"),
        "disable" => format!("Disable-LocalUser -Name {user}"),
        "setpassword" => {
            let pw = p.password.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("a password is required"))?;
            env.push(("PSCCM_PW", pw.to_string()));
            format!("Set-LocalUser -Name {user} -Password {secure}")
        }
        "addgroup" => {
            let g = p.group.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("a group is required"))?;
            format!("Add-LocalGroupMember -Group {} -Member {user}", ps_quote(g))
        }
        "removegroup" => {
            let g = p.group.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("a group is required"))?;
            format!("Remove-LocalGroupMember -Group {} -Member {user}", ps_quote(g))
        }
        a => bail!("unknown user action '{a}'"),
    };
    let (code, out) = run_powershell(&format!("$ErrorActionPreference='Stop'\n{cmd}\nWrite-Output 'ok'"), &env).await?;
    if code != 0 {
        bail!("{out}");
    }
    Ok((format!("user {} {}", p.user, p.action), serde_json::Value::Null))
}

// ---------------------------------------------------------------- runners

/// Where job scripts are written before they run. On Linux this is inside the
/// agent's own root-only data folder rather than the shared /tmp, so no local
/// user can pre-create the folder and read or swap a script; on Windows the
/// temp folder of LocalSystem is already private to it.
pub(crate) fn work_dir() -> Result<std::path::PathBuf> {
    #[cfg(windows)]
    let dir = std::env::temp_dir().join("psccm-agent");
    #[cfg(not(windows))]
    let dir = crate::agent::runtime::data_dir().join("work");
    std::fs::create_dir_all(&dir)?;
    crate::agent::runtime::restrict_dir(&dir);
    Ok(dir)
}

/// Run a PowerShell script (default identity) and return `(exit_code, output)`.
async fn run_powershell(script: &str, env: &[(&str, String)]) -> Result<(i32, String)> {
    let dir = work_dir()?;
    let file = dir.join(format!("{}.ps1", uuid::Uuid::new_v4().simple()));
    std::fs::write(&file, [b"\xEF\xBB\xBF".as_slice(), script.as_bytes()].concat())?;
    let _guard = RemoveOnDrop(file.clone());
    let mut cmd = Command::new(ps_exe());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().await?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str(&err);
    }
    Ok((out.status.code().unwrap_or(-1), text))
}

/// Run a PowerShell script and parse its stdout as JSON (empty output -> `[]`).
async fn run_ps_json(script: &str) -> Result<serde_json::Value> {
    let (code, out) = run_powershell(script, &[]).await?;
    if code != 0 {
        bail!("{out}");
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Array(vec![]));
    }
    Ok(serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::json!({ "raw": trimmed })))
}

/// Run a command-job shell (any OS), honouring `run_as` on Windows.
async fn run_shell_capture(shell: &str, script: &str, run_as: &RunAs) -> Result<(i32, String)> {
    if run_as.is_elevated_identity() {
        #[cfg(windows)]
        {
            return winproc::run_capture(shell, script, run_as);
        }
        #[cfg(not(windows))]
        {
            bail!("running a command as another user is only supported on Windows");
        }
    }
    match shell {
        "powershell" => run_powershell(script, &[]).await,
        "cmd" => run_script_file(script, "cmd.exe", &["/d", "/c"], "cmd").await,
        "bash" => run_interpreter(script, "bash").await,
        "sh" => run_interpreter(script, "sh").await,
        other => bail!("unknown shell '{other}'"),
    }
}

async fn run_script_file(script: &str, program: &str, pre: &[&str], ext: &str) -> Result<(i32, String)> {
    let dir = work_dir()?;
    let file = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&file, script)?;
    let _guard = RemoveOnDrop(file.clone());
    let out = Command::new(program)
        .args(pre)
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text))
}

async fn run_interpreter(script: &str, program: &str) -> Result<(i32, String)> {
    run_script_file(script, program, &[], "sh").await
}

#[cfg(windows)]
fn ps_exe() -> &'static str {
    "powershell.exe"
}
#[cfg(not(windows))]
fn ps_exe() -> &'static str {
    // On a Linux agent PowerShell is unlikely; the Windows-only jobs reject
    // themselves earlier, so this is only reached if pwsh is installed.
    "pwsh"
}

struct RemoveOnDrop(std::path::PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
