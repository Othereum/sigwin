//! Animated stickers: frames per sticker under a memory budget, and the timer that flips them.
//!
//! Message list: a sticker row reports itself on screen (`Anim.shown`), then its frames are
//! asked from the backend once; stills answer with no frames. The least recently shown sticker
//! goes back to its still when the frames would exceed `ANIM_BUDGET`. Picker: only the hovered
//! sticker plays.
//!
//! Timing: each sticker plays from when its frames arrived, by the wall clock, with exact frame
//! times (µs; APNG's 1/30 s stays 33.3ms, so every frame lands on the same number of display
//! refreshes). One single-shot timer sleeps until the earliest frame change, then updates only
//! the stickers whose frame changed: a one-element `frame` model their rows share, so no row is
//! touched. It stops while nothing plays or the window is minimized.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use super::{Ui, UI};
use crate::types::{Command, Frames, ANIM_BUDGET, STICKER_THUMB_PREFIX};
use crate::{Anim, AppWindow};

struct Playing {
    frames: ModelRc<Image>,
    /// the shown frame's index, one element, shared by every row of this sticker
    frame: Rc<VecModel<i32>>,
    /// end of each frame within a loop (µs, cumulative); the last is the loop length
    ends: Vec<u64>,
    start: Instant,
    bytes: usize,
    /// `Anims.clock` when last shown
    used: u64,
}

impl Playing {
    /// The frame due at `now`, and when the next one is.
    fn at(&self, now: Instant) -> (usize, Instant) {
        let total = *self.ends.last().unwrap_or(&1);
        let t = now.saturating_duration_since(self.start).as_micros() as u64;
        let pos = t % total;
        let i = self.ends.partition_point(|&e| e <= pos).min(self.ends.len() - 1);
        (i, now + Duration::from_micros(self.ends[i] - pos))
    }

    fn models(&self) -> (ModelRc<Image>, ModelRc<i32>) {
        (self.frames.clone(), ModelRc::from(self.frame.clone()))
    }
}

#[derive(Default)]
pub(crate) struct Anims {
    /// message-list stickers by thumbnail key, open thread only
    list: HashMap<String, Playing>,
    /// keys asked from the backend, answered or not (an evicted key leaves, so it can come back)
    asked: HashSet<String>,
    clock: u64,
    /// the hovered picker sticker's key, and its frames once they arrive
    hovered: Option<String>,
    picker: Option<Playing>,
}

impl Anims {
    /// Frames and current-frame model for a message row (empty: show the still).
    pub(crate) fn models(&self, key: &str) -> (ModelRc<Image>, ModelRc<i32>) {
        self.list.get(key).map(Playing::models).unwrap_or_default()
    }

    /// Thread switch: the old thread's frames go.
    pub(crate) fn clear_list(&mut self) {
        self.list.clear();
        self.asked.clear();
        if self.picker.is_none() {
            TIMER.with(|t| t.stop());
        }
    }

    fn bytes(&self) -> usize {
        self.list.values().map(|p| p.bytes).sum()
    }
}

thread_local! {
    static TIMER: slint::Timer = slint::Timer::default();
    static APP: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static PAUSED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn wire(app: &AppWindow) {
    APP.with_borrow_mut(|a| *a = Some(app.as_weak()));
    let g = app.global::<Anim>();
    // these run while Slint evaluates the row: act on the next turn of the event loop
    g.on_shown(|id| {
        let id = id.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || shown(&id));
    });
    g.on_hover(|pack, id, on| {
        let pack = pack.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || hover(&pack, id as u32, on));
    });
}

/// The window was minimized (true) or restored. Nothing ticks meanwhile: updates while
/// minimized also confuse the software renderer's damage tracking (`winhook.rs`).
pub(crate) fn pause(paused: bool) {
    PAUSED.set(paused);
    // called from the window procedure: don't touch UI state from inside it
    slint::Timer::single_shot(Duration::ZERO, || UI.with_borrow(sync_timer));
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
        sync_timer(ui);
    });
}

/// The hovered picker sticker stops and its frames go (also when the grid changes or closes).
pub(crate) fn stop_picker(ui: &mut Ui) {
    let Some(key) = ui.anims.hovered.take() else { return };
    if ui.anims.picker.take().is_some() {
        set_picker_row(ui, &key, Default::default());
    }
    sync_timer(ui);
}

