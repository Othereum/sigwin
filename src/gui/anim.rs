//! Animated stickers, and the one GIF playing in the message list.
//!
//! Stickers: a row reports itself on screen (`Anim.shown`), then the backend is asked once for
//! the sticker as an APNG at the size shown (stills answer with none). Slint's `Image` plays it,
//! decoding a frame when it draws one, so nothing ticks here and nothing decodes while it isn't
//! drawn. Those off screen keep `OFF_SCREEN` together, the longest off screen going back to its
//! still first (asked for again when its row comes back), and all of them keep `ANIM_BUDGET`.
//! Picker: the animated stickers on screen play, asked for as the grid reports its rows on screen
//! (`Anim.picker-view`). Those that scroll away keep `OFF_SCREEN` together, so scrolling back
//! plays them right away.
//! With Windows' animation effects off, a sticker plays only while hovered, in the list and the
//! picker, and animated ones have a mark until then (the list asks only whether one is).
//! Without the window's focus no sticker plays (`focus_changed`).
//!
//! GIFs play from frames the backend streams (`backend::gif`): one at a time, decoded off the UI
//! thread, which Slint's player can't do for big ones.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use super::{Ui, UI};
use crate::types::{Command, Frames, GifSlot, GifTarget, ANIM_BUDGET, STICKER_THUMB_PREFIX};
use crate::{Anim, AppWindow};

struct Sticker {
    /// the APNG, played by the rows' `Image`s
    image: Image,
    /// what it holds loaded: a copy of the APNG and its first frame
    loaded: usize,
    _held: crate::alloc::Held,
    /// what an `Image` playing it adds, while one does (`PLAYING_FRAMES`)
    playing: Option<crate::alloc::Held>,
}

/// What the open chat's playing stickers off screen may hold together, and the picker's. Coming
/// back on screen, one let go of is decoded again.
const OFF_SCREEN: usize = 4 << 20;

/// What an `Image` playing an APNG holds besides it, in frames: the decoder's canvas and
/// buffers (about four) and the frame shown, plus up to `PLAYING_EXTRA` of zlib state. Measured
/// on stickers at picker size; it goes when the `Image` stops showing it.
const PLAYING_FRAMES: usize = 5;
const PLAYING_EXTRA: usize = 32 << 10;

impl Sticker {
    fn new(f: &Frames) -> Option<Self> {
        let image = Image::load_from_data(&f.png, Some("png")).ok()?;
        let loaded = f.png.len() + frame_bytes(&image);
        Some(Sticker { image, loaded, _held: crate::alloc::Held::new(crate::alloc::Gauge::Anims, loaded), playing: None })
    }

    fn playing_bytes(&self) -> usize {
        PLAYING_FRAMES * frame_bytes(&self.image) + PLAYING_EXTRA
    }

    fn set_playing(&mut self, on: bool) {
        self.playing = on.then(|| crate::alloc::Held::new(crate::alloc::Gauge::Anims, self.playing_bytes()));
    }

    fn bytes(&self) -> usize {
        self.loaded + self.playing.as_ref().map_or(0, |_| self.playing_bytes())
    }
}

fn frame_bytes(image: &Image) -> usize {
    let size = image.size();
    4 * (size.width * size.height) as usize
}

/// The GIF playing in the message list (one at a time): its frames come from the backend one by
/// one into a single image, `frame[0]` stays 0.
struct GifPlay {
    /// the row, and the cell of it that plays (the same picture twice in a grid is two cells)
    id: String,
    cell: usize,
    seq: u64,
    frames: Rc<VecModel<Image>>,
    frame: Rc<VecModel<i32>>,
}

#[derive(Default)]
pub(crate) struct Anims {
    gif: Option<GifPlay>,
    gif_seq: u64,
    /// message-list stickers by thumbnail key, open thread only
    list: HashMap<String, Sticker>,
    /// keys asked from the backend, answered or not (an evicted key leaves, so it can come back)
    asked: HashSet<String>,
    /// Windows' animation effects are off: a sticker plays only while hovered, and those known
    /// to be animated (`animated`, message list) have a mark until then
    hover_only: bool,
    animated: HashSet<String>,
    /// the hovered message-list sticker's key, while `hover_only`
    list_hovered: Option<String>,
    picker: Picker,
}

