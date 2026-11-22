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
    /// the pack shown before it is installed, in a tab after the others
    preview: Option<types::PreviewDto>,
    /// the sticker to show once its pack's grid is here: (pack, sticker id)
    focus: Option<(String, u32)>,
}

/// The pack page's id in `current-pack`.
pub(super) const PACK_PAGE: &str = "+";

pub(crate) fn close(app: &AppWindow) {
    app.set_sticker_open(false);
    let d = app.global::<StickerData>();
    d.set_packs_loaded(false);
    d.set_has_preview(false);
    d.set_focus_index(-1);
    UI.with_borrow_mut(|ui| {
        if ui.sticker.preview.take().is_some() {
            if let Some(tx) = &ui.tx {
                let _ = tx.send(Command::ClosePreview);
            }
        }
        let p = &mut ui.sticker;
        p.focus = None;
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
        if let Some(v) = &p.preview {
            if let Some(img) = fresh.get(&v.pack.cover_key) {
                p.covers.insert(v.pack.cover_key.clone(), img.clone());
                let d = app.global::<StickerData>();
                d.set_preview(preview_item(v, &p.covers, d.get_preview().installing));
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
    let (first, preview) = UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        p.packs.set_vec(list.iter().map(|d| pack_item(d, &p.covers)).collect::<Vec<_>>());
        let first = list.first().map(|d| d.id.clone());
        // installed now: a tab like the others (the backend let go of it)
        if p.preview.as_ref().is_some_and(|v| list.iter().any(|d| d.id == v.pack.id)) {
            p.preview = None;
        }
        p.pack_dtos = list;
        (first, p.preview.as_ref().map(|v| v.pack.id.clone()))
    });
    d.set_has_preview(preview.is_some());
    let cur = d.get_current_pack().to_string();
    // the pack page stays open while packs are removed there (opening the panel leaves it)
    if cur == PACK_PAGE {
        return;
    }
    if preview.as_deref() == Some(cur.as_str()) {
        send_cmd(Command::OpenPack(cur));
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

/// Opens the panel on an installed pack, showing `sticker` of it once its grid is here.
pub(super) fn open_at(app: &AppWindow, pack: String, sticker: Option<u32>) {
    UI.with_borrow_mut(|ui| ui.sticker.focus = sticker.map(|s| (pack.clone(), s)));
    open(app, Some(pack));
}

/// A pack not installed, or news of it: the panel opens on it, its grid once the manifest is here.
pub(super) fn preview_arrived(app: &AppWindow, v: types::PreviewDto) {
    let d = app.global::<StickerData>();
    let id = v.pack.id.clone();
    let ready = v.state == types::PreviewState::Ready;
    let item = UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        // a new pack's cover isn't the old one's
        if p.preview.as_ref().is_some_and(|old| old.pack.id != id) {
            p.covers.retain(|k, _| p.pack_dtos.iter().any(|d| &d.cover_key == k));
        }
        let item = preview_item(&v, &p.covers, false);
        p.preview = Some(v);
        item
    });
    d.set_preview(item);
    d.set_has_preview(true);
    if !app.get_sticker_open() || d.get_current_pack() != id.as_str() {
        open(app, Some(id));
    } else if ready {
        send_cmd(Command::OpenPack(id));
    }
}

fn preview_item(v: &types::PreviewDto, covers: &HashMap<String, Image>, installing: bool) -> PreviewItem {
    PreviewItem {
        pack: pack_item(&v.pack, covers),
        count: v.count as i32,
        loading: v.state == types::PreviewState::Loading,
        failed: v.state == types::PreviewState::Failed,
        installing,
    }
}

pub(super) fn install_preview(app: &AppWindow) {
    let d = app.global::<StickerData>();
    let mut item = d.get_preview();
    item.installing = true;
    d.set_preview(item);
    send_cmd(Command::InstallPreview);
}

/// A sticker of the previewed pack arrived (its thumb came just before) or failed.
pub(super) fn preview_sticker(key: &str, animated: bool, failed: bool) {
    UI.with_borrow_mut(|ui| {
        let Some(i) = ui.sticker.grid.iter().position(|s| s.key == key) else { return };
        ui.sticker.grid[i].animated = animated;
        if let Some(mut row) = slint::Model::row_data(&*ui.sticker.stickers, i) {
            row.animated = animated;
            row.failed = failed;
            slint::Model::set_row_data(&*ui.sticker.stickers, i, row);
        }
        if animated {
            anim::update_picker(ui);
        }
    });
}

/// The previewed pack's grid shows these cells: the backend downloads what's missing of them.
pub(super) fn preview_want(ui: &Ui) {
    let Some(v) = &ui.sticker.preview else { return };
    let (first, end) = anim::picker_range(ui);
    if anim::picker_pack(ui) != v.pack.id || first >= end {
        return;
    }
    let ids = ui.sticker.grid.iter().skip(first).take(end - first).map(|s| s.id).collect();
    if let Some(tx) = &ui.tx {
        let _ = tx.send(Command::PreviewWant(ids));
    }
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
    let d = app.global::<StickerData>();
    d.set_current_pack(pack.as_str().into());
    let focus = UI.with_borrow_mut(|ui| {
        let focus = match ui.sticker.focus.take() {
            Some((p, id)) if p == pack => items.iter().position(|s| s.id == id),
            other => {
                ui.sticker.focus = other;
                None
            }
        };
        let rows: Vec<StickerItem> = items
            .iter()
            .map(|s| StickerItem { id: s.id as i32, emoji: s.emoji.as_str().into(), animated: s.animated, ..Default::default() })
            .collect();
        ui.sticker.stickers.set_vec(rows);
        ui.sticker.grid = items;
        anim::picker_grid(ui, &pack);
        preview_want(ui);
        focus
    });
    if let Some(i) = focus {
        d.set_focus_index(i as i32);
    }
}
