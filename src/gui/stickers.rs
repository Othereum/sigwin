//! The sticker panel: the recently used tab and pack tabs, the open tab's grid, the pack page
//! (adding, ordering, removing).

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
    d.set_has_recent(false);
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
            if take_covers(&mut p.covers, &fresh, &d.cover_key) {
                slint::Model::set_row_data(&*p.packs, i, pack_item(d, &p.covers));
            }
        }
        if let Some(v) = &p.preview {
            if take_covers(&mut p.covers, &fresh, &v.pack.cover_key) {
                let d = app.global::<StickerData>();
                d.set_preview(preview_item(v, &p.covers, &d.get_preview()));
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

pub(super) fn packs_arrived(app: &AppWindow, list: Vec<types::PackDto>, recent: bool) {
    if !app.get_sticker_open() {
        return;
    }
    let d = app.global::<StickerData>();
    d.set_packs_loaded(true);
    d.set_has_recent(recent);
    let (first, preview, installed) = UI.with_borrow_mut(|ui| {
        let p = &mut ui.sticker;
        p.packs.set_vec(list.iter().map(|d| pack_item(d, &p.covers)).collect::<Vec<_>>());
        let first = list.first().map(|d| d.id.clone());
        // installed now: a tab like the others (the backend let go of it)
        let installed = p.preview.as_ref().is_some_and(|v| list.iter().any(|d| d.id == v.pack.id));
        if installed {
            p.preview = None;
        }
        p.pack_dtos = list;
        (first, p.preview.as_ref().map(|v| v.pack.id.clone()), installed)
    });
    if installed {
        set_installing(app, None, None);
    }
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
    let still_there = if cur == types::RECENT_TAB { recent } else { UI.with_borrow(|ui| ui.sticker.pack_dtos.iter().any(|d| d.id == cur)) };
    // the grid is emptied when the panel closes, so always (re)load the selected tab; else the
    // first one
    let target = if still_there {
        Some(cur)
    } else if recent {
        Some(types::RECENT_TAB.to_string())
    } else {
        first
    };
    match target {
        Some(id) => {
            d.set_current_pack(id.as_str().into());
            send_cmd(Command::OpenPack(id));
        }
        None => d.set_current_pack(PACK_PAGE.into()),
    }
}

/// The installed pack's place among the tabs, -1 if it isn't one.
pub(super) fn pack_index(id: &str) -> i32 {
    UI.with_borrow(|ui| ui.sticker.pack_dtos.iter().position(|d| d.id == id).map_or(-1, |i| i as i32))
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
            p.covers.retain(|k, _| {
                p.pack_dtos.iter().any(|d| *k == d.cover_key || *k == types::small_cover_key(&d.cover_key))
            });
        }
        let item = preview_item(&v, &p.covers, &d.get_preview());
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

/// Moves a pack's covers (both sizes) found in `fresh` into `covers`; whether there were any.
fn take_covers(covers: &mut held::Pictures<{ held::STICKER_COVERS }>, fresh: &HashMap<String, Image>, cover_key: &str) -> bool {
    let mut any = false;
    for k in [cover_key.to_owned(), types::small_cover_key(cover_key)] {
        if let Some(img) = fresh.get(&k) {
            covers.insert(k, img.clone());
            any = true;
        }
    }
    any
}

/// `shown`: the item shown now; an install under way for the same pack carries over.
fn preview_item(v: &types::PreviewDto, covers: &HashMap<String, Image>, shown: &PreviewItem) -> PreviewItem {
    let same = shown.pack.id == v.pack.id.as_str();
    PreviewItem {
        pack: pack_item(&v.pack, covers),
        count: v.count as i32,
        loading: v.state == types::PreviewState::Loading,
        failed: v.state == types::PreviewState::Failed,
        installing: same && shown.installing,
        progress: if same { shown.progress } else { 0.0 },
    }
}

pub(super) fn install_preview(app: &AppWindow) {
    set_installing(app, None, Some(0.0));
    send_cmd(Command::InstallPreview);
}

/// The previewed pack (`pack`, or whichever is shown) is being added this far along (0 to 1), or
/// isn't any more (`None`).
fn set_installing(app: &AppWindow, pack: Option<&str>, progress: Option<f32>) {
    let d = app.global::<StickerData>();
    let mut item = d.get_preview();
    if pack.is_some_and(|p| item.pack.id != p) {
        return;
    }
    item.installing = progress.is_some();
    item.progress = progress.unwrap_or(0.0);
    d.set_preview(item);
}

pub(super) fn add_progress(app: &AppWindow, pack: &str, progress: f32) {
    set_installing(app, Some(pack), Some(progress));
}

/// Failed, the preview's button is back. Added, the button stays full until the pack's own tab
/// takes the preview's place (`packs_arrived`).
pub(super) fn add_ended(app: &AppWindow, pack: &str, ok: bool) {
    if !ok {
        set_installing(app, Some(pack), None);
    }
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

/// Takes the recently used tab's sticker at `index` off it, at once. The last one gone, the tab
/// goes too: the pack list reloads and picks the tab to show.
pub(super) fn forget_recent(index: usize) {
    let Some((s, left)) = UI.with_borrow_mut(|ui| {
        if anim::picker_pack(ui) != types::RECENT_TAB || index >= ui.sticker.grid.len() {
            return None;
        }
        let s = ui.sticker.grid.remove(index);
        ui.sticker.stickers.remove(index);
        anim::picker_removed(ui, &s.key);
        Some((s, ui.sticker.grid.len()))
    }) else {
        return;
    };
    send_cmd(Command::ForgetRecentSticker { pack: s.pack, id: s.id });
    if left == 0 {
        send_cmd(Command::StickerPanel);
    }
}

pub(super) fn ask_remove(app: &AppWindow, index: usize) {
    let Some((id, title)) = UI.with_borrow(|ui| ui.sticker.pack_dtos.get(index).map(|d| (d.id.clone(), d.title.clone()))) else { return };
    let text = tr!("Remove “{}”? It's also removed from your other devices.", title);
    use super::window::{ask, Action};
    ask(app, tr!("Remove sticker pack"), text, Action::Danger(tr!("Remove")), move || send_cmd(Command::RemovePack(id)));
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
            .map(|s| StickerItem {
                pack: s.pack.as_str().into(),
                id: s.id as i32,
                emoji: s.emoji.as_str().into(),
                animated: s.animated,
                ..Default::default()
            })
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
