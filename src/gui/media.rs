//! Pictures and photos arriving for rows already shown: thumbnails, their labels, avatars.
//!
//! They often come after their rows: each one patches the rows that show it.

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
        // every place that shows it: the same picture twice in a grid has one key for both cells
        rows::redraw_rows(ui, |m| rows::shows_key(m, &t.key));
    });
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
