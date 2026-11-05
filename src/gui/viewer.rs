//! The image viewer: its own window, created per opening and dropped on close, so the image and
//! the window's frame buffer don't outlive it.

use std::time::Duration;

use super::*;
use crate::types::ViewerImage;

/// Share of the monitor's work area the window may take.
const SCREEN_SHARE: f32 = 0.9;
/// Window frame around the client area, logical px (it must fit in the work area too).
const TITLE_BAR: f32 = 32.0;
/// `min-width` / `min-height` of `ImageViewer`.
const MIN_SIZE: (f32, f32) = (360.0, 240.0);
/// Refit this long after the last resize step: each refit decodes the original again.
const REFIT_AFTER: Duration = Duration::from_millis(200);

struct Viewer {
    win: ImageViewer,
    id: String,
    index: usize,
    count: usize,
    full: (u32, u32),
    refit: slint::Timer,
}

thread_local! {
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
    /// row whose first image is on its way (the window opens when it arrives)
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn open(app: &AppWindow, id: String, from_file: bool) {
    close();
    PENDING.with_borrow_mut(|p| *p = Some(id.clone()));
    send_cmd(Command::ViewImage { id, index: 0, fit: screen_box(app), from_file });
}

/// Callable from the window's own callbacks: the window is dropped after they return.
pub(crate) fn close() {
    PENDING.with_borrow_mut(|p| *p = None);
    let Some(v) = VIEWER.with_borrow_mut(Option::take) else { return };
    let _ = v.win.hide();
    slint::Timer::single_shot(Duration::ZERO, move || drop(v));
}

/// After a message list update: the message was deleted or expired, or another thread was opened.
pub(crate) fn close_if_gone() {
    let id = VIEWER.with_borrow(|v| v.as_ref().map(|v| v.id.clone()));
    if id.is_some_and(|id| !row_has_image(&id)) {
        close();
    }
}

fn row_has_image(id: &str) -> bool {
    UI.with_borrow(|ui| ui.last.iter().any(|m| m.id == id && !m.image_key.is_empty()))
}

pub(crate) fn arrived(app: &AppWindow, img: ViewerImage) {
    let same = VIEWER.with_borrow(|v| v.as_ref().is_some_and(|v| v.id == img.id));
    if !same {
        let wanted = PENDING.with_borrow_mut(|p| if p.as_deref() == Some(&img.id) { p.take() } else { None });
        if wanted.is_none() || !row_has_image(&img.id) {
            return;
        }
        if let Err(e) = create(app, &img) {
            tracing::warn!("image viewer: {e}");
            return;
        }
    }
    VIEWER.with_borrow_mut(|v| {
        let Some(v) = v.as_mut() else { return };
        v.index = img.index;
        v.count = img.count;
        v.full = img.full;
        let caption = if img.count > 1 { format!("{} / {}", img.index + 1, img.count) } else { String::new() };
        v.win.set_caption(caption.into());
        v.win.set_source(Image::from_rgba8(img.pixels));
    });
}

fn create(app: &AppWindow, img: &ViewerImage) -> Result<(), slint::PlatformError> {
    let win = ImageViewer::new()?;
    win.on_prev(|| step(-1));
    win.on_next(|| step(1));
    win.on_dismiss(close);
    win.on_save(|| {
        with_current(|v| {
            send_cmd(Command::SaveViewed { id: v.id.clone(), index: v.index, owner: viewer_hwnd(&v.win) });
        });
    });
    win.on_open_outside(|| {
        with_current(|v| send_cmd(Command::OpenViewed { id: v.id.clone(), index: v.index }));
    });
    win.on_resized(schedule_refit);
    win.window().on_close_requested(|| {
        close();
        slint::CloseRequestResponse::HideWindow
    });
    let scale = window::monitor_scale(window::hwnd(app));
    let size = (
        img.pixels.width().max((MIN_SIZE.0 * scale) as u32),
        img.pixels.height().max((MIN_SIZE.1 * scale) as u32),
    );
    win.window().set_size(slint::PhysicalSize::new(size.0, size.1));
    if let Some((x, y, w, h)) = work_area(app) {
        let (full_w, full_h) = (size.0 as i32, size.1 as i32 + (TITLE_BAR * scale) as i32);
        win.window().set_position(slint::PhysicalPosition::new(x + (w - full_w) / 2, y + (h - full_h) / 2));
    }
    win.show()?;
    VIEWER.with_borrow_mut(|v| {
        *v = Some(Viewer { win, id: img.id.clone(), index: img.index, count: img.count, full: img.full, refit: slint::Timer::default() });
    });
    Ok(())
}

fn with_current(f: impl FnOnce(&Viewer)) {
    VIEWER.with_borrow(|v| {
        if let Some(v) = v {
            f(v);
        }
    });
}

fn client_size(v: &Viewer) -> (u32, u32) {
    let s = v.win.window().size();
    (s.width, s.height)
}

fn step(by: i64) {
    with_current(|v| {
        if v.count > 1 {
            let index = (v.index as i64 + by).rem_euclid(v.count as i64) as usize;
            send_cmd(Command::ViewImage { id: v.id.clone(), index, fit: client_size(v), from_file: false });
        }
    });
}

fn schedule_refit() {
    // `resized` also fires while the window is being created, before it is stored
    with_current(|v| v.refit.start(slint::TimerMode::SingleShot, REFIT_AFTER, refit));
}

/// The window changed size: fetch the image at the new one, if that gives other pixels.
fn refit() {
    with_current(|v| {
        let fit = client_size(v);
        let want = sigwin_thumb::fit_size(v.full, fit);
        let has = v.win.get_source().size();
        if want != (has.width, has.height) {
            send_cmd(Command::ViewImage { id: v.id.clone(), index: v.index, fit, from_file: false });
        }
    });
}

/// Largest client area for a new window, physical px.
fn screen_box(app: &AppWindow) -> (u32, u32) {
    let scale = window::monitor_scale(window::hwnd(app));
    let (w, h) = work_area(app).map_or((1920, 1080), |(_, _, w, h)| (w, h));
    let title = TITLE_BAR * scale;
    ((w as f32 * SCREEN_SHARE) as u32, (h as f32 * SCREEN_SHARE - title).max(1.0) as u32)
}

/// Work area (x, y, w, h) of the monitor the main window is on.
#[cfg(windows)]
fn work_area(app: &AppWindow) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    let hwnd = window::hwnd(app);
    if hwnd == 0 {
        return None;
    }
    // SAFETY: plain queries on our own window; `info` is a local with its size set
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let monitor = MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let r = info.rcWork;
        Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
}

#[cfg(not(windows))]
fn work_area(_: &AppWindow) -> Option<(i32, i32, i32, i32)> {
    None
}

fn viewer_hwnd(win: &ImageViewer) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = win.window().window_handle();
    match handle.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}
