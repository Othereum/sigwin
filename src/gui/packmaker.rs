//! The page that makes a sticker pack (`ui/packmaker.slint`): what the user has added, the steps,
//! and the pictures' thumbnails. Everything here is dropped when the page closes. Pictures are
//! only looked at while being added (for a thumbnail); they are read again, one by one, when
//! the pack is uploaded (`backend/packmaker.rs`).

use slint::Model as _;
use sigwin_thumb::sticker::{self, Reject};

use super::*;
use crate::stickerpack;

/// A thumbnail's size on the page (`ui/packmaker.slint`: a cell less its margins).
const THUMB: u32 = 76;

struct Item {
    path: PathBuf,
    /// "" until chosen
    emoji: String,
    /// what its cell's animation holds, if it moves
    _held: Option<crate::alloc::Held>,
}

/// A cell's picture: a still thumbnail, or the animation at the cell's size, which `Image` plays.
struct Look {
    still: sigwin_thumb::Pixels,
    anim: Option<Vec<u8>>,
}

impl Look {
    fn image(self) -> (Image, Option<crate::alloc::Held>) {
        if let Some(img) = self.anim.as_deref().and_then(|png| Image::load_from_data(png, Some("png")).ok()) {
            let size = img.size();
            // its bytes and the two frames `Image` keeps decoded
            let bytes = self.anim.map_or(0, |a| a.len()) + 2 * 4 * (size.width * size.height) as usize;
            return (img, Some(crate::alloc::Held::new(crate::alloc::Gauge::Anims, bytes)));
        }
        (Image::from_rgba8(self.still), None)
    }
}

#[derive(Default)]
pub(crate) struct State {
    items: Vec<Item>,
    pub(crate) cells: Rc<VecModel<PackCell>>,
    /// the cover the user picked; None: the first sticker
    cover: Option<PathBuf>,
    cover_held: Option<crate::alloc::Held>,
    /// files dropped on the page, taken together once the drop's events are in
    dropped: Vec<PathBuf>,
    /// pictures are being read for thumbnails
    loading: bool,
    /// a file dialog is open
    picking: bool,
    /// bumped when the page is reset: work started before it is dropped on arrival
    epoch: u64,
    /// the new pack's hex id and key
    made: Option<(String, String)>,
}

/// What a thumbnail worker found.
enum Found {
    Added(PathBuf, String, Look),
    Done(Vec<(String, Reject)>),
}

