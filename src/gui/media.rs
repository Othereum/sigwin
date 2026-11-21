//! Pictures and photos arriving for rows already shown: thumbnails, their labels, avatars.
//!
//! They often come after their rows: each one patches the rows that show it.

use std::collections::HashSet;
use std::sync::Arc;

use super::*;

pub(super) fn avatar_arrived(app: &AppWindow, t: Arc<types::Thumb>) {
    UI.with_borrow_mut(|ui| {
        let img = (t.width > 0)
            .then(|| Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height)));
        if t.key == avatar::ME_SIDEBAR {
            app.set_me_has_photo(img.is_some());
            app.set_me_photo(img.unwrap_or_default());
            return;
        }
        if t.key == avatar::ME_CARD {
            app.global::<SettingsData>().set_account_has_photo(img.is_some());
            app.global::<SettingsData>().set_account_photo(img.unwrap_or_default());
            return;
        }
        if let Some(uuid) = t.key.strip_prefix(avatar::READ_PREFIX) {
            let Some(img) = img else { return };
            ui.sender_avatars.insert(t.key.clone(), img);
            rows::redraw_rows(ui, |m| m.readers.iter().take(types::MAX_READER_PHOTOS).any(|(u, _)| u == uuid));
            return;
        }
        if let Some(uuid) = t.key.strip_prefix(avatar::MSG_PREFIX) {
            let Some(img) = img else { return };
            ui.sender_avatars.insert(uuid.to_string(), img);
            rows::redraw_rows(ui, |m| m.show_avatar && m.sender_key == uuid);
            if ui.typing.first().is_some_and(|(u, _)| u == uuid) {
                sync_typing(ui);
            }
            return;
        }
        if let Some(uuid) = t.key.strip_prefix(avatar::LIST_READ_PREFIX) {
            let Some(img) = img else { return };
            ui.avatars.insert(t.key.clone(), img);
            for i in 0..slint::Model::row_count(&*ui.threads) {
                let Some(mut row) = slint::Model::row_data(&*ui.threads, i) else { continue };
                let Some(dto) = ui.thread_dtos.iter().find(|d| d.key.as_str() == row.key.as_str()) else { continue };
                if dto.readers.iter().any(|(u, _)| u == uuid) {
                    row.readers = readers_model(&dto.readers, &ui.avatars, avatar::LIST_READ_PREFIX);
                    slint::Model::set_row_data(&*ui.threads, i, row);
                }
            }
            return;
        }
        let changed = match img.clone() {
            Some(i) => {
                ui.avatars.insert(t.key.clone(), i);
                true
            }
            None => ui.avatars.remove(&t.key).is_some(),
        };
        if !changed {
            return;
        }
        for i in 0..slint::Model::row_count(&*ui.threads) {
            if let Some(mut row) = slint::Model::row_data(&*ui.threads, i) {
                if row.key.as_str() == t.key {
                    row.has_photo = img.is_some();
                    row.photo = img.clone().unwrap_or_default();
                    slint::Model::set_row_data(&*ui.threads, i, row);
                }
            }
        }
        if t.key == ui.open_key {
            header_avatar(app, ui);
        }
    });
}

pub(super) fn thumb_arrived(t: Arc<types::Thumb>) {
    if UI.with_borrow(|ui| ui.released) {
        return;
    }
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height);
    let img = Image::from_rgba8(buf);
    UI.with_borrow_mut(|ui| {
        ui.thumbs.insert(t.key.clone(), img);
        ui.shown.dropped.remove(&t.key);
        // the rows' pictures come before the rows are on screen: kept like one that just left it
        let clock = ui.shown.tick();
        ui.shown.left.insert(t.key.clone(), clock);
        // every place that shows it: the same picture twice in a grid has one key for both cells
        rows::redraw_rows(ui, |m| rows::shows_key(m, &t.key));
        trim_pictures(ui);
    });
}

/// What the open chat's pictures off screen may hold together. The ones longest off screen are
/// let go of past it, and asked for again when their row comes back (from the thumbnail cache:
/// no download, no original decoded). Those on screen are always kept.
const OFF_SCREEN: usize = 8 << 20;