pub(crate) fn frames_arrived(f: Frames) {
    UI.with_borrow_mut(|ui| {
        if f.key.starts_with(STICKER_THUMB_PREFIX) {
            if f.frames.is_empty() || ui.anims.hovered.as_deref() != Some(f.key.as_str()) {
                return;
            }
            let p = playing(&f, 0);
            set_picker_row(ui, &f.key, p.models());
            ui.anims.picker = Some(p);
        } else {
            // stills stay in `asked`; a key not asked belongs to a thread left meanwhile
            if f.frames.is_empty() || !ui.anims.asked.contains(&f.key) || ui.anims.list.contains_key(&f.key) {
                return;
            }
            let p = playing(&f, ui.anims.clock);
            while !ui.anims.list.is_empty() && ui.anims.bytes() + p.bytes > ANIM_BUDGET {
                let Some(old) = ui.anims.list.iter().min_by_key(|(_, p)| p.used).map(|(k, _)| k.clone()) else { break };
                tracing::debug!("anim budget: {old} back to its still");
                ui.anims.list.remove(&old);
                ui.anims.asked.remove(&old);
                set_rows(ui, &old);
            }
            ui.anims.list.insert(f.key.clone(), p);
            set_rows(ui, &f.key);
            tracing::debug!(playing = ui.anims.list.len(), kb = ui.anims.bytes() / 1024, "anim frames");
        }
        sync_timer(ui);
    });
}

fn playing(f: &Frames, used: u64) -> Playing {
    let frames: Vec<Image> = f
        .frames
        .iter()
        .map(|px| Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(px, f.width, f.height)))
        .collect();
    let ends = f
        .delays_us
        .iter()
        .scan(0u64, |end, &d| {
            *end += d.max(1) as u64;
            Some(*end)
        })
        .collect();
    Playing {
        frames: ModelRc::new(VecModel::from(frames)),
        frame: Rc::new(VecModel::from(vec![0])),
        ends,
        start: Instant::now(),
        bytes: f.frames.iter().map(Vec::len).sum(),
        used,
    }
}

/// Puts a key's frames (or none) on its message rows.
fn set_rows(ui: &Ui, key: &str) {
    let (frames, frame) = ui.anims.models(key);
    for (i, m) in ui.last.iter().enumerate() {
        if m.image_key == key {
            if let Some(mut row) = ui.messages.row_data(i) {
                row.frames = frames.clone();
                row.frame = frame.clone();
                ui.messages.set_row_data(i, row);
            }
        }
    }
}

fn set_picker_row(ui: &Ui, key: &str, (frames, frame): (ModelRc<Image>, ModelRc<i32>)) {
    let Some(i) = ui.sticker.grid.iter().position(|s| s.key == key) else { return };
    if let Some(mut row) = ui.sticker.stickers.row_data(i) {
        row.frames = frames;
        row.frame = frame;
        ui.sticker.stickers.set_row_data(i, row);
    }
}

/// Shows each sticker's current frame and sleeps until the earliest next one. Frames follow
/// the wall clock, so a late tick never slows an animation down.
fn tick() {
    let now = Instant::now();
    let next = UI.with_borrow(|ui| {
        let playing = ui.anims.list.values().chain(ui.anims.picker.as_ref());
        playing
            .map(|p| {
                let (i, next) = p.at(now);
                if p.frame.row_data(0) != Some(i as i32) {
                    p.frame.set_row_data(0, i as i32);
                }
                next
            })
            .min()
    });
    if let Some(next) = next {
        TIMER.with(|t| t.start(slint::TimerMode::SingleShot, next.saturating_duration_since(Instant::now()), tick));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_follow_exact_times() {
        let f = Frames { key: "k".into(), width: 1, height: 1, frames: vec![vec![0; 4]; 3], delays_us: vec![33_333; 3] };
        let p = playing(&f, 0);
        let us = |n: u64| p.start + Duration::from_micros(n);
        assert_eq!(p.at(us(0)), (0, us(33_333)));
        assert_eq!(p.at(us(33_333)), (1, us(66_666)));
        assert_eq!(p.at(us(66_665)), (1, us(66_666)));
        // loops; the boundary keeps its fraction instead of drifting by 1ms per frame
        assert_eq!(p.at(us(99_999 + 40_000)), (1, us(99_999 + 66_666)));
    }
}

fn sync_timer(ui: &Ui) {
    let run = !PAUSED.get() && (!ui.anims.list.is_empty() || ui.anims.picker.is_some());
    TIMER.with(|t| {
        if !run {
            t.stop();
        } else {
            // a new sticker may change sooner than the one the timer waits for
            t.start(slint::TimerMode::SingleShot, Duration::ZERO, tick);
        }
    });
}
