//! Window-level settings: theme, zoom, native window handle and focus.

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use slint::winit_030::{EventResult, WinitWindowAccessor};

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
    let d = app.global::<SettingsData>();
    d.set_notify(s.notify);
    d.set_preview(s.preview as i32);
    d.set_sound(s.sound);
    d.set_badge_count(s.badge_count as i32);
    d.set_theme(s.theme as i32);
    d.set_zoom(settings::ZOOMS.iter().position(|&z| z == s.zoom).unwrap_or(1) as i32);
    d.set_enter_sends(s.enter_sends);
    d.set_link_previews(s.link_previews);
    d.set_autostart(s.autostart);
    d.set_start_hidden(s.start_hidden);
    d.set_auto_update(s.auto_update);
    d.set_muted_mentions(s.muted_mentions);
    d.set_muted_replies(s.muted_replies);
    d.set_badge_muted(s.badge_muted);
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

thread_local! {
    static SHOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// starts the native-window hooks (`hook_window`); set by `wire`
    static STARTER: std::cell::RefCell<Option<Box<dyn Fn(&AppWindow)>>> = std::cell::RefCell::new(None);
}

pub(crate) fn set_starter(f: impl Fn(&AppWindow) + 'static) {
    STARTER.with_borrow_mut(|s| *s = Some(Box::new(f)));
}

/// Shows the Slint window the first time (an autostart launch keeps it unshown until the tray
/// icon is used) and starts the window hooks. True if it was shown just now.
pub(crate) fn ensure_shown(app: &AppWindow) -> bool {
    if SHOWN.replace(true) {
        return false;
    }
    restore_geometry(app);
    if let Err(e) = app.show() {
        tracing::warn!("show window: {e}");
    }
    STARTER.with_borrow(|s| {
        if let Some(s) = s {
            s(app);
        }
    });
    true
}

thread_local! {
    /// the last normal rect seen (updated on move / resize)
    static GEOM: std::cell::Cell<Option<settings::WindowGeom>> = const { std::cell::Cell::new(None) };
}

/// Remembers the window's rect unless it is minimized; a maximized window keeps its old
/// normal rect and only sets the flag.
fn track_geometry(w: &slint::Window) {
    if w.is_minimized() {
        return;
    }
    let maximized = w.is_maximized();
    let prev = GEOM.get();
    let g = if maximized && prev.is_some() {
        settings::WindowGeom { maximized, ..prev.unwrap() }
    } else {
        let (p, s) = (w.position(), w.size());
        if s.width == 0 || s.height == 0 {
            return;
        }
        settings::WindowGeom { x: p.x, y: p.y, w: s.width, h: s.height, maximized }
    };
    GEOM.set(Some(g));
    // save soon after the window stops moving, so a kill or crash keeps the last rect
    GEOM_SAVE.with(|t| {
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_secs(1), persist_geometry);
    });
}

thread_local! {
    static GEOM_SAVE: slint::Timer = slint::Timer::default();
}

/// Writes the tracked rect into the settings file if it changed.
fn persist_geometry() {
    if headless::active() || !SHOWN.get() {
        return;
    }
    let Some(g) = GEOM.get() else { return };
    UI.with_borrow_mut(|ui| {
        if ui.settings.window != Some(g) {
            ui.settings.window = Some(g);
            ui.settings.save(&ui.settings_path);
        }
    });
}

/// Before the first show: the saved rect, if it still lands on a monitor.
fn restore_geometry(app: &AppWindow) {
    if headless::active() {
        return;
    }
    let Some(g) = UI.with_borrow(|ui| ui.settings.window) else { return };
    GEOM.set(Some(g));
    let w = app.window();
    if g.w >= 200 && g.h >= 200 && on_screen(&g) {
        w.set_size(slint::PhysicalSize::new(g.w, g.h));
        w.set_position(slint::PhysicalPosition::new(g.x, g.y));
    }
    if g.maximized {
        w.set_maximized(true);
    }
}

#[cfg(windows)]
fn on_screen(g: &settings::WindowGeom) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromRect, MONITOR_DEFAULTTONULL};
    // the title bar's strip must touch a monitor, or the window couldn't be dragged back
    let r = RECT { left: g.x, top: g.y, right: g.x + g.w as i32, bottom: g.y + 40 };
    // SAFETY: plain query with a local rect
    !unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONULL) }.is_null()
}

