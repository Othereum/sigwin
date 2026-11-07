//! The "what's new" offer after an update (`crate::whatsnew` decides, this shows).

use slint::{ComponentHandle, ModelRc, VecModel};

use super::*;
use crate::whatsnew::{self, Action, Kind};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// An account exists and the main page is up: offer this version's notes if they are new.
pub(crate) fn on_ready(app: &AppWindow) {
    let notes = !whatsnew::notes().is_empty();
    app.set_whatsnew_has(notes);
    // a build without notes (dev) has nothing to show and must not mark anything seen
    if !notes {
        return;
    }
    let last = UI.with_borrow(|ui| ui.settings.last_seen_version.clone());
    match whatsnew::decide(&last, CURRENT, true) {
        Action::Show => app.set_whatsnew_offer(true),
        Action::Record => record(),
        Action::Nothing => {}
    }
}

/// No account: a fresh install, whose first version needs no "what's new".
pub(crate) fn on_need_link() {
    if whatsnew::notes().is_empty() {
        return;
    }
    let last = UI.with_borrow(|ui| ui.settings.last_seen_version.clone());
    if whatsnew::decide(&last, CURRENT, false) == Action::Record {
        record();
    }
}

fn record() {
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
        let Some(app) = weak.upgrade() else { return };
        // from the offer: what the update brought. From settings: everything
        let notes = whatsnew::notes();
        let notes = if app.get_whatsnew_offer() {
            let last = UI.with_borrow(|ui| ui.settings.last_seen_version.clone());
            whatsnew::since(notes, &last, CURRENT)
        } else {
            notes
        };
        let lines: Vec<NoteLine> = notes
            .into_iter()
            .map(|l| NoteLine {
                kind: match l.kind {
                    Kind::Heading => 0,
                    Kind::Bullet => 1,
                    Kind::Text => 2,
                    Kind::Version => 3,
                },
                text: l.text.into(),
            })
            .collect();
        app.set_whatsnew_lines(ModelRc::new(VecModel::from(lines)));
        app.set_whatsnew_open(true);
        app.set_whatsnew_offer(false);
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