/// Which rows are on screen, and what became of the pictures that aren't.
#[derive(Default)]
pub(crate) struct Shown {
    /// the ids of the rows at the list's top and bottom edges ("": its first or last). Not the
    /// rows the list's instances show: it drops them without a word as they leave the screen.
    top: String,
    bottom: String,
    /// when each picture was last on screen (or arrived), by `clock`
    left: HashMap<String, u64>,
    /// pictures let go of while their rows are loaded: asked for again when shown
    dropped: HashSet<String>,
    clock: u64,
}

impl Shown {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The rows are now `items`: what was kept of others goes.
    pub(crate) fn keep_rows(&mut self, items: &[MsgDto]) {
        let keys: HashSet<&str> = items.iter().flat_map(rows::picture_keys).collect();
        self.left.retain(|k, _| keys.contains(k.as_str()));
        self.dropped.retain(|k| keys.contains(k.as_str()));
    }

    /// When the picture under `key` was last on screen (or arrived): 0 for never.
    pub(super) fn last_seen(&self, key: &str) -> u64 {
        self.left.get(key).copied().unwrap_or(0)
    }

    /// The pictures of the rows on screen, of `items`.
    pub(super) fn on_screen_keys<'a>(&self, items: &'a [MsgDto]) -> HashSet<&'a str> {
        self.on_screen(items).iter().flat_map(rows::picture_keys).collect()
    }

    /// The rows on screen, of `items`.
    fn on_screen<'a>(&self, items: &'a [MsgDto]) -> &'a [MsgDto] {
        let at = |id: &str| items.iter().position(|m| m.id == id);
        let top = at(&self.top).unwrap_or(0);
        let bottom = at(&self.bottom).unwrap_or(items.len().saturating_sub(1));
        items.get(top.min(bottom)..=top.max(bottom)).unwrap_or_default()
    }
}

/// The message list scrolled: `top` and `bottom` are the rows at its edges.
pub(super) fn view_moved(top: &str, bottom: &str) {
    let ask = UI.with_borrow_mut(|ui| {
        if ui.released {
            return Vec::new();
        }
        (ui.shown.top, ui.shown.bottom) = (top.to_string(), bottom.to_string());
        let clock = ui.shown.tick();
        let mut ask = Vec::new();
        let keys: Vec<String> = ui.shown.on_screen(&ui.last).iter().flat_map(rows::picture_keys).map(str::to_string).collect();
        for k in keys {
            if ui.shown.dropped.remove(&k) {
                ask.push(k.clone());
            }
            ui.shown.left.insert(k, clock);
        }
        trim_pictures(ui);
        ask
    });
    if !ask.is_empty() {
        send_cmd(Command::WantPictures(ask));
    }
}

/// Lets go of what pictures off screen hold past their budgets: thumbnails, then animations.
fn trim_pictures(ui: &mut Ui) {
    trim_thumbs(ui);
    super::anim::trim_list(ui);
}

/// Lets go of the pictures longest off screen past `OFF_SCREEN`, taking them out of their rows.
fn trim_thumbs(ui: &mut Ui) {
    let on_screen = ui.shown.on_screen_keys(&ui.last);
    // the rows' only: the composer's and the link card's stay
    let rows: HashSet<&str> = ui.last.iter().flat_map(rows::picture_keys).collect();
    let mut off: Vec<(u64, &String, usize)> = ui
        .thumbs
        .iter()
        .filter(|(k, _)| rows.contains(k.as_str()) && !on_screen.contains(k.as_str()))
        .map(|(k, img)| (ui.shown.last_seen(k), k, held::Measured::bytes(img)))
        .collect();
    let mut over = off.iter().map(|o| o.2).sum::<usize>().saturating_sub(OFF_SCREEN);
    if over == 0 {
        return;
    }
    off.sort_by_key(|o| o.0);
    let mut gone = HashSet::new();
    for (_, k, bytes) in off {
        if over == 0 {
            break;
        }
        gone.insert(k.clone());
        over = over.saturating_sub(bytes);
    }
    for k in &gone {
        ui.thumbs.remove(k);
        ui.shown.left.remove(k);
        ui.shown.dropped.insert(k.clone());
    }
    rows::redraw_rows(ui, |m| rows::picture_keys(m).any(|k| gone.contains(k)));
}

pub(super) fn label_arrived(key: String, label: types::MediaLabel) {
    UI.with_borrow_mut(|ui| {
        if ui.released {
            return;
        }
        // in place when a cell shows it already: no row is drawn again for a percent
        if !ui.media_labels.set(&key, &label) {
            rows::redraw_rows(ui, |m| rows::shows_key(m, &key));
        }
    });
}
