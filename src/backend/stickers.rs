//! Sticker packs: picker, grid thumbnails, sending, installing.

use super::*;

impl App {
    /// Installed packs come from the `ls_packs` cache. Only packs it doesn't know yet (or whose
    /// cover thumb is gone) are read from presage, one at a time: a pack row holds every sticker.
    pub(super) async fn sticker_packs(&mut self) {
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let mut installed = HashSet::new();
        let ids = self.index.pack_ids().await.unwrap_or_default();
        let mut cached: HashMap<Vec<u8>, (String, Option<u32>)> = self
            .index
            .cached_packs()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, title, cover)| (id, (title, cover)))
            .collect();
        for id in cached.keys().filter(|k| !ids.contains(k)).cloned().collect::<Vec<_>>() {
            let _ = self.index.uncache_pack(&id).await;
            cached.remove(&id);
        }
        let mut loaded = 0;
        let mut jobs = Vec::new();
        for raw in ids {
            let id = hex::encode(&raw);
            let hit = cached.get(&raw).filter(|(_, cover)| match cover {
                Some(c) => self.sticker_cover_path(&id, *c).exists(),
                None => true,
            });
            let (title, cover_id, bytes) = match hit {
                Some((title, cover)) => (title.clone(), *cover, None),
                None => {
                    let Ok(Some(p)) = self.store.sticker_pack(&raw).await else { continue };
                    loaded += 1;
                    let cover_id = p.manifest.cover.as_ref().map(|c| c.id);
                    let cover = cover_id
                        .and_then(|cid| p.manifest.stickers.iter().find(|s| s.id == cid && s.bytes.is_some()))
                        .or_else(|| p.manifest.stickers.iter().find(|s| s.bytes.is_some()));
                    let (cid, bytes) = match cover {
                        Some(s) => (Some(s.id), s.bytes.clone()),
                        None => (None, None),
                    };
                    if let Err(e) = self.index.cache_pack(&raw, &p.manifest.title, cid).await {
                        warn!("cache pack: {e}");
                    }
                    (p.manifest.title, cid, bytes)
                }
            };
            installed.insert(id.clone());
            let cover_key = match cover_id {
                Some(cid) => {
                    let base = sticker_thumb_key(&id, cid);
                    let key = format!("{base}-c");
                    jobs.push(StickerJob { base, key: key.clone(), bytes, fit: PACK_COVER });
                    key
                }
                None => String::new(),
            };
            out.push(PackDto { id, title, installed: true, cover_key });
        }
        debug!(ms = started.elapsed().as_millis() as u64, packs = out.len(), loaded, "sticker packs");
        for (id, _, title) in BLESSED_PACKS {
            if !installed.contains(*id) {
                out.push(PackDto { id: id.to_string(), title: title.to_string(), installed: false, cover_key: String::new() });
            }
        }
        (self.emit)(Event::StickerPacks(out));
        self.sticker_thumbs(jobs, false);
    }

    /// The grid comes from `ls_stickers` and the thumb files when all are cached; otherwise the
    /// pack is read from presage once (it holds every sticker's bytes) and cached.
    pub(super) async fn open_pack(&mut self, hex_id: &str) {
        let started = std::time::Instant::now();
        let Ok(id) = hex::decode(hex_id) else { return };
        let cached = self.index.cached_stickers(&id).await.unwrap_or_default();
        let all_thumbs = |me: &Self| cached.iter().all(|(sid, _)| me.sticker_cover_path(hex_id, *sid).exists());
        let mut items = Vec::new();
        let marker = anim_marker(&self.paths.thumbs, hex_id);
        let animated = if cached.is_empty() { None } else { read_marker(&marker) };
        let from_cache = animated.is_some() && all_thumbs(self);
        let mut jobs = Vec::new();
        if let (true, Some(animated)) = (from_cache, animated) {
            for (sid, emoji) in cached {
                let key = sticker_thumb_key(hex_id, sid);
                jobs.push(StickerJob { base: key.clone(), key: key.clone(), bytes: None, fit: PICKER_CELL });
                items.push(StickerDto { key, id: sid, emoji, animated: animated.contains(&sid) });
            }
        } else {
            let Ok(Some(pack)) = self.store.sticker_pack(&id).await else { return };
            let mut meta = Vec::with_capacity(pack.manifest.stickers.len());
            let mut sources = Vec::new();
            items.reserve(pack.manifest.stickers.len());
            for s in pack.manifest.stickers {
                let key = sticker_thumb_key(hex_id, s.id);
                let animated = s.bytes.as_deref().is_some_and(sigwin_thumb::is_animated);
                if let (true, Some(b)) = (animated, &s.bytes) {
                    sources.push((anim_source(&self.paths.thumbs, hex_id, s.id), b.clone()));
                }
                jobs.push(StickerJob { base: key.clone(), key: key.clone(), bytes: s.bytes, fit: PICKER_CELL });
                let emoji = s.emoji.unwrap_or_default();
                meta.push((s.id, emoji.clone()));
                items.push(StickerDto { key, id: s.id, emoji, animated });
            }
            if let Err(e) = self.index.cache_stickers(&id, &meta).await {
                warn!("cache stickers: {e}");
            }
            // hovering a picker sticker plays it: keep the animated ones' bytes next to the thumbs
            let ids: Vec<String> = items.iter().filter(|s| s.animated).map(|s| s.id.to_string()).collect();
            tokio::task::spawn_blocking(move || {
                for (path, bytes) in sources {
                    if let Err(e) = sigwin_vault::write_cache(&path, &bytes) {
                        warn!("save {}: {e}", path.display());
                        return;
                    }
                }
                if let Err(e) = sigwin_vault::write_cache(&marker, ids.join(",").as_bytes()) {
                    warn!("save {}: {e}", marker.display());
                }
            });
        }
        debug!(ms = started.elapsed().as_millis() as u64, stickers = items.len(), from_cache, "open pack");
        // grid first: the UI only keeps images for the grid it shows
        (self.emit)(Event::StickerGrid { pack: hex_id.to_string(), items });
        self.sticker_thumbs(jobs, true);
    }

    /// A message-list sticker's frames, from its downloaded attachment (`key` is its thumbnail key).
    pub(super) fn sticker_frames(&self, key: String) {
        let path = attachment_path(&self.paths.attachments, &key);
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let f = match sigwin_vault::read(&path) {
                Ok(bytes) => Frames::decode(key, &bytes, view::STICKER_SIZE, ANIM_BUDGET / 2),
                Err(_) => Frames::still(key),
            };
            debug!(ms = started.elapsed().as_millis() as u64, frames = f.frames.len(), "sticker frames");
            emit(Event::Frames(f));
        });
    }

    /// A hovered picker sticker's frames, from the bytes `open_pack` kept for animated stickers.
    pub(super) fn picker_frames(&self, pack_hex: &str, id: u32) {
        let path = anim_source(&self.paths.thumbs, pack_hex, id);
        let key = sticker_thumb_key(pack_hex, id);
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let f = match sigwin_vault::read(&path) {
                Ok(bytes) => Frames::decode(key, &bytes, PICKER_CELL, ANIM_BUDGET / 4),
                Err(_) => Frames::still(key),
            };
            emit(Event::Frames(f));
        });
    }

    pub(super) fn sticker_cover_path(&self, pack_hex: &str, id: u32) -> PathBuf {
        self.paths.thumbs.join(format!("{}-{THUMB_MAX}.enc", sticker_thumb_key(pack_hex, id)))
    }

    /// Picker images, made on both blocking threads (interleaved, so the top of the grid fills
    /// first) and sent in batches: one UI update per ~30ms instead of one per sticker.
    /// `grid`: these are a pack's grid, which supersedes the previous grid's unfinished work.
    pub(super) fn sticker_thumbs(&self, jobs: Vec<StickerJob>, grid: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let mut lanes: [Vec<StickerJob>; 2] = Default::default();
        for (i, j) in jobs.into_iter().filter(|j| j.bytes.is_some() || j.has_cache(&self.paths.thumbs)).enumerate() {
            lanes[i % 2].push(j);
        }
        let mine = if grid { self.grid_gen.fetch_add(1, Relaxed) + 1 } else { 0 };
        for lane in lanes.into_iter().filter(|l| !l.is_empty()) {
            let emit = self.emit.clone();
            let dir = self.paths.thumbs.clone();
            let cur = self.grid_gen.clone();
            tokio::task::spawn_blocking(move || {
                let mut batch = Vec::new();
                let mut last = std::time::Instant::now();
                for j in lane {
                    if grid && cur.load(Relaxed) != mine {
                        return;
                    }
                    let key = j.key.clone();
                    match j.make(&dir) {
                        Ok(t) => batch.push(t),
                        Err(e) => debug!("sticker thumb {key}: {e}"),
                    }
                    if last.elapsed() >= std::time::Duration::from_millis(30) {
                        emit(Event::StickerThumbs(std::mem::take(&mut batch)));
                        last = std::time::Instant::now();
                    }
                }
                if !batch.is_empty() {
                    emit(Event::StickerThumbs(batch));
                }
            });
        }
    }

    pub(super) async fn send_sticker(&mut self, hex_id: &str, sticker_id: u32) {
        if self.manager.is_none() {
            return;
        }
        let Ok(id) = hex::decode(hex_id) else { return };
        let Ok(Some(pack)) = self.store.sticker_pack(&id).await else { return };
        let Some(sticker) = pack.manifest.stickers.into_iter().find(|s| s.id == sticker_id) else { return };
        let Some(bytes) = sticker.bytes else {
            self.notice("스티커 데이터가 없습니다");
            return;
        };
        let content_type = sticker.content_type.filter(|c| !c.is_empty()).unwrap_or_else(|| "image/webp".into());
        let s = StickerSend { pack_id: pack.id, pack_key: pack.key, id: sticker_id, emoji: sticker.emoji, content_type, bytes };
        let ts = self.next_send_ts();
        let image_key = format!("pending-{ts}");
        let Some(thread_key) = self.current.as_ref().map(|o| o.key.clone()) else { return };
        // preview straight from the sticker bytes, no disk cache
        let bytes = s.bytes.clone();
        self.pending.push(Pending {
            ts,
            thread_key,
            text: String::new(),
            quote: String::new(),
            quote_author: String::new(),
            quote_id: String::new(),
            attachment: String::new(),
            image_key: image_key.clone(),
            link: None,
            error: None,
            after_seq: None,
            job: SendJob::Sticker(s),
        });
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let img = sigwin_thumb::decode(&bytes).map(|i| {
                sigwin_thumb::fit(sigwin_thumb::to_rgba(&i), crate::types::physical_px(view::STICKER_SIZE))
            });
            if let Ok(rgba) = img {
                let t = Thumb { key: image_key, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() };
                emit(Event::Thumb(Arc::new(t)));
            }
        });
        self.emit_messages(Scroll::Bottom).await;
        self.start_send(ts).await;
    }

    pub(super) fn install_pack(&self, id: Vec<u8>, key: Vec<u8>) {
        let Some(mut m) = self.manager.clone() else { return };
        let tx = self.tx.clone();
        self.progress("스티커 팩 내려받는 중…");
        tokio::task::spawn_local(async move {
            let res = Box::pin(m.install_sticker_pack(&id, &key)).await;
            let _ = tx.send(Internal::PackInstalled(res.map(|_| hex::encode(&id)).map_err(|e| e.to_string())));
        });
    }

    /// Installed pack: open the panel on it. Otherwise ask the user whether to install it.
    pub(super) async fn sticker_clicked(&mut self, msg_id: &str) {
        let pack_id = self.current.as_ref().and_then(|open| {
            let ts = msg_id.parse::<u64>().ok()?;
            let (_, c) = open.window.iter().find(|(t, _)| *t == ts)?;
            let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return None };
            dm.sticker.as_ref()?.pack_id.clone()
        });
        let Some(pack_id) = pack_id else { return };
        match self.store.sticker_pack(&pack_id).await {
            Ok(Some(_)) => (self.emit)(Event::OpenStickerPanel(hex::encode(&pack_id))),
            _ => (self.emit)(Event::AskInstallPack(msg_id.to_string())),
        }
    }

    pub(super) fn install_pack_from_message(&self, msg_id: &str) {
        let Some(open) = self.current.as_ref() else { return };
        let Ok(ts) = msg_id.parse::<u64>() else { return };
        let Some((_, c)) = open.window.iter().find(|(t, _)| *t == ts) else { return };
        let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return };
        let Some(s) = &dm.sticker else { return };
        if let (Some(id), Some(key)) = (s.pack_id.clone(), s.pack_key.clone()) {
            self.install_pack(id, key);
        }
    }
}

