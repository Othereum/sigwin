//! Window-procedure subclass for events Slint doesn't expose.
//!
//! - Restore from minimized: the software renderer redraws only damaged regions, and when
//!   the UI changed while minimized it loses track of them. The restored window then shows
//!   only the last-changed bits over a transparent background. We force a full repaint.
//!   A GIF playing in the message list stops when minimized for the same reason (`gui::anim::pause`).
//! - Activation changes: tell the backend whether the window is in the foreground
//!   (replaces polling). Slint doesn't say whether the window has the focus.
//! - Input in the window (keys, buttons, mouse moves, the wheel): whether the user is at it
//!   (`gui::presence`).
//! - DPI change: winit resets the scale factor to the monitor's; re-apply the zoom setting.

use std::cell::{Cell, RefCell};

use slint::ComponentHandle;

use crate::AppWindow;

thread_local! {
    static APP: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static MINIMIZED: Cell<bool> = const { Cell::new(false) };
    static ON_FOCUS: RefCell<Option<Box<dyn Fn(bool)>>> = RefCell::new(None);
    /// where the last `WM_MOUSEMOVE` was (its `lparam`)
    static MOUSE_AT: Cell<isize> = const { Cell::new(-1) };
}

#[cfg(windows)]
use crate::gui::presence::input;

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
        SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED, WA_INACTIVE, WM_ACTIVATE, WM_CLOSE, WM_DPICHANGED,
        WM_KEYDOWN, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_POINTERDOWN,
        WM_RBUTTONDOWN, WM_SETTINGCHANGE, WM_SHOWWINDOW, WM_SIZE, WM_SYSKEYDOWN, WM_XBUTTONDOWN,
    };
    use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
    // what Signal Desktop counts as the user at the window (`ActiveWindowService`); moves
    // are counted below
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        | WM_POINTERDOWN => input(false),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => input(true),
        _ => {}
    }
    match msg {
        // close = hide to the tray (only while the icon exists), the app keeps running
        WM_CLOSE if crate::tray::active() => {
            crate::tray::hide_window(hwnd as isize);
            return 0;
        }
        WM_SHOWWINDOW => {
            // hidden windows are not drawn to: same pause as minimized
            let shown = wparam != 0;
            crate::gui::anim::pause(!shown);
            let _ = slint::invoke_from_event_loop(move || {
                APP.with_borrow(|a| {
                    if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                        crate::gui::visibility(&app, shown);
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
                        crate::gui::apply_zoom(&app);
                    }
                });
            });
        }
        WM_SETTINGCHANGE if lparam != 0 && is_color_set(lparam) => {
            let _ = slint::invoke_from_event_loop(|| {
                APP.with_borrow(|a| {
                    if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                        crate::gui::system_theme_changed(&app);
                    }
                });
            });
        }
        // a press anywhere, before the UI sees it: a list that scrolls takes presses on its
        // empty space for itself, so the UI can't tell that one fell outside the search box
        WM_LBUTTONDOWN => {
            let x = (lparam & 0xffff) as i16 as f32;
            let _ = slint::invoke_from_event_loop(move || {
                APP.with_borrow(|a| {
                    if let Some(app) = a.as_ref().and_then(|w| w.upgrade()) {
                        app.invoke_pressed_at(x / app.window().scale_factor());
                    }
                });
            });
        }
        // Windows also sends a move when a window above ours appears or goes while the cursor
        // rests: only a new position is the user's doing
        WM_MOUSEMOVE => {
            if MOUSE_AT.replace(lparam) != lparam {
                input(false);
            }
        }
        // winit asks for it (TrackMouseEvent) to report the cursor leaving
        WM_MOUSELEAVE => MOUSE_AT.set(-1),
        m if m == taskbar_created() => crate::badge::refresh(),
        _ => {}
    }
    // SAFETY: forwarding to the next handler in the subclass chain
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// `WM_SETTINGCHANGE` for "ImmersiveColorSet": the light/dark app setting (among other colors)
/// changed.
#[cfg(windows)]
fn is_color_set(lparam: windows_sys::Win32::Foundation::LPARAM) -> bool {
    let want = "ImmersiveColorSet".encode_utf16().chain(Some(0));
    // SAFETY: for this message a non-zero `lparam` is a NUL-terminated wide string; we stop
    // at the first mismatch or the NUL
    let p = lparam as *const u16;
    for (i, c) in want.enumerate() {
        if unsafe { *p.add(i) } != c {
            return false;
        }
    }
    true
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
