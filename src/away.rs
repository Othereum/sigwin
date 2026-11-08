//! Whether the user is away from the PC: no input for a while, the screen locked or off.
//!
//! Away counts like a window in the background: nothing is read (no read receipts), and
//! notifications show. Signal Desktop does the same after 15 seconds without input in its
//! window; here reading is by scrolling rows into view, so a pause over a long message costs
//! nothing and the wait can be long.

use std::cell::Cell;
use std::time::Duration;

/// Without input this long, the user is away.
const AWAY_AFTER: Duration = Duration::from_secs(180);

/// 0: the real state; 1: always present; 2: always away. Scenarios pin it, so a run doesn't
/// depend on whether the person at the PC happens to be idle or has locked it.
static FORCED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Pins the answer of `is_away` (`None`: the real state again).
pub fn force(away: Option<bool>) {
    let v = match away {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    FORCED.store(v, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    static DISPLAY_OFF: Cell<bool> = const { Cell::new(false) };
}

/// The monitor was turned off or on (power setting notification).
pub fn set_display_off(off: bool) {
    DISPLAY_OFF.set(off);
}

/// The user isn't at the PC.
pub fn is_away() -> bool {
    match FORCED.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    DISPLAY_OFF.get() || locked() || idle_for() >= AWAY_AFTER
}

/// 0: the real state; 1: always on screen; 2: never (see `FORCED`).
static FORCED_SHOWN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Pins the answer of `on_screen` (`None`: the real state again).
pub fn force_on_screen(shown: Option<bool>) {
    let v = match shown {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    };
    FORCED_SHOWN.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Part of the window can be seen: not minimized, not on another virtual desktop, and not
/// mostly covered by other windows. Says nothing about whether anyone looks at it.
///
/// Windows has no call for this, so it walks the windows above ours and takes their frames off
/// ours. Windows that don't hide what is below don't count: invisible, minimized, cloaked,
/// click-through, and see-through ones (game and chat overlays).
pub fn on_screen(hwnd: isize) -> bool {
    match FORCED_SHOWN.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    uncovered(hwnd)
}

/// A window with at least this share of its area uncovered is on screen.
#[cfg(windows)]
const SHOWN_SHARE: f64 = 0.5;

#[cfg(windows)]
fn uncovered(hwnd: isize) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, GetRegionData, RDH_RECTANGLES, RGN_DIFF, RGNDATA};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindow, GW_HWNDPREV};
    let me = hwnd as HWND;
    if me.is_null() || !hides_below(me, false) {
        return false;
    }
    let Some(frame) = frame_of(me) else { return false };
    let area = rect_area(&frame);
    if area <= 0 {
        return false;
    }
    // SAFETY: GDI regions are created and freed here; the data buffer is sized by the call
    // that fills it (8-byte aligned for the header), and only read inside its length
    unsafe {
        let left = CreateRectRgn(frame.left, frame.top, frame.right, frame.bottom);
        let mut above = GetWindow(me, GW_HWNDPREV);
        while !above.is_null() {
            if hides_below(above, true) {
                if let Some(f) = frame_of(above) {
                    let r = CreateRectRgn(f.left, f.top, f.right, f.bottom);
                    CombineRgn(left, left, r, RGN_DIFF);
                    DeleteObject(r);
                }
            }
            above = GetWindow(above, GW_HWNDPREV);
        }
        let size = GetRegionData(left, 0, std::ptr::null_mut()) as usize;
        let mut buf = vec![0u64; size.div_ceil(8)];
        let data = buf.as_mut_ptr() as *mut RGNDATA;
        let mut visible = 0i64;
        if size > 0 && GetRegionData(left, size as u32, data) as usize == size {
            let header = &(*data).rdh;
            if header.iType == RDH_RECTANGLES {
                let rects = (*data).Buffer.as_ptr() as *const windows_sys::Win32::Foundation::RECT;
                for i in 0..header.nCount as usize {
                    visible += rect_area(&*rects.add(i));
                }
            }
        }
        DeleteObject(left);
        visible as f64 >= area as f64 * SHOWN_SHARE
    }
}