pub(crate) fn wire(app: &AppWindow) {
    let d = app.global::<PackData>();
    let weak = app.as_weak();
    d.on_start(move || {
        let Some(app) = weak.upgrade() else { return };
        reset(&app);
        stickers::close(&app);
        let d = app.global::<PackData>();
        d.set_step(1);
        d.set_open(true);
    });
    let weak = app.as_weak();
    d.on_choose_images(move || {
        if let Some(app) = weak.upgrade() {
            pick(&app, picker::Kind::Images, add_pictures);
        }
    });
    let weak = app.as_weak();
    d.on_load_json(move || {
        if let Some(app) = weak.upgrade() {
            pick(&app, picker::Kind::PackJson, load_json);
        }
    });
    let weak = app.as_weak();
    d.on_remove(move |i| {
        let Some(app) = weak.upgrade() else { return };
        UI.with_borrow_mut(|ui| {
            let m = &mut ui.maker;
            if (i as usize) < m.items.len() {
                m.items.remove(i as usize);
                m.cells.remove(i as usize);
            }
        });
        count_missing(&app);
    });
    let weak = app.as_weak();
    d.on_picker_tab_changed(move |tab| {
        if let Some(app) = weak.upgrade() {
            app.global::<PackData>().set_picker_rows(emoji_rows(tab));
        }
    });
    let weak = app.as_weak();
    d.on_emoji_picked(move |i, emoji| {
        let Some(app) = weak.upgrade() else { return };
        UI.with_borrow_mut(|ui| {
            let m = &mut ui.maker;
            if let Some(item) = m.items.get_mut(i as usize) {
                item.emoji = emoji.to_string();
                if let Some(mut cell) = m.cells.row_data(i as usize) {
                    cell.emoji = emoji;
                    m.cells.set_row_data(i as usize, cell);
                }
            }
        });
        count_missing(&app);
        app.global::<PackData>().set_picker_rows(ModelRc::default());
    });
    let weak = app.as_weak();
    d.on_choose_cover(move || {
        if let Some(app) = weak.upgrade() {
            pick(&app, picker::Kind::Image, choose_cover);
        }
    });
    let weak = app.as_weak();
    d.on_use_first_cover(move || {
        if let Some(app) = weak.upgrade() {
            UI.with_borrow_mut(|ui| {
                ui.maker.cover = None;
                ui.maker.cover_held = None;
            });
            let d = app.global::<PackData>();
            d.set_has_cover(false);
            d.set_cover(Image::default());
        }
    });
    let weak = app.as_weak();
    d.on_back(move || {
        let Some(app) = weak.upgrade() else { return };
        let d = app.global::<PackData>();
        d.set_step((d.get_step() - 1).max(1));
    });
    let weak = app.as_weak();
    d.on_next(move || {
        let Some(app) = weak.upgrade() else { return };
        let d = app.global::<PackData>();
        match d.get_step() {
            1 | 2 => d.set_step(d.get_step() + 1),
            // Signal Desktop asks for both too
            3 if !d.get_title().trim().is_empty() && !d.get_author().trim().is_empty() => {
                let weak = app.as_weak();
                let text = tr!("Once created, a pack can't be edited or deleted. Create it?");
                window::ask(&app, tr!("Create a sticker pack"), text, window::Action::Go(tr!("Create")), move || {
                    if let Some(app) = weak.upgrade() {
                        upload(&app);
                    }
                });
            }
            _ => {}
        }
    });
    let weak = app.as_weak();
    d.on_retry(move || {
        if let Some(app) = weak.upgrade() {
            upload(&app);
        }
    });
    d.on_add_to_mine(|| {
        if let Some((id, key)) = UI.with_borrow(|ui| ui.maker.made.clone()) {
            send_cmd(Command::InstallPack { id, key });
        }
    });
    let weak = app.as_weak();
    d.on_another(move || {
        let Some(app) = weak.upgrade() else { return };
        reset(&app);
        app.global::<PackData>().set_step(1);
    });
    let weak = app.as_weak();
    d.on_close(move || {
        let Some(app) = weak.upgrade() else { return };
        let d = app.global::<PackData>();
        if d.get_open() {
            if d.get_step() == 4 && d.get_error().is_empty() {
                send_cmd(Command::CancelPack);
            }
            reset(&app);
            d.set_open(false);
        }
    });
}

/// Back to an empty page, whatever was being worked on dropped.
fn reset(app: &AppWindow) {
    UI.with_borrow_mut(|ui| {
        let m = &mut ui.maker;
        m.epoch += 1;
        m.items.clear();
        m.cells.set_vec(Vec::new());
        m.cover = None;
        m.cover_held = None;
        m.dropped.clear();
        m.loading = false;
        m.picking = false;
        m.made = None;
    });
    let d = app.global::<PackData>();
    d.set_step(1);
    d.set_loading(false);
    d.set_missing(0);
    d.set_title(SharedString::new());
    d.set_author(SharedString::new());
    d.set_has_cover(false);
    d.set_cover(Image::default());
    d.set_progress(SharedString::new());
    d.set_error(SharedString::new());
    d.set_link(SharedString::new());
    d.set_added(false);
    d.set_picker_rows(ModelRc::default());
}

fn count_missing(app: &AppWindow) {
    let n = UI.with_borrow(|ui| ui.maker.items.iter().filter(|i| i.emoji.is_empty()).count());
    app.global::<PackData>().set_missing(n as i32);
}

/// Opens a dialog (one at a time) and hands what it picked to `then`.
fn pick(app: &AppWindow, kind: picker::Kind, then: fn(&AppWindow, Vec<PathBuf>)) {
    let busy = UI.with_borrow_mut(|ui| ui.maker.loading || std::mem::replace(&mut ui.maker.picking, true));
    if busy {
        return;
    }
    let weak = app.as_weak();
    let epoch = UI.with_borrow(|ui| ui.maker.epoch);
    picker::spawn_kind(hwnd(app), kind, move |picked| {
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            let current = UI.with_borrow_mut(|ui| {
                let same = ui.maker.epoch == epoch;
                if same {
                    ui.maker.picking = false;
                }
                same
            });
            if current {
                then(&app, picked);
            }
        });
    });
}