#[derive(Clone)]
pub(super) struct StickerSend {
    pack_id: Vec<u8>,
    pack_key: Vec<u8>,
    id: u32,
    emoji: Option<String>,
    content_type: String,
    bytes: Vec<u8>,
}

pub(super) async fn send_sticker_task(
    mut m: Mgr,
    thread: Thread,
    s: StickerSend,
    group_revision: u32,
    attachments_dir: PathBuf,
    ts: u64,
) -> anyhow::Result<()> {
    let spec = AttachmentSpec {
        content_type: s.content_type.clone(),
        length: s.bytes.len(),
        file_name: None,
        preview: None,
        voice_note: None,
        borderless: Some(true),
        width: None,
        height: None,
        caption: None,
        blur_hash: None,
    };
    let ptr = m
        .upload_attachments(vec![(spec, s.bytes.clone())])
        .await?
        .pop()
        .context("upload returned nothing")??;
    // keep a local copy so our own sticker renders without downloading it back
    let path = attachment_path(&attachments_dir, &view::attachment_key(&ptr, ts, 99));
    if let Err(e) = sigwin_vault::write(&path, &s.bytes) {
        warn!("save sticker: {e}");
    }
    let dm = DataMessage {
        sticker: Some(presage::proto::data_message::Sticker {
            pack_id: Some(s.pack_id),
            pack_key: Some(s.pack_key),
            sticker_id: Some(s.id),
            data: Some(ptr),
            emoji: s.emoji,
        }),
        timestamp: Some(ts),
        ..Default::default()
    };
    deliver(&mut m, thread, dm.into(), ts, group_revision).await
}

