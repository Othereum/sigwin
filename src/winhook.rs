//! Window-procedure subclass for events Slint doesn't expose.
//!
//! - Restore from minimized: the software renderer redraws only damaged regions, and when
//!   the UI changed while minimized it loses track of them. The restored window then shows
//!   only the last-changed bits over a transparent background. We force a full repaint.
//!   Sticker animations pause while minimized for the same reason (`gui::anim::pause`).
//! - Activation changes: tell the backend whether the window is in the foreground
//!   (replaces polling).
//! - The monitor turned off or on: the user is away (`away`).
//! - DPI change: winit resets the scale factor to the monitor's; re-apply the zoom setting.

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
    let ok = unsafe { SetWindowSubclass(hwnd as _, Some(subclass_proc), 1, 0) != 0 };
    // SAFETY: the window is ours; the notification goes to it until it is destroyed
    unsafe {
        windows_sys::Win32::System::Power::RegisterPowerSettingNotification(
            hwnd as _,
            &GUID_CONSOLE_DISPLAY_STATE,
            0, // DEVICE_NOTIFY_WINDOW_HANDLE
        );
    }
    ok
}

/// The monitor's power state: `POWERBROADCAST_SETTING::Data` is 0 off, 1 on, 2 dimmed.
#[cfg(windows)]
const GUID_CONSOLE_DISPLAY_STATE: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);

/// `POWERBROADCAST_SETTING` (only registered for the display state).
#[cfg(windows)]
#[repr(C)]
struct PowerSetting {
    setting: windows_sys::core::GUID,
    len: u32,
    data: u8,
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
        SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED, WA_INACTIVE, WM_ACTIVATE, WM_CLOSE, WM_DPICHANGED,
        WM_POWERBROADCAST, WM_SHOWWINDOW, WM_SIZE,
    };
    /// `PBT_POWERSETTINGCHANGE`
    const SETTING_CHANGE: usize = 0x8013;
    match msg {
        // close = hide to the tray (only while the icon exists), the app keeps running
        WM_CLOSE if crate::tray::active() => {
            crate::tray::hide_window(hwnd as isize);
            return 0;
        }
        WM_POWERBROADCAST if wparam == SETTING_CHANGE && lparam != 0 => {
            // SAFETY: for this event `lparam` points to a `POWERBROADCAST_SETTING`
            let off = unsafe { (*(lparam as *const PowerSetting)).data } == 0;
            crate::away::set_display_off(off);
            let _ = slint::invoke_from_event_loop(|| {
                APP.with_borrow(|a| {
                    if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                        crate::refresh_active(&app);
                    }
                });
            });
        }
        WM_SHOWWINDOW => {
            // hidden windows are not drawn to: same pause as minimized
            let shown = wparam != 0;
            crate::gui::anim::pause(!shown);
            let _ = slint::invoke_from_event_loop(move || {
                APP.with_borrow(|a| {
                    if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                        crate::visibility(&app, shown);
                    }
                });
            });
            if shown {
                schedule_repaint();
            }
        }
        WM_SIZE => match wparam as u32 {
            SIZE_MINIMIZED => {
                MINIMIZED.set(true);
                crate::gui::anim::pause(true);
            }
            SIZE_RESTORED | SIZE_MAXIMIZED if MINIMIZED.get() => {
                MINIMIZED.set(false);
                crate::gui::anim::pause(false);
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
        WM_DPICHANGED => {
            crate::badge::refresh();
            // winit is about to apply the monitor's scale: put the user's zoom back on top after it
            let _ = slint::invoke_from_event_loop(|| {
                APP.with_borrow(|a| {
                    if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                        crate::apply_zoom(&app);
                    }
                });
            });
        }
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
