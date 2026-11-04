//! Notification-area icon: closing the window hides it and the app keeps running.
//!
//! The icon belongs to its own hidden top-level window (not the main window), so it exists
//! before the main window does: an autostart launch (`--background`) never shows the main
//! window until the icon is used. Left click shows the window, right click opens a small menu
//! (열기 / 종료). If the icon can't be added (no taskbar yet) closing still quits and
//! `--background` shows the window anyway, so the app is never hidden with no way back.
//! Explorer's restart sends "TaskbarCreated": the icon is added again then.

#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
mod stub {
    pub fn install(_on_open: impl Fn() + 'static) {}
    pub fn remove() {}
    pub fn active() -> bool {
        false
    }
    pub fn hide_window(_hwnd: isize) {}
}
#[cfg(not(windows))]
pub use stub::*;

#[cfg(windows)]
mod win {
    use std::cell::{Cell, RefCell};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    /// Callback message of the icon (WM_APP + 1).
    const WM_TRAY: u32 = WM_APP + 1;
    const ID: u32 = 1;
    const CMD_OPEN: usize = 1;
    const CMD_QUIT: usize = 2;

    thread_local! {
        static ADDED: Cell<bool> = const { Cell::new(false) };
        static OWNER: Cell<isize> = const { Cell::new(0) };
        static ON_OPEN: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
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

    /// Creates the owner window and adds the icon. `on_open` runs on the Slint event loop
    /// when the user asks for the window.
    pub fn install(on_open: impl Fn() + 'static) {
        ON_OPEN.with_borrow_mut(|o| *o = Some(Box::new(on_open)));
        let class = wide("SigwinTray");
        // SAFETY: registers a class for our own hidden window; the proc only touches thread-locals
        let hwnd = unsafe {
            let inst = GetModuleHandleW(std::ptr::null());
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(proc);
            wc.hInstance = inst;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                inst,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            tracing::warn!("tray window not created");
            return;
        }
        OWNER.set(hwnd as isize);
        add_icon();
    }

    fn add_icon() {
        let hwnd = OWNER.get();
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

    pub fn remove() {
        if ADDED.replace(false) {
            // SAFETY: valid struct for our window
            unsafe { Shell_NotifyIconW(NIM_DELETE, &data(OWNER.get())) };
        }
    }

    /// Whether closing the window may hide it.
    pub fn active() -> bool {
        ADDED.get()
    }

    /// Hides the main window (taskbar button goes with it).
    pub fn hide_window(hwnd: isize) {
        // SAFETY: our own window
        unsafe { ShowWindow(hwnd as HWND, SW_HIDE) };
    }

    /// "TaskbarCreated": Explorer (re)started, notification-area icons are gone.
    fn taskbar_created() -> u32 {
        thread_local!(static ID: Cell<u32> = const { Cell::new(0) });
        if ID.get() == 0 {
            // SAFETY: NUL-terminated name
            ID.set(unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) });
        }
        ID.get()
    }

    fn open() {
        // not from inside the window procedure: showing the main window belongs to Slint's loop
        let _ = slint::invoke_from_event_loop(|| ON_OPEN.with_borrow(|o| o.as_ref().map(|f| f())).unwrap_or(()));
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            match lparam as u32 {
                WM_LBUTTONUP => open(),
                WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd),
                _ => {}
            }
            return 0;
        }
        if msg == taskbar_created() {
            add_icon();
            return 0;
        }
        // SAFETY: default handling of a message for our own window
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn menu(hwnd: HWND) {
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
            CMD_OPEN => open(),
            CMD_QUIT => {
                let _ = slint::quit_event_loop();
            }
            _ => {}
        }
    }
}