/// The sticker picker's animations, emptied when its grid changes or closes.
#[derive(Default)]
struct Picker {
    /// animations by key, with the turn each was last on its cell (the oldest goes first)
    made: HashMap<String, (Sticker, u64)>,
    /// keys whose cells show their animation now
    shown: HashSet<String>,
    /// keys answered with a still (nothing to play, don't ask again)
    stills: HashSet<String>,
    /// what the last `PickerFrames` asked for
    asked: Vec<String>,
    /// the grid's tab: a pack's id or `RECENT_TAB`
    pack: String,
    /// the grid's cells on screen, [first, end)
    range: (usize, usize),
    hovered: Option<String>,
    turn: u64,
    peek: Option<Peek>,
}

/// The picker sticker shown big (`StickerData.peek-img`), at its size in chats.
struct Peek {
    key: String,
    /// its still arrived, or the backend found none
    answered: bool,
    still: Option<(Image, crate::alloc::Held)>,
    anim: Option<Sticker>,
}

impl Anims {
    /// A sticker's animation for a message row (empty: show the still). Without the focus none
    /// plays; the animations stay, to play again at once when it comes back.
    pub(crate) fn image(&self, key: &str) -> Image {
        if !super::presence::focused() {
            return Image::default();
        }
        self.list.get(key).map(|s| s.image.clone()).unwrap_or_default()
    }

    /// The playing GIF's frames for cell `cell` of row `id` (empty: show the still).
    pub(crate) fn gif_models(&self, id: &str, cell: usize) -> (ModelRc<Image>, ModelRc<i32>) {
        match self.gif.as_ref().filter(|g| g.id == id && g.cell == cell && g.frames.row_count() > 0) {
            Some(g) => (ModelRc::from(g.frames.clone()), ModelRc::from(g.frame.clone())),
            None => Default::default(),
        }
    }

    /// Whether a message row's sticker has the mark of an animated one that plays when hovered.
    pub(crate) fn marked(&self, key: &str) -> bool {
        self.hover_only && self.animated.contains(key)
    }

    /// Thread switch: the old thread's stickers go, and Windows' animation effects are read again.
    pub(crate) fn clear_list(&mut self) {
        self.list.clear();
        self.asked.clear();
        self.animated.clear();
        self.list_hovered = None;
        self.read_motion();
    }

    fn read_motion(&mut self) {
        self.hover_only = !super::window::animations_on();
        if let Some(app) = APP.with_borrow(|a| a.as_ref().and_then(|w| w.upgrade())) {
            app.global::<Anim>().set_hover_only(self.hover_only);
        }
    }

    fn bytes(&self) -> usize {
        self.list.values().map(Sticker::bytes).sum()
    }
}

/// Puts the stickers back to their stills past `OFF_SCREEN` off screen (the longest off screen
/// first) and past `ANIM_BUDGET` in all (the longest since on screen first).
pub(super) fn trim_list(ui: &mut Ui) {
    let on_screen = ui.shown.on_screen_keys(&ui.last);
    let mut by_age: Vec<(u64, &String, usize, bool)> = ui
        .anims
        .list
        .iter()
        .map(|(k, s)| (ui.shown.last_seen(k), k, s.bytes(), on_screen.contains(k.as_str())))
        .collect();
    by_age.sort_by_key(|o| o.0);
    let mut off = by_age.iter().filter(|o| !o.3).map(|o| o.2).sum::<usize>();
    let mut all = ui.anims.bytes();
    let mut gone = Vec::new();
    for (_, k, bytes, shown) in by_age {
        if off <= OFF_SCREEN && all <= ANIM_BUDGET {
            break;
        }
        if shown && all <= ANIM_BUDGET {
            continue;
        }
        if !shown {
            off -= bytes;
        }
        all -= bytes;
        gone.push(k.clone());
    }
    for k in gone {
        tracing::debug!("anim budget: {k} back to its still");
        ui.anims.list.remove(&k);
        ui.anims.asked.remove(&k);
        set_rows(ui, &k);
    }
}

