//! The link preview above the composer: which link it's for, what's shown, what goes with the
//! message. Fetching contacts the link's site, so nothing happens with the setting off, and a
//! draft that comes back with a thread fetches nothing until it's edited.

use std::time::Duration;

use super::*;
use crate::backend::linkpreview::COMPOSER_KEY;
use crate::types::LinkCard;

/// Typing a link by hand shouldn't fetch every prefix of it.
const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct State {
    /// the link the bar is about (it may have no preview)
    url: Option<String>,
    /// ✕ pressed; forgotten when the composer empties or the thread changes
    dismissed: Vec<String>,
    /// thumbnails of what's shown
    pub(crate) key: String,
    timer: slint::Timer,
}

/// The first link of the composer's text that gets a preview.
fn candidate(ui: &Ui, text: &str) -> Option<String> {
    if !ui.settings.link_previews || ui.editing.is_some() || !ui.files.is_empty() {
        return None;
    }
    let (plain, styles) = format::parse(text);
    // the preview would give away what's behind the spoiler
    if styles.iter().any(|s| s.2 == view::STYLE_SPOILER) {
        return None;
    }
    link::preview_urls(&plain).into_iter().find(|u| !ui.link.dismissed.contains(u))
}

fn clear_bar(app: &AppWindow) {
    app.set_compose_link_url(SharedString::new());
    app.set_compose_link_title(SharedString::new());
    app.set_compose_link_desc(SharedString::new());
    app.set_compose_link_domain(SharedString::new());
    app.set_compose_link_has_image(false);
    app.set_compose_link_image(Image::default());
}

fn forget_thumbs(ui: &mut Ui, card_too: bool) {
    let key = std::mem::take(&mut ui.link.key);
    ui.thumbs.remove(&format!("{key}{COMPOSER_KEY}"));
    if card_too {
        ui.thumbs.remove(&key);
    }
}

/// The composer was edited: looks at it again once typing pauses.
pub(crate) fn edited(app: &AppWindow) {
    let weak = app.as_weak();
    UI.with_borrow(|ui| {
        ui.link.timer.start(slint::TimerMode::SingleShot, DEBOUNCE, move || {
            if let Some(app) = weak.upgrade() {
                update(&app);
            }
        });
    });
}

/// Brings the bar and the fetch in line with the composer's text.
pub(crate) fn update(app: &AppWindow) {
    let text = app.get_draft();
    let changed = UI.with_borrow_mut(|ui| {
        if text.trim().is_empty() {
            ui.link.dismissed.clear();
        }
        let next = candidate(ui, &text);
        if ui.link.url == next {
            return None;
        }
        ui.link.url = next.clone();
        forget_thumbs(ui, true);
        Some(next)
    });
    let Some(next) = changed else { return };
    clear_bar(app);
    if let Some(url) = &next {
        app.set_compose_link_url(url.as_str().into());
        app.set_compose_link_title("미리보기를 불러오는 중…".into());
        app.set_compose_link_domain(link::domain(url).into());
    }
    send_cmd(Command::LinkPreview(next));
}

/// The backend's result for the composer's link.
pub(crate) fn fetched(app: &AppWindow, url: &str, card: Option<LinkCard>) {
    let Some(thumb) = UI.with_borrow_mut(|ui| {
        if ui.link.url.as_deref() != Some(url) {
            return None;
        }
        let key = card.as_ref().map(|c| c.image_key.clone()).unwrap_or_default();
        let thumb = ui.thumbs.get(&format!("{key}{COMPOSER_KEY}")).cloned();
        ui.link.key = key;
        Some(thumb)
    }) else {
        return;
    };
    // the site has nothing to show: no bar, and the link isn't tried again
    let Some(card) = card else { return clear_bar(app) };
    app.set_compose_link_url(card.url.into());
    app.set_compose_link_title(card.title.into());
    app.set_compose_link_desc(card.description.into());
    app.set_compose_link_domain(card.domain.into());
    app.set_compose_link_has_image(thumb.is_some());
    app.set_compose_link_image(thumb.unwrap_or_default());
}

/// ✕ on the bar: this link goes without a preview; a later one in the text may get one.
pub(crate) fn dismiss(app: &AppWindow) {
    UI.with_borrow_mut(|ui| {
        if let Some(url) = ui.link.url.clone() {
            ui.link.dismissed.push(url);
        }
    });
    update(app);
}

/// Sending `text` now: the link to preview on it. The bar's preview goes with the message.
pub(crate) fn take_for_send(app: &AppWindow, text: &str) -> Option<String> {
    let url = UI.with_borrow_mut(|ui| {
        ui.link.timer.stop();
        let url = candidate(ui, text);
        ui.link.url = None;
        ui.link.dismissed.clear();
        forget_thumbs(ui, false);
        url
    });
    clear_bar(app);
    url
}

/// Another thread opened: the backend drops its side when it opens the thread.
pub(crate) fn reset(ui: &mut Ui) {
    ui.link.timer.stop();
    ui.link.url = None;
    ui.link.dismissed.clear();
    forget_thumbs(ui, true);
}

pub(crate) fn clear(app: &AppWindow) {
    clear_bar(app);
}
