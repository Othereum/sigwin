//! The sticker panel: pack tabs, the open pack's grid, adding packs.

use super::*;

/// What the sticker panel shows; emptied when it closes.
#[derive(Default)]
pub(crate) struct Panel {
    pub(crate) packs: Rc<VecModel<PackItem>>,
    pack_dtos: Vec<types::PackDto>,
    pub(crate) stickers: Rc<VecModel<StickerItem>>,
    pub(crate) grid: Vec<types::StickerDto>,
    /// pack cover images (grid images live only in `stickers` rows)
    covers: HashMap<String, Image>,
}

pub(crate) fn close(app: &AppWindow) {
    app.set_sticker_open(false);
    UI.with_borrow_mut(|ui| {
        anim::stop_picker(ui);
        let p = &mut ui.sticker;
        p.covers.clear();
        p.grid.clear();
        p.stickers.set_vec(Vec::new());
        p.packs.set_vec(Vec::new());
    });
}

pub(super) fn thumbs_arrived(app: &AppWindow, list: Vec<types::Thumb>) {
    if !app.get_sticker_open() {
        return;
    }
    UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        let mut fresh = HashMap::with_capacity(list.len());
        for t in list {
            let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height);
            fresh.insert(t.key, Image::from_rgba8(buf));
        }
        for (i, d) in p.pack_dtos.iter().enumerate() {
            if let Some(img) = fresh.get(&d.cover_key) {
                p.covers.insert(d.cover_key.clone(), img.clone());
                slint::Model::set_row_data(&*p.packs, i, pack_item(d, &p.covers));
            }
        }
        // grid images live only in the model rows; ones for a pack no longer shown are
        // dropped here (the backend sends the grid before its images)
        for (i, s) in p.grid.iter().enumerate() {
            if let Some(img) = fresh.get(&s.key) {
                if let Some(mut row) = slint::Model::row_data(&*p.stickers, i) {
                    row.img = img.clone();
                    slint::Model::set_row_data(&*p.stickers, i, row);
                }
            }
        }
    });
}

pub(super) fn packs_arrived(app: &AppWindow, list: Vec<types::PackDto>) {
    if !app.get_sticker_open() {
        return;
    }
    let first = UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        p.packs.set_vec(list.iter().map(|d| pack_item(d, &p.covers)).collect::<Vec<_>>());
        let first = list.iter().find(|d| d.installed).map(|d| d.id.clone());
        p.pack_dtos = list;
        first
    });
    let cur = app.get_current_pack().to_string();
    let still_there = UI.with_borrow(|ui| ui.sticker.pack_dtos.iter().any(|d| d.installed && d.id == cur));
    // the grid is emptied when the panel closes, so always (re)load the selected pack
    let target = if still_there { Some(cur) } else { first };
    match target {
        Some(id) => {
            app.set_current_pack(id.as_str().into());
            send_cmd(Command::OpenPack(id));
        }
        None => app.set_current_pack("+".into()),
    }
}

pub(super) fn open(app: &AppWindow, pack: String) {
    app.set_current_pack(pack.into());
    app.set_sticker_open(true);
    // reloads the pack list, then opens the current pack
    send_cmd(Command::StickerPanel);
}

pub(super) fn ask_install(msg_id: String) {
    let ok = rfd::MessageDialog::new()
        .set_title("스티커 팩")
        .set_description("이 스티커가 들어 있는 팩을 추가할까요?")
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();
    if ok == rfd::MessageDialogResult::Yes {
        send_cmd(Command::InstallPackFromMessage(msg_id));
    }
}

pub(super) fn grid_arrived(app: &AppWindow, pack: String, items: Vec<types::StickerDto>) {
    if !app.get_sticker_open() {
        return;
    }
    app.set_current_pack(pack.into());
    UI.with_borrow_mut(|ui| {
        anim::stop_picker(ui);
        let rows: Vec<StickerItem> = items
            .iter()
            .map(|s| StickerItem { id: s.id as i32, emoji: s.emoji.as_str().into(), animated: s.animated, ..Default::default() })
            .collect();
        ui.sticker.stickers.set_vec(rows);
        ui.sticker.grid = items;
    });
}
