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
/// Smooth what's visible this long after the last change of view: longer than the gaps within a
/// wheel burst (measured up to 65ms; slow single notches are 400ms and more apart).
const REFIT_AFTER: Duration = Duration::from_millis(100);
/// Image scale per wheel notch.
const ZOOM_STEP: f64 = 1.1;
/// Largest scale, unless the screen takes a smaller image further.
const ZOOM_MAX: f64 = 8.0;
/// From this scale the original's pixels show as they are.
const SMOOTH_BELOW: f64 = 3.0;
/// `fit` that makes the backend send the original.
const ORIGINAL: (u32, u32) = (u32::MAX, u32::MAX);

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
    /// window size we last set: another one means the system resized it (a snap)
    placed: Cell<(u32, u32)>,
    refit: slint::Timer,
    /// Decoded on the first change of size and kept until the image or the window goes: pans and
    /// zooms only resample the visible part of it. Shared with `source`, not a copy.
    original: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    /// the original was asked for
    loading: Cell<bool>,
    opaque: bool,
    /// the latest patch asked for; older ones are dropped when they finish
    patch_id: Cell<u64>,
    /// a patch is being resampled: only one at a time, the next one starts when it ends
    patch_busy: Cell<bool>,
}

thread_local! {
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
    /// row whose first image is on its way (the window opens when it arrives)
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn open(app: &AppWindow, id: String, from_file: bool) {
    close();
    PENDING.with_borrow_mut(|p| *p = Some(id.clone()));
    send_cmd(Command::ViewImage { id, index: 0, fit: screen_box(window::hwnd(app)), from_file });
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
        v.opaque = img.opaque;
        if another {
            v.original = None;
            v.loading.set(false);
            clear_patch(v);
        }
        if size == img.full {
            v.original = Some(img.pixels.clone());
        }
        v.win.set_source(Image::from_rgba8(img.pixels));
        if !another {
            return None;
        }
        // another image starts over at its own size, around the window's center
        v.wanted.set(size);
        set_shown(v, size);
        v.win.set_pan_x(0.0);
        v.win.set_pan_y(0.0);
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
    win.on_panned(|| {
        // bare pixels while dragging: swapping patches stalls frames (a 8MP one took 86ms to make
        // and its frame 43ms instead of 10), and a kept one leaves a visible edge past its end
        with_current(clear_patch);
        schedule_refit();
    });
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
            placed: Cell::new(size),
            refit: slint::Timer::default(),
            original: None,
            loading: Cell::new(false),
            opaque: img.opaque,
            patch_id: Cell::new(0),
            patch_busy: Cell::new(false),
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

/// The window for an image of this size: as big as the image, within the screen.
fn window_size(v: &Viewer, image: (u32, u32)) -> (u32, u32) {
    let min = min_size(v);
    let (area_w, area_h) = area_size(v);
    (image.0.min(area_w).max(min.0), image.1.min(area_h).max(min.1))
}

fn area_size(v: &Viewer) -> (u32, u32) {
    work_area(viewer_hwnd(&v.win)).map_or((1920, 1080), |(_, _, w, h)| (w as u32, h as u32))
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
            send_cmd(Command::ViewImage { id: v.id.clone(), index, fit, from_file: false });
        }
    });
}

