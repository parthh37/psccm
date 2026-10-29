//! Windows run-as: launch a process under another identity.
//!
//! Two paths, both requiring the agent to run as LocalSystem for full effect:
//! - **password runas** via `CreateProcessWithLogonW`, for any local/domain
//!   account whether or not it is logged on;
//! - **impersonate** the interactive (console-session) user via
//!   `WTSQueryUserToken` + `CreateProcessAsUserW`, with no password.
//!
//! Both redirect the child's stdio so the console can capture output or drive
//! an interactive shell. The unsafe FFI is confined to this module.

#![cfg(windows)]

use anyhow::{Result, anyhow, bail};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, SetHandleInformation};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, TokenPrimary,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessWithLogonW, GetExitCodeProcess, PROCESS_INFORMATION,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

use crate::agent::RunAs;
use crate::agent::terminal::shell::{ShellProc, shell_command_line};

const HANDLE_FLAG_INHERIT: u32 = 1;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
const LOGON_WITH_PROFILE: u32 = 1;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const STILL_ACTIVE: u32 = 259;

/// A process handle that is safe to move across threads (we only ever call
/// thread-safe Win32 APIs — `WaitForSingleObject`, `TerminateProcess` — on it).
struct Proc(HANDLE);
unsafe impl Send for Proc {}
unsafe impl Sync for Proc {}
impl Drop for Proc {
    fn drop(&mut self) {
        // SAFETY: self.0 is a process handle we own.
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Split `DOMAIN\user`, `user@domain` or bare `user` into (user, domain).
fn split_user(user: &str) -> (String, Option<String>) {
    if let Some((dom, u)) = user.split_once('\\') {
        (u.to_string(), Some(dom.to_string()))
    } else if user.contains('@') {
        (user.to_string(), None)
    } else {
        (user.to_string(), Some(".".to_string()))
    }
}

/// An inheritable anonymous pipe. The parent end is marked non-inheritable.
struct Pipe {
    read: HANDLE,
    write: HANDLE,
}

fn make_pipe(parent_end_is_read: bool) -> Result<Pipe> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: valid out-pointers and attributes.
    if unsafe { CreatePipe(&mut read, &mut write, &sa, 0) } == 0 {
        bail!("CreatePipe failed: {}", std::io::Error::last_os_error());
    }
    let parent = if parent_end_is_read { read } else { write };
    // SAFETY: parent is a handle just returned by CreatePipe.
    if unsafe { SetHandleInformation(parent, HANDLE_FLAG_INHERIT, 0) } == 0 {
        bail!("SetHandleInformation failed: {}", std::io::Error::last_os_error());
    }
    Ok(Pipe { read, write })
}

/// Wire up three pipes and launch `cmdline` under `run_as`, returning the live
/// [`ShellProc`] (parent stdin writer + stdout reader + process control).
pub fn spawn_interactive(shell: &str, run_as: &RunAs) -> Result<ShellProc> {
    let cmdline = shell_command_line(shell).to_string();
    let stdin = make_pipe(false)?; // child reads, parent writes
    let stdout = make_pipe(true)?; // child writes, parent reads
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = stdin.read;
    si.hStdOutput = stdout.write;
    si.hStdError = stdout.write;

    let pi = create_process(&cmdline, run_as, &si)?;

    // SAFETY: child-side ends are no longer needed in the parent.
    unsafe {
        CloseHandle(stdin.read);
        CloseHandle(stdout.write);
        CloseHandle(pi.hThread);
    }
    // SAFETY: the parent ends are valid handles we now own exclusively.
    let writer = unsafe { std::fs::File::from_raw_handle(stdin.write as RawHandle) };
    let reader = unsafe { std::fs::File::from_raw_handle(stdout.read as RawHandle) };

    let proc = Arc::new(Proc(pi.hProcess));
    let (p_kill, p_alive) = (proc.clone(), proc);
    Ok(ShellProc::from_parts(
        Box::new(writer),
        reader,
        Box::new(move || {
            // SAFETY: p_kill.0 is our process handle.
            unsafe { TerminateProcess(p_kill.0, 1) };
        }),
        Box::new(move || {
            // SAFETY: p_alive.0 is our process handle.
            unsafe { WaitForSingleObject(p_alive.0, 0) == WAIT_TIMEOUT }
        }),
    ))
}

/// Run a non-interactive script under `run_as`, capturing combined output.
pub fn run_capture(shell: &str, script: &str, run_as: &RunAs) -> Result<(i32, String)> {
    let dir = std::env::temp_dir().join("psccm-agent");
    std::fs::create_dir_all(&dir)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let (ext, cmdline_for) = match shell {
        "powershell" | "" => ("ps1", true),
        "cmd" => ("cmd", false),
        other => bail!("running a {other} command as another user is not supported on Windows"),
    };
    let script_path = dir.join(format!("{id}.{ext}"));
    let out_path = dir.join(format!("{id}.out"));
    if cmdline_for {
        std::fs::write(&script_path, [b"\xEF\xBB\xBF".as_slice(), script.as_bytes()].concat())?;
    } else {
        std::fs::write(&script_path, script)?;
    }
    let _g1 = super_remove(script_path.clone());
    let _g2 = super_remove(out_path.clone());
    let cmdline = if cmdline_for {
        format!("powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"", script_path.display())
    } else {
        format!("cmd.exe /d /c \"{}\"", script_path.display())
    };

    // Child stdout/stderr -> an inheritable file; stdin -> NUL.
    let out_file = inheritable_file_for_write(&out_path)?;
    let nul = inheritable_nul()?;
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = nul;
    si.hStdOutput = out_file;
    si.hStdError = out_file;

    let pi = create_process(&cmdline, run_as, &si)?;
    // SAFETY: parent copies of the inherited handles are no longer needed.
    unsafe {
        CloseHandle(out_file);
        CloseHandle(nul);
        CloseHandle(pi.hThread);
    }
    let proc = Proc(pi.hProcess);
    // Wait up to 30 minutes, matching the default step timeout.
    // SAFETY: proc.0 is our process handle.
    let waited = unsafe { WaitForSingleObject(proc.0, 30 * 60 * 1000) };
    let mut code: u32 = 0;
    // SAFETY: valid handle and out-pointer.
    unsafe { GetExitCodeProcess(proc.0, &mut code) };
    if waited == WAIT_TIMEOUT || code == STILL_ACTIVE {
        // SAFETY: proc.0 is our process handle.
        unsafe { TerminateProcess(proc.0, 1) };
        bail!("command as {} timed out", run_as.label());
    }
    let output = std::fs::read_to_string(&out_path).unwrap_or_default();
    Ok((code as i32, output))
}

/// Shared launcher: `CreateProcessWithLogonW` (password) or
/// `CreateProcessAsUserW` (impersonate the console user).
fn create_process(cmdline: &str, run_as: &RunAs, si: &STARTUPINFOW) -> Result<PROCESS_INFORMATION> {
    let mut cmd = wide(cmdline);
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;

    if run_as.impersonate {
        let token = console_user_token()?;
        // SAFETY: token is a primary token we own; cmd is NUL-terminated and
        // writable; si/pi are valid. bInheritHandles = TRUE for the std pipes.
        let ok = unsafe {
            CreateProcessAsUserW(
                token.0,
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                std::ptr::null(),
                std::ptr::null(),
                si,
                &mut pi,
            )
        };
        if ok == 0 {
            bail!("CreateProcessAsUser failed: {}", std::io::Error::last_os_error());
        }
        return Ok(pi);
    }

    let user = run_as.user.as_deref().filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("a user is required"))?;
    let password = run_as.password.as_deref().unwrap_or("");
    let (username, domain) = split_user(user);
    let user_w = wide(&username);
    let domain_w = domain.as_deref().map(wide);
    let pass_w = wide(password);
    // SAFETY: all string pointers are NUL-terminated and live for the call;
    // cmd is writable; si/pi are valid.
    let ok = unsafe {
        CreateProcessWithLogonW(
            user_w.as_ptr(),
            domain_w.as_ref().map(|d| d.as_ptr()).unwrap_or(std::ptr::null()),
            pass_w.as_ptr(),
            LOGON_WITH_PROFILE,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            flags,
            std::ptr::null(),
            std::ptr::null(),
            si,
            &mut pi,
        )
    };
    if ok == 0 {
        bail!("CreateProcessWithLogon failed for {user}: {}", std::io::Error::last_os_error());
    }
    Ok(pi)
}

/// A primary token for the interactive (console-session) user.
fn console_user_token() -> Result<Proc> {
    // SAFETY: returns the active console session id (or 0xFFFFFFFF if none).
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        bail!("no interactive user is logged on (no active console session)");
    }
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: valid out-pointer; requires the caller to be LocalSystem.
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        bail!(
            "cannot get the logged-on user's token (the agent must run as LocalSystem): {}",
            std::io::Error::last_os_error()
        );
    }
    let primary_holder = Proc(token);
    let mut primary: HANDLE = std::ptr::null_mut();
    // SAFETY: token is valid; produce a primary token for CreateProcessAsUser.
    let ok = unsafe {
        DuplicateTokenEx(
            primary_holder.0,
            MAXIMUM_ALLOWED,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    if ok == 0 {
        bail!("DuplicateTokenEx failed: {}", std::io::Error::last_os_error());
    }
    Ok(Proc(primary))
}

/// Open a file for writing with an inheritable handle.
fn inheritable_file_for_write(path: &std::path::Path) -> Result<HANDLE> {
    use std::os::windows::io::IntoRawHandle;
    let f = std::fs::File::create(path)?;
    let h = f.into_raw_handle() as HANDLE;
    make_inheritable(h)?;
    Ok(h)
}

fn inheritable_nul() -> Result<HANDLE> {
    use std::os::windows::io::IntoRawHandle;
    let f = std::fs::OpenOptions::new().read(true).open("NUL")?;
    let h = f.into_raw_handle() as HANDLE;
    make_inheritable(h)?;
    Ok(h)
}

fn make_inheritable(h: HANDLE) -> Result<()> {
    if h == INVALID_HANDLE_VALUE {
        bail!("invalid handle");
    }
    // SAFETY: h is a valid handle we own.
    if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        bail!("SetHandleInformation failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Delete a temp file when dropped (distinct name to avoid clashing with the
/// sibling helper in `mod.rs`).
fn super_remove(path: std::path::PathBuf) -> impl Drop {
    struct R(std::path::PathBuf);
    impl Drop for R {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    R(path)
}
