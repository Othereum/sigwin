//! The image viewer: its own window, created per opening and dropped on close, so the image and
//! the window's frame buffer don't outlive it.

use std::cell::Cell;
use std::time::Duration;

use slint::winit_030::WinitWindowAccessor;

use super::*;
use crate::types::ViewerImage;

/// Share of the monitor's work area a newly shown image may take.
const SCREEN_SHARE: f32 = 0.9;
/// `min-width` / `min-height` of `ImageViewer`.
const MIN_SIZE: (f32, f32) = (360.0, 240.0);
/// Refit this long after the last size change: each refit decodes the original again.
const REFIT_AFTER: Duration = Duration::from_millis(200);
/// Image scale per wheel notch.
const ZOOM_STEP: f64 = 1.1;

/// Window position and size, physical px.
type Placement = (i32, i32, u32, u32);

struct Viewer {
    win: ImageViewer,
    id: String,
    /// the image on screen
    index: usize,
    /// the image last asked for; other answers are late and dropped
    wanted_index: Cell<usize>,
    count: usize,
    full: (u32, u32),
    /// size the user chose for the image, physical px (the window may be smaller after a snap)
    wanted: Cell<(u32, u32)>,
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
    send_cmd(Command::ViewImage { id, index: 0, fit: screen_box(window::hwnd(app)), exact: false, from_file });
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
    let moved = VIEWER.with_borrow_mut(|v| {
        let v = v.as_mut()?;
        if img.index != v.wanted_index.get() {
            return None;
        }
        let another = img.index != v.index;
        let size = (img.pixels.width(), img.pixels.height());
        v.index = img.index;
        v.count = img.count;
        v.full = img.full;
        v.win.set_original_w(img.full.0 as i32);
        let caption = if img.count > 1 { format!("{} / {}", img.index + 1, img.count) } else { String::new() };
        v.win.set_caption(caption.into());
        v.win.set_source(Image::from_rgba8(img.pixels));
        if !another {
            return None;
        }
        // another image starts over at its own size, around the window's center
        v.wanted.set(size);
        set_shown(v, size);
        let (old_w, old_h) = client_size(v);
        let (w, h) = window_size(v, size);
        let pos = v.win.window().position();
        Some(on_screen(
            work_area(viewer_hwnd(&v.win)),
            pos.x + (old_w as i32 - w as i32) / 2,
            pos.y + (old_h as i32 - h as i32) / 2,
            w,
            h,
        ))
    });
    if let Some(p) = moved {
        place(p);
    }
    // a late answer may have left other pixels than the size on screen
    schedule_refit();
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
    win.on_zoom(zoom);
    win.on_drag_window(|| {
        with_current(|v| {
            v.win.window().with_winit_window(|w| w.drag_window());
        });
    });
    win.window().on_close_requested(|| {
        close();
        slint::CloseRequestResponse::HideWindow
    });
    let scale = window::monitor_scale(window::hwnd(app));
    let pixels = (img.pixels.width(), img.pixels.height());
    let size = (pixels.0.max((MIN_SIZE.0 * scale) as u32), pixels.1.max((MIN_SIZE.1 * scale) as u32));
    win.set_shown_w(pixels.0 as i32);
    win.set_shown_h(pixels.1 as i32);
    win.window().set_size(slint::PhysicalSize::new(size.0, size.1));
    if let Some((x, y, w, h)) = work_area(window::hwnd(app)) {
        win.window().set_position(slint::PhysicalPosition::new(x + (w - size.0 as i32) / 2, y + (h - size.1 as i32) / 2));
    }
    win.show()?;
    attach(&win, window::hwnd(app));
    VIEWER.with_borrow_mut(|v| {
        *v = Some(Viewer {
            win,
            id: img.id.clone(),
            index: img.index,
            wanted_index: Cell::new(img.index),
            count: img.count,
            full: img.full,
            wanted: Cell::new(pixels),
            refit: slint::Timer::default(),
        });
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

fn min_size(v: &Viewer) -> (u32, u32) {
    let scale = v.win.window().scale_factor();
    ((MIN_SIZE.0 * scale) as u32, (MIN_SIZE.1 * scale) as u32)
}

fn window_size(v: &Viewer, image: (u32, u32)) -> (u32, u32) {
    let min = min_size(v);
    (image.0.max(min.0), image.1.max(min.1))
}

fn shown(v: &Viewer) -> (u32, u32) {
    (v.win.get_shown_w().max(1) as u32, v.win.get_shown_h().max(1) as u32)
}

fn set_shown(v: &Viewer, size: (u32, u32)) {
    v.win.set_shown_w(size.0 as i32);
    v.win.set_shown_h(size.1 as i32);
}

fn step(by: i64) {
    with_current(|v| {
        if v.count > 1 {
            let index = (v.wanted_index.get() as i64 + by).rem_euclid(v.count as i64) as usize;
            v.wanted_index.set(index);
            let fit = screen_box(viewer_hwnd(&v.win));
            send_cmd(Command::ViewImage { id: v.id.clone(), index, fit, exact: false, from_file: false });
        }
    });
}

/// The wheel scales the image and the window with it, keeping the image point under the cursor
/// (around the center, shrinking could leave the cursor outside the window). `cursor` is in
/// physical px within the window.
fn zoom(notches: f32, cursor_x: f32, cursor_y: f32) {
    let moved = VIEWER.with_borrow(|v| {
        let v = v.as_ref()?;
        let (full_w, full_h) = (v.full.0 as f64, v.full.1 as f64);
        let old = shown(v);
        let area = work_area(viewer_hwnd(&v.win));
        let (area_w, area_h) = area.map_or((1920, 1080), |(_, _, w, h)| (w, h));
        let min = min_size(v);
        let scale = old.0 as f64 / full_w;
        // down to the minimum window or the original size, whichever is larger; up to the screen
        let low = (min.0 as f64 / full_w).min(min.1 as f64 / full_h).min(1.0).min(scale);
        let high = (area_w as f64 / full_w).min(area_h as f64 / full_h).max(scale);
        let scale = (scale * ZOOM_STEP.powf(notches as f64)).clamp(low, high);
        let new = (((full_w * scale).round() as u32).max(1), ((full_h * scale).round() as u32).max(1));
        if new == old {
            return None;
        }
        if v.win.window().is_maximized() {
            v.win.window().set_maximized(false);
        }
        v.wanted.set(new);
        set_shown(v, new);
        let client = client_size(v);
        let (w, h) = window_size(v, new);
        let anchor = |cursor: f32, client: u32, old: u32, new: u32, window: u32| {
            let at = ((cursor as f64 - (client as f64 - old as f64) / 2.0) / old as f64).clamp(0.0, 1.0);
            (cursor as f64 - (at * new as f64 + (window as f64 - new as f64) / 2.0)).round() as i32
        };
        let pos = v.win.window().position();
        Some(on_screen(
            area,
            pos.x + anchor(cursor_x, client.0, old.0, new.0, w),
            pos.y + anchor(cursor_y, client.1, old.1, new.1, h),
            w,
            h,
        ))
    });
    if let Some(p) = moved {
        place(p);
        schedule_refit();
    }
}

fn on_screen(area: Option<(i32, i32, i32, i32)>, x: i32, y: i32, w: u32, h: u32) -> Placement {
    let Some((ax, ay, aw, ah)) = area else { return (x, y, w, h) };
    (x.min(ax + aw - w as i32).max(ax), y.min(ay + ah - h as i32).max(ay), w, h)
}

/// Moves and resizes in one step: two separate calls would shake the window.
fn place((x, y, w, h): Placement) {
    let Some(hwnd) = VIEWER.with_borrow(|v| v.as_ref().map(|v| viewer_hwnd(&v.win))) else { return };
    #[cfg(windows)]
    if hwnd != 0 {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
        // SAFETY: our own live window, on its UI thread. Outside the VIEWER borrow: it sends
        // WM_SIZE right away.
        unsafe { SetWindowPos(hwnd as _, std::ptr::null_mut(), x, y, w as i32, h as i32, SWP_NOZORDER | SWP_NOACTIVATE) };
        return;
    }
    let _ = hwnd;
    with_current(|v| {
        v.win.window().set_size(slint::PhysicalSize::new(w, h));
        v.win.window().set_position(slint::PhysicalPosition::new(x, y));
    });
}

fn schedule_refit() {
    // `resized` also fires while the window is being created, before it is stored
    with_current(|v| v.refit.start(slint::TimerMode::SingleShot, REFIT_AFTER, refit));
}

/// The size on screen changed: fetch the image at the new one, if that gives other pixels.
fn refit() {
    with_current(|v| {
        let target = clipped(v.wanted.get(), client_size(v));
        set_shown(v, target);
        let has = v.win.get_source().size();
        if target != (has.width, has.height) {
            send_cmd(Command::ViewImage { id: v.id.clone(), index: v.index, fit: target, exact: true, from_file: false });
        }
    });
}

/// `size`, shrunk to fit in `bounds` if needed.
fn clipped(size: (u32, u32), bounds: (u32, u32)) -> (u32, u32) {
    if size.0 <= bounds.0 && size.1 <= bounds.1 {
        return size;
    }
    let s = (bounds.0 as f64 / size.0 as f64).min(bounds.1 as f64 / size.1 as f64);
    (((size.0 as f64 * s).round() as u32).max(1), ((size.1 as f64 * s).round() as u32).max(1))
}

/// Largest image size for a new image, physical px.
fn screen_box(hwnd: isize) -> (u32, u32) {
    let (w, h) = work_area(hwnd).map_or((1920, 1080), |(_, _, w, h)| (w, h));
    ((w as f32 * SCREEN_SHARE) as u32, (h as f32 * SCREEN_SHARE) as u32)
}

/// Work area (x, y, w, h) of the monitor the window is on.
#[cfg(windows)]
fn work_area(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
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
fn work_area(_: isize) -> Option<(i32, i32, i32, i32)> {
    None
}

/// Keeps the shown window above the main one and off the taskbar, with a shadow despite having
/// no frame.
#[cfg(windows)]
fn attach(win: &ImageViewer, owner: isize) {
    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_HWNDPARENT};

    let hwnd = viewer_hwnd(win);
    // no native window in headless runs
    if hwnd == 0 || owner == 0 {
        return;
    }
    let done = win.window().with_winit_window(|w| {
        w.set_skip_taskbar(true);
        w.set_undecorated_shadow(true);
    });
    if done.is_none() {
        tracing::warn!("image viewer: no winit window");
    }
    // SAFETY: both are our own live windows, on their UI thread
    unsafe { SetWindowLongPtrW(hwnd as _, GWLP_HWNDPARENT, owner) };
}

#[cfg(not(windows))]
fn attach(_: &ImageViewer, _: isize) {}

fn viewer_hwnd(win: &ImageViewer) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = win.window().window_handle();
    match handle.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}
