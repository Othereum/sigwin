//! Notification-area icon: closing the window hides it and the app keeps running.
//!
//! The icon reports to the main window (callback message `WM_TRAY`, handled in `winhook`).
//! Left click shows the window, right click opens a small menu (열기 / 종료). If the icon
//! can't be added (no taskbar yet) closing still quits, so the app is never hidden with no
//! way back. Explorer's restart sends "TaskbarCreated": the icon is added again then.

#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
mod stub {
    pub fn install(_hwnd: isize) {}
    pub fn remove() {}
    pub fn active() -> bool {
        false
    }
}
#[cfg(not(windows))]
pub use stub::*;

#[cfg(windows)]
mod win {
    use std::cell::Cell;

    use windows_sys::Win32::Foundation::{HWND, POINT};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    /// Callback message of the icon (WM_APP + 1).
    pub const WM_TRAY: u32 = WM_APP + 1;
    const ID: u32 = 1;
    const CMD_OPEN: usize = 1;
    const CMD_QUIT: usize = 2;

    thread_local! {
        static ADDED: Cell<bool> = const { Cell::new(false) };
        static HWND_: Cell<isize> = const { Cell::new(0) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn data(hwnd: isize) -> NOTIFYICONDATAW {
        // SAFETY: plain C struct, all-zero is a valid empty value
        let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = hwnd as HWND;
        d.uID = ID;
        d
    }

    /// Adds the icon (again). Remembers the window for later retries.
    pub fn install(hwnd: isize) {
        HWND_.set(hwnd);
        if hwnd == 0 {
            return;
        }
        // SAFETY: loads our own icon resource (id 1, set in build.rs) at the small-icon size;
        // shared icons need no cleanup
        let icon = unsafe {
            let inst = GetModuleHandleW(std::ptr::null());
            let size = GetSystemMetrics(SM_CXSMICON);
            LoadImageW(inst, 1 as _, IMAGE_ICON, size, size, LR_SHARED)
        };
        let mut d = data(hwnd);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = icon as _;
        for (dst, src) in d.szTip.iter_mut().zip(wide("Sigwin")) {
            *dst = src;
        }
        // SAFETY: valid struct for our window
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &d) } != 0;
        ADDED.set(ok);
        if !ok {
            tracing::warn!("tray icon not added");
        }
    }

    /// Re-adds the icon after Explorer restarted.
    pub fn reinstall() {
        install(HWND_.get());
    }

    pub fn remove() {
        if ADDED.replace(false) {
            // SAFETY: valid struct for our window
            unsafe { Shell_NotifyIconW(NIM_DELETE, &data(HWND_.get())) };
        }
    }

    /// Whether closing the window may hide it.
    pub fn active() -> bool {
        ADDED.get()
    }

    /// Hides the main window (taskbar button goes with it).
    pub fn hide_window(hwnd: HWND) {
        // SAFETY: our own window
        unsafe { ShowWindow(hwnd, SW_HIDE) };
    }

    /// Mouse message from the icon.
    pub fn on_event(hwnd: HWND, lparam: isize, show: impl FnOnce()) {
        match lparam as u32 {
            WM_LBUTTONUP => show(),
            WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd, show),
            _ => {}
        }
    }

    fn menu(hwnd: HWND, show: impl FnOnce()) {
        // SAFETY: popup menu owned here, destroyed before returning
        let cmd = unsafe {
            let m = CreatePopupMenu();
            AppendMenuW(m, MF_STRING, CMD_OPEN, wide("열기").as_ptr());
            AppendMenuW(m, MF_STRING, CMD_QUIT, wide("종료").as_ptr());
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            // the menu only closes on an outside click when our window is foreground
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, 0, hwnd, std::ptr::null());
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(m);
            cmd as usize
        };
        match cmd {
            CMD_OPEN => show(),
            CMD_QUIT => {
                let _ = slint::quit_event_loop();
            }
            _ => {}
        }
    }
}
