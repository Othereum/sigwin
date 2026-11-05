//! In-app messages: short notices over the conversation pane and the image viewer, and the
//! connection banner above the thread list.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use slint::{ComponentHandle, SharedString, Timer, TimerMode};

use super::viewer;
use crate::AppWindow;

const NOTICE_SHOWN: Duration = Duration::from_secs(6);
/// A connection problem shows only if it lasts this long: a short drop fixes itself unseen.
const CONNECTION_DELAY: Duration = Duration::from_secs(5);

thread_local! {
    static HIDE: Timer = Timer::default();
    /// the notice showing is work in progress
    static IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
    static CONNECTION: Timer = Timer::default();
    /// what the banner will say when `CONNECTION` fires
    static NEXT_CONNECTION: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set(app: &AppWindow, text: SharedString) {
    viewer::set_notice(&text);
    app.set_notice_text(text);
}

pub(crate) fn show(app: &AppWindow, text: &str) {
    IN_PROGRESS.set(false);
    set(app, text.into());
    let weak = app.as_weak();
    HIDE.with(|t| {
        t.start(TimerMode::SingleShot, NOTICE_SHOWN, move || {
            if let Some(app) = weak.upgrade() {
                set(&app, SharedString::new());
            }
        });
    });
}

pub(crate) fn progress(app: &AppWindow, text: &str) {
    HIDE.with(Timer::stop);
    IN_PROGRESS.set(true);
    set(app, text.into());
}

/// Leaves a notice alone: a failure elsewhere must stay readable.
pub(crate) fn progress_done(app: &AppWindow) {
    if IN_PROGRESS.replace(false) {
        set(app, SharedString::new());
    }
}

pub(crate) fn connection(app: &AppWindow, text: String, at_once: bool) {
    CONNECTION.with(|t| {
        if text.is_empty() || at_once || !app.get_connection_text().is_empty() {
            t.stop();
            app.set_connection_text(text.into());
            return;
        }
        NEXT_CONNECTION.with_borrow_mut(|n| *n = text);
        if !t.running() {
            let weak = app.as_weak();
            t.start(TimerMode::SingleShot, CONNECTION_DELAY, move || {
                if let Some(app) = weak.upgrade() {
                    app.set_connection_text(NEXT_CONNECTION.take().into());
                }
            });
        }
    });
}
