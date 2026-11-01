//! Window-level settings: theme, zoom, native window handle and focus.

use super::*;

/// Light / dark per the setting. "System" is Slint's default binding at startup; switching
/// back to it at runtime reads the current Windows setting (the binding can't be restored).
pub(crate) fn apply_theme(app: &AppWindow, startup: bool) {
    if headless::active() {
        return; // SIGWIN_HEADLESS_THEME decides
    }
    match UI.with_borrow(|ui| ui.settings.theme) {
        1 => app.invoke_set_dark(false),
        2 => app.invoke_set_dark(true),
        _ if !startup => app.invoke_set_dark(settings::system_dark()),
        _ => {}
    }
}

/// Window scale = the monitor's DPI scale x the zoom setting. winit only knows the former and
/// resets to it on DPI changes (`winhook` calls this again then).
pub(crate) fn apply_zoom(app: &AppWindow) {
    if headless::active() {
        return;
    }
    let h = hwnd(app);
    if h == 0 {
        types::set_ui_scale(app.window().scale_factor());
        return;
    }
    let zoom = UI.with_borrow(|ui| ui.settings.zoom) as f32 / 100.0;
    let sf = monitor_scale(h) * zoom;
    let w = app.window();
    if (w.scale_factor() - sf).abs() > 0.001 {
        let size = w.size();
        w.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: sf });
        w.dispatch_event(slint::platform::WindowEvent::Resized { size: size.to_logical(sf) });
        winhook::repaint_all(app);
    }
    // thumbnails are pre-scaled to physical pixels: the renderer samples images nearest-neighbor
    types::set_ui_scale(sf);
}

#[cfg(windows)]
pub(crate) fn monitor_scale(hwnd: isize) -> f32 {
    // SAFETY: plain query on our own window
    let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd as _) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

#[cfg(not(windows))]
pub(crate) fn monitor_scale(_: isize) -> f32 {
    1.0
}

/// Settings -> the page's controls.
pub(crate) fn show_settings(app: &AppWindow, s: &settings::Settings) {
    app.set_set_notify(s.notify);
    app.set_set_preview(s.preview as i32);
    app.set_set_sound(s.sound);
    app.set_set_badge(s.badge);
    app.set_set_theme(s.theme as i32);
    app.set_set_zoom(settings::ZOOMS.iter().position(|&z| z == s.zoom).unwrap_or(1) as i32);
    app.set_enter_sends(s.enter_sends);
}

pub(crate) fn hwnd(app: &AppWindow) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let wh = app.window().window_handle();
    match wh.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

#[cfg(windows)]
pub(crate) fn is_foreground(app: &AppWindow) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic};
    let hwnd = hwnd(app) as *mut std::ffi::c_void;
    if hwnd.is_null() {
        return true;
    }
    // SAFETY: plain Win32 queries on our own window handle
    unsafe { GetForegroundWindow() == hwnd && IsIconic(hwnd) == 0 }
}

#[cfg(not(windows))]
pub(crate) fn is_foreground(_: &AppWindow) -> bool {
    true
}

/// A toast was clicked: window to the front, its chat open at the message (an outdated
/// toast just raises the window).
pub(crate) fn open_from_toast(app: &AppWindow, hit: Option<crate::toast::Click>) {
    to_front(app);
    let Some(hit) = hit else { return };
    app.set_settings_open(false);
    send_cmd(Command::OpenFromToast { key: hit.key, msg: hit.msg });
}

#[cfg(windows)]
fn to_front(app: &AppWindow) {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SetForegroundWindow, ShowWindow,
        SW_RESTORE,
    };
    let hwnd = hwnd(app) as *mut std::ffi::c_void;
    if hwnd.is_null() {
        return;
    }
    // SAFETY: Win32 calls on our own window; the input attach is undone right after
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd) != 0 {
            return;
        }
        // the foreground lock can refuse us: borrow the foreground thread's input state
        let fg = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        let me = GetCurrentThreadId();
        let attached = fg != 0 && fg != me && AttachThreadInput(me, fg, 1) != 0;
        BringWindowToTop(hwnd);
        let ok = SetForegroundWindow(hwnd) != 0;
        if attached {
            AttachThreadInput(me, fg, 0);
        }
        tracing::debug!(ok, attached, "toast: window to front");
    }
}

#[cfg(not(windows))]
fn to_front(app: &AppWindow) {
    app.show().ok();
}

/// Slint exposes neither focus nor minimize/restore: hook the window procedure once the
/// native window exists (it is created when the event loop starts).
pub(crate) fn hook_window(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    let send = send.clone();
    let weak = app.as_weak();
    let tries = std::cell::Cell::new(0u32);
    let timer = Rc::new(slint::Timer::default());
    let this = timer.clone();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
        let Some(app) = weak.upgrade() else { return };
        let h = hwnd(&app);
        tries.set(tries.get() + 1);
        if h == 0 && tries.get() < 100 {
            return; // native window not created yet
        }
        this.stop();
        // the zoom setting on top of the monitor's scale; also tells the backend the scale
        apply_zoom(&app);
        let last = std::rc::Rc::new(std::cell::Cell::new(true));
        let s2 = send.clone();
        let l2 = last.clone();
        let hooked = winhook::install(&app, h, move |f| {
            if f != l2.get() {
                l2.set(f);
                s2(Command::WindowFocus(f));
            }
        });
        tracing::info!(hwnd = h, hooked, "window hook");
        badge::set_hwnd(h);
        if !hooked {
            // fallback: poll the foreground window
            let weak = app.as_weak();
            let s = send.clone();
            let poll = Box::leak(Box::new(slint::Timer::default()));
            poll.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(700), move || {
                let Some(app) = weak.upgrade() else { return };
                let f = is_foreground(&app);
                if f != last.get() {
                    last.set(f);
                    s(Command::WindowFocus(f));
                }
            });
        }
    });
    // the timer holds an Rc to itself (to stop itself); it lives for the app's lifetime
    std::mem::forget(timer);
}