#[cfg(windows)]
fn rect_area(r: &windows_sys::Win32::Foundation::RECT) -> i64 {
    (r.right - r.left).max(0) as i64 * (r.bottom - r.top).max(0) as i64
}

/// The window's visible frame, without the invisible resize borders and shadow.
#[cfg(windows)]
fn frame_of(h: windows_sys::Win32::Foundation::HWND) -> Option<windows_sys::Win32::Foundation::RECT> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `r` is the size the attribute needs
    let hr = unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut r as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32) };
    (hr == 0).then_some(r)
}

/// Whether the window, shown, hides what is below it. `as_cover` is for windows above ours:
/// those that let the view through don't count.
#[cfg(windows)]
fn hides_below(h: windows_sys::Win32::Foundation::HWND, as_cover: bool) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetLayeredWindowAttributes, GetWindowLongW, IsIconic, IsWindowVisible, GWL_EXSTYLE, LWA_ALPHA, LWA_COLORKEY, WS_EX_LAYERED,
        WS_EX_TRANSPARENT,
    };
    // SAFETY: plain queries on a window handle (which may be gone by now: they then fail)
    unsafe {
        if IsWindowVisible(h) == 0 || IsIconic(h) != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, 4);
        if cloaked != 0 {
            return false;
        }
        if !as_cover {
            return true;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT != 0 {
            return false;
        }
        if ex & WS_EX_LAYERED != 0 {
            let (mut key, mut alpha, mut flags) = (0u32, 0u8, 0u32);
            // per-pixel alpha (no attributes to read), color keys and partial opacity: see-through
            if GetLayeredWindowAttributes(h, &mut key, &mut alpha, &mut flags) == 0 || flags & LWA_COLORKEY != 0 || (flags & LWA_ALPHA != 0 && alpha < 200) {
                return false;
            }
        }
        true
    }
}

#[cfg(not(windows))]
fn uncovered(_: isize) -> bool {
    false
}

/// Time since the last keyboard or mouse input, system-wide.
#[cfg(windows)]
fn idle_for() -> Duration {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: `info` is initialised with its size, as the call requires
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return Duration::ZERO;
    }
    // SAFETY: plain query
    ticks_between(info.dwTime, unsafe { GetTickCount() })
}

/// Both are 32-bit millisecond tick counts that wrap around every 49 days.
fn ticks_between(last: u32, now: u32) -> Duration {
    Duration::from_millis(now.wrapping_sub(last) as u64)
}

/// The secure desktop is up (lock screen, another user's session, a UAC prompt): the input
/// desktop can't be opened then.
#[cfg(windows)]
fn locked() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, DESKTOP_SWITCHDESKTOP};
    // SAFETY: opens and closes a handle; nothing else is touched
    unsafe {
        let desk = OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP);
        if desk.is_null() {
            return true;
        }
        CloseDesktop(desk);
    }
    false
}

#[cfg(not(windows))]
fn idle_for() -> Duration {
    Duration::ZERO
}

#[cfg(not(windows))]
fn locked() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_answer_wins_over_the_real_state() {
        force(Some(true));
        assert!(is_away());
        force(Some(false));
        assert!(!is_away());
        force(None);
    }

    #[test]
    fn forced_screen_state_wins_over_the_real_one() {
        force_on_screen(Some(true));
        assert!(on_screen(0));
        force_on_screen(Some(false));
        assert!(!on_screen(0));
        force_on_screen(None);
        assert!(!on_screen(0), "no window, nothing to see");
    }

    #[test]
    fn ticks_wrap_around() {
        assert_eq!(ticks_between(1_000, 4_000), Duration::from_secs(3));
        assert_eq!(ticks_between(u32::MAX - 999, 1_000), Duration::from_millis(2_000));
    }
}
