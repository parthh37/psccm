//! Target-side interactive shell: spawn a child shell with piped stdio and
//! stream its bytes. Pipe-based (not full ConPTY), which is right for a web
//! cmd/powershell/bash. Reader threads feed a shared buffer the agent drains on
//! each check-in.
//!
//! Run-as: the default identity (the agent's own — LocalSystem when installed
//! as a service) works everywhere through [`ShellProc::spawn`]. Impersonating a
//! logged-on user and password runas are Windows-only, implemented in the
//! [`super::super::jobs::winproc`] helpers, which build a [`ShellProc`] from raw
//! Win32 handles via [`ShellProc::from_parts`].

use anyhow::{Result, bail};
use std::io::Write;
use std::process::Child;
use std::sync::{Arc, Mutex};

use crate::agent::RunAs;

/// A running shell owned by the agent for one terminal session. It is agnostic
/// to how the process was launched (std `Child` or a raw Win32 handle).
pub struct ShellProc {
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
    out: Arc<Mutex<String>>,
    done: Arc<Mutex<bool>>,
    killer: Box<dyn Fn() + Send + Sync>,
    is_alive: Box<dyn Fn() -> bool + Send + Sync>,
}

impl ShellProc {
    /// Launch `shell` ("cmd", "powershell", "bash", "sh") under `run_as`.
    pub fn spawn(shell: &str, run_as: &RunAs) -> Result<ShellProc> {
        if run_as.is_elevated_identity() {
            return spawn_as(shell, run_as);
        }
        let mut cmd = base_command(shell)?;
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        Ok(ShellProc::from_child(cmd.spawn()?))
    }

    /// Build a [`ShellProc`] from a spawned std child (default identity).
    pub fn from_child(mut child: Child) -> ShellProc {
        let out = Arc::new(Mutex::new(String::new()));
        let stdin = child.stdin.take().map(|s| Box::new(s) as Box<dyn Write + Send>);
        if let Some(so) = child.stdout.take() {
            pump(so, out.clone());
        }
        if let Some(se) = child.stderr.take() {
            pump(se, out.clone());
        }
        let child = Arc::new(Mutex::new(child));
        let (c_kill, c_alive) = (child.clone(), child);
        ShellProc {
            stdin: Mutex::new(stdin),
            out,
            done: Arc::new(Mutex::new(false)),
            killer: Box::new(move || {
                let _ = c_kill.lock().unwrap().kill();
            }),
            is_alive: Box::new(move || matches!(c_alive.lock().unwrap().try_wait(), Ok(None))),
        }
    }

    /// Build a [`ShellProc`] from already-wired parts (used by the Win32 run-as
    /// helpers): a stdin writer, a stdout reader, a kill closure and a liveness
    /// probe.
    pub fn from_parts(
        stdin: Box<dyn Write + Send>,
        stdout: impl std::io::Read + Send + 'static,
        killer: Box<dyn Fn() + Send + Sync>,
        is_alive: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> ShellProc {
        let out = Arc::new(Mutex::new(String::new()));
        pump(stdout, out.clone());
        ShellProc {
            stdin: Mutex::new(Some(stdin)),
            out,
            done: Arc::new(Mutex::new(false)),
            killer,
            is_alive,
        }
    }

    /// A dead "shell" that only reports a message, so a spawn failure can be
    /// shown in the browser terminal and the session closes cleanly.
    pub fn failed(message: String) -> ShellProc {
        ShellProc {
            stdin: Mutex::new(None),
            out: Arc::new(Mutex::new(message)),
            done: Arc::new(Mutex::new(true)),
            killer: Box::new(|| {}),
            is_alive: Box::new(|| false),
        }
    }

    /// Write keystrokes to the shell's stdin.
    pub fn write_stdin(&self, data: &str) {
        if let Some(si) = self.stdin.lock().unwrap().as_mut() {
            let _ = si.write_all(data.as_bytes());
            let _ = si.flush();
        }
    }

    /// Drain buffered output; the `bool` is whether the shell has exited.
    pub fn take_output(&self) -> (String, bool) {
        let text = std::mem::take(&mut *self.out.lock().unwrap());
        let exited = !(self.is_alive)();
        if exited {
            *self.done.lock().unwrap() = true;
        }
        (text, *self.done.lock().unwrap())
    }

    /// Terminate the shell.
    pub fn close(&self) {
        *self.done.lock().unwrap() = true;
        *self.stdin.lock().unwrap() = None;
        (self.killer)();
    }
}

impl Drop for ShellProc {
    fn drop(&mut self) {
        (self.killer)();
    }
}

/// The base command for a shell name, rejecting a shell for the wrong OS.
pub(crate) fn base_command(shell: &str) -> Result<std::process::Command> {
    let cmd = match shell {
        #[cfg(windows)]
        "cmd" => std::process::Command::new("cmd.exe"),
        #[cfg(windows)]
        "powershell" | "" => {
            let mut c = std::process::Command::new("powershell.exe");
            c.args(["-NoLogo", "-NoProfile", "-NoExit", "-Command", "-"]);
            c
        }
        #[cfg(not(windows))]
        "bash" | "" => {
            let mut c = std::process::Command::new("bash");
            c.arg("-i");
            c
        }
        #[cfg(not(windows))]
        "sh" => std::process::Command::new("sh"),
        other => bail!("shell '{other}' is not available on this platform"),
    };
    Ok(cmd)
}

/// The command line a Win32 run-as launcher should execute for `shell`.
#[cfg(windows)]
pub(crate) fn shell_command_line(shell: &str) -> &'static str {
    match shell {
        "cmd" => "cmd.exe",
        _ => "powershell.exe -NoLogo -NoProfile -NoExit -Command -",
    }
}

/// Copy a reader into the shared output buffer on a background thread.
fn pump(mut r: impl std::io::Read + Send + 'static, out: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    });
}

#[cfg(windows)]
fn spawn_as(shell: &str, run_as: &RunAs) -> Result<ShellProc> {
    super::super::jobs::winproc::spawn_interactive(shell, run_as)
}

#[cfg(not(windows))]
fn spawn_as(_shell: &str, _run_as: &RunAs) -> Result<ShellProc> {
    bail!("running the terminal as another user is only supported on Windows")
}
