//! Windows targets over WinRM, driven through the console's own PowerShell
//! (`New-PSSession` / `Invoke-Command` / `Copy-Item -ToSession`).
//!
//! Using the built-in PowerShell remoting client gives Kerberos/NTLM,
//! message encryption and the logged-on user's credentials for free, with
//! nothing installed on the targets. The console must run on Windows.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;

use super::{LogFn, Transport, ps_quote, pump_lines};
use crate::package::Shell;

const EXIT_MARK: &str = "__PSCCM_EXIT__=";
const CONNECT_MARK: &str = "__PSCCM_CONNECT_ERROR__";
const REMOTE_TMP: &str = r"C:\Windows\Temp\psccm";

const HEADER: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}
try {
    $p = @{ ComputerName = $env:PSCCM_HOST }
    if ($env:PSCCM_PORT) { $p.Port = [int]$env:PSCCM_PORT }
    if ($env:PSCCM_SSL -eq '1') {
        $p.UseSSL = $true
        if ($env:PSCCM_SKIPCA -eq '1') { $p.SessionOption = New-PSSessionOption -SkipCACheck -SkipCNCheck -SkipRevocationCheck }
    }
    if ($env:PSCCM_USER) {
        $sec = ConvertTo-SecureString $env:PSCCM_PASS -AsPlainText -Force
        $p.Credential = New-Object System.Management.Automation.PSCredential($env:PSCCM_USER, $sec)
    }
    Remove-Item Env:PSCCM_PASS -ErrorAction SilentlyContinue
    if ($env:PSCCM_LOCAL -eq '1') { $s = $null; $sess = @{} } else { $s = New-PSSession @p; $sess = @{ Session = $s } }
} catch {
    Write-Output "__PSCCM_CONNECT_ERROR__ $($_.Exception.Message)"
    exit 2
}
$ErrorActionPreference = 'Continue'
try {
"#;

const FOOTER: &str = r#"
} catch {
    Write-Output "ERROR: $($_.Exception.Message)"
} finally {
    if ($s) { Remove-PSSession $s -ErrorAction SilentlyContinue }
}
"#;

pub struct WinRm {
    pub host: String,
    pub port: Option<u16>,
    pub ssl: bool,
    pub skip_ca_check: bool,
    /// None = connect as the Windows account running the console (Kerberos).
    pub user: Option<String>,
    pub password: Option<String>,
}

impl WinRm {
    /// Run a local PowerShell wrapper that opens a session to the target,
    /// executes `body` and prints the exit marker.
    async fn invoke(&self, body: &str, timeout: Duration, log: &LogFn) -> Result<i32> {
        if !cfg!(windows) {
            bail!("Windows targets need the console to run on Windows (it uses PowerShell remoting)");
        }
        let dir = std::env::temp_dir().join("psccm");
        std::fs::create_dir_all(&dir)?;
        let file = TempFile(dir.join(format!("{}.ps1", uuid::Uuid::new_v4())));
        let script = format!("{HEADER}\n{body}\n{FOOTER}");
        // BOM so Windows PowerShell 5.1 reads the file as UTF-8.
        std::fs::write(&file.0, [b"\xEF\xBB\xBF".as_slice(), script.as_bytes()].concat())?;

        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&file.0)
            .env("PSCCM_HOST", &self.host)
            .env("PSCCM_SSL", if self.ssl { "1" } else { "0" })
            .env("PSCCM_SKIPCA", if self.skip_ca_check { "1" } else { "0" })
            .env_remove("PSCCM_USER")
            .env_remove("PSCCM_PASS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(port) = self.port {
            cmd.env("PSCCM_PORT", port.to_string());
        }
        // Debug builds only: run on the console itself without WinRM, to
        // test scripts and the remote side of this wrapper on a dev box.
        cmd.env_remove("PSCCM_LOCAL");
        if cfg!(debug_assertions) && std::env::var("PSCCM_WINRM_LOCAL").as_deref() == Ok("1") {
            cmd.env("PSCCM_LOCAL", "1");
        }
        if let Some(user) = self.user.as_deref().filter(|u| !u.is_empty()) {
            cmd.env("PSCCM_USER", user).env("PSCCM_PASS", self.password.as_deref().unwrap_or(""));
        }
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start powershell.exe: {e}"))?;

