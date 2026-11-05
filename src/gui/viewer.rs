//! The image viewer: its own window, created per opening and dropped on close, so the image and
//! the window's frame buffer don't outlive it. It also closes when its row goes away (deleted,
//! expired, another thread opened).
//!
//! An image opens at its own size within the screen, drawn 1:1. Dragging moves the window, or the
//! image once that is bigger than the window; a click, Esc or the ✕ near the top right corner
//! closes it. The wheel scales image and window together around the cursor; the window stops at
//! the screen, the image goes on. A notch that would pass the original's size stops at it for a
//! moment, even in the middle of a fast spin. The window's size follows the image, so the system
//! can't snap, maximize or minimize it.
//!
//! While the view changes the original's pixels are stretched as they are, then the visible part
//! is resampled on a thread and drawn over them.

use std::cell::Cell;
use std::time::{Duration, Instant};

use slint::Model;
use slint::winit_030::WinitWindowAccessor;

use super::*;
use crate::types::{ViewerImage, ViewerPixels};

/// Share of the monitor's work area a newly shown image may take.
const SCREEN_SHARE: f32 = 0.8;
/// Size of a cell of the thumbnail strip in `ImageViewer`.
const THUMB_CELL: f32 = 44.0;
/// `min-width` / `min-height` of `ImageViewer`.
const MIN_SIZE: (f32, f32) = (360.0, 240.0);
/// Smooth what's visible this long after the last change of view: longer than the gaps within a
/// wheel burst (measured up to 65ms; slow single notches are 400ms and more apart).
const REFIT_AFTER: Duration = Duration::from_millis(100);
/// Image scale per wheel notch.
const ZOOM_STEP: f64 = 1.1;
/// Largest scale, unless the screen takes a smaller image further.
const ZOOM_MAX: f64 = 8.0;
/// How close to 100% the wheel may end up before it goes all the way there.
const NEAR_ORIGINAL: f64 = 0.05;
/// The wheel does nothing for this long after landing on 100%: notches within a burst are at most
/// 65ms apart, so a fast spin still catches there.
const HOLD_AT_ORIGINAL: Duration = Duration::from_millis(120);
/// The scale readout stays this long after the last wheel notch.
const SCALE_SHOWN: Duration = Duration::from_secs(1);
/// The thumbnail strip stays this long after going to an image.
const STRIP_SHOWN: Duration = Duration::from_secs(1);
/// From this scale the original's pixels show as they are.
const SMOOTH_BELOW: f64 = 3.0;

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
    /// the wheel does nothing until then
    wheel_held: Cell<Option<Instant>>,
    hide_scale: slint::Timer,
    hide_strip: slint::Timer,
    refit: slint::Timer,
    /// Pans and zooms only resample the visible part of `pixels.original`. Kept until the image or
    /// the window goes, and shared with `source` once that shows it, not a copy.
    pixels: ViewerPixels,
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

