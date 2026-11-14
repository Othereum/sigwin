//! The "updated, what's new" offer after an update. The notes themselves live on the GitHub
//! releases page: embedding them took a notes chain across releases that one release made
//! without the script would break.

use slint::ComponentHandle;

use super::*;

const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// every release newest first, so an update that skipped versions loses nothing
const RELEASES: &str = "https://github.com/Othereum/sigwin/releases";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    /// offer the notes, and remember the version once they were opened or dismissed
    Show,
    /// nothing to offer, but this version counts as seen
    Record,
    Nothing,
}

/// What to do at startup. `last` is the version whose notes were last seen ("" = never
/// recorded). An app from before the offer recorded nothing, so an account that already
/// exists means it was updated, while no account means a fresh install.
fn decide(last: &str, current: &str, has_account: bool) -> Action {
    if last.is_empty() {
        return if has_account { Action::Show } else { Action::Record };
    }
    // a downgrade or a rerun of the same version
    if crate::install::version(current) > crate::install::version(last) {
        Action::Show
    } else {
        Action::Nothing
    }
}

/// An account exists and the main page is up: offer this version's notes if they are new.
pub(crate) fn on_ready(app: &AppWindow) {
    // a dev build carries the next version's number before its release exists, and recording
    // it would hide the offer once that release is installed
    if cfg!(sigwin_dev) {
        return;
    }
    let last = UI.with_borrow(|ui| ui.settings.last_seen_version.clone());
    match decide(&last, CURRENT, true) {
        Action::Show => app.set_whatsnew_offer(true),
        Action::Record => record(),
        Action::Nothing => {}
    }
}

/// No account: a fresh install, whose first version needs no "what's new".
pub(crate) fn on_need_link() {
    if cfg!(sigwin_dev) {
        return;
    }
    let last = UI.with_borrow(|ui| ui.settings.last_seen_version.clone());
    if decide(&last, CURRENT, false) == Action::Record {
        record();
    }
}

fn record() {
    if cfg!(sigwin_dev) {
        return;
    }
    UI.with_borrow_mut(|ui| {
        if ui.settings.last_seen_version != CURRENT {
            ui.settings.last_seen_version = CURRENT.to_string();
            ui.settings.save(&ui.settings_path);
        }
    });
}

pub(crate) fn wire(app: &AppWindow) {
    let weak = app.as_weak();
    app.on_open_whatsnew(move || {
        link::open(RELEASES);
        if let Some(app) = weak.upgrade() {
            app.set_whatsnew_offer(false);
        }
        record();
    });
    let weak = app.as_weak();
    app.on_dismiss_whatsnew(move || {
        if let Some(app) = weak.upgrade() {
            app.set_whatsnew_offer(false);
        }
        record();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decides_by_last_seen_version() {
        // updated from an app that didn't record versions
        assert_eq!(decide("", "0.5.0", true), Action::Show);
        // fresh install
        assert_eq!(decide("", "0.5.0", false), Action::Record);
        assert_eq!(decide("0.4.9", "0.5.0", true), Action::Show);
        assert_eq!(decide("0.4.9", "0.5.0", false), Action::Show);
        assert_eq!(decide("0.5.0", "0.5.0", true), Action::Nothing);
        assert_eq!(decide("0.5.0", "0.4.9", true), Action::Nothing);
        assert_eq!(decide("0.9.0", "0.10.0", true), Action::Show);
    }
}