/// Logical sizes the picker shows stickers at (ui/stickers.slint: `cell - 12px`, pack tab cover).
pub(super) const PICKER_CELL: u32 = 72;

pub(super) const PACK_COVER: u32 = 32;

/// Picker thumbs used to be cached at 96px as `sp_<pack>_<id>.png`; they are `...-260.enc` now.
pub(super) fn remove_legacy_sticker_thumbs(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(STICKER_THUMB_PREFIX) && !name.contains('-') {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// One picker image: `base` names the disk cache, `key` the emitted image (a pack cover and its
/// grid cell differ in size).
pub(super) struct StickerJob {
    base: String,
    key: String,
    bytes: Option<Vec<u8>>,
    fit: u32,
}

impl StickerJob {
    pub(super) fn source(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}-{THUMB_MAX}.enc", self.base))
    }

    pub(super) fn fitted(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}-{}px.enc", self.base, crate::types::physical_px(self.fit)))
    }

    pub(super) fn has_cache(&self, dir: &Path) -> bool {
        self.fitted(dir).exists() || self.source(dir).exists()
    }

    /// The resampled result is cached too (`-<px>px.enc`): 10-25x faster than decoding the source
    /// and resampling it again, at a fraction of the memory, and only a few KB on disk.
    pub(super) fn make(self, dir: &Path) -> anyhow::Result<Thumb> {
        let fitted = self.fitted(dir);
        // older caches kept small sources at their own size
        let px = crate::types::physical_px(self.fit);
        if let Ok(rgba) = load_cached(&fitted).map_err(drop).and_then(|i| if i.width().max(i.height()) == px { Ok(i) } else { Err(()) }) {
            return Ok(Thumb { key: self.key, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() });
        }
        let source = self.source(dir);
        let bytes = self.bytes;
        let t = make_thumb_from(&self.key, || bytes.context("no sticker bytes"), THUMB_MAX, &source, self.fit)?;
        if let Some(img) = image::RgbaImage::from_raw(t.width, t.height, t.rgba.clone()) {
            save_cached(&fitted, sigwin_thumb::png_rgba(&img));
        }
        Ok(t)
    }
}