fn add_pictures(app: &AppWindow, files: Vec<PathBuf>) {
    add(app, files.into_iter().map(|f| (f, String::new())).collect());
}

/// A pack.json: its title, author, pictures and emoji, while nothing is added yet.
fn load_json(app: &AppWindow, files: Vec<PathBuf>) {
    let Some(file) = files.into_iter().next() else { return };
    if !UI.with_borrow(|ui| ui.maker.items.is_empty()) {
        return;
    }
    let read = std::fs::metadata(&file).ok().filter(|m| m.len() <= 4 << 20).and_then(|_| std::fs::read(&file).ok());
    let loaded = read.and_then(|bytes| stickerpack::parse_pack_json(&bytes, file.parent().unwrap_or(std::path::Path::new("."))));
    match loaded {
        Some(l) if !l.stickers.is_empty() => {
            let d = app.global::<PackData>();
            d.set_title(l.title.as_str().into());
            d.set_author(l.author.as_str().into());
            add(app, l.stickers);
        }
        _ => notice::show(app, tr!("Not a pack.json with stickers")),
    }
}

/// Reads the pictures on a thread of its own, one at a time: the ones that can be stickers come
/// back as thumbnails, the others with why not.
fn add(app: &AppWindow, mut files: Vec<(PathBuf, String)>) {
    let (epoch, room) = UI.with_borrow_mut(|ui| {
        ui.maker.loading = true;
        (ui.maker.epoch, stickerpack::MAX_STICKERS.saturating_sub(ui.maker.items.len()))
    });
    let over = files.len().saturating_sub(room);
    files.truncate(room);
    if files.is_empty() {
        UI.with_borrow_mut(|ui| ui.maker.loading = false);
        if over > 0 {
            notice::show(app, &tr!("A pack holds up to {} stickers", stickerpack::MAX_STICKERS));
        }
        return;
    }
    app.global::<PackData>().set_loading(true);
    let weak = app.as_weak();
    let px = types::physical_px(THUMB);
    std::thread::spawn(move || {
        let send = |found: Found| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    found_one(&app, epoch, over, found);
                }
            });
        };
        let mut rejected = Vec::new();
        for (path, emoji) in files {
            match look(&path, px) {
                Ok(thumb) => send(Found::Added(path, emoji, thumb)),
                Err(r) => rejected.push((path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), r)),
            }
        }
        send(Found::Done(rejected));
    });
}

/// A file's thumbnail (and animation), if it can be a sticker.
fn look(path: &std::path::Path, px: u32) -> Result<Look, Reject> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(sticker::MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| Reject::Unreadable)?;
    if bytes.len() as u64 > sticker::MAX_SOURCE_BYTES {
        return Err(Reject::TooLarge);
    }
    let facts = sticker::inspect(&bytes)?;
    let rgba = sticker::thumb(&bytes, px)?;
    let still = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
    let anim = if facts.apng { sigwin_thumb::animation_png(&bytes, px).ok().flatten() } else { None };
    Ok(Look { still, anim })
}

fn found_one(app: &AppWindow, epoch: u64, over: usize, found: Found) {
    if UI.with_borrow(|ui| ui.maker.epoch != epoch) {
        return;
    }
    match found {
        Found::Added(path, emoji, look) => {
            let (img, held) = look.image();
            UI.with_borrow_mut(|ui| {
                let m = &mut ui.maker;
                m.cells.push(PackCell { img, emoji: emoji.as_str().into() });
                m.items.push(Item { path, emoji, _held: held });
            });
            count_missing(app);
        }
        Found::Done(rejected) => {
            UI.with_borrow_mut(|ui| ui.maker.loading = false);
            app.global::<PackData>().set_loading(false);
            let mut why = match rejected.first() {
                Some((name, r)) if rejected.len() == 1 => format!("{name}: {}", stickerpack::reject_reason(*r)),
                Some((name, r)) => {
                    let head = tr!("{n} picture wasn't added" | "{n} pictures weren't added" % rejected.len());
                    format!("{head}: {name}: {}", stickerpack::reject_reason(*r))
                }
                None => String::new(),
            };
            if over > 0 {
                let full = tr!("A pack holds up to {} stickers", stickerpack::MAX_STICKERS);
                why = if why.is_empty() { full } else { format!("{why}. {full}") };
            }
            if !why.is_empty() {
                notice::show(app, &why);
            }
        }
    }
}