thread_local! {
    static APP: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static PAUSED: Cell<bool> = const { Cell::new(false) };
    /// a hover refresh is queued
    static REFRESH_PENDING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn wire(app: &AppWindow) {
    APP.with_borrow_mut(|a| *a = Some(app.as_weak()));
    let g = app.global::<Anim>();
    // these run while Slint evaluates the row: act on the next turn of the event loop
    g.on_shown(|id| {
        let id = id.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || shown(&id));
    });
    app.global::<crate::Hover>().on_refresh(refresh_hover);
    g.on_gif_hover(|id, cell, on| {
        let id = id.to_string();
        // these run while Slint evaluates the row: act on the next turn of the event loop
        slint::Timer::single_shot(Duration::ZERO, move || gif_hover(&id, cell.max(0) as usize, on));
    });
    g.on_hover(|pack, id, on| {
        let pack = pack.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || hover(&pack, id as u32, on));
    });
    g.on_sticker_hover(|id, on| {
        let id = id.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || sticker_hover(&id, on));
    });
    g.on_picker_view(|first, end| {
        slint::Timer::single_shot(Duration::ZERO, move || {
            UI.with_borrow_mut(|ui| {
                ui.anims.picker.range = (first.max(0) as usize, end.max(0) as usize);
                update_picker(ui);
                super::stickers::preview_want(ui);
            })
        });
    });
}

/// Slint moves the hover only with the mouse: content that moves under a resting cursor never
/// gets it, what moved away keeps it, and an element drawn again (a row was) starts without it.
/// Tell it where the cursor is, on the next turn (this can run while a list updates), once for
/// any number of calls (`Hover.refresh`: every scroll).
pub(crate) fn refresh_hover() {
    if REFRESH_PENDING.replace(true) {
        return;
    }
    slint::Timer::single_shot(Duration::ZERO, move || {
        REFRESH_PENDING.set(false);
        let Some(app) = APP.with_borrow(|a| a.as_ref().and_then(|w| w.upgrade())) else { return };
        if !super::presence::is_active() {
            return;
        }
        if let Some(position) = super::window::cursor_in_window(&app).or_else(|| crate::headless::pointer(app.window())) {
            app.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
        }
    });
}

/// The window was minimized (true) or restored. The GIF stops: its frames would keep coming,
/// and updates while minimized confuse the software renderer's damage tracking (`winhook.rs`).
/// Stickers need nothing: their `Image`s decode only when drawn.
pub(crate) fn pause(paused: bool) {
    PAUSED.set(paused);
    if paused {
        // called from the window procedure: don't touch UI state from inside it
        slint::Timer::single_shot(Duration::ZERO, || UI.with_borrow_mut(stop_gif));
    }
}

/// The main window gained or lost the focus. Without it no sticker plays, and the GIFs playing
/// stop, as the pointer often rests on one while the user is in another app; one the pointer
/// moves onto still plays. Called from the window procedure: acts on the next turn.
pub(crate) fn focus_changed() {
    slint::Timer::single_shot(Duration::ZERO, || {
        let focused = super::presence::focused();
        UI.with_borrow_mut(|ui| {
            if !focused {
                stop_gif(ui);
            }
            let list = &ui.anims.list;
            super::rows::redraw_rows(ui, |m| m.sticker && list.contains_key(&m.image_key));
            update_picker(ui);
            show_peek(&ui.anims.picker);
        });
        if !focused {
            if let Some(app) = APP.with_borrow(|a| a.as_ref().and_then(|w| w.upgrade())) {
                super::attach::stop_gif(&app);
            }
        }
    });
}

/// The cursor came onto cell `cell` of row `id` or left it. A GIF plays while the cursor is on
/// it; clicking is for the viewer, as on any picture.
fn gif_hover(id: &str, cell: usize, on: bool) {
    if on {
        start_gif(id, cell);
    } else {
        // moving from one GIF to the next, the new one's "on" can come before the old one's
        // "off": only the GIF that plays in this cell stops
        UI.with_borrow_mut(|ui| {
            if ui.anims.gif.as_ref().is_some_and(|g| g.id == id && g.cell == cell) {
                stop_gif(ui);
            }
        });
    }
}

fn start_gif(id: &str, cell: usize) {
    if PAUSED.get() || super::viewer::is_open() {
        return;
    }
    UI.with_borrow_mut(|ui| {
        if ui.anims.gif.as_ref().is_some_and(|g| g.id == id && g.cell == cell) {
            return;
        }
        let Some(m) = ui.last.iter().find(|m| m.id == id) else { return };
        // one that isn't here has nothing to play: the click is for getting it
        let Some(c) = m.cells.get(cell).filter(|c| c.gif && c.more == 0 && !ui.media_labels.undownloaded(&c.key)) else { return };
        // which of the message's images it is, as the viewer counts them
        let crate::types::ClickedMedia::Image(index) = m.clicked_media(cell) else { return };
        let size = (crate::types::physical_px(c.w), crate::types::physical_px(c.h));
        stop_gif(ui);
        ui.anims.gif_seq += 1;
        let seq = ui.anims.gif_seq;
        ui.anims.gif = Some(GifPlay {
            id: id.to_string(),
            cell,
            seq,
            frames: Rc::new(VecModel::default()),
            frame: Rc::new(VecModel::from(vec![0])),
        });
        if let Some(tx) = &ui.tx {
            let _ = tx.send(Command::PlayGif { id: id.to_string(), index, seq, size });
        }
    });
}