/// A main window notice also shows here: the viewer usually covers it.
pub(crate) fn set_notice(text: &SharedString) {
    VIEWER.with_borrow(|v| {
        if let Some(v) = v {
            v.win.set_notice_text(text.clone());
        }
    });
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
        if let Err(e) = create(app, img) {
            tracing::warn!("image viewer: {e}");
        }
        return;
    }
    let moved = VIEWER.with_borrow_mut(|v| {
        let v = v.as_mut()?;
        // late, or back at the image already on screen
        if img.index != v.wanted_index.get() || img.index == v.index {
            return None;
        }
        v.index = img.index;
        v.count = img.count;
        show_details(&v.win, &img);
        v.pixels = img.pixels;
        clear_patch(v);
        let first = v.pixels.take_first();
        let size = (first.width(), first.height());
        v.win.set_source(Image::from_rgba8(first));
        // another image starts over at its own size, around the window's center
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
}

/// A thumbnail of the strip, unless the viewer went on to another message.
pub(crate) fn thumb_arrived(id: &str, index: usize, t: crate::types::Thumb) {
    with_current(|v| {
        if v.id == id && index < v.win.get_thumbs().row_count() {
            let pixels = slint::SharedPixelBuffer::clone_from_slice(&t.rgba, t.width, t.height);
            v.win.get_thumbs().set_row_data(index, Image::from_rgba8(pixels));
        }
    });
}

/// What the window shows about an image besides its pixels.
fn show_details(win: &ImageViewer, img: &ViewerImage) {
    win.set_original_w(img.pixels.full().0 as i32);
    win.set_current(img.index as i32);
}

fn create(app: &AppWindow, mut img: ViewerImage) -> Result<(), slint::PlatformError> {
    let win = ImageViewer::new()?;
    win.on_go(|index| go_to(index as usize));
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
    let first = img.pixels.take_first();
    let pixels = (first.width(), first.height());
    let size = (pixels.0.max((MIN_SIZE.0 * scale) as u32), pixels.1.max((MIN_SIZE.1 * scale) as u32));
    win.set_source(Image::from_rgba8(first));
    show_details(&win, &img);
    if img.count > 1 {
        win.set_thumbs(slint::ModelRc::new(slint::VecModel::from(vec![Image::default(); img.count])));
        send_cmd(Command::ViewerThumbs { id: img.id.clone(), px: (THUMB_CELL * scale).round() as u32 });
    }
    win.set_shown_w(pixels.0 as i32);
    win.set_shown_h(pixels.1 as i32);
    win.window().set_size(slint::PhysicalSize::new(size.0, size.1));
    if let Some((x, y, w, h)) = work_area(window::hwnd(app)) {
        win.window().set_position(slint::PhysicalPosition::new(x + (w - size.0 as i32) / 2, y + (h - size.1 as i32) / 2));
    }
    win.show()?;
    // the native window doesn't exist yet: show only asks for it
    let (weak, owner) = (win.as_weak(), window::hwnd(app));
    let attaching = slint::spawn_local(async move {
        let Some(win) = weak.upgrade() else { return };
        match win.window().winit_window().await {
            Ok(_) => attach(&win, owner),
            // headless runs
            Err(e) => tracing::debug!("image viewer: {e}"),
        }
    });
    if let Err(e) = attaching {
        tracing::warn!("image viewer: {e}");
    }
    VIEWER.with_borrow_mut(|v| {
        *v = Some(Viewer {
            win,
            id: img.id,
            index: img.index,
            wanted_index: Cell::new(img.index),
            count: img.count,
            wheel_held: Cell::new(None),
            hide_scale: slint::Timer::default(),
            hide_strip: slint::Timer::default(),
            refit: slint::Timer::default(),
            pixels: img.pixels,
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
            go_to((v.wanted_index.get() as i64 + by).rem_euclid(v.count as i64) as usize);
        }
    });
}

fn go_to(index: usize) {
    with_current(|v| {
        if index != v.wanted_index.get() {
            v.wanted_index.set(index);
            v.win.set_current(index as i32);
            v.win.set_strip_on(true);
            v.hide_strip.start(slint::TimerMode::SingleShot, STRIP_SHOWN, || with_current(|v| v.win.set_strip_on(false)));
            let fit = screen_box(viewer_hwnd(&v.win));
            send_cmd(Command::ViewImage { id: v.id.clone(), index, fit, from_file: false });
        }
    });
}

/// The scale after some wheel notches. Steps rarely land on 100%, the sharpest view, so one that
/// would pass it or end up near it stops there. Only on the way to it: on the way out, the small
/// steps of a touchpad could never leave.
fn wheel_scale(scale: f64, notches: f32) -> f64 {
    let next = scale * ZOOM_STEP.powf(notches as f64);
    let passes = (scale < 1.0 && next > 1.0) || (scale > 1.0 && next < 1.0);
    let (from, to) = ((scale - 1.0).abs(), (next - 1.0).abs());
    if passes || (to < from && to <= NEAR_ORIGINAL) { 1.0 } else { next }
}

/// The wheel scales the image, and the window with it up to the screen, keeping the image point
/// under the cursor (around the center, shrinking could leave the cursor outside the window).
/// `cursor` is in physical px within the window.
fn zoom(notches: f32, cursor_x: f32, cursor_y: f32) {
    let moved = VIEWER.with_borrow(|v| {
        let v = v.as_ref()?;
        // also at the limits and during the hold: it tells why nothing moves
        v.win.set_scale_on(true);
        v.hide_scale.start(slint::TimerMode::SingleShot, SCALE_SHOWN, || with_current(|v| v.win.set_scale_on(false)));
        if v.wheel_held.get().is_some_and(|until| Instant::now() < until) {
            return None;
        }
        let full = v.pixels.full();
        let (full_w, full_h) = (full.0 as f64, full.1 as f64);
        let old = shown(v);
        let (area_w, area_h) = area_size(v);
        let min = min_size(v);
        let scale = old.0 as f64 / full_w;
        // down to the minimum window or the original size, whichever is larger
        let low = (min.0 as f64 / full_w).min(min.1 as f64 / full_h).min(1.0).min(scale);
        let high = (area_w as f64 / full_w).min(area_h as f64 / full_h).max(ZOOM_MAX).max(scale);
        let scale = wheel_scale(scale, notches).clamp(low, high);
        // The renderer steps through source pixels in 1/256ths, truncated: at other scales the
        // error adds up across the image, up to 8px at 1080p, and the patch lands elsewhere. So
        // only scales where the step is a whole number of 1/256ths.
        let step = (256.0 / scale).round().clamp((256.0 / high).ceil().max(1.0), (256.0 / low).floor().max(1.0));
        let new = (((full_w * 256.0 / step) as u32).max(1), ((full_h * 256.0 / step) as u32).max(1));
        if new == old {
            return None;
        }
        if new == full {
            v.wheel_held.set(Some(Instant::now() + HOLD_AT_ORIGINAL));
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
        show_original(v);
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
    let Some(hwnd) = VIEWER.with_borrow(|v| v.as_ref().map(|v| viewer_hwnd(&v.win))) else {
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

/// The fitted image only looks right at its own size, and patches line up with the original.
fn show_original(v: &Viewer) {
    let has = v.win.get_source().size();
    if (has.width, has.height) != v.pixels.full() {
        v.win.set_source(Image::from_rgba8(v.pixels.original.clone()));
    }
}

fn schedule_refit() {
    // `resized` also fires while the window is being created, before it is stored
    with_current(|v| v.refit.start(slint::TimerMode::SingleShot, REFIT_AFTER, refit));
}

/// The view changed: smooth what's visible now, unless the image is drawn 1:1 or as bare pixels.
fn refit() {
    with_current(|v| {
        let client = client_size(v);
        let target = shown(v);
        let has = v.win.get_source().size();
        if target == (has.width, has.height) {
            return clear_patch(v);
        }
        show_original(v);
        let full = v.pixels.full();
        if target.0 as f64 / full.0 as f64 >= SMOOTH_BELOW {
            return clear_patch(v);
        }
        let Some(area) = visible_part(target, client, (v.win.get_cur_x(), v.win.get_cur_y())) else { return };
        let id = v.patch_id.get() + 1;
        v.patch_id.set(id);
        if v.patch_busy.replace(true) {
            return;
        }
        let (src, opaque) = (v.pixels.original.clone(), v.pixels.opaque);
        let crop = patch_crop(area, full, target);
        std::thread::spawn(move || {
            let pixels = sigwin_thumb::resize_view(src.as_bytes(), full, crop, (area.2, area.3), opaque);
            drop(src);
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
    use windows_sys::Win32::UI::Shell::SetWindowSubclass;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWLP_HWNDPARENT, GWL_EXSTYLE, GWL_STYLE, WS_EX_APPWINDOW,
    };

    let hwnd = viewer_hwnd(win);
    // no native window in headless runs
    if hwnd == 0 || owner == 0 {
        return;
    }
    // SAFETY: both are our own live windows, on their UI thread; the subclass goes with the window
    unsafe {
        SetWindowSubclass(hwnd as _, Some(filter_styles), 1, 0);
        let style = GetWindowLongPtrW(hwnd as _, GWL_STYLE);
        SetWindowLongPtrW(hwnd as _, GWL_STYLE, style & !(NO_SNAP as isize));
        let style = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, style & !(WS_EX_APPWINDOW as isize));
        SetWindowLongPtrW(hwnd as _, GWLP_HWNDPARENT, owner);
    }
    // removes the button the window got when shown; without the style it stays away
    let done = win.window().with_winit_window(|w| {
        w.set_skip_taskbar(true);
        w.set_undecorated_shadow(true);
    });
    if done.is_none() {
        tracing::warn!("image viewer: no winit window");
    }
}

/// A hidden window shows up transparent except for what changed last: the software renderer lost
/// track of the damage, as in the main window (`winhook`).
#[cfg(windows)]
fn repaint() {
    with_current(|v| v.win.set_repaint_flip(!v.win.get_repaint_flip()));
}

/// The styles the system snaps, maximizes and minimizes a window by. Minimized alone, the viewer
/// would have no taskbar button to come back from.
#[cfg(windows)]
const NO_SNAP: windows_sys::Win32::UI::WindowsAndMessaging::WINDOW_STYLE = windows_sys::Win32::UI::WindowsAndMessaging::WS_THICKFRAME
    | windows_sys::Win32::UI::WindowsAndMessaging::WS_MAXIMIZEBOX
    | windows_sys::Win32::UI::WindowsAndMessaging::WS_MINIMIZEBOX;

/// winit gives every window `WS_EX_APPWINDOW`, which keeps even an owned one on the taskbar, and
/// a resizable one `NO_SNAP`, and sets them again whenever its flags change.
#[cfg(windows)]
unsafe extern "system" fn filter_styles(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _: usize,
    _: usize,
) -> isize {
    use windows_sys::Win32::UI::Shell::DefSubclassProc;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, STYLESTRUCT, WM_SHOWWINDOW, WM_STYLECHANGING, WS_EX_APPWINDOW,
    };
    // back with the owner after it was minimized
    if msg == WM_SHOWWINDOW && wparam != 0 {
        // after winit has handled the message, and again in case a frame was presented in between
        let _ = slint::invoke_from_event_loop(repaint);
        slint::Timer::single_shot(Duration::from_millis(120), repaint);
    }
    if msg == WM_STYLECHANGING {
        // SAFETY: for this message lparam points to the STYLESTRUCT being applied
        let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
        match wparam as i32 {
            GWL_STYLE => style.styleNew &= !NO_SNAP,
            GWL_EXSTYLE => style.styleNew &= !WS_EX_APPWINDOW,
            _ => {}
        }
    }
    // SAFETY: forwards this very call
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
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
    fn wheel_stops_at_the_original_size() {
        assert_eq!(wheel_scale(0.95, 1.0), 1.0);
        assert_eq!(wheel_scale(1.05, -1.0), 1.0);
        // a burst of notches in one event stops too
        assert_eq!(wheel_scale(0.5, 10.0), 1.0);
        assert_eq!(wheel_scale(1.0, 1.0), ZOOM_STEP);
        assert_eq!(wheel_scale(1.0, -1.0), 1.0 / ZOOM_STEP);
        assert_eq!(wheel_scale(0.5, 1.0), 0.5 * ZOOM_STEP);
    }

    #[test]
    fn wheel_goes_to_the_original_size_from_near_it() {
        // would end up at 99% and 104%
        assert_eq!(wheel_scale(0.9, 1.0), 1.0);
        assert_eq!(wheel_scale(1.144, -1.0), 1.0);
        // not near enough
        assert_eq!(wheel_scale(0.8, 1.0), 0.8 * ZOOM_STEP);
        // small steps can leave 100%, and don't fall back while leaving
        let out = wheel_scale(1.0, 0.1);
        assert!(out > 1.0 && out < 1.05);
        assert!(wheel_scale(out, 0.1) > out);
    }

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