#[cfg(not(windows))]
fn on_screen(_: &settings::WindowGeom) -> bool {
    true
}

/// On exit: writes the window's rect into the settings file.
pub(crate) fn save_geometry(app: &AppWindow) {
    if headless::active() || !SHOWN.get() {
        return;
    }
    track_geometry(app.window());
    persist_geometry();
}

/// Before the window exists: image sizes follow the system DPI and the zoom setting.
#[cfg(windows)]
pub(crate) fn preset_scale(zoom: u32) {
    // SAFETY: plain query
    let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() };
    types::set_ui_scale(dpi as f32 / 96.0 * zoom as f32 / 100.0);
}

#[cfg(not(windows))]
pub(crate) fn preset_scale(_: u32) {}

#[cfg(windows)]
pub(crate) fn to_front(app: &AppWindow) {
    if ensure_shown(app) {
        // the native window appears a moment later: raise it then
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
            if let Some(app) = weak.upgrade() {
                to_front(&app);
            }
        });
        return;
    }
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let hwnd = hwnd(app) as *mut std::ffi::c_void;
    if hwnd.is_null() {
        return;
    }
    // SAFETY: Win32 calls on our own window; the input attach is undone right after
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, SW_SHOW); // hidden to the tray (a minimized window is still "visible")
        }
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
pub(crate) fn to_front(app: &AppWindow) {
    ensure_shown(app);
    app.show().ok();
}

/// The letter Slint's shortcuts expect for a Ctrl+key press, when winit reports it differently.
/// Slint matches only lowercase "a", "c", ...: with Caps Lock on, winit says "A". While an IME
/// handles the key (Hangul composition), winit says `Process` and Slint drops the press.
fn ctrl_letter(logical: &Key, physical: PhysicalKey, mods: ModifiersState) -> Option<char> {
    if !mods.control_key() || mods.alt_key() {
        return None;
    }
    match logical {
        Key::Character(s) if !mods.shift_key() => {
            let mut chars = s.chars();
            let c = chars.next().filter(|c| c.is_ascii_uppercase() && chars.next().is_none())?;
            Some(c.to_ascii_lowercase())
        }
        Key::Named(NamedKey::Process) => {
            let PhysicalKey::Code(code) = physical else { return None };
            let name = format!("{code:?}");
            let c = name.strip_prefix("Key").filter(|l| l.len() == 1)?.chars().next()?;
            Some(c.to_ascii_lowercase())
        }
        _ => None,
    }
}

