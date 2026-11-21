//! Animated stickers, and the one GIF playing in the message list.
//!
//! Stickers: a row reports itself on screen (`Anim.shown`), then the backend is asked once for
//! the sticker as an APNG at the size shown (stills answer with none). Slint's `Image` plays it,
//! decoding a frame when it draws one, so nothing ticks here and nothing decodes while it isn't
//! drawn. The least recently shown sticker goes back to its still when the list's stickers
//! would exceed `ANIM_BUDGET`. Picker: only the hovered sticker plays.
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
    /// what it holds while playing: its bytes and the two frames `Image` keeps decoded
    bytes: usize,
    /// `Anims.clock` when last shown
    used: u64,
    _held: crate::alloc::Held,
}

impl Sticker {
    fn new(f: &Frames, used: u64) -> Option<Self> {
        let image = Image::load_from_data(&f.png, Some("png")).ok()?;
        let size = image.size();
        let bytes = f.png.len() + 2 * 4 * (size.width * size.height) as usize;
        Some(Sticker { image, bytes, used, _held: crate::alloc::Held::new(crate::alloc::Gauge::Anims, bytes) })
    }
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
    clock: u64,
    /// the hovered picker sticker's key, and whether it plays (its APNG arrived)
    hovered: Option<String>,
    picker: bool,
}

impl Anims {
    /// A sticker's animation for a message row (empty: show the still).
    pub(crate) fn image(&self, key: &str) -> Image {
        self.list.get(key).map(|s| s.image.clone()).unwrap_or_default()
    }

    /// The playing GIF's frames for cell `cell` of row `id` (empty: show the still).
    pub(crate) fn gif_models(&self, id: &str, cell: usize) -> (ModelRc<Image>, ModelRc<i32>) {
        match self.gif.as_ref().filter(|g| g.id == id && g.cell == cell && g.frames.row_count() > 0) {
            Some(g) => (ModelRc::from(g.frames.clone()), ModelRc::from(g.frame.clone())),
            None => Default::default(),
        }
    }

    /// Thread switch: the old thread's stickers go.
    pub(crate) fn clear_list(&mut self) {
        self.list.clear();
        self.asked.clear();
    }

    fn bytes(&self) -> usize {
        self.list.values().map(|s| s.bytes).sum()
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
    app.on_gif_scrolled(refresh_hover);
    g.on_gif_hover(|id, cell, on| {
        let id = id.to_string();
        // these run while Slint evaluates the row: act on the next turn of the event loop
        slint::Timer::single_shot(Duration::ZERO, move || gif_hover(&id, cell.max(0) as usize, on));
    });
    g.on_hover(|pack, id, on| {
        let pack = pack.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || hover(&pack, id as u32, on));
    });
}

/// Slint moves the hover only with the mouse: rows that scroll under a resting cursor never get
/// it, the one that left keeps it, and a cell drawn again (its row was) starts without it. Tell
/// it where the cursor is, on the next turn (this can run while the list updates), once for any
/// number of calls.
pub(crate) fn refresh_hover() {
    if REFRESH_PENDING.replace(true) {
        return;
    }
    slint::Timer::single_shot(Duration::ZERO, move || {
        REFRESH_PENDING.set(false);
        let Some(app) = APP.with_borrow(|a| a.as_ref().and_then(|w| w.upgrade())) else { return };
        if app.get_inactive() {
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
        ui.anims.clock += 1;
        let clock = ui.anims.clock;
        if let Some(p) = ui.anims.list.get_mut(&key) {
            p.used = clock;
            return;
        }
        // not downloaded yet: the still's arrival reports the row again
        if !ui.thumbs.contains_key(&key) || !ui.anims.asked.insert(key.clone()) {
            return;
        }
        if let Some(tx) = &ui.tx {
            let _ = tx.send(Command::StickerFrames(key));
        }
    });
}

fn hover(pack: &str, id: u32, on: bool) {
    UI.with_borrow_mut(|ui| {
        let key = format!("{STICKER_THUMB_PREFIX}{pack}_{id}");
        if on {
            stop_picker(ui);
            ui.anims.hovered = Some(key);
            if let Some(tx) = &ui.tx {
                let _ = tx.send(Command::PickerFrames { pack: pack.to_string(), id });
            }
        } else if ui.anims.hovered.as_deref() == Some(key.as_str()) {
            stop_picker(ui);
        }
    });
}

/// The hovered picker sticker stops and its animation goes (also when the grid changes or closes).
pub(crate) fn stop_picker(ui: &mut Ui) {
    let Some(key) = ui.anims.hovered.take() else { return };
    if std::mem::take(&mut ui.anims.picker) {
        set_picker_row(ui, &key, Image::default());
    }
}

pub(crate) fn frames_arrived(f: Frames) {
    UI.with_borrow_mut(|ui| {
        if f.png.is_empty() {
            // a still; it stays in `asked`
            return;
        }
        if f.key.starts_with(STICKER_THUMB_PREFIX) {
            if ui.anims.hovered.as_deref() != Some(f.key.as_str()) {
                return;
            }
            let Some(s) = Sticker::new(&f, 0) else { return };
            set_picker_row(ui, &f.key, s.image);
            ui.anims.picker = true;
        } else {
            // a key not asked belongs to a thread left meanwhile
            if !ui.anims.asked.contains(&f.key) || ui.anims.list.contains_key(&f.key) {
                return;
            }
            let Some(s) = Sticker::new(&f, ui.anims.clock) else { return };
            while !ui.anims.list.is_empty() && ui.anims.bytes() + s.bytes > ANIM_BUDGET {
                let Some(old) = ui.anims.list.iter().min_by_key(|(_, s)| s.used).map(|(k, _)| k.clone()) else { break };
                tracing::debug!("anim budget: {old} back to its still");
                ui.anims.list.remove(&old);
                ui.anims.asked.remove(&old);
                set_rows(ui, &old);
            }
            ui.anims.list.insert(f.key.clone(), s);
            set_rows(ui, &f.key);
            tracing::debug!(playing = ui.anims.list.len(), kb = ui.anims.bytes() / 1024, "anim frames");
        }
    });
}

/// Puts a key's animation (or none) on its message rows.
fn set_rows(ui: &Ui, key: &str) {
    super::rows::redraw_rows(ui, |m| m.image_key == key);
}

fn set_picker_row(ui: &Ui, key: &str, anim: Image) {
    let Some(i) = ui.sticker.grid.iter().position(|s| s.key == key) else { return };
    if let Some(mut row) = ui.sticker.stickers.row_data(i) {
        row.anim = anim;
        ui.sticker.stickers.set_row_data(i, row);
    }
}
