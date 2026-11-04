//! PSCCM status icon for the notification area of the console server.
//!
//! Shows whether the PSCCM service runs and the web console answers, pops up
//! a notice when that changes, and offers Open console / Start / Stop /
//! Restart / View log (the service actions ask for elevation). The installer
//! starts it for every user at logon; one instance runs per session. It reads
//! the port from HKLM\SOFTWARE\PSCCM, since the database is admin-only.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("psccm-tray is for Windows; on Linux use: systemctl status psccm");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    tray::run();
}

#[cfg(windows)]
mod tray {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};
    use windows_service::service::{ServiceAccess, ServiceExitCode, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
        ShellExecuteW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    const SERVICE: &str = "PSCCM";
    const WM_TRAY: u32 = WM_APP + 1;
    const POLL: usize = 1;
    const SOON: usize = 2;
    const POLL_MS: u32 = 5000;

    const ID_OPEN: usize = 1;
    const ID_START: usize = 2;
    const ID_STOP: usize = 3;
    const ID_RESTART: usize = 4;
    const ID_LOG: usize = 5;
    const ID_EXIT: usize = 6;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Health {
        /// Service running and the console answers.
        Ok,
        /// Starting or stopping.
        Busy,
        /// Service running but nothing answers on the port.
        Silent,
        Stopped,
        Missing,
    }

    impl Health {
        fn color(self) -> u32 {
            match self {
                Health::Ok => 0x2E9E4F,
                Health::Busy | Health::Silent => 0xE0A000,
                Health::Stopped => 0xD13438,
                Health::Missing => 0x8A8A8A,
            }
        }
    }

    /// Where the console listens, as `psccm service install` recorded it.
    struct Config {
        port: u16,
        bind: String,
        data_dir: PathBuf,
    }

    impl Config {
        fn load() -> Config {
            Config {
                port: reg_dword("Port").and_then(|p| u16::try_from(p).ok()).unwrap_or(8080),
                bind: reg_string("Bind").unwrap_or_else(|| "127.0.0.1".into()),
                data_dir: reg_string("DataDir")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(std::env::var_os("ProgramData").unwrap_or_default()).join("PSCCM")),
            }
        }

        fn ip(&self) -> IpAddr {
            match self.bind.parse::<IpAddr>() {
                Ok(ip) if !ip.is_unspecified() => ip,
                _ => Ipv4Addr::LOCALHOST.into(),
            }
        }

        fn url(&self) -> String {
            let ip = self.ip();
            let host = if ip.is_loopback() { "localhost".to_string() } else { ip.to_string() };
            format!("http://{host}:{}/", self.port)
        }
    }

    struct State {
        hwnd: HWND,
        icons: [(Health, HICON); 5],
        health: Health,
        /// The last state other than Busy (None until the first check).
        settled: Option<Health>,
        text: String,
        cfg: Config,
        /// This program's file and its timestamp at start, to notice an upgrade.
        exe: PathBuf,
        stamp: Option<SystemTime>,
        changed: Option<SystemTime>,
        mutex: HANDLE,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    /// Run `f` on the state without holding the borrow across anything that
    /// pumps messages (menus, ShellExecute), which would re-enter the window
    /// procedure.
    fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
        STATE.with(|s| s.borrow_mut().as_mut().map(f))
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Copy `s` into a fixed, NUL-terminated buffer, cutting it if needed.
    fn fill(dst: &mut [u16], s: &str) {
        let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
        dst[..src.len()].copy_from_slice(&src);
        dst[src.len()] = 0;
    }

    fn reg_string(name: &str) -> Option<String> {
        let (key, val) = (wide(r"SOFTWARE\PSCCM"), wide(name));
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: buf is valid for `size` bytes.
        let rc = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size)
        };
        if rc != 0 {
            return None;
        }
        let len = (size as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..len])).filter(|s| !s.is_empty())
    }

    fn reg_dword(name: &str) -> Option<u32> {
        let (key, val) = (wide(r"SOFTWARE\PSCCM"), wide(name));
        let mut v = 0u32;
        let mut size = 4u32;
        // SAFETY: v is valid for 4 bytes.
        let rc = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), val.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut v as *mut u32).cast(), &mut size)
        };
        (rc == 0).then_some(v)
    }

    /// Does the console serve its page? A plain GET of "/" with a local Host
    /// header, which the console accepts with or without an access token.
    /// Returns the HTTP status: 403 means the console runs but lets in
    /// administrators only, and this user is not one.
    fn console_status(cfg: &Config) -> Option<u16> {
        let addr = SocketAddr::new(cfg.ip(), cfg.port);
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        write!(s, "GET / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n", cfg.port).ok()?;
        let mut head = [0u8; 12];
        s.read_exact(&mut head).ok()?;
        if !head.starts_with(b"HTTP/1.") {
            return None;
        }
        std::str::from_utf8(&head[9..12]).ok()?.parse().ok()
    }

    fn check(cfg: &Config) -> (Health, String) {
        let svc = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .and_then(|m| m.open_service(SERVICE, ServiceAccess::QUERY_STATUS))
            .and_then(|s| s.query_status());
        let Ok(st) = svc else {
            return (Health::Missing, "PSCCM service is not installed".into());
        };
        match st.current_state {
            ServiceState::Running => match console_status(cfg) {
                Some(200) => (Health::Ok, format!("PSCCM is running\n{}", cfg.url())),
                Some(403) => (Health::Ok, "PSCCM is running\nOnly administrators of this server can open the console".into()),
                _ => (Health::Silent, format!("PSCCM service runs, but nothing answers on port {}", cfg.port)),
            },
            ServiceState::StartPending | ServiceState::ContinuePending => (Health::Busy, "PSCCM is starting…".into()),
            ServiceState::StopPending | ServiceState::PausePending | ServiceState::Paused => (Health::Busy, "PSCCM is stopping…".into()),
            ServiceState::Stopped => {
                let failed = !matches!(st.exit_code, ServiceExitCode::Win32(0) | ServiceExitCode::ServiceSpecific(0));
                (Health::Stopped, if failed { "PSCCM stopped after an error (see View log)".into() } else { "PSCCM service is stopped".into() })
            }
        }
    }

    /// A round dot in the status colour, with a light rim so it shows on
    /// dark and light taskbars.
    fn dot_icon(rgb: u32) -> HICON {
        // SAFETY: plain metric query.
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as usize;
        let c = size as f32 / 2.0;
        let r = c - 0.5;
        let rim = (size as f32 / 10.0).max(1.0);
        let mut px = vec![0u32; size * size];
        for y in 0..size {
            for x in 0..size {
                let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
                let a = (r - d + 0.5).clamp(0.0, 1.0);
                if a == 0.0 {
                    continue;
                }
                let color = if d > r - rim { 0xF2F2F2 } else { rgb };
                px[y * size + x] = ((a * 255.0) as u32) << 24 | color;
            }
        }
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        };
        let info = BITMAPINFO { bmiHeader: header, ..Default::default() };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: the DIB section has room for size*size 32-bit pixels; both
        // bitmaps are copied by CreateIconIndirect and deleted afterwards.
        unsafe {
            let color = CreateDIBSection(std::ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if color.is_null() || bits.is_null() {
                return LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
            }
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits.cast::<u32>(), px.len());
            let mask = CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());
            let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            let icon = CreateIconIndirect(&ii);
            DeleteObject(color);
            DeleteObject(mask);
            icon
        }
    }

    fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    fn add_icon(st: &State) {
        let mut nid = notify_data(st.hwnd);
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = st.icon();
        fill(&mut nid.szTip, &st.text);
        // SAFETY: nid is fully initialised.
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    }

    impl State {
        fn icon(&self) -> HICON {
            self.icons.iter().find(|(h, _)| *h == self.health).map(|(_, i)| *i).unwrap_or(std::ptr::null_mut())
        }

        /// Re-check and update the icon; pop up a notice when the console
        /// goes down or comes back.
        fn refresh(&mut self) {
            self.cfg = Config::load();
            let (health, text) = check(&self.cfg);
            // Compared with the last settled state: a stop passes through Busy.
            let notice = match (self.settled, health) {
                (Some(Health::Ok), Health::Stopped | Health::Silent | Health::Missing) => Some(("PSCCM console is down", NIIF_WARNING)),
                (Some(Health::Stopped | Health::Silent), Health::Ok) => Some(("PSCCM console is running again", NIIF_INFO)),
                _ => None,
            };
            if health != Health::Busy {
                self.settled = Some(health);
            }
            if health == self.health && text == self.text {
                return;
            }
            self.health = health;
            self.text = text;
            let mut nid = notify_data(self.hwnd);
            nid.uFlags = NIF_ICON | NIF_TIP;
            nid.hIcon = self.icon();
            fill(&mut nid.szTip, &self.text);
            if let Some((title, flags)) = notice {
                nid.uFlags |= NIF_INFO;
                nid.dwInfoFlags = flags;
                fill(&mut nid.szInfoTitle, title);
                fill(&mut nid.szInfo, &self.text);
            }
            // SAFETY: nid is fully initialised.
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        }

        /// After an upgrade replaced this program's file, start the new one
        /// (stable for two polls, so a half-written file is never run).
        fn upgraded(&mut self) -> bool {
            let now = std::fs::metadata(&self.exe).and_then(|m| m.modified()).ok();
            if now.is_none() || now == self.stamp {
                self.changed = None;
                return false;
            }
            if self.changed != now {
                self.changed = now;
                return false;
            }
            true
        }
    }

    fn shell(hwnd: HWND, verb: &str, file: &str, params: &str, show: SHOW_WINDOW_CMD) {
        let (verb, file, params) = (wide(verb), wide(file), wide(params));
        // SAFETY: all strings are NUL-terminated and outlive the call.
        unsafe { ShellExecuteW(hwnd, verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), show) };
    }

    /// Start/stop the service through an elevated, hidden PowerShell (UAC prompt).
    fn control(hwnd: HWND, cmdlet: &str) {
        let params = format!("-NoProfile -NonInteractive -WindowStyle Hidden -Command {cmdlet} -Name {SERVICE}");
        shell(hwnd, "runas", "powershell.exe", &params, SW_HIDE);
        // SAFETY: plain timer on our window; checks again shortly after.
        unsafe { SetTimer(hwnd, SOON, 2000, None) };
    }

    fn show_menu(hwnd: HWND) {
        let Some((health, text)) = with(|s| (s.health, s.text.clone())) else { return };
        let running = matches!(health, Health::Ok | Health::Silent);
        let installed = health != Health::Missing;
        // SAFETY: menu handles are created, used and destroyed here.
        let cmd = unsafe {
            let menu = CreatePopupMenu();
            let item = |id: usize, label: &str, enabled: bool| {
                let l = wide(label);
                AppendMenuW(menu, MF_STRING | if enabled { 0 } else { MF_GRAYED }, id, l.as_ptr());
            };
            item(0, text.lines().next().unwrap_or("PSCCM"), false);
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            item(ID_OPEN, "Open console", running);
            item(ID_START, "Start service", installed && health == Health::Stopped);
            item(ID_STOP, "Stop service", running);
            item(ID_RESTART, "Restart service", running);
            item(ID_LOG, "View log", installed);
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            item(ID_EXIT, "Exit", true);
            SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // Needed so the menu closes when clicking elsewhere.
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, std::ptr::null());
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            cmd as usize
        };
        command(hwnd, cmd);
    }

    fn command(hwnd: HWND, cmd: usize) {
        match cmd {
            ID_OPEN => {
                if let Some(url) = with(|s| s.cfg.url()) {
                    shell(hwnd, "open", &url, "", SW_SHOWNORMAL);
                }
            }
            ID_START => control(hwnd, "Start-Service"),
            ID_STOP => control(hwnd, "Stop-Service"),
            ID_RESTART => control(hwnd, "Restart-Service"),
            ID_LOG => {
                // The data folder is admin-only, so open the log elevated.
                if let Some(log) = with(|s| s.cfg.data_dir.join("psccm.log")) {
                    shell(hwnd, "runas", "notepad.exe", &format!("\"{}\"", log.display()), SW_SHOWNORMAL);
                }
            }
            // SAFETY: destroying our own window ends the message loop.
            ID_EXIT => unsafe {
                DestroyWindow(hwnd);
            },
            _ => {}
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        // SAFETY: registering a named message is always safe.
        let taskbar_created = *TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) });
        match msg {
            WM_TRAY => {
                match lp as u32 {
                    WM_LBUTTONDBLCLK => command(hwnd, ID_OPEN),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    _ => {}
                }
                0
            }
            WM_TIMER => {
                if wp == SOON {
                    // SAFETY: one-shot timer on our window.
                    unsafe { KillTimer(hwnd, SOON) };
                }
                let restart = with(|s| {
                    s.refresh();
                    s.upgraded()
                });
                if restart == Some(true) {
                    relaunch(hwnd);
                }
                0
            }
            WM_DESTROY => {
                with(|s| {
                    let nid = notify_data(s.hwnd);
                    // SAFETY: removes our icon; the mutex handle is ours.
                    unsafe {
                        Shell_NotifyIconW(NIM_DELETE, &nid);
                        CloseHandle(s.mutex);
                    }
                });
                // SAFETY: ends the message loop.
                unsafe { PostQuitMessage(0) };
                0
            }
            // Explorer restarted: put the icon back.
            m if m == taskbar_created => {
                with(|s| add_icon(s));
                0
            }
            // SAFETY: default handling for everything else.
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    /// Hand over to the upgraded program file and quit.
    fn relaunch(hwnd: HWND) {
        let exe = with(|s| s.exe.clone());
        // Destroying the window releases the single-instance mutex first.
        // SAFETY: our own window.
        unsafe { DestroyWindow(hwnd) };
        if let Some(exe) = exe {
            let _ = std::process::Command::new(exe).spawn();
        }
    }

    /// Remove program files an upgrade renamed while they were running.
    fn clean_old(exe: &std::path::Path) {
        let Some(dir) = exe.parent() else { return };
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with("psccm-tray") && name.ends_with(".old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    pub fn run() {
        // SAFETY: Win32 setup and message loop on this thread.
        unsafe {
            let mutex = CreateMutexW(std::ptr::null(), 0, wide(r"Local\PSCCM-Tray").as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return;
            }
            let instance = GetModuleHandleW(std::ptr::null());
            let class = wide("PSCCMTray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            // A hidden top-level window (not message-only, which would miss TaskbarCreated).
            let hwnd = CreateWindowExW(0, class.as_ptr(), wide("PSCCM status").as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
            if hwnd.is_null() {
                return;
            }
            let exe = std::env::current_exe().unwrap_or_default();
            clean_old(&exe);
            let icons = [Health::Ok, Health::Busy, Health::Silent, Health::Stopped, Health::Missing].map(|h| (h, dot_icon(h.color())));
            let state = State {
                hwnd,
                icons,
                health: Health::Busy,
                settled: None,
                text:"PSCCM: checking…".into(),
                cfg: Config::load(),
                stamp: std::fs::metadata(&exe).and_then(|m| m.modified()).ok(),
                exe,
                changed: None,
                mutex,
            };
            add_icon(&state);
            STATE.with(|s| *s.borrow_mut() = Some(state));
            with(|s| s.refresh());
            SetTimer(hwnd, POLL, POLL_MS, None);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
