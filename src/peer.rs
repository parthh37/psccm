//! Who is on the other end of a loopback connection to the console.
//!
//! A console without an access token trusts whoever can reach 127.0.0.1,
//! which on a shared server is everyone signed in to it. When the console
//! runs as the Windows service (or `serve --admins-only`), it asks Windows
//! which process owns the client end of each connection and lets in only
//! Administrators of this computer, SYSTEM and the account PSCCM runs as.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;

/// Connection info axum keeps for every accepted connection: both ends,
/// plus the verdict, worked out on the first request and kept for the
/// lifetime of the connection.
#[derive(Clone, Debug)]
pub struct Conn {
    remote: SocketAddr,
    local: Option<SocketAddr>,
    verdict: Arc<OnceLock<Result<String, String>>>,
}

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for Conn {
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Conn::new(*stream.remote_addr(), stream.io().local_addr().ok())
    }
}

impl Conn {
    fn new(remote: SocketAddr, local: Option<SocketAddr>) -> Conn {
        Conn { remote, local, verdict: Arc::default() }
    }

    /// `Ok(account)` when the account that opened this connection may use
    /// the console, otherwise `Err(why not)`.
    pub fn verdict(&self) -> &Result<String, String> {
        self.verdict.get_or_init(|| {
            let v = check(self.remote, self.local);
            if let Err(why) = &v {
                tracing::warn!("refused a console connection from {}: {why}", self.remote);
            }
            v
        })
    }
}

/// Well-known SIDs in their binary form.
const SID_ADMINISTRATORS: &[u8] = &[1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 0x02, 0, 0]; // S-1-5-32-544
const SID_SYSTEM: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]; // S-1-5-18

/// The rule: SYSTEM, the account PSCCM runs as, or a member of the local
/// Administrators group. Membership counts even when UAC filtered the group
/// to deny-only: a browser started by an administrator runs unelevated.
pub fn allowed(user: &[u8], groups: &[Vec<u8>], me: &[u8]) -> bool {
    user == SID_SYSTEM || user == me || groups.iter().any(|g| g == SID_ADMINISTRATORS)
}

fn check(remote: SocketAddr, local: Option<SocketAddr>) -> Result<String, String> {
    let local = local.ok_or("the console's own address is unknown")?;
    if !remote.ip().is_loopback() {
        return Err("the connection does not come from this computer".into());
    }
    imp::check(remote, local)
}

/// Get ready to look at other users' processes; false when this process
/// cannot, so every other user (administrators included) would be refused.
pub fn prepare() -> bool {
    imp::prepare()
}

