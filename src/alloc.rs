//! Global allocator wrapper that counts live Rust heap bytes, so memory reports can tell
//! Rust allocations apart from native ones (sqlite, Windows DLLs).

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub struct Counting;

/// What allocates under the count: DHAT's recorder in a heap profiling build (`dhat-heap`),
/// which passes through to the system allocator while no profile runs (`profile`).
#[cfg(feature = "dhat-heap")]
static INNER: dhat::Alloc = dhat::Alloc;
#[cfg(not(feature = "dhat-heap"))]
static INNER: std::alloc::System = std::alloc::System;

/// Heap profiling build: records allocations from now until the guard drops (the end of
/// `main`), then writes where the heap's peak was allocated to `dhat-heap.json` in `dir`.
/// Backtraces and sizes only, no contents.
#[cfg(feature = "dhat-heap")]
pub fn profile(dir: &std::path::Path) -> dhat::Profiler {
    let file = dir.join("dhat-heap.json");
    tracing::info!("heap profile: written on exit");
    dhat::Profiler::builder().file_name(file).build()
}

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { INNER.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { INNER.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { INNER.alloc_zeroed(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { INNER.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = LIVE.fetch_add(new - l.size(), Relaxed) + new - l.size();
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

pub fn live() -> usize {
    LIVE.load(Relaxed)
}

pub fn peak() -> usize {
    PEAK.load(Relaxed)
}

/// Private (commit) bytes of this process.
#[cfg(windows)]
pub fn private_bytes() -> usize {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: valid out-pointer with cb set
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut _, c.cb) };
    if ok == 0 { 0 } else { c.PrivateUsage }
}

#[cfg(not(windows))]
pub fn private_bytes() -> usize {
    0
}

/// What holds Rust heap for a while, counted where it is kept, so the report says what the heap
/// is made of. What isn't counted shows as the rest.
#[derive(Clone, Copy)]
pub enum Gauge {
    /// pictures of the open chat's rows (UI)
    Thumbs,
    /// round photos of the thread list, for the session
    Avatars,
    /// photos next to group messages and of readers, open chat only
    SenderAvatars,
    /// animated stickers playing in the open chat and the picker, and the picker's big one
    Anims,
    /// the composer's attachment tiles
    Composer,
    /// the sticker panel's pack covers
    StickerCovers,
    /// the media page's cells
    Gallery,
    /// the picture open in the image viewer
    Viewer,
    /// the downloaded stickers of a pack shown before it is installed (backend)
    StickerPreview,
}

impl Gauge {
    /// By number, for a const generic (`gui::held::ImageMap`).
    pub const ALL: [Gauge; 9] = [
        Gauge::Thumbs,
        Gauge::Avatars,
        Gauge::SenderAvatars,
        Gauge::Anims,
        Gauge::Composer,
        Gauge::StickerCovers,
        Gauge::Gallery,
        Gauge::Viewer,
        Gauge::StickerPreview,
    ];
}

const GAUGES: [&str; 9] =
    ["thumbs", "avatars", "sender avatars", "anims", "composer", "sticker covers", "gallery", "viewer", "sticker preview"];

/// Bytes and items of each gauge.
static HELD: [(AtomicUsize, AtomicUsize); GAUGES.len()] = [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; GAUGES.len()];

/// `items` more of `bytes` held under `g`.
pub fn add(g: Gauge, bytes: usize, items: usize) {
    HELD[g as usize].0.fetch_add(bytes, Relaxed);
    HELD[g as usize].1.fetch_add(items, Relaxed);
}

/// `items` of `bytes` no longer held under `g`.
pub fn sub(g: Gauge, bytes: usize, items: usize) {
    HELD[g as usize].0.fetch_sub(bytes, Relaxed);
    HELD[g as usize].1.fetch_sub(items, Relaxed);
}

/// Bytes and items held under `g` now.
#[cfg(test)]
pub fn held(g: Gauge) -> (usize, usize) {
    (HELD[g as usize].0.load(Relaxed), HELD[g as usize].1.load(Relaxed))
}

/// One item counted under its gauge for as long as this lives: a field of what holds the memory.
pub struct Held {
    gauge: Gauge,
    bytes: usize,
}

impl Held {
    pub fn new(gauge: Gauge, bytes: usize) -> Self {
        add(gauge, bytes, 1);
        Self { gauge, bytes }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        sub(self.gauge, self.bytes, 1);
    }
}

fn mb(b: usize) -> f64 {
    b as f64 / 1048576.0
}

pub fn report() -> String {
    let live = live();
    let mut s = format!("rust heap {:.1}MB (peak {:.1}MB), private {:.1}MB", mb(live), mb(peak()), mb(private_bytes()));
    let mut counted = 0;
    for (name, (bytes, items)) in GAUGES.iter().zip(&HELD) {
        let (bytes, items) = (bytes.load(Relaxed), items.load(Relaxed));
        counted += bytes;
        if items > 0 {
            s += &format!(" | {name} {:.1}MB/{items}", mb(bytes));
        }
    }
    s + &format!(" | rest {:.1}MB", mb(live.saturating_sub(counted)))
}