/// The cover the user picked: a picture that could be a sticker too.
fn choose_cover(app: &AppWindow, files: Vec<PathBuf>) {
    let Some(path) = files.into_iter().next() else { return };
    let epoch = UI.with_borrow(|ui| ui.maker.epoch);
    let weak = app.as_weak();
    let px = types::physical_px(THUMB);
    std::thread::spawn(move || {
        let looked = look(&path, px);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            if UI.with_borrow(|ui| ui.maker.epoch != epoch) {
                return;
            }
            match looked {
                Ok(look) => {
                    let (img, held) = look.image();
                    UI.with_borrow_mut(|ui| {
                        ui.maker.cover = Some(path);
                        ui.maker.cover_held = held;
                    });
                    let d = app.global::<PackData>();
                    d.set_cover(img);
                    d.set_has_cover(true);
                }
                Err(r) => notice::show(&app, &stickerpack::reject_reason(r)),
            }
        });
    });
}

/// Starts (or starts over) making the pack: a new form from the server, every picture again.
fn upload(app: &AppWindow) {
    let d = app.global::<PackData>();
    let job = UI.with_borrow(|ui| types::PackJob {
        title: d.get_title().trim().to_string(),
        author: d.get_author().trim().to_string(),
        stickers: ui.maker.items.iter().map(|i| (i.path.clone(), i.emoji.clone())).collect(),
        cover: ui.maker.cover.clone(),
    });
    if job.stickers.is_empty() {
        return;
    }
    d.set_error(SharedString::new());
    d.set_progress(format!("0/{}", job.stickers.len() + job.cover.is_some() as usize).into());
    d.set_step(4);
    send_cmd(Command::CreatePack(job));
}

fn open_step_4(app: &AppWindow) -> Option<PackData<'_>> {
    let d = app.global::<PackData>();
    (d.get_open() && d.get_step() == 4).then_some(d)
}

pub(super) fn progress(app: &AppWindow, done: usize, total: usize) {
    if let Some(d) = open_step_4(app) {
        d.set_progress(format!("{done}/{total}").into());
    }
}

pub(super) fn created(app: &AppWindow, id: String, key: String) {
    let Some(d) = open_step_4(app) else { return };
    let link = hex::decode(&key).map(|k| stickerpack::link(&id, &k)).unwrap_or_default();
    UI.with_borrow_mut(|ui| ui.maker.made = Some((id, key)));
    d.set_link(link.into());
    d.set_step(5);
}

pub(super) fn failed(app: &AppWindow, why: String) {
    if let Some(d) = open_step_4(app) {
        d.set_error(why.into());
    }
}

/// A file dropped on the page. Explorer sends one event per file, so they are taken together on
/// the next turn of the event loop.
pub(crate) fn dropped(app: &AppWindow, path: PathBuf) {
    let first = UI.with_borrow_mut(|ui| {
        ui.maker.dropped.push(path);
        ui.maker.dropped.len() == 1
    });
    if !first {
        return;
    }
    let weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        let Some(app) = weak.upgrade() else { return };
        let (files, busy) = UI.with_borrow_mut(|ui| (std::mem::take(&mut ui.maker.dropped), ui.maker.loading || ui.maker.picking));
        let d = app.global::<PackData>();
        if busy || !d.get_open() || d.get_step() != 1 {
            return;
        }
        let files: Vec<PathBuf> = files.into_iter().filter(|f| f.is_file()).collect();
        let is_json = |f: &PathBuf| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
        if !files.iter().any(is_json) {
            add_pictures(&app, files);
        } else if files.len() == 1 && UI.with_borrow(|ui| ui.maker.items.is_empty()) {
            load_json(&app, files);
        } else {
            notice::show(&app, tr!("A pack.json is loaded by itself, before any images"));
        }
    });
}

/// The demo's way in: what the dialogs would have picked (`json`: a pack.json).
pub(crate) fn demo_files(app: &AppWindow, files: Vec<PathBuf>, json: bool) {
    if json { load_json(app, files) } else { add_pictures(app, files) }
}

pub(crate) fn demo_upload(app: &AppWindow) {
    upload(app);
}