/// The GIF in the message list stops and its cell goes back to the still.
pub(crate) fn stop_gif(ui: &mut Ui) {
    let Some(g) = ui.anims.gif.take() else { return };
    if let Some(tx) = &ui.tx {
        let _ = tx.send(Command::StopGif);
    }
    // no frame shown, nothing to take back: drawing the row again would make its cells anew,
    // which drops the hover and starts it all over (a GIF that can't play, again and again)
    if g.frames.row_count() > 0 {
        super::rows::redraw_rows(ui, |m| m.id == g.id);
    }
}

/// A frame of the playing GIF, or its end.
pub(crate) fn gif_arrived(slot: std::sync::Arc<GifSlot>) {
    let (frame, done) = slot.take();
    let img = frame.map(|t| Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height)));
    match slot.target {
        GifTarget::Viewer(index) => super::viewer::gif_frame(&slot.id, index, img, done),
        GifTarget::List => UI.with_borrow_mut(|ui| {
            let Some(g) = ui.anims.gif.as_ref().filter(|g| g.seq == slot.seq) else { return };
            let id = g.id.clone();
            if let Some(img) = img {
                if g.frames.row_count() == 0 {
                    g.frames.push(img);
                    super::rows::redraw_rows(ui, |m| m.id == id);
                    // its cells are new, and the pointer is still on one of them
                    refresh_hover();
                } else {
                    g.frames.set_row_data(0, img);
                }
            }
            if done {
                // it ended by itself: the backend has nothing to stop. Without a frame shown the
                // row is as it was (see `stop_gif`)
                let shown = g.frames.row_count() > 0;
                ui.anims.gif = None;
                if shown {
                    super::rows::redraw_rows(ui, |m| m.id == id);
                }
            }
        }),
        // routed to the composer by the event dispatch
        GifTarget::Composer => {}
    }
}

fn shown(id: &str) {
    UI.with_borrow_mut(|ui| {
        let Some(m) = ui.last.iter().find(|m| m.id == id) else { return };
        if !m.sticker || m.image_key.is_empty() {
            return;
        }
        let key = m.image_key.clone();
        // not downloaded yet: the still's arrival reports the row again
        if ui.anims.list.contains_key(&key) || !ui.thumbs.contains_key(&key) || !ui.anims.asked.insert(key.clone()) {
            return;
        }
        // played only when hovered: until then, only whether it gets the mark
        let cmd = if ui.anims.hover_only { Command::StickerAnimated(key) } else { Command::StickerFrames(key) };
        if let Some(tx) = &ui.tx {
            let _ = tx.send(cmd);
        }
    });
}

/// A message-list sticker turned out animated: its rows get the mark.
pub(crate) fn sticker_animated(key: &str) {
    UI.with_borrow_mut(|ui| {
        // a key not asked belongs to a thread left meanwhile
        if ui.anims.hover_only && ui.anims.asked.contains(key) && ui.anims.animated.insert(key.to_string()) {
            set_rows(ui, key);
        }
    });
}

/// The pointer came onto a marked message-list sticker (row `id`) or left it: it plays meanwhile.
fn sticker_hover(id: &str, on: bool) {
    UI.with_borrow_mut(|ui| {
        let Some(key) = ui.last.iter().find(|m| m.id == id).map(|m| m.image_key.clone()) else { return };
        if on {
            if let Some(old) = ui.anims.list_hovered.replace(key.clone()) {
                if old != key && ui.anims.list.remove(&old).is_some() {
                    set_rows(ui, &old);
                }
            }
            if !ui.anims.list.contains_key(&key) {
                if let Some(tx) = &ui.tx {
                    let _ = tx.send(Command::StickerFrames(key));
                }
            }
        } else if ui.anims.list_hovered.as_deref() == Some(key.as_str()) {
            ui.anims.list_hovered = None;
            if ui.anims.list.remove(&key).is_some() {
                set_rows(ui, &key);
            }
        }
    });
}