/// Keep Ctrl shortcuts working in every text field regardless of Caps Lock and the IME.
fn fix_ctrl_shortcuts(app: &AppWindow) {
    let mods = std::cell::Cell::new(ModifiersState::empty());
    app.window().on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::ModifiersChanged(m) => mods.set(m.state()),
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => track_geometry(window),
            WindowEvent::KeyboardInput { event: e, is_synthetic: false, .. } if e.state.is_pressed() => {
                if let Some(c) = ctrl_letter(&e.logical_key, e.physical_key, mods.get()) {
                    let text = c.to_string().into();
                    let event = if e.repeat {
                        slint::platform::WindowEvent::KeyPressRepeated { text }
                    } else {
                        slint::platform::WindowEvent::KeyPressed { text }
                    };
                    let _ = window.dispatch_event_with_result(event);
                    return EventResult::PreventDefault;
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

thread_local! {
    static FOCUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// what the list and the backend were last told
    static ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static WATCHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// runs while rows wait out `WATCH_DWELL`
    static DWELL: slint::Timer = slint::Timer::default();
}

/// A row shown in a window that is only watched counts as seen after this long on screen.
const WATCH_DWELL: std::time::Duration = std::time::Duration::from_secs(5);

/// The window gained or lost the focus.
pub(crate) fn focus_changed(app: &AppWindow, focused: bool) {
    FOCUSED.set(focused);
    refresh_active(app);
}

/// How the user stands to the window, told to the list and the backend (notifications, read
/// receipts). Active: they work in it (focused, not away). Watching: it shows but isn't the one
/// they work in (a second monitor next to a game), so what comes in is read after a while.
/// Otherwise nothing is read and the list keeps its place. Called when the focus changes, the
/// monitor turns off or on, and every few seconds for the input, lock and cover checks.
pub(crate) fn refresh_active(app: &AppWindow) {
    let away = crate::away::is_away();
    let active = FOCUSED.get() && !away;
    let watching = !active && !away && crate::away::on_screen(hwnd(app));
    let was_active = ACTIVE.replace(active);
    let was_watching = WATCHING.replace(watching);
    if was_active == active && was_watching == watching {
        return;
    }
    let reading = active || watching;
    UI.with_borrow_mut(|ui| {
        ui.inactive = !reading;
        // coming back: the rows on screen are reported again
        if reading != (was_active || was_watching) || active != was_active {
            ui.read_sent = None;
        }
    });
    tracing::info!(active, watching, "user presence");
    app.set_inactive(!reading);
    if active {
        // the user is here now: what waited for its time was in front of them
        flush_dwell(true);
    } else if !watching {
        UI.with_borrow_mut(|ui| ui.dwell.clear());
    }
    send_cmd(Command::WindowWatching(watching));
    send_cmd(Command::WindowFocus(active));
}

/// The user saw row `id` fully on screen: read now when they work in the window, after
/// `WATCH_DWELL` when it is only watched.
pub(crate) fn row_seen(id: &str) {
    if ACTIVE.get() {
        send_read(id);
        return;
    }
    UI.with_borrow_mut(|ui| ui.dwell.push((id.to_string(), std::time::Instant::now())));
    DWELL.with(|t| {
        if !t.running() {
            t.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), || flush_dwell(false));
        }
    });
}

/// Reports the rows that stayed long enough (all of them with `all`).
fn flush_dwell(all: bool) {
    let due: Vec<String> = UI.with_borrow_mut(|ui| {
        let n = ui.dwell.iter().rposition(|(_, at)| all || at.elapsed() >= WATCH_DWELL).map_or(0, |i| i + 1);
        ui.dwell.drain(..n).map(|(id, _)| id).collect()
    });
    for id in due {
        send_read(&id);
    }
    if UI.with_borrow(|ui| ui.dwell.is_empty()) {
        DWELL.with(|t| t.stop());
    }
}

/// `ReadUpTo` for a row: each row only once, and nothing for rows above one already sent.
fn send_read(id: &str) {
    let go = UI.with_borrow_mut(|ui| {
        let pos = |ui: &Ui, id: &str| ui.last.iter().position(|m| m.id == id);
        let Some(at) = pos(ui, id) else { return false };
        if ui.read_sent.as_deref().and_then(|s| pos(ui, s)).is_some_and(|sent| sent >= at) {
            return false;
        }
        ui.read_sent = Some(id.to_string());
        true
    });
    if go {
        send_cmd(Command::ReadUpTo(id.to_string()));
    }
}

/// Slint exposes neither focus nor minimize/restore: hook the window procedure once the
/// native window exists (it is created when the event loop starts).
pub(crate) fn hook_window(app: &AppWindow) {
    fix_ctrl_shortcuts(app);
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
        let l2 = last.clone();
        let w2 = app.as_weak();
        let hooked = winhook::install(&app, h, move |f| {
            if f != l2.get() {
                l2.set(f);
                if f {
                    crate::update::poke();
                }
                if let Some(app) = w2.upgrade() {
                    focus_changed(&app, f);
                }
            }
        });
        // away: no input for a while, or the screen locked (no event for those)
        let weak = app.as_weak();
        let away = Box::leak(Box::new(slint::Timer::default()));
        away.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(2), move || {
            if let Some(app) = weak.upgrade() {
                refresh_active(&app);
            }
        });
        tracing::info!(hwnd = h, hooked, "window hook");
        badge::set_hwnd(h);
        #[cfg(all(windows, sigwin_dev))]
        set_dev_icon(h);
        if !hooked {
            // fallback: poll the foreground window
            let weak = app.as_weak();
            let poll = Box::leak(Box::new(slint::Timer::default()));
            poll.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(700), move || {
                let Some(app) = weak.upgrade() else { return };
                let f = is_foreground(&app);
                if f != last.get() {
                    last.set(f);
                    if f {
                        crate::update::poke();
                    }
                    focus_changed(&app, f);
                }
            });
        }
    });
    // the timer holds an Rc to itself (to stop itself); it lives for the app's lifetime
    std::mem::forget(timer);
}

