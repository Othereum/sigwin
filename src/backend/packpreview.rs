//! A sticker pack shown in the panel before it is installed: its manifest, then the stickers on
//! screen, downloaded from the sticker CDN and held in memory only until the panel lets go.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use presage::store::StickerPackManifest;

use super::stickers::{cover_key, sticker_thumb_key, still_thumb, PACK_COVER, PACK_COVER_SMALL, PICKER_CELL};
use super::*;
use crate::types::{PreviewDto, PreviewState};

/// Downloads at once, here and when adding a pack: a sticker takes most of a second, one after
/// another a screenful would take tens of seconds.
pub(super) const PARALLEL: usize = 3;

/// Tells one preview (and each retry) from the next, so late answers of an older one are dropped.
static SEQ: AtomicU64 = AtomicU64::new(0);

pub(super) struct Preview {
    pub(super) hex: String,
    id: Vec<u8>,
    key: Vec<u8>,
    seq: u64,
    state: PreviewState,
    manifest: Option<StickerPackManifest>,
    /// stickers downloaded, by id
    pub(super) bytes: HashMap<u32, (Arc<[u8]>, crate::alloc::Held)>,
    animated: HashSet<u32>,
    failed: HashSet<u32>,
    running: HashSet<u32>,
    /// what's on screen and not here yet, in order
    want: std::collections::VecDeque<u32>,
}

impl Preview {
    fn dto(&self) -> PreviewDto {
        let (title, author, count) = match &self.manifest {
            Some(m) => (m.title.clone(), m.author.clone(), m.stickers.len()),
            None => Default::default(),
        };
        let cover = self.cover_id().map(|c| cover_key(&sticker_thumb_key(&self.hex, c))).unwrap_or_default();
        PreviewDto { pack: PackDto { id: self.hex.clone(), title, author, cover_key: cover }, state: self.state, count }
    }

    fn cover_id(&self) -> Option<u32> {
        let m = self.manifest.as_ref()?;
        m.cover.as_ref().map(|c| c.id).or_else(|| m.stickers.first().map(|s| s.id))
    }
}

impl App {
    /// Opens the panel on this pack: installed, on `sticker` of it; otherwise as a preview.
    pub(super) async fn preview_pack(&mut self, id: Vec<u8>, key: Option<Vec<u8>>, sticker: Option<u32>) {
        let hex = hex::encode(&id);
        if let Ok(Some(_)) = self.store.sticker_pack(&id).await {
            (self.emit)(Event::OpenStickerPanel { pack: hex, sticker });
            return;
        }
        if let Some(p) = self.preview.as_ref().filter(|p| p.hex == hex) {
            (self.emit)(Event::PackPreview(p.dto()));
            return;
        }
        // a sticker message without its pack's key can't show the pack
        let Some(key) = key else { return };
        self.preview = Some(Preview {
            hex,
            id,
            key,
            seq: 0,
            state: PreviewState::Loading,
            manifest: None,
            bytes: HashMap::new(),
            animated: HashSet::new(),
            failed: HashSet::new(),
            running: HashSet::new(),
            want: Default::default(),
        });
        self.fetch_manifest();
    }

    pub(super) fn retry_preview(&mut self) {
        if self.preview.as_ref().is_some_and(|p| p.state == PreviewState::Failed) {
            self.fetch_manifest();
        }
    }