fn hover(pack: &str, id: u32, on: bool) {
    UI.with_borrow_mut(|ui| {
        if !ui.anims.hover_only {
            return;
        }
        let p = &mut ui.anims.picker;
        let key = format!("{STICKER_THUMB_PREFIX}{pack}_{id}");
        if on {
            p.hovered = Some(key);
        } else if p.hovered.as_deref() == Some(key.as_str()) {
            p.hovered = None;
        } else {
            return;
        }
        update_picker(ui);
    });
}

/// The picker's cells on screen, [first, end).
pub(crate) fn picker_range(ui: &Ui) -> (usize, usize) {
    ui.anims.picker.range
}

/// The tab whose grid the picker shows ("" none).
pub(crate) fn picker_pack(ui: &Ui) -> &str {
    &ui.anims.picker.pack
}

/// The sticker panel opened: whether all on screen play follows Windows' animation effects.
pub(crate) fn picker_opened(ui: &mut Ui) {
    ui.anims.read_motion();
}

/// The picker shows the grid of tab `pack` (in `ui.sticker.grid`) or none (""): the old grid's
/// animations go, and what's still to make for it is dropped.
pub(crate) fn picker_grid(ui: &mut Ui, pack: &str) {
    let p = std::mem::take(&mut ui.anims.picker);
    ui.anims.picker.range = p.range;
    ui.anims.picker.pack = pack.to_string();
    if p.peek.is_some() {
        show_peek(&ui.anims.picker);
    }
    if p.asked.iter().any(|k| !p.made.contains_key(k) && !p.stills.contains(k)) {
        if let Some(tx) = &ui.tx {
            let _ = tx.send(Command::PickerFrames(Vec::new()));
        }
    }
    update_picker(ui);
}

/// Puts the animations on the picker cells that should play and takes them off the others, asks
/// for the ones missing, and lets go of those not shown past `OFF_SCREEN`.
pub(crate) fn update_picker(ui: &mut Ui) {
    let hover_only = ui.anims.hover_only;
    let p = &mut ui.anims.picker;
    let grid = &ui.sticker.grid;
    p.turn += 1;
    let (first, end) = p.range;
    let focused = super::presence::focused();
    let want: HashSet<&str> = if !focused {
        HashSet::new()
    } else if hover_only {
        p.hovered.iter().map(String::as_str).collect()
    } else {
        grid.iter().skip(first).take(end.saturating_sub(first)).filter(|s| s.animated).map(|s| s.key.as_str()).collect()
    };
    let mut ask = Vec::new();
    for (i, s) in grid.iter().enumerate() {
        let key = s.key.as_str();
        let on = want.contains(key);
        if p.shown.contains(key) == on {
            continue;
        }
        let image = match p.made.get_mut(key) {
            Some((st, seen)) => {
                *seen = p.turn;
                st.set_playing(on);
                if on { st.image.clone() } else { Image::default() }
            }
            None if on => {
                if !p.stills.contains(key) {
                    ask.push(s);
                }
                continue;
            }
            None => Image::default(),
        };
        if on {
            p.shown.insert(s.key.clone());
        } else {
            p.shown.remove(key);
        }
        if let Some(mut row) = ui.sticker.stickers.row_data(i) {
            row.anim = image;
            ui.sticker.stickers.set_row_data(i, row);
        }
    }
    // the backend works through what it was asked; ask again only when that changes
    let pending = p.asked.iter().filter(|k| !p.made.contains_key(*k) && !p.stills.contains(*k));
    if !pending.eq(ask.iter().map(|s| &s.key)) {
        p.asked = ask.iter().map(|s| s.key.clone()).collect();
        if let Some(tx) = &ui.tx {
            let _ = tx.send(Command::PickerFrames(ask.iter().map(|s| (s.pack.clone(), s.id)).collect()));
        }
    }
    let mut off: Vec<(u64, &String, usize)> =
        p.made.iter().filter(|(k, _)| !p.shown.contains(*k)).map(|(k, (s, seen))| (*seen, k, s.bytes())).collect();
    let mut held = off.iter().map(|o| o.2).sum::<usize>();
    // without the focus none is shown, but those on screen play again when it comes back
    if held > OFF_SCREEN && focused {
        off.sort_by_key(|o| o.0);
        let gone: Vec<String> = off
            .into_iter()
            .take_while(|o| {
                let over = held > OFF_SCREEN;
                held -= o.2;
                over
            })
            .map(|o| o.1.clone())
            .collect();
        for k in gone {
            p.made.remove(&k);
        }
    }
}

