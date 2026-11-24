//! Whether the user is at the window, decided as Signal Desktop does (`ActiveWindowService`).
//!
//! Active: the window has the focus and there was input in it (a key, a button, a mouse move,
//! the wheel) within `ACTIVE_TIMEOUT`, or it has no focus and the wheel turned over it since it
//! lost the focus, within `WHEEL_TIMEOUT`. Input elsewhere on the PC doesn't count, and gaining
//! the focus isn't input. While inactive nothing is read and notifications show.
//!
//! Rows the list shows are reported all the time (`row_seen`) and read at once while active.
//! Becoming active reads what is on screen only after `READ_DELAY`, and only if still active,
//! so passing through the window reads nothing.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use super::{send_cmd, UI};
use crate::types::Command;
use crate::AppWindow;

/// Focused, active this long after the last input in the window.
const ACTIVE_TIMEOUT: Duration = Duration::from_secs(15);
/// Not focused, active this long after the wheel turned over the window.
const WHEEL_TIMEOUT: Duration = Duration::from_secs(1);
/// Becoming active reads the rows on screen this long after.
const READ_DELAY: Duration = Duration::from_secs(1);

/// What decides `active`, as times of the last events.
#[derive(Clone, Copy, Debug)]
struct State {
    focused: bool,
    /// any input in the window
    input: Instant,
    /// the wheel over the window
    wheel: Option<Instant>,
    /// the window lost the focus
    blur: Option<Instant>,
}

impl State {
    fn new(now: Instant) -> Self {
        Self { focused: true, input: now, wheel: None, blur: None }
    }

    fn active(&self, now: Instant) -> bool {
        self.ends().is_some_and(|end| now < end)
    }

    /// When being active ends with no further event (None: it isn't active at all).
    fn ends(&self) -> Option<Instant> {
        if self.focused {
            return Some(self.input + ACTIVE_TIMEOUT);
        }
        // a wheel before the window lost the focus doesn't count
        let wheel = self.wheel.filter(|w| self.blur.is_none_or(|b| b < *w))?;
        Some(wheel + WHEEL_TIMEOUT)
    }
}

thread_local! {
    static STATE: Cell<Option<State>> = const { Cell::new(None) };
    /// the clock of a headless run: it moves only with `pass` (None: the real one)
    static CLOCK: Cell<Option<Instant>> = const { Cell::new(None) };
    /// what the backend was last told
    static TOLD: Cell<bool> = const { Cell::new(true) };
    /// when becoming active reads the rows on screen
    static READ_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    /// the furthest row of the open thread the list reported on screen, read or not
    static SEEN: RefCell<Option<String>> = const { RefCell::new(None) };
    static TIMER: slint::Timer = slint::Timer::default();
    /// the window `input` refreshes for, kept by `refresh`
    static APP: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
}

fn now() -> Instant {
    CLOCK.get().unwrap_or_else(Instant::now)
}

fn state() -> State {
    STATE.get().unwrap_or_else(|| {
        let s = State::new(now());
        STATE.set(Some(s));
        s
    })
}

/// The user is at the window.
pub(crate) fn is_active() -> bool {
    state().active(now())
}

/// Headless: the clock stands still, so a run doesn't depend on how fast it goes; `pass`
/// moves it.
pub(crate) fn freeze() {
    CLOCK.set(Some(Instant::now()));
    STATE.set(Some(State::new(now())));
}

/// Headless: `d` passes with no input.
pub(crate) fn pass(app: &AppWindow, d: Duration) {
    if let Some(t) = CLOCK.get() {
        CLOCK.set(Some(t + d));
    }
    refresh(app);
}

/// Input in the window (`wheel`: the wheel turned). Called from the window procedure for every
/// such message, so it only keeps the time unless that made the window active.
pub(crate) fn input(wheel: bool) {
    let now = now();
    let mut s = state();
    let was = s.active(now);
    s.input = now;
    if wheel {
        s.wheel = Some(now);
    }
    STATE.set(Some(s));
    if !was && s.active(now) {
        // after the window procedure, which can run while the UI is busy
        let _ = slint::invoke_from_event_loop(|| {
            if let Some(app) = APP.with_borrow(|a| a.as_ref().and_then(|w| w.upgrade())) {
                refresh(&app);
            }
        });
    }
}

