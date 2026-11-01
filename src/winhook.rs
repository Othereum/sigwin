//! Window-procedure subclass for events Slint doesn't expose.
//!
//! - Restore from minimized: the software renderer redraws only damaged regions, and when
//!   the UI changed while minimized it loses track of them. The restored window then shows
//!   only the last-changed bits over a transparent background. We force a full repaint.
//! - Activation changes: tell the backend whether the window is in the foreground
//!   (replaces polling).

use std::cell::{Cell, RefCell};

use slint::ComponentHandle;

use crate::AppWindow;

thread_local! {
    static APP: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static MINIMIZED: Cell<bool> = const { Cell::new(false) };
    static ON_FOCUS: RefCell<Option<Box<dyn Fn(bool)>>> = RefCell::new(None);
}

/// Flip the window background between two identical-looking colors: repaints everything.
pub fn repaint_all(app: &AppWindow) {
    app.set_repaint_flip(!app.get_repaint_flip());
}

fn schedule_repaint() {
    // run after winit has finished handling the message (and resized its buffer)
    let _ = slint::invoke_from_event_loop(|| {
        APP.with_borrow(|a| {
            if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                repaint_all(&app);
            }
        });
    });
    // and once more shortly after, in case a frame was presented in between
    slint::Timer::single_shot(std::time::Duration::from_millis(120), || {
        APP.with_borrow(|a| {
            if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                repaint_all(&app);
            }
        });
    });
}

#[cfg(windows)]
pub fn install(app: &AppWindow, hwnd: isize, on_focus: impl Fn(bool) + 'static) -> bool {
    use windows_sys::Win32::UI::Shell::SetWindowSubclass;
    APP.with_borrow_mut(|a| *a = Some(app.as_weak()));
    ON_FOCUS.with_borrow_mut(|f| *f = Some(Box::new(on_focus)));
    if hwnd == 0 {
        return false;
    }
    // SAFETY: subclassing our own window on its UI thread; the proc only touches thread-locals
    unsafe { SetWindowSubclass(hwnd as _, Some(subclass_proc), 1, 0) != 0 }
}

#[cfg(windows)]
unsafe extern "system" fn subclass_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::Shell::DefSubclassProc;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED, WA_INACTIVE, WM_ACTIVATE, WM_DPICHANGED, WM_SIZE,
    };
    match msg {
        WM_SIZE => match wparam as u32 {
            SIZE_MINIMIZED => MINIMIZED.set(true),
            SIZE_RESTORED | SIZE_MAXIMIZED if MINIMIZED.get() => {
                MINIMIZED.set(false);
                schedule_repaint();
            }
            _ => {}
        },
        WM_ACTIVATE => {
            let active = (wparam & 0xffff) as u32 != WA_INACTIVE;
            // WM_ACTIVATE's high word is non-zero while minimized
            let minimized = (wparam >> 16) & 0xffff != 0;
            ON_FOCUS.with_borrow(|f| {
                if let Some(f) = f {
                    f(active && !minimized);
                }
            });
            if active {
                schedule_repaint();
            }
        }
        WM_DPICHANGED => crate::badge::refresh(),
        m if m == taskbar_created() => crate::badge::refresh(),
        _ => {}
    }
    // SAFETY: forwarding to the next handler in the subclass chain
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// "TaskbarButtonCreated": the taskbar button exists (again, after an Explorer restart).
#[cfg(windows)]
fn taskbar_created() -> u32 {
    thread_local!(static ID: Cell<u32> = const { Cell::new(0) });
    if ID.get() == 0 {
        let name: Vec<u16> = "TaskbarButtonCreated".encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name
        ID.set(unsafe { windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(name.as_ptr()) });
    }
    ID.get()
}

#[cfg(not(windows))]
pub fn install(app: &AppWindow, _hwnd: isize, _on_focus: impl Fn(bool) + 'static) -> bool {
    APP.with_borrow_mut(|a| *a = Some(app.as_weak()));
    false
}