        let exit = Arc::new(Mutex::new(None::<i32>));
        let connect_err = Arc::new(Mutex::new(None::<String>));
        let handle_line = {
            let (exit, connect_err, log) = (exit.clone(), connect_err.clone(), log.clone());
            move |line: String| {
                if let Some(code) = line.strip_prefix(EXIT_MARK) {
                    *exit.lock().unwrap() = Some(code.trim().parse().unwrap_or(-1));
                } else if let Some(msg) = line.strip_prefix(CONNECT_MARK) {
                    *connect_err.lock().unwrap() = Some(msg.trim().to_string());
                } else {
                    log(&line);
                }
            }
        };
        let out = pump_lines(child.stdout.take().unwrap(), handle_line.clone());
        let err = pump_lines(child.stderr.take().unwrap(), handle_line);
        let run = async {
            tokio::join!(out, err);
            child.wait().await
        };
        match tokio::time::timeout(timeout, run).await {
            Err(_) => bail!("timed out after {}s", timeout.as_secs()),
            Ok(status) => {
                status?;
            }
        }
        if let Some(msg) = connect_err.lock().unwrap().take() {
            bail!("WinRM connection to {} failed: {msg}", self.host);
        }
        let code = *exit.lock().unwrap();
        code.ok_or_else(|| anyhow!("remote command did not report an exit code (see log)"))
    }
}

#[async_trait]
impl Transport for WinRm {
    async fn run(&self, script: &str, shell: Shell, timeout: Duration, log: &LogFn) -> Result<i32> {
        let (ext, bytes) = match shell {
            Shell::Powershell => ("ps1", [b"\xEF\xBB\xBF".as_slice(), script.as_bytes()].concat()),
            Shell::Cmd => ("cmd", script.as_bytes().to_vec()),
            other => bail!("{other:?} scripts cannot run on Windows"),
        };
        let remote = format!(r"{REMOTE_TMP}\{}.{ext}", uuid::Uuid::new_v4());
        let shell_name = if shell == Shell::Cmd { "cmd" } else { "powershell" };
        let body = format!(
            r#"
$b64 = '{b64}'
Invoke-Command @sess -ArgumentList {remote}, $b64, '{shell_name}' -ScriptBlock {{
    param($path, $b64, $shell)
    New-Item -ItemType Directory -Force -Path (Split-Path $path) | Out-Null
    [IO.File]::WriteAllBytes($path, [Convert]::FromBase64String($b64))
    # Through cmd so stdout and stderr arrive merged and in order, decoded
    # explicitly: the remote host has no console, so PowerShell's own decoding
    # of native output would guess the wrong code page. PowerShell steps write
    # UTF-8 (see the prelude); cmd writes the OEM code page.
    $psi = New-Object Diagnostics.ProcessStartInfo 'cmd.exe'
    if ($shell -eq 'cmd') {{
        $psi.Arguments = '/d /s /c ""' + $path + '" 2>&1"'
        $oem = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Nls\CodePage' -ErrorAction SilentlyContinue).OEMCP
        $psi.StandardOutputEncoding = try {{ [Text.Encoding]::GetEncoding([int]$oem) }} catch {{ [Text.Encoding]::Default }}
    }} else {{
        $psi.Arguments = '/d /s /c ""powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $path + '" 2>&1"'
        $psi.StandardOutputEncoding = New-Object Text.UTF8Encoding $false
    }}
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $p = [Diagnostics.Process]::Start($psi)
    # Installers that prompt get end-of-input instead of hanging.
    $p.StandardInput.Close()
    while ($null -ne ($line = $p.StandardOutput.ReadLine())) {{ $line }}
    $p.WaitForExit()
    $code = $p.ExitCode
    Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    "__PSCCM_EXIT__=$code"
}} 2>&1 | ForEach-Object {{ "$_" }}
"#,
            b64 = B64.encode(&bytes),
            remote = ps_quote(&remote),
        );
        self.invoke(&body, timeout, log).await
    }

    async fn upload(&self, local: &Path, remote: &str, timeout: Duration, log: &LogFn) -> Result<()> {
        let body = format!(
            r#"
$remote = {remote}
Invoke-Command @sess -ArgumentList (Split-Path $remote) -ScriptBlock {{
    param($d) New-Item -ItemType Directory -Force -Path $d | Out-Null
}}
$ErrorActionPreference = 'Stop'
if ($s) {{ Copy-Item -LiteralPath {local} -Destination $remote -ToSession $s -Force }} else {{ Copy-Item -LiteralPath {local} -Destination $remote -Force }}
"__PSCCM_EXIT__=0"
"#,
            remote = ps_quote(remote),
            local = ps_quote(&local.to_string_lossy()),
        );
        match self.invoke(&body, timeout, log).await? {
            0 => Ok(()),
            c => bail!("upload failed with code {c}"),
        }
    }
}

/// Deletes the local wrapper script when dropped.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
