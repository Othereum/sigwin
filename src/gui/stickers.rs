//! The sticker panel: pack tabs, the open pack's grid, the pack page (adding, ordering, removing).

use super::*;

/// What the sticker panel shows; emptied when it closes.
#[derive(Default)]
pub(crate) struct Panel {
    pub(crate) packs: Rc<VecModel<PackItem>>,
    pack_dtos: Vec<types::PackDto>,
    pub(crate) stickers: Rc<VecModel<StickerItem>>,
    pub(crate) grid: Vec<types::StickerDto>,
    /// pack cover images (grid images live only in `stickers` rows)
    covers: held::Pictures<{ held::STICKER_COVERS }>,
}

/// The pack page's id in `current-pack`.
pub(super) const PACK_PAGE: &str = "+";

pub(crate) fn close(app: &AppWindow) {
    app.set_sticker_open(false);
    app.global::<StickerData>().set_packs_loaded(false);
    UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        p.covers.clear();
        p.grid.clear();
        p.pack_dtos.clear();
        anim::picker_grid(ui, "");
        let p = &mut ui.sticker;
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
    let d = app.global::<StickerData>();
    d.set_packs_loaded(true);
    let first = UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        p.packs.set_vec(list.iter().map(|d| pack_item(d, &p.covers)).collect::<Vec<_>>());
        let first = list.first().map(|d| d.id.clone());
        p.pack_dtos = list;
        first
    });
    let cur = d.get_current_pack().to_string();
    // the pack page stays open while packs are removed there (opening the panel leaves it)
    if cur == PACK_PAGE {
        return;
    }
    let still_there = UI.with_borrow(|ui| ui.sticker.pack_dtos.iter().any(|d| d.id == cur));
    // the grid is emptied when the panel closes, so always (re)load the selected pack
    let target = if still_there { Some(cur) } else { first };
    match target {
        Some(id) => {
            d.set_current_pack(id.as_str().into());
            send_cmd(Command::OpenPack(id));
        }
        None => d.set_current_pack(PACK_PAGE.into()),
    }
}

/// Opens the panel on `pack`, or on the last pack shown (the pack page is a place to visit, not
/// to come back to).
pub(super) fn open(app: &AppWindow, pack: Option<String>) {
    let d = app.global::<StickerData>();
    match pack {
        Some(pack) => d.set_current_pack(pack.into()),
        None if d.get_current_pack() == PACK_PAGE => d.set_current_pack(SharedString::new()),
        None => {}
    }
    app.set_sticker_open(true);
    UI.with_borrow_mut(anim::picker_opened);
    // reloads the pack list, then opens the current pack
    send_cmd(Command::StickerPanel);
}

pub(super) fn ask_install(app: &AppWindow, msg_id: String) {
    let text = tr!("Add the pack this sticker is from?").into();
    super::window::ask(app, super::window::Ask::YesNo, tr!("Sticker pack"), text, move || send_cmd(Command::InstallPackFromMessage(msg_id)));
}

/// Moves a pack on the pack page; the tabs follow at once, the backend keeps the order.
pub(super) fn move_pack(from: usize, to: usize) {
    let order = UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        if from >= p.pack_dtos.len() || to >= p.pack_dtos.len() || from == to {
            return None;
        }
        let d = p.pack_dtos.remove(from);
        p.pack_dtos.insert(to, d);
        let row = p.packs.remove(from);
        p.packs.insert(to, row);
        Some(p.pack_dtos.iter().map(|d| d.id.clone()).collect::<Vec<_>>())
    });
    if let Some(order) = order {
        send_cmd(Command::SetPackOrder(order));
    }
}

pub(super) fn ask_remove(app: &AppWindow, index: usize) {
    let Some((id, title)) = UI.with_borrow(|ui| ui.sticker.pack_dtos.get(index).map(|d| (d.id.clone(), d.title.clone()))) else { return };
    let text = tr!("Remove “{}”? It's also removed from your other devices.", title);
    super::window::ask(app, super::window::Ask::YesNo, tr!("Remove sticker pack"), text, move || send_cmd(Command::RemovePack(id)));
}

pub(super) fn grid_arrived(app: &AppWindow, pack: String, items: Vec<types::StickerDto>) {
    if !app.get_sticker_open() {
        return;
    }
    app.global::<StickerData>().set_current_pack(pack.as_str().into());
    UI.with_borrow_mut(|ui| {
        let rows: Vec<StickerItem> = items
            .iter()
            .map(|s| StickerItem { id: s.id as i32, emoji: s.emoji.as_str().into(), animated: s.animated, ..Default::default() })
            .collect();
        ui.sticker.stickers.set_vec(rows);
        ui.sticker.grid = items;
        anim::picker_grid(ui, &pack);
    });
}
