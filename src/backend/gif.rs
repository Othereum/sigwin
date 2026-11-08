//! A GIF playing in its message or in the viewer.
//!
//! Frames are decoded one at a time on a thread of their own and handed to the UI through a
//! `GifSlot`: nothing but the file's bytes and the frame in flight is kept, however long the GIF
//! is. A pre-decoded one would take tens of MB (a 7 MB GIF is 300 MB of frames). One GIF plays at
//! a time; starting another, or anything else that replaces it, stops the old one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::types::{GifSlot, GifTarget};

/// A GIF in the message list stops after this long: nobody watches it forever, and it keeps
/// decoding even when scrolled out of sight.
const LIST_LIMIT: Duration = Duration::from_secs(60);
/// A frame this late starts the timing over instead of racing to catch up.
const LATE: Duration = Duration::from_millis(250);

static GEN: AtomicU64 = AtomicU64::new(1 << 40);

/// Numbers for the viewer's playbacks, apart from the ones the message list picks.
pub(super) fn next_gen() -> u64 {
    GEN.fetch_add(1, Ordering::Relaxed)
}

/// The first frame of a GIF for the viewer, shrunk to fit in `fit` (never enlarged).
pub(super) fn first_frame(bytes: &[u8], fit: (u32, u32)) -> Option<crate::types::ViewerPixels> {
    let full = sigwin_thumb::gif_size(bytes)?;
    let (w, h) = sigwin_thumb::fit_size(full, fit);
    let mut first = None;
    sigwin_thumb::stream_gif(bytes, w.max(h), |f, _| {
        first = Some(f);
        false
    })
    .ok()?;
    let f = first?;
    let original = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(f.as_raw(), f.width(), f.height());
    Some(crate::types::ViewerPixels { original, fitted: None, opaque: false })
}

/// Plays `bytes` until `stop` is set or `limit` runs out, then reports the end.
pub(super) fn stream(bytes: Vec<u8>, px: u32, slot: Arc<GifSlot>, stop: Arc<AtomicBool>, emit: Emit, limit: Option<Duration>) {
    let spawned = std::thread::Builder::new().name("gif".into()).spawn(move || {
        let started = Instant::now();
        let mut due = started;
        let mut shown = 0u32;
        let played = sigwin_thumb::stream_gif(&bytes, px, |frame, delay_us| {
            // wait for this frame's time in slices, so a stop doesn't wait out a long frame
            loop {
                if stop.load(Ordering::Relaxed) || limit.is_some_and(|l| started.elapsed() > l) {
                    return false;
                }
                let now = Instant::now();
                if now >= due {
                    break;
                }
                std::thread::sleep((due - now).min(Duration::from_millis(50)));
            }
            let now = Instant::now();
            if now > due + LATE {
                due = now;
            }
            due += Duration::from_micros(delay_us as u64);
            let (width, height) = frame.dimensions();
            if slot.put(crate::types::Thumb { key: String::new(), width, height, rgba: frame.into_raw() }) {
                emit(Event::Gif(slot.clone()));
            }
            shown += 1;
            true
        });
        tracing::debug!(frames = shown, ms = started.elapsed().as_millis() as u64, ok = played.is_ok(), "gif stopped");
        if slot.finish() {
            emit(Event::Gif(slot.clone()));
        }
    });
    if let Err(e) = spawned {
        warn!("gif thread: {e}");
    }
}

impl App {
    /// A new stop flag for the GIF that is about to play; the old one stops.
    pub(super) fn new_gif_stop(&mut self) -> Arc<AtomicBool> {
        self.stop_gif();
        let stop = Arc::new(AtomicBool::new(false));
        self.gif_stop = Some(stop.clone());
        stop
    }

    pub(super) fn stop_gif(&mut self) {
        if let Some(stop) = self.gif_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// Plays the GIF of row `id` in the message list, at `px` physical px on its long side.
    pub(super) fn play_gif(&mut self, id: &str, seq: u64, px: u32) {
        let Some((path, ptr)) = self.viewed(id, 0) else { return };
        let stop = self.new_gif_stop();
        let (manager, tx, emit) = (self.manager.clone(), self.tx.clone(), self.emit.clone());
        let slot = Arc::new(GifSlot::new(id.to_string(), seq, GifTarget::List));
        tokio::task::spawn_local(async move {
            if !super::files::fetch_attachment(&manager, &tx, &path, &ptr).await {
                let _ = slot.finish() && {
                    emit(Event::Gif(slot.clone()));
                    true
                };
                return;
            }
            let _ = tx.send(Internal::ProgressDone);
            let read = tokio::task::spawn_blocking(move || sigwin_vault::read(&path)).await;
            match read {
                Ok(Ok(bytes)) if !stop.load(Ordering::Relaxed) && sigwin_thumb::is_animated_gif(&bytes) => {
                    stream(bytes, px, slot, stop, emit, Some(LIST_LIMIT));
                }
                other => {
                    if let Ok(Err(e)) = other {
                        warn!("play gif: {e}");
                    }
                    if slot.finish() {
                        emit(Event::Gif(slot.clone()));
                    }
                }
            }
        });
    }
}