#[cfg(windows)]
mod imp {
    use super::{SID_SYSTEM, allowed};
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, LUID};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
    };
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, GetLengthSid, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupAccountSidW, LookupPrivilegeValueW, PSID,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
        TokenGroups, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};

    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;

    pub fn check(remote: SocketAddr, local: SocketAddr) -> Result<String, String> {
        let pid = owner_pid(remote, local).map_err(|e| format!("could not tell which program opened the connection: {e}"))?;
        let who = identity(pid).map_err(|e| format!("could not tell which account opened the connection (process {pid}): {e}"))?;
        let me = me().ok_or("could not read the account PSCCM runs as")?;
        if allowed(&who.user, &who.groups, me) {
            Ok(who.name)
        } else {
            Err(format!("{} is not an administrator of this computer", who.name))
        }
    }

    pub fn prepare() -> bool {
        let debug = enable_debug_privilege();
        debug || me().is_some_and(|m| m == SID_SYSTEM)
    }

    /// The SID of the account this process runs as.
    fn me() -> Option<&'static [u8]> {
        static ME: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
        ME.get_or_init(|| identity(std::process::id()).ok().map(|i| i.user)).as_deref()
    }

    /// The process that owns the TCP connection `client` → `server`, as
    /// seen from the client end.
    pub fn owner_pid(client: SocketAddr, server: SocketAddr) -> io::Result<u32> {
        let found = match (client, server) {
            (SocketAddr::V4(c), SocketAddr::V4(s)) => {
                let buf = tcp_table(AF_INET)?;
                // SAFETY: the buffer holds a MIB_TCPTABLE_OWNER_PID with dwNumEntries rows.
                let rows = unsafe {
                    let t = buf.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
                    std::slice::from_raw_parts((*t).table.as_ptr(), (*t).dwNumEntries as usize)
                };
                let v4 = |addr: u32, port: u32| SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr.to_ne_bytes())), port_of(port));
                rows.iter()
                    .find(|r: &&MIB_TCPROW_OWNER_PID| {
                        v4(r.dwLocalAddr, r.dwLocalPort) == SocketAddr::V4(c) && v4(r.dwRemoteAddr, r.dwRemotePort) == SocketAddr::V4(s)
                    })
                    .map(|r| r.dwOwningPid)
            }
            (SocketAddr::V6(c), SocketAddr::V6(s)) => {
                let buf = tcp_table(AF_INET6)?;
                // SAFETY: the buffer holds a MIB_TCP6TABLE_OWNER_PID with dwNumEntries rows.
                let rows = unsafe {
                    let t = buf.as_ptr().cast::<MIB_TCP6TABLE_OWNER_PID>();
                    std::slice::from_raw_parts((*t).table.as_ptr(), (*t).dwNumEntries as usize)
                };
                let v6 = |addr: [u8; 16], port: u32| (Ipv6Addr::from(addr), port_of(port));
                rows.iter()
                    .find(|r: &&MIB_TCP6ROW_OWNER_PID| {
                        v6(r.ucLocalAddr, r.dwLocalPort) == (*c.ip(), c.port()) && v6(r.ucRemoteAddr, r.dwRemotePort) == (*s.ip(), s.port())
                    })
                    .map(|r| r.dwOwningPid)
            }
            _ => None,
        };
        found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not in the TCP table"))
    }

    /// Ports sit in network byte order in the low 16 bits.
    fn port_of(p: u32) -> u16 {
        u16::from_be(p as u16)
    }

    /// The connected TCP sockets of one address family with their owners.
    fn tcp_table(af: u32) -> io::Result<Vec<u64>> {
        let mut size = 16 * 1024u32;
        for _ in 0..8 {
            // u64 elements keep the rows aligned.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let mut len = (buf.len() * 8) as u32;
            // SAFETY: buf is valid for `len` bytes.
            let rc = unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut len, 0, af, TCP_TABLE_OWNER_PID_CONNECTIONS, 0) };
            match rc {
                0 => return Ok(buf),
                ERROR_INSUFFICIENT_BUFFER => size = len + 4096, // connections may appear meanwhile
                e => return Err(io::Error::from_raw_os_error(e as i32)),
            }
        }
        Err(io::Error::other("the TCP table keeps growing"))
    }

    /// The account a process runs as and the groups in its token.
    pub struct Identity {
        pub name: String,
        pub user: Vec<u8>,
        pub groups: Vec<Vec<u8>>,
    }

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub fn identity(pid: u32) -> io::Result<Identity> {
        // SAFETY: plain handle calls; both handles are closed by `Handle`.
        let token = unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let process = Handle(process);
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(process.0, TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            Handle(token)
        };
        let user_buf = token_info(&token, TokenUser)?;
        let group_buf = token_info(&token, TokenGroups)?;
        // SAFETY: the buffers hold a TOKEN_USER and a TOKEN_GROUPS whose SIDs
        // point into the same buffers.
        unsafe {
            let user = sid_bytes((*user_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid);
            let g = group_buf.as_ptr().cast::<TOKEN_GROUPS>();
            let groups = std::slice::from_raw_parts((*g).Groups.as_ptr(), (*g).GroupCount as usize)
                .iter()
                .map(|sa| sid_bytes(sa.Sid))
                .collect();
            Ok(Identity { name: account_name(&user), user, groups })
        }
    }

    fn token_info(token: &Handle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
        let mut len = 0u32;
        // SAFETY: a size query with no buffer.
        unsafe { GetTokenInformation(token.0, class, std::ptr::null_mut(), 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: buf is valid for at least `len` bytes.
        if unsafe { GetTokenInformation(token.0, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// # Safety
    /// `sid` must point at a valid SID.
    unsafe fn sid_bytes(sid: PSID) -> Vec<u8> {
        // SAFETY: per the contract, GetLengthSid bytes are readable at `sid`.
        unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec() }
    }

    /// `DOMAIN\user`, or "an unknown account".
    fn account_name(sid: &[u8]) -> String {
        let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
        let (mut name_len, mut domain_len, mut kind) = (name.len() as u32, domain.len() as u32, 0);
        // SAFETY: the buffers are valid for the lengths passed; LookupAccountSid does not write to the SID.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                sid.as_ptr() as PSID,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if ok == 0 {
            return "an unknown account".into();
        }
        let (name, domain) = (String::from_utf16_lossy(&name[..name_len as usize]), String::from_utf16_lossy(&domain[..domain_len as usize]));
        if domain.is_empty() { name } else { format!("{domain}\\{name}") }
    }

    /// Administrators hold SeDebugPrivilege, which opens other users'
    /// processes, but it is off until enabled. Local System has it on.
    fn enable_debug_privilege() -> bool {
        let name: Vec<u16> = "SeDebugPrivilege".encode_utf16().chain(Some(0)).collect();
        // SAFETY: plain token calls on our own process; the handle is closed by `Handle`.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let token = Handle(token);
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) == 0 {
                return false;
            }
            let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
            // Succeeds with ERROR_NOT_ALL_ASSIGNED when the privilege is not held.
            AdjustTokenPrivileges(token.0, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) != 0 && GetLastError() == 0
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::net::SocketAddr;

    pub fn check(_: SocketAddr, _: SocketAddr) -> Result<String, String> {
        Err("checking who opened a connection is only supported on Windows".into())
    }

    pub fn prepare() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &[u8] = &[1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xe9, 0x03, 0, 0];
    const OTHER: &[u8] = &[1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xea, 0x03, 0, 0];
    const USERS: &[u8] = &[1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x21, 0x02, 0, 0]; // S-1-5-32-545

    #[test]
    fn only_admins_system_and_self_are_allowed() {
        let plain = vec![USERS.to_vec()];
        let admin = vec![USERS.to_vec(), SID_ADMINISTRATORS.to_vec()];
        assert!(!allowed(OTHER, &plain, USER), "an ordinary user");
        assert!(allowed(OTHER, &admin, USER), "a member of Administrators");
        assert!(allowed(USER, &plain, USER), "the account PSCCM runs as");
        assert!(allowed(SID_SYSTEM, &[], USER), "Local System");
        assert!(!allowed(OTHER, &[], SID_SYSTEM), "a user when PSCCM runs as Local System");
    }

    #[test]
    fn non_loopback_peers_are_refused() {
        let c = Conn::new("10.0.0.5:50000".parse().unwrap(), Some("10.0.0.1:8080".parse().unwrap()));
        assert!(c.verdict().is_err());
        assert!(Conn::new("127.0.0.1:50000".parse().unwrap(), None).verdict().is_err());
    }

    /// A connection this test opens to itself belongs to this process, and
    /// so to the account PSCCM runs as, which is let in.
    #[cfg(windows)]
    #[test]
    fn own_connection_is_found_and_allowed() {
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let Ok(listener) = std::net::TcpListener::bind(bind) else { continue }; // no IPv6 loopback
            let server = listener.local_addr().unwrap();
            let client = std::net::TcpStream::connect(server).unwrap();
            let (_accepted, remote) = listener.accept().unwrap();
            assert_eq!(remote, client.local_addr().unwrap());
            assert_eq!(imp::owner_pid(remote, server).unwrap(), std::process::id(), "{bind}");
            let who = imp::identity(std::process::id()).unwrap();
            assert!(!who.user.is_empty() && !who.groups.is_empty());
            let conn = Conn::new(remote, Some(server));
            assert_eq!(conn.verdict().as_deref(), Ok(who.name.as_str()), "{bind}");
        }
    }
}