pub(crate) fn frames_arrived(f: Frames) {
    UI.with_borrow_mut(|ui| {
        if f.key.starts_with(STICKER_THUMB_PREFIX) {
            // one of a grid no longer shown
            if !ui.sticker.grid.iter().any(|s| s.key == f.key) || ui.anims.picker.made.contains_key(&f.key) {
                return;
            }
            let p = &mut ui.anims.picker;
            match Sticker::new(&f) {
                Some(s) => {
                    p.made.insert(f.key.clone(), (s, p.turn));
                }
                None => {
                    p.stills.insert(f.key.clone());
                }
            }
            update_picker(ui);
            let p = &ui.anims.picker;
            let kb = p.made.values().map(|s| s.0.bytes()).sum::<usize>() / 1024;
            tracing::debug!(playing = p.shown.len(), made = p.made.len(), kb, "picker frames");
            return;
        }
        if f.png.is_empty() {
            // a still; it stays in `asked`
            return;
        }
        // a key not asked belongs to a thread left meanwhile
        if !ui.anims.asked.contains(&f.key) || ui.anims.list.contains_key(&f.key) {
            return;
        }
        // played only while hovered, and the pointer has left
        if ui.anims.hover_only && ui.anims.list_hovered.as_deref() != Some(f.key.as_str()) {
            return;
        }
        let Some(mut s) = Sticker::new(&f) else { return };
        // counted as playing for as long as it's kept: off screen, its rows are gone and it
        // holds less, but which rows are on screen isn't followed here
        s.set_playing(true);
        ui.anims.list.insert(f.key.clone(), s);
        set_rows(ui, &f.key);
        trim_list(ui);
        tracing::debug!(playing = ui.anims.list.len(), kb = ui.anims.bytes() / 1024, "anim frames");
    });
}

/// The picker's grid cell `index` is to be shown big, or none (-1).
pub(crate) fn peek(index: i32) {
    UI.with_borrow_mut(|ui| {
        let s = usize::try_from(index).ok().and_then(|i| ui.sticker.grid.get(i));
        let p = &mut ui.anims.picker;
        if p.peek.as_ref().map(|k| &k.key) == s.map(|s| &s.key) {
            return;
        }
        let Some(s) = s else {
            p.peek = None;
            show_peek(p);
            return;
        };
        // the one shown stays until this one's still arrives, so moving between cells doesn't
        // blink
        p.peek = Some(Peek { key: s.key.clone(), answered: false, still: None, anim: None });
        if let Some(tx) = &ui.tx {
            let _ = tx.send(Command::PeekSticker(s.pack.clone(), s.id, s.animated));
        }
    });
}

pub(crate) fn peek_still(t: crate::types::Thumb) {
    UI.with_borrow_mut(|ui| {
        let Some(k) = ui.anims.picker.peek.as_mut().filter(|k| k.key == t.key && !k.answered) else { return };
        k.answered = true;
        if t.width > 0 {
            let held = crate::alloc::Held::new(crate::alloc::Gauge::Anims, t.rgba.len());
            k.still = Some((Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height)), held));
        }
        show_peek(&ui.anims.picker);
    });
}

pub(crate) fn peek_frames(f: Frames) {
    UI.with_borrow_mut(|ui| {
        let Some(k) = ui.anims.picker.peek.as_mut().filter(|k| k.key == f.key && k.anim.is_none()) else { return };
        let Some(mut s) = Sticker::new(&f) else { return };
        s.set_playing(true);
        k.anim = Some(s);
        show_peek(&ui.anims.picker);
    });
}

/// Puts the big sticker in the picker: its animation while the window has the focus, else its
/// still; nothing once it isn't wanted or can't be read.
fn show_peek(p: &Picker) {
    let Some(app) = APP.with_borrow(|a| a.as_ref().and_then(|w| w.upgrade())) else { return };
    let d = app.global::<crate::StickerData>();
    let Some(k) = &p.peek else {
        d.set_peek_img(Image::default());
        return;
    };
    let anim = k.anim.as_ref().filter(|_| super::presence::focused()).map(|s| s.image.clone());
    match anim.or_else(|| k.still.as_ref().map(|s| s.0.clone())) {
        Some(img) => {
            d.set_peek_size(crate::view::media::sticker_size() as f32);
            d.set_peek_img(img);
        }
        // not here yet: the one shown stays (see `peek`)
        None if !k.answered => {}
        None => d.set_peek_img(Image::default()),
    }
}

/// Puts a key's animation (or none) on its message rows.
fn set_rows(ui: &Ui, key: &str) {
    super::rows::redraw_rows(ui, |m| m.image_key == key);
}
