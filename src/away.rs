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

thread_local! {
    static DISPLAY_OFF: Cell<bool> = const { Cell::new(false) };
}

/// The monitor was turned off or on (power setting notification).
pub fn set_display_off(off: bool) {
    DISPLAY_OFF.set(off);
}

/// The user isn't at the PC.
pub fn is_away() -> bool {
    DISPLAY_OFF.get() || locked() || idle_for() >= AWAY_AFTER
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
    fn ticks_wrap_around() {
        assert_eq!(ticks_between(1_000, 4_000), Duration::from_secs(3));
        assert_eq!(ticks_between(u32::MAX - 999, 1_000), Duration::from_millis(2_000));
    }
}