    fn fetch_manifest(&mut self) {
        let Some(p) = self.preview.as_mut() else { return };
        p.seq = SEQ.fetch_add(1, Relaxed) + 1;
        p.state = PreviewState::Loading;
        (self.emit)(Event::PackPreview(p.dto()));
        let Some(m) = self.manager.clone() else {
            p.state = PreviewState::Failed;
            (self.emit)(Event::PackPreview(p.dto()));
            return;
        };
        let (seq, id, key) = (p.seq, p.id.clone(), p.key.clone());
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let started = std::time::Instant::now();
            let res = m.sticker_pack_manifest(&id, &key).await.map_err(|e| e.to_string());
            debug!(ms = started.elapsed().as_millis() as u64, ok = res.is_ok(), "pack manifest");
            let _ = tx.send(Internal::PreviewManifest(seq, res));
        });
    }

    pub(super) fn manifest_arrived(&mut self, seq: u64, res: Result<StickerPackManifest, String>) {
        let Some(p) = self.preview.as_mut().filter(|p| p.seq == seq) else { return };
        match res {
            Ok(m) => {
                p.manifest = Some(m);
                p.state = PreviewState::Ready;
                // the tab and the header show the cover before the grid asks for anything
                if let Some(c) = p.cover_id() {
                    p.want.push_front(c);
                }
            }
            Err(e) => {
                warn!("pack manifest: {e}");
                p.state = PreviewState::Failed;
            }
        }
        (self.emit)(Event::PackPreview(p.dto()));
        self.pump_preview();
    }

    /// The grid of the previewed pack, and the thumbs of what's downloaded. Nothing until the
    /// manifest is here: the panel shows it loading.
    pub(super) fn preview_grid(&self) {
        let Some(p) = &self.preview else { return };
        let Some(m) = &p.manifest else { return };
        let items: Vec<StickerDto> = m
            .stickers
            .iter()
            .map(|s| StickerDto {
                key: sticker_thumb_key(&p.hex, s.id),
                pack: p.hex.clone(),
                id: s.id,
                emoji: s.emoji.clone().unwrap_or_default(),
                animated: p.animated.contains(&s.id),
            })
            .collect();
        let failed: Vec<String> = items.iter().filter(|s| p.failed.contains(&s.id)).map(|s| s.key.clone()).collect();
        let held: Vec<(String, Arc<[u8]>)> =
            items.iter().filter_map(|s| p.bytes.get(&s.id).map(|(b, _)| (s.key.clone(), b.clone()))).collect();
        (self.emit)(Event::StickerGrid { pack: p.hex.clone(), items });
        for key in failed {
            (self.emit)(Event::PreviewSticker { key, animated: false, failed: true });
        }
        if held.is_empty() {
            return;
        }
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let thumbs = held.into_iter().filter_map(|(key, b)| still_thumb(key, &b, PICKER_CELL)).collect();
            emit(Event::StickerThumbs(thumbs));
        });
    }

    /// The grid shows these: they come first, in this order, after what's under way.
    pub(super) fn preview_want(&mut self, ids: Vec<u32>) {
        let Some(p) = self.preview.as_mut() else { return };
        // the cover, if it hasn't started yet, stays first
        let cover = p.cover_id().filter(|c| p.want.contains(c));
        p.want = cover
            .into_iter()
            .chain(ids)
            .filter(|id| !p.bytes.contains_key(id) && !p.failed.contains(id) && !p.running.contains(id))
            .collect();
        self.pump_preview();
    }

    fn pump_preview(&mut self) {
        let Some(m) = self.manager.clone() else { return };
        let Some(p) = self.preview.as_mut() else { return };
        while p.running.len() < PARALLEL {
            let Some(sid) = p.want.pop_front() else { break };
            if p.bytes.contains_key(&sid) || p.failed.contains(&sid) || !p.running.insert(sid) {
                continue;
            }
            let (seq, id, key, m) = (p.seq, p.id.clone(), p.key.clone(), m.clone());
            let tx = self.tx.clone();
            tokio::task::spawn_local(async move {
                let res = m.download_sticker(&id, &key, sid).await.map_err(|e| e.to_string());
                let _ = tx.send(Internal::PreviewSticker(seq, sid, res));
            });
        }
    }

    pub(super) fn preview_sticker_arrived(&mut self, seq: u64, sid: u32, res: Result<Vec<u8>, String>) {
        let Some(p) = self.preview.as_mut().filter(|p| p.seq == seq) else { return };
        p.running.remove(&sid);
        let key = sticker_thumb_key(&p.hex, sid);
        match res {
            Ok(bytes) => {
                let bytes: Arc<[u8]> = bytes.into();
                let animated = sigwin_thumb::is_animated(&bytes);
                if animated {
                    p.animated.insert(sid);
                }
                p.bytes.insert(sid, (bytes.clone(), crate::alloc::Held::new(crate::alloc::Gauge::StickerPreview, bytes.len())));
                let cover = p.cover_id() == Some(sid);
                let emit = self.emit.clone();
                tokio::task::spawn_blocking(move || {
                    let mut thumbs = Vec::new();
                    if cover {
                        let ck = cover_key(&key);
                        thumbs.extend(still_thumb(crate::types::small_cover_key(&ck), &bytes, PACK_COVER_SMALL));
                        thumbs.extend(still_thumb(ck, &bytes, PACK_COVER));
                    }
                    thumbs.extend(still_thumb(key.clone(), &bytes, PICKER_CELL));
                    emit(Event::StickerThumbs(thumbs));
                    emit(Event::PreviewSticker { key, animated, failed: false });
                });
            }
            Err(e) => {
                warn!("preview sticker: {e}");
                p.failed.insert(sid);
                (self.emit)(Event::PreviewSticker { key, animated: false, failed: true });
            }
        }
        self.pump_preview();
    }

    pub(super) fn install_preview(&self) {
        if let Some(p) = &self.preview {
            let have = p.manifest.clone().map(|m| (m, p.bytes.iter().map(|(id, (b, _))| (*id, b.clone())).collect()));
            self.install_pack(p.id.clone(), p.key.clone(), have);
        }
    }

    /// A pack was added: previewed, it is a pack like any other now.
    pub(super) fn preview_installed(&mut self, hex: &str) {
        if self.preview.as_ref().is_some_and(|p| p.hex == hex) {
            self.preview = None;
        }
    }
}