/// How long the window stays hidden before its rows and images are dropped: a quick
/// hide / show shouldn't reload anything.
const RELEASE_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

thread_local! {
    static RELEASE: slint::Timer = slint::Timer::default();
}

/// The window was hidden (tray) or shown. Hidden for a while: the open thread's rows,
/// thumbnails, photos and sticker frames go, so only the thread list stays in memory.
/// Shown again: the thread is loaded afresh (at its newest messages).
pub(crate) fn visibility(app: &AppWindow, shown: bool) {
    if shown {
        RELEASE.with(|t| t.stop());
        let reload = UI.with_borrow_mut(|ui| std::mem::take(&mut ui.released).then(|| ui.open_key.clone()));
        if let Some(key) = reload.filter(|k| !k.is_empty()) {
            send_cmd(Command::OpenThread(key));
        }
        return;
    }
    // an owned window would stay up while the main one sits in the tray
    viewer::close();
    let weak = app.as_weak();
    RELEASE.with(|t| {
        t.start(slint::TimerMode::SingleShot, RELEASE_AFTER, move || {
            if let Some(app) = weak.upgrade() {
                stickers::close(&app);
            }
            UI.with_borrow_mut(|ui| {
                ui.anims.clear_list();
                ui.thumbs.clear();
                ui.media_labels.clear();
                ui.sender_avatars.clear();
                ui.messages.set_vec(Vec::new());
                ui.last.clear();
                ui.typing_row = false;
                ui.older = None;
                ui.released = true;
            });
            sigwin_clear_plain_styles();
            viewer::close();
            tracing::debug!("hidden: dropped the open thread's rows and images");
        });
    });
}

/// Dev builds: the marked icon (resource 2, build.rs) on the window and its taskbar button.
/// Not through Slint's `icon`: it never applies a pixel-buffer image (no cache key, so it looks
/// unchanged), and an `@image-url` would ship the icon in releases. Slint only sets the icon
/// again when its own image changes, so this one stays.
#[cfg(all(windows, sigwin_dev))]
fn set_dev_icon(hwnd: isize) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, SM_CXICON,
        SM_CXSMICON, WM_SETICON,
    };
    if hwnd == 0 {
        return;
    }
    // SAFETY: our own icon resource on our own window; shared icons need no cleanup
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let size = GetSystemMetrics(metric);
            let icon = LoadImageW(inst, 2 as _, IMAGE_ICON, size, size, LR_SHARED);
            if !icon.is_null() {
                SendMessageW(hwnd as _, WM_SETICON, which as usize, icon as isize);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::winit_030::winit::keyboard::KeyCode;

    const A: PhysicalKey = PhysicalKey::Code(KeyCode::KeyA);

    #[test]
    fn ctrl_letter_fixes_caps_lock_and_ime() {
        let ctrl = ModifiersState::CONTROL;
        let upper = Key::Character("A".into());
        let process = Key::Named(NamedKey::Process);
        assert_eq!(ctrl_letter(&upper, A, ctrl), Some('a'));
        assert_eq!(ctrl_letter(&process, A, ctrl), Some('a'));
        assert_eq!(ctrl_letter(&process, A, ctrl | ModifiersState::SHIFT), Some('a'));
    }

    #[test]
    fn ctrl_letter_leaves_other_keys_alone() {
        let ctrl = ModifiersState::CONTROL;
        let upper = Key::Character("A".into());
        // already what Slint expects
        assert_eq!(ctrl_letter(&Key::Character("a".into()), A, ctrl), None);
        // Ctrl+Shift+letter is the app's own shortcut, Ctrl+Alt may be AltGr, no Ctrl is typing
        assert_eq!(ctrl_letter(&upper, A, ctrl | ModifiersState::SHIFT), None);
        assert_eq!(ctrl_letter(&upper, A, ctrl | ModifiersState::ALT), None);
        assert_eq!(ctrl_letter(&upper, A, ModifiersState::empty()), None);
        // a function key during composition isn't a letter
        let f1 = PhysicalKey::Code(KeyCode::F1);
        assert_eq!(ctrl_letter(&Key::Named(NamedKey::Process), f1, ctrl), None);
    }
}