/// The wheel scales the image, and the window with it up to the screen, keeping the image point
/// under the cursor (around the center, shrinking could leave the cursor outside the window).
/// `cursor` is in physical px within the window.
fn zoom(notches: f32, cursor_x: f32, cursor_y: f32) {
    let moved = VIEWER.with_borrow(|v| {
        let v = v.as_ref()?;
        let (full_w, full_h) = (v.full.0 as f64, v.full.1 as f64);
        let old = shown(v);
        let (area_w, area_h) = area_size(v);
        let min = min_size(v);
        let scale = old.0 as f64 / full_w;
        // down to the minimum window or the original size, whichever is larger
        let low = (min.0 as f64 / full_w).min(min.1 as f64 / full_h).min(1.0).min(scale);
        let high = (area_w as f64 / full_w).min(area_h as f64 / full_h).max(ZOOM_MAX).max(scale);
        let scale = (scale * ZOOM_STEP.powf(notches as f64)).clamp(low, high);
        // The renderer steps through source pixels in 1/256ths, truncated: at other scales the
        // error adds up across the image, up to 8px at 1080p, and the patch lands elsewhere. So
        // only scales where the step is a whole number of 1/256ths.
        let step = (256.0 / scale).round().clamp((256.0 / high).ceil().max(1.0), (256.0 / low).floor().max(1.0));
        let new = (((full_w * 256.0 / step) as u32).max(1), ((full_h * 256.0 / step) as u32).max(1));
        if new == old {
            return None;
        }
        if v.win.window().is_maximized() {
            v.win.window().set_maximized(false);
        }
        let client = client_size(v);
        let (w, h) = window_size(v, new);
        // where the image point under the cursor is, 0..1, and where the new image starts
        // relative to the window's left or top if that is to stay under the cursor
        let at = |cursor: f32, client: u32, old: u32, cur: f32| {
            ((cursor as f64 - (client as f64 - old as f64) / 2.0 - cur as f64) / old as f64).clamp(0.0, 1.0)
        };
        let at = (at(cursor_x, client.0, old.0, v.win.get_cur_x()), at(cursor_y, client.1, old.1, v.win.get_cur_y()));
        let pos = v.win.window().position();
        let cursor = (pos.x as f64 + cursor_x as f64, pos.y as f64 + cursor_y as f64);
        let edge = |cursor: f64, at: f64, new: u32, window: u32| cursor - (at * new as f64 + (window as f64 - new as f64) / 2.0);
        let (ex, ey) = (edge(cursor.0, at.0, new.0, w), edge(cursor.1, at.1, new.1, h));
        let placement = on_screen(work_area(viewer_hwnd(&v.win)), ex.round() as i32, ey.round() as i32, w, h);
        // the window stopped at the screen: the image moves the rest of the way
        v.win.set_pan_x((ex - placement.0 as f64) as f32);
        v.win.set_pan_y((ey - placement.1 as f64) as f32);
        v.wanted.set(new);
        set_shown(v, new);
        clear_patch(v);
        Some(placement)
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
    let Some(hwnd) = VIEWER.with_borrow(|v| {
        v.as_ref().map(|v| {
            v.placed.set((w, h));
            viewer_hwnd(&v.win)
        })
    }) else {
        return;
    };
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

/// The view changed: smooth what's visible now, unless the image is drawn 1:1 or as bare pixels.
fn refit() {
    with_current(|v| {
        // the system resized the window (a snap): the image follows, it isn't cut off
        let client = client_size(v);
        if client != v.placed.get() {
            v.placed.set(client);
            let fitted = clipped(v.wanted.get(), client);
            if fitted != v.wanted.get() {
                v.wanted.set(fitted);
                set_shown(v, fitted);
                clear_patch(v);
            }
        }
        let target = v.wanted.get();
        let has = v.win.get_source().size();
        if target == (has.width, has.height) {
            return clear_patch(v);
        }
        let Some(original) = &v.original else {
            if !v.loading.replace(true) {
                send_cmd(Command::ViewImage { id: v.id.clone(), index: v.index, fit: ORIGINAL, from_file: false });
            }
            return;
        };
        if target.0 as f64 / v.full.0 as f64 >= SMOOTH_BELOW {
            return clear_patch(v);
        }
        let Some(area) = visible_part(target, client, (v.win.get_cur_x(), v.win.get_cur_y())) else { return };
        let id = v.patch_id.get() + 1;
        v.patch_id.set(id);
        if v.patch_busy.replace(true) {
            return;
        }
        let (src, full, opaque) = (original.clone(), v.full, v.opaque);
        let crop = patch_crop(area, full, target);
        std::thread::spawn(move || {
            let img = sigwin_thumb::resize_view(src.as_bytes(), full, crop, (area.2, area.3), opaque);
            drop(src);
            let pixels = img.map(|i| slint::SharedPixelBuffer::clone_from_slice(i.as_raw(), i.width(), i.height()));
            let _ = slint::invoke_from_event_loop(move || patch_done(id, pixels, (area.0, area.1)));
        });
    });
}

/// The part of the original (left, top, width, height) that `area` of an image drawn at `target`
/// shows. Uses the renderer's step through the source (see `zoom`), so the patch lands on its
/// pixels. Also keeps `resize_view` from crashing: `fast_image_resize` (5.5 and 6.1) reads out of
/// bounds when enlarging from a crop whose first output pixel centers exactly on a source pixel.
/// With a step of k/256 below 1 that can't happen: (a + 0.5) * k / 256 never ends in .5.
fn patch_crop(area: (u32, u32, u32, u32), full: (u32, u32), target: (u32, u32)) -> (f64, f64, f64, f64) {
    let step = |full: u32, target: u32| (full as f64 * 256.0 / target as f64).floor() / 256.0;
    let (sx, sy) = (step(full.0, target.0), step(full.1, target.1));
    let (left, top) = (area.0 as f64 * sx, area.1 as f64 * sy);
    (left, top, (area.2 as f64 * sx).min(full.0 as f64 - left), (area.3 as f64 * sy).min(full.1 as f64 - top))
}

/// The part of an image of `size`, centered in the window and offset by `offset`, that the window
/// shows: left, top, width and height within the image, physical px.
fn visible_part(size: (u32, u32), client: (u32, u32), offset: (f32, f32)) -> Option<(u32, u32, u32, u32)> {
    let axis = |size: u32, client: u32, offset: f32| {
        // as `image-x` / `image-y` in the window
        let start = ((client as f64 - size as f64) / 2.0 + offset as f64).round() as i64;
        let from = (-start).max(0);
        let to = (client as i64 - start).min(size as i64);
        (to > from).then_some((from as u32, (to - from) as u32))
    };
    let (x, w) = axis(size.0, client.0, offset.0)?;
    let (y, h) = axis(size.1, client.1, offset.1)?;
    Some((x, y, w, h))
}

fn patch_done(id: u64, pixels: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>, at: (u32, u32)) {
    let mut stale = false;
    with_current(|v| {
        v.patch_busy.set(false);
        if id != v.patch_id.get() {
            stale = true;
        } else if let Some(pixels) = pixels {
            v.win.set_patch(Image::from_rgba8(pixels));
            v.win.set_patch_x(at.0 as i32);
            v.win.set_patch_y(at.1 as i32);
            v.win.set_patch_on(true);
        }
    });
    if stale {
        schedule_refit();
    }
}

/// Drops the patch, and the one being made if any.
fn clear_patch(v: &Viewer) {
    v.patch_id.set(v.patch_id.get() + 1);
    v.win.set_patch_on(false);
    v.win.set_patch(Image::default());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_crops_avoid_the_resizer_crash() {
        for full in [37u32, 101, 257] {
            let px = vec![128u8; full as usize * 8 * 4];
            for target in (full / 2).max(1)..full * 4 {
                for a in (0..target).step_by(1 + target as usize / 13) {
                    for w in [1, 3, 64, target] {
                        let w = w.min(target - a);
                        let crop = patch_crop((a, 0, w, 16), (full, 8), (target, 16));
                        let scale = crop.2 / w as f64;
                        // what crashes it (a step of 1 needs no horizontal pass)
                        let center = crop.0 + scale / 2.0;
                        assert!(scale >= 1.0 || center.fract() != 0.5, "{full} {target} {a} {w}");
                        assert!(sigwin_thumb::resize_view(&px, (full, 8), crop, (w, 16), true).is_some());
                    }
                }
            }
        }
    }
}