pub(super) fn sticker_thumb_key(pack_hex: &str, id: u32) -> String {
    format!("{STICKER_THUMB_PREFIX}{pack_hex}_{id}")
}

/// Which of a pack's stickers are animated (comma-separated ids); written by `open_pack`.
fn anim_marker(dir: &Path, pack_hex: &str) -> PathBuf {
    dir.join(format!("{STICKER_THUMB_PREFIX}{pack_hex}-anim.enc"))
}

/// An animated picker sticker's original bytes.
fn anim_source(dir: &Path, pack_hex: &str, id: u32) -> PathBuf {
    dir.join(format!("{}-anim.enc", sticker_thumb_key(pack_hex, id)))
}

fn read_marker(path: &Path) -> Option<HashSet<u32>> {
    let bytes = sigwin_vault::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).split(',').filter_map(|s| s.parse().ok()).collect())
}

/// Signal's built-in packs (Signal-Desktop `BLESSED_PACKS`): hex id, base64 key, title.
pub(super) const BLESSED_PACKS: &[(&str, &str, &str)] = &[
    ("42fb75e1827c0c945cfb5ca0975db03c", "7uJ+K593PgpV6iTDQLe+hYcRpuK9m27nBEND4OQovmU=", "Rocky Talk"),
    ("ccc89a05dc077856b57351e90697976c", "RXMOYPCdVWYRUiN0RTemt9nqmc7qy3eh+9aAG5YH+88=", "My Daily Life 1"),
    ("fb535407d2f6497ec074df8b9c51dd1d", "F+lxwTQDViJ4HS7iSeZHO3dFg3ULaMEbuCt1CcaLbf0=", "Zozo the French Bulldog"),
    ("3044281a51307306e5442f2e9070953a", "xMqqhDl+GmMKWWD1SguCdTyIpeUuDe/mFbpN2A8TDL8=", "Croco's Feelings"),
    ("a2414255948558316f37c1d36c64cd28", "/aEpNxltI28cqeEZalZULh0c72/4TivgOChxf6IK02Y=", "My Daily Life 2"),
    ("684d2b7bcfc2eec6f57f2e7be0078e0f", "hm4Ny0obJfKwTfJwzXQnI+SmVVwKGrw/PzDcxaIBDFU=", "Cozy Season"),
    ("f19548e5afa38d1ce4f5c3191eba5e30", "LLMHZ0D2aapExsBjKQskmn0ApLAu2PnppbkCo38bvEE=", "Chug the Mouse"),
    ("9acc9e8aba563d26a4994e69263e3b25", "Wm3/OUjCjvubeq+T7MN1xp/DFueAd+0mhnoU0QoPahI=", "Bandit the Cat"),
];