/// The window gained or lost the focus.
pub(crate) fn focus_changed(app: &AppWindow, focused: bool) {
    let mut s = state();
    s.focused = focused;
    if !focused {
        s.blur = Some(now());
    }
    STATE.set(Some(s));
    refresh(app);
    super::anim::focus_changed();
}

/// Whether the main window has the focus.
pub(crate) fn focused() -> bool {
    state().focused
}

/// Tells the backend when being active starts or ends, reads the rows on screen once
/// `READ_DELAY` has passed since it started, and comes back when the next of those is due.
pub(crate) fn refresh(app: &AppWindow) {
    APP.set(Some(app.as_weak()));
    let now = now();
    let s = state();
    let active = s.active(now);
    if TOLD.replace(active) != active {
        // every few seconds while the user works: kept out of users' log files
        tracing::debug!(active, focused = s.focused, "user presence");
        send_cmd(Command::WindowActive(active));
        READ_AT.set(active.then_some(now + READ_DELAY));
    }
    if READ_AT.get().is_some_and(|t| t <= now) {
        READ_AT.set(None);
        read_shown(app);
    }
    if CLOCK.get().is_some() {
        return;
    }
    let next = [s.ends().filter(|_| active), READ_AT.get()].into_iter().flatten().min();
    TIMER.with(|t| match next {
        Some(at) => {
            let weak = app.as_weak();
            // a moment late, so the time has surely come
            let wait = at.saturating_duration_since(now) + Duration::from_millis(20);
            t.start(slint::TimerMode::SingleShot, wait, move || {
                if let Some(app) = weak.upgrade() {
                    refresh(&app);
                }
            });
        }
        None => t.stop(),
    });
}

/// The furthest row on screen and where the list stands go to the backend again, which reads
/// them now that the user is at the window.
fn read_shown(app: &AppWindow) {
    if let Some(id) = SEEN.with_borrow(|s| s.clone()) {
        send_read(&id);
    }
    send_cmd(Command::AtBottom(app.get_near_bottom()));
}

/// Another thread opened: what was seen of the last one is gone.
pub(crate) fn thread_switched() {
    SEEN.set(None);
}

/// The list showed row `id` fully: the furthest such row is kept, and read at once while
/// active.
pub(crate) fn row_seen(id: &str) {
    let further = UI.with_borrow(|ui| {
        let pos = |id: &str| ui.last.iter().position(|m| m.id == id);
        let Some(at) = pos(id) else { return false };
        SEEN.with_borrow(|s| s.as_deref().and_then(pos).is_none_or(|seen| seen < at))
    });
    if further {
        SEEN.set(Some(id.to_string()));
    }
    if is_active() {
        send_read(id);
    }
}

/// `ReadUpTo` row `id`, each row only once, and nothing for rows above one already sent.
fn send_read(id: &str) {
    let go = UI.with_borrow_mut(|ui| {
        let pos = |ui: &super::Ui, id: &str| ui.last.iter().position(|m| m.id == id);
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

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn focused_is_active_until_the_input_is_old() {
        let t = Instant::now();
        let s = State::new(t);
        assert!(s.active(t + 14 * S));
        assert!(!s.active(t + 15 * S));
    }

    #[test]
    fn gaining_the_focus_is_not_input() {
        let t = Instant::now();
        let s = State { focused: true, input: t, wheel: None, blur: Some(t + 20 * S) };
        assert!(!s.active(t + 30 * S));
    }

    #[test]
    fn without_the_focus_only_a_fresh_wheel_counts() {
        let t = Instant::now();
        let moved = State { focused: false, input: t + 5 * S, wheel: None, blur: Some(t) };
        assert!(!moved.active(t + 5 * S));
        let wheel = State { focused: false, input: t + 5 * S, wheel: Some(t + 5 * S), blur: Some(t) };
        assert!(wheel.active(t + 5 * S));
        assert!(!wheel.active(t + 6 * S));
    }

    #[test]
    fn a_wheel_before_losing_the_focus_doesnt_count() {
        let t = Instant::now();
        let s = State { focused: false, input: t, wheel: Some(t), blur: Some(t) };
        assert!(!s.active(t));
    }
}
