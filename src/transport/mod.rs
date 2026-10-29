//! Agentless remote execution: WinRM (PowerShell remoting) for Windows and
//! SSH for Linux.

pub mod ssh;
pub mod winrm;

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpStream;

use crate::package::{Os, Shell};

/// Receives each output line from the remote side.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Run a complete script (already wrapped, see [`wrap_script`]) and
    /// return its exit code.
    async fn run(&self, script: &str, shell: Shell, timeout: Duration, log: &LogFn) -> Result<i32>;

    /// Copy a local file to an absolute path on the target, creating parent
    /// directories.
    async fn upload(&self, local: &Path, remote: &str, timeout: Duration, log: &LogFn) -> Result<()>;

    /// Forget any cached connection (used after a reboot).
    async fn reset(&self) {}
}

pub const PRELUDE_PS: &str = include_str!("../assets/prelude.ps1");
pub const PRELUDE_SH: &str = include_str!("../assets/prelude.sh");

/// Add the helper prelude and exit-code handling to a user script.
pub fn wrap_script(shell: Shell, body: &str) -> String {
    match shell {
        Shell::Powershell => format!(
            "{PRELUDE_PS}\ntry {{\n{body}\n}} catch {{\n    Write-Output \"ERROR: $($_.Exception.Message)\"\n    Write-Output $_.InvocationInfo.PositionMessage\n    exit 1\n}}\nif ($LASTEXITCODE) {{ exit $LASTEXITCODE }}\nexit 0\n"
        ),
        Shell::Cmd => format!("@echo off\r\n{}\r\n", body.replace("\r\n", "\n").replace('\n', "\r\n")),
        Shell::Bash => format!("#!/bin/bash\n{PRELUDE_SH}\n{body}\n"),
        Shell::Sh => format!("#!/bin/sh\n{body}\n"),
    }
}

pub fn default_port(os: Os, winrm_ssl: bool) -> u16 {
    match os {
        Os::Windows if winrm_ssl => 5986,
        Os::Windows => 5985,
        Os::Linux => 22,
    }
}

pub async fn port_open(host: &str, port: u16, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

/// Guess the OS of a host from which management port answers.
pub async fn detect_os(host: &str, winrm_ssl: bool) -> Option<Os> {
    let t = Duration::from_secs(3);
    let (win, lin) = tokio::join!(
        port_open(host, default_port(Os::Windows, winrm_ssl), t),
        port_open(host, 22, t)
    );
    match (win, lin) {
        (true, _) => Some(Os::Windows),
        (false, true) => Some(Os::Linux),
        _ => None,
    }
}

/// Quote a string for POSIX sh.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Quote a string as a PowerShell single-quoted literal. PowerShell also ends
/// such a literal at the typographic quotes U+2018..U+201B, so those are
/// doubled as well.
pub fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Read `reader` line by line (lossy UTF-8) and hand each line to `f`.
pub async fn pump_lines<R: AsyncRead + Unpin>(reader: R, mut f: impl FnMut(String)) {
    let mut r = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                f(line.trim_end_matches(['\r', '\n']).to_string());
            }
        }
    }
}

/// Splits a byte stream into lines, for transports that deliver chunks.
#[derive(Default)]
pub struct LineBuffer(Vec<u8>);

impl LineBuffer {
    pub fn push(&mut self, data: &[u8], log: &LogFn) {
        self.0.extend_from_slice(data);
        while let Some(pos) = self.0.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.0.drain(..=pos).collect();
            log(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']));
        }
    }

    pub fn flush(&mut self, log: &LogFn) {
        if !self.0.is_empty() {
            log(String::from_utf8_lossy(&self.0).trim_end());
            self.0.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
    }

    #[test]
    fn line_buffer_splits_chunks() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = lines.clone();
        let log: LogFn = Arc::new(move |s: &str| l2.lock().unwrap().push(s.to_string()));
        let mut b = LineBuffer::default();
        b.push(b"hel", &log);
        b.push(b"lo\r\nwor", &log);
        b.push(b"ld\n!", &log);
        b.flush(&log);
        assert_eq!(*lines.lock().unwrap(), ["hello", "world", "!"]);
    }
}