pub(super) fn blessed_key(hex_id: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let (_, b64, _) = BLESSED_PACKS.iter().find(|(id, _, _)| *id == hex_id)?;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

/// Accepts `https://signal.art/addstickers/#pack_id=<hex>&pack_key=<hex>` (or a bare query string).
pub(super) fn parse_pack_url(url: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut id = None;
    let mut key = None;
    for part in url.split(['#', '&', '?']) {
        if let Some(v) = part.trim().strip_prefix("pack_id=") {
            id = hex::decode(v.trim()).ok();
        } else if let Some(v) = part.trim().strip_prefix("pack_key=") {
            key = hex::decode(v.trim()).ok();
        }
    }
    Some((id?, key?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blessed_keys_are_32_bytes() {
        for (id, _, title) in BLESSED_PACKS {
            assert_eq!(hex::decode(id).unwrap().len(), 16, "{title} id");
            assert_eq!(blessed_key(id).unwrap().len(), 32, "{title} key");
        }
    }

    #[test]
    fn parses_signal_art_links() {
        let (id, key) = parse_pack_url(
            "https://signal.art/addstickers/#pack_id=9acc9e8aba563d26a4994e69263e3b25&pack_key=5a6dff3948c28efb9b7aaf93ecc375c69fc316e78077ed26867a14d10a0f6a12",
        )
        .unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(key.len(), 32);
        assert!(parse_pack_url("https://signal.art/addstickers/").is_none());
        assert!(parse_pack_url("pack_id=zz&pack_key=00").is_none());
    }
}
