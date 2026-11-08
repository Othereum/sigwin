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
            for (i, m) in ui.last.iter().enumerate() {
                if m.readers.iter().take(types::MAX_READER_PHOTOS).any(|(u, _)| u == uuid) {
                    if let Some(mut row) = slint::Model::row_data(&*ui.messages, i) {
                        row.readers = reader_items(m, &ui.sender_avatars);
                        slint::Model::set_row_data(&*ui.messages, i, row);
                    }
                }
            }
            return;
        }
        if let Some(uuid) = t.key.strip_prefix(avatar::MSG_PREFIX) {
            let Some(img) = img else { return };
            ui.sender_avatars.insert(uuid.to_string(), img.clone());
            for (i, m) in ui.last.iter().enumerate() {
                if m.show_avatar && m.sender_key == uuid {
                    if let Some(mut row) = slint::Model::row_data(&*ui.messages, i) {
                        row.has_avatar = true;
                        row.avatar = img.clone();
                        slint::Model::set_row_data(&*ui.messages, i, row);
                    }
                }
            }
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
        ui.thumbs.insert(t.key.clone(), img.clone());
        for (i, m) in ui.last.iter().enumerate() {
            let cell = m.cells.iter().position(|c| c.key == t.key);
            let link = m.link.as_ref().is_some_and(|l| l.image_key == t.key);
            let quote = !m.quote_image_key.is_empty() && m.quote_image_key == t.key;
            if cell.is_none() && !link && !quote && m.image_key != t.key {
                continue;
            }
            let Some(mut row) = slint::Model::row_data(&*ui.messages, i) else { continue };
            if quote {
                row.quote_image = img.clone();
            } else if link {
                row.link_image = img.clone();
            } else if let Some(n) = cell {
                // a new model, not a change inside the old one: a ListView row didn't
                // show that until it was scrolled away and back
                let mut cells: Vec<ImageCell> = slint::Model::iter(&row.cells).collect();
                cells[n].image = img.clone();
                row.cells = ModelRc::new(VecModel::from(cells));
            } else {
                row.image = img.clone();
            }
            slint::Model::set_row_data(&*ui.messages, i, row);
        }
    });
}

pub(super) fn label_arrived(key: String, label: types::MediaLabel) {
    UI.with_borrow_mut(|ui| {
        if ui.released {
            return;
        }
        let shown: SharedString = label.text.as_str().into();
        let undownloaded = label.undownloaded;
        let progress = rows::progress_of(&label);
        let ring = rows::ring_of(&label);
        if label == types::MediaLabel::default() {
            ui.media_labels.remove(&key);
        } else {
            ui.media_labels.insert(key.clone(), label);
        }
        for (i, m) in ui.last.iter().enumerate() {
            let cell = m.cells.iter().position(|c| c.key == key);
            if cell.is_none() && (m.image_key != key || m.sticker) {
                continue;
            }
            let Some(mut row) = slint::Model::row_data(&*ui.messages, i) else { continue };
            if let Some(n) = cell {
                // a new model, as for `thumb_arrived`
                let mut cells: Vec<ImageCell> = slint::Model::iter(&row.cells).collect();
                cells[n].label = shown.clone();
                cells[n].undownloaded = undownloaded;
                cells[n].progress = progress;
                cells[n].ring = ring.clone();
                row.cells = ModelRc::new(VecModel::from(cells));
            } else {
                row.media_label = shown.clone();
                row.undownloaded = undownloaded;
                row.progress = progress;
                row.ring = ring.clone();
            }
            slint::Model::set_row_data(&*ui.messages, i, row);
        }
    });
}
