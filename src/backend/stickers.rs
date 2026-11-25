//! Sticker packs: picker, grid thumbnails, sending, installing.

use presage::proto::sync_message::{sticker_pack_operation, StickerPackOperation};
use presage::store::{StickerPack, StickerPackManifest};

use super::*;
use crate::types::RECENT_TAB;

impl App {
    /// Installed packs come from the `ls_packs` cache. Only packs it doesn't know yet (or whose
    /// cover thumb is gone) are read from presage, one at a time: a pack row holds every sticker.
    pub(super) async fn sticker_packs(&mut self) {
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let ids = self.index.pack_ids().await.unwrap_or_default();
        let mut cached: HashMap<Vec<u8>, ((String, String), Option<u32>)> = self
            .index
            .cached_packs()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, names, cover)| (id, (names, cover)))
            .collect();
        // packs removed since, here or on another device: their files go too
        let gone: Vec<Vec<u8>> = cached.keys().filter(|k| !ids.contains(k)).cloned().collect();
        for id in &gone {
            let _ = self.index.uncache_pack(id).await;
            cached.remove(id);
        }
        if !gone.is_empty() {
            let dir = self.paths.thumbs.clone();
            tokio::task::spawn_blocking(move || remove_pack_files(&dir, &gone));
        }
        let mut loaded = 0;
        let mut jobs = Vec::new();
        for raw in ids {
            let id = hex::encode(&raw);
            let hit = cached.get(&raw).filter(|(_, cover)| match cover {
                Some(c) => self.sticker_cover_path(&id, *c).exists(),
                None => true,
            });
            let ((title, author), cover_id, bytes) = match hit {
                Some((names, cover)) => (names.clone(), *cover, None),
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
                    if let Err(e) = self.index.cache_pack(&raw, &p.manifest.title, &p.manifest.author, cid).await {
                        warn!("cache pack: {e}");
                    }
                    ((p.manifest.title, p.manifest.author), cid, bytes)
                }
            };
            let cover_key = match cover_id {
                Some(cid) => {
                    let base = sticker_thumb_key(&id, cid);
                    let key = cover_key(&base);
                    let small = crate::types::small_cover_key(&key);
                    jobs.push(StickerJob { base: base.clone(), key: small, bytes: bytes.clone(), fit: PACK_COVER_SMALL });
                    jobs.push(StickerJob { base, key: key.clone(), bytes, fit: PACK_COVER });
                    key
                }
                None => String::new(),
            };
            out.push(PackDto { id, title, author, cover_key });
        }
        let recent = !self.recent_grid().await.0.is_empty();
        // made here and not installed (an installed one is among the packs above)
        let installed: HashSet<&str> = out.iter().map(|p| p.id.as_str()).collect();
        let mut made = Vec::new();
        let mut covers = Vec::new();
        for p in self.index.made_packs().await.unwrap_or_default() {
            let id = hex::encode(&p.id);
            if installed.contains(id.as_str()) {
                continue;
            }
            let cover_key = match p.cover {
                Some(png) => {
                    let key = format!("{MADE_COVER_PREFIX}{id}");
                    covers.push((key.clone(), png));
                    key
                }
                None => String::new(),
            };
            made.push(PackDto { id, title: p.title, author: p.author, cover_key });
        }
        // a pack that is also installed or made here shows there
        let listed: HashSet<String> = out.iter().chain(&made).map(|p| p.id.clone()).collect();
        let mut shared = Vec::new();
        // a new list asks again for what failed or came without its cover
        self.shared_fetch.failed.clear();
        self.shared_fetch.done.clear();
        let (found, gone) = self.index.shared_packs().await.unwrap_or_default();
        // no message holds them any more: their covers go too, unless the pack is installed
        let gone: Vec<Vec<u8>> = gone.into_iter().filter(|id| !installed.contains(hex::encode(id).as_str())).collect();
        if !gone.is_empty() {
            let dir = self.paths.thumbs.clone();
            tokio::task::spawn_blocking(move || remove_pack_files(&dir, &gone));
        }
        for p in found {
            let id = hex::encode(&p.id);
            if listed.contains(&id) {
                continue;
            }
            let (title, author, cover_key) = match (p.names, p.cover) {
                (Some((title, author)), None) => (title, author, String::new()),
                (Some((title, author)), Some(cid)) if self.sticker_cover_path(&id, cid).exists() => {
                    let base = sticker_thumb_key(&id, cid);
                    let key = cover_key(&base);
                    jobs.push(StickerJob { base, key: key.clone(), bytes: None, fit: PACK_COVER });
                    (title, author, key)
                }
                // not fetched, or its cover thumb is gone (the cache was cleared): fetched with it
                _ => Default::default(),
            };
            shared.push(PackDto { id, title, author, cover_key });
        }
        let shared_partial = !self.persist.sticker_msgs_indexed;
        debug!(
            ms = started.elapsed().as_millis() as u64,
            packs = out.len(),
            made = made.len(),
            shared = shared.len(),
            loaded,
            recent,
            "sticker packs"
        );
        (self.emit)(Event::StickerPacks { packs: out, made, shared, shared_partial, recent });
        self.sticker_thumbs(jobs, false);
        if !covers.is_empty() {
            let emit = self.emit.clone();
            tokio::task::spawn_blocking(move || {
                emit(Event::StickerThumbs(covers.into_iter().filter_map(|(key, png)| still_thumb(key, &png, PACK_COVER)).collect()));
            });
        }
    }

    /// A pack's key: previewed, installed, made here, or carried by a sticker someone sent.
    async fn any_pack_key(&self, id: &[u8]) -> Option<Vec<u8>> {
        if let Some(k) = self.preview.as_ref().and_then(|p| p.key_of(id)) {
            return Some(k);
        }
        if let Ok(Some(k)) = self.index.pack_key(id).await {
            return Some(k);
        }
        if let Ok(Some(k)) = self.index.made_pack_key(id).await {
            return Some(k);
        }
        self.index.shared_pack_key(id).await.ok().flatten()
    }

    /// Puts a pack's signal.art link on the clipboard.
    pub(super) async fn copy_pack_link(&self, hex_id: &str) {
        let Ok(id) = hex::decode(hex_id) else { return };
        let Some(key) = self.any_pack_key(&id).await else { return };
        let link = crate::stickerpack::link(hex_id, &key);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Internal::Notice(match crate::clip::set_text(&link) {
                Ok(()) => tr!("Link copied.").into(),
                Err(e) => tr!("Copy failed: {}", format!("{e:#}")),
            }));
        });
    }

    /// A pack made here or shared with us, before it is installed: its preview.
    pub(super) async fn open_unadded_pack(&mut self, hex_id: &str) {
        let Ok(id) = hex::decode(hex_id) else { return };
        let Some(key) = self.any_pack_key(&id).await else { return };
        self.preview_pack(id, Some(key), None).await;
    }

    /// The pack page shows these shared packs untitled: they are fetched, a few at a time, in
    /// this order after what's under way. Only for rows on screen: each is a manifest and a
    /// cover from the sticker CDN.
    pub(super) async fn shared_want(&mut self, ids: Vec<String>) {
        let f = &mut self.shared_fetch;
        f.want = ids
            .iter()
            .filter_map(|h| hex::decode(h).ok())
            .filter(|id| !f.running.contains(id) && !f.failed.contains(id) && !f.done.contains(id))
            .collect();
        self.pump_shared().await;
    }

    async fn pump_shared(&mut self) {
        let Some(m) = self.manager.clone() else { return };
        while self.shared_fetch.running.len() < super::packpreview::PARALLEL {
            let Some(id) = self.shared_fetch.want.pop_front() else { break };
            let Ok(Some(key)) = self.index.shared_pack_key(&id).await else { continue };
            self.shared_fetch.running.insert(id.clone());
            let (m, tx) = (m.clone(), self.tx.clone());
            tokio::task::spawn_local(async move {
                let res = async {
                    let manifest = m.sticker_pack_manifest(&id, &key).await.map_err(|e| e.to_string())?;
                    let cover_id = manifest.cover.as_ref().map(|c| c.id).or_else(|| manifest.stickers.first().map(|s| s.id));
                    // without its cover the pack still has its title
                    let bytes = match cover_id {
                        Some(cid) => m.download_sticker(&id, &key, cid).await.inspect_err(|e| warn!("shared pack cover: {e}")).ok(),
                        None => None,
                    };
                    Ok(SharedFetched { title: manifest.title, author: manifest.author, cover_id, bytes })
                }
                .await;
                let _ = tx.send(Internal::SharedFetched(id, res));
            });
        }
    }

    pub(super) async fn shared_fetched(&mut self, id: Vec<u8>, res: Result<SharedFetched, String>) {
        self.shared_fetch.running.remove(&id);
        match res {
            Ok(f) => {
                self.shared_fetch.done.insert(id.clone());
                // one whose cover didn't come is fetched again with the next pack list
                if f.cover_id.is_none() || f.bytes.is_some() {
                    if let Err(e) = self.index.cache_shared_pack(&id, &f.title, &f.author, f.cover_id).await {
                        warn!("cache shared pack: {e}");
                    }
                }
                let hex = hex::encode(&id);
                let cover_key = match f.cover_id.zip(f.bytes) {
                    Some((cid, bytes)) => {
                        let base = sticker_thumb_key(&hex, cid);
                        let key = cover_key(&base);
                        self.sticker_thumbs(vec![StickerJob { base, key: key.clone(), bytes: Some(bytes), fit: PACK_COVER }], false);
                        key
                    }
                    None => String::new(),
                };
                (self.emit)(Event::SharedPack(PackDto { id: hex, title: f.title, author: f.author, cover_key }));
            }
            Err(e) => {
                warn!("shared pack manifest: {e}");
                self.shared_fetch.failed.insert(id);
            }
        }
        self.pump_shared().await;
    }

    /// Its link is gone for good; answered by a new `StickerPacks`.
    pub(super) async fn forget_made_pack(&mut self, hex_id: &str) {
        let Ok(id) = hex::decode(hex_id) else { return };
        if let Err(e) = self.index.forget_made_pack(&id).await {
            warn!("forget made pack: {e}");
        }
        self.sticker_packs().await;
    }

    /// The grid comes from `ls_stickers` and the thumb files when all are cached; otherwise the
    /// pack is read from presage once (it holds every sticker's bytes) and cached.
    pub(super) async fn open_pack(&mut self, hex_id: &str) {
        if self.preview.as_ref().is_some_and(|p| p.hex == hex_id) {
            self.preview_grid();
            return;
        }
        if hex_id == RECENT_TAB {
            let (items, jobs) = self.recent_grid().await;
            (self.emit)(Event::StickerGrid { pack: RECENT_TAB.to_string(), items });
            self.sticker_thumbs(jobs, true);
            return;
        }
        let started = std::time::Instant::now();
        let Ok(id) = hex::decode(hex_id) else { return };
        let cached = self.index.cached_stickers(&id).await.unwrap_or_default();
        let all_thumbs = |me: &Self| cached.iter().all(|(sid, _)| me.sticker_cover_path(hex_id, *sid).exists());
        let mut items = Vec::new();
        let marker = anim_marker(&self.paths.thumbs, hex_id);
        let animated = if cached.is_empty() { None } else { read_marker(&marker) };
        let from_cache = animated.is_some() && all_thumbs(self);
        let mut jobs = Vec::new();
        let mut saved = None;
        if let (true, Some(animated)) = (from_cache, animated) {
            for (sid, emoji) in cached {
                let key = sticker_thumb_key(hex_id, sid);
                jobs.push(StickerJob { base: key.clone(), key: key.clone(), bytes: None, fit: PICKER_CELL });
                items.push(StickerDto { key, pack: hex_id.to_string(), id: sid, emoji, animated: animated.contains(&sid) });
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
                items.push(StickerDto { key, pack: hex_id.to_string(), id: s.id, emoji, animated });
            }
            if let Err(e) = self.index.cache_stickers(&id, &meta).await {
                warn!("cache stickers: {e}");
            }
            // the picker plays them from the animated ones' bytes kept next to the thumbs
            let ids: Vec<String> = items.iter().filter(|s| s.animated).map(|s| s.id.to_string()).collect();
            saved = Some(tokio::task::spawn_blocking(move || {
                for (path, bytes) in sources {
                    if let Err(e) = sigwin_vault::write_cache(&path, &bytes) {
                        warn!("save {}: {e}", path.display());
                        return;
                    }
                }
                if let Err(e) = sigwin_vault::write_cache(&marker, ids.join(",").as_bytes()) {
                    warn!("save {}: {e}", marker.display());
                }
            }));
        }
        debug!(ms = started.elapsed().as_millis() as u64, stickers = items.len(), from_cache, "open pack");
        // grid first: the UI only keeps images for the grid it shows
        (self.emit)(Event::StickerGrid { pack: hex_id.to_string(), items });
        self.sticker_thumbs(jobs, true);
        // the grid asks for its frames as it shows: commands wait for this, so they find the files
        if let Some(saved) = saved {
            let _ = saved.await;
        }
    }

    /// The recently used tab's grid. Its stickers were sent from the picker, so their packs'
    /// thumbs and animation markers are cached; one whose thumb is gone anyway is left out
    /// rather than reading its whole pack from presage.
    async fn recent_grid(&self) -> (Vec<StickerDto>, Vec<StickerJob>) {
        let rows = self.index.recent_stickers().await.unwrap_or_default();
        let mut markers: HashMap<String, Option<HashSet<u32>>> = HashMap::new();
        let mut items = Vec::with_capacity(rows.len());
        let mut jobs = Vec::with_capacity(rows.len());
        for (raw, id, emoji) in rows {
            let pack = hex::encode(&raw);
            let key = sticker_thumb_key(&pack, id);
            let job = StickerJob { base: key.clone(), key: key.clone(), bytes: None, fit: PICKER_CELL };
            if !job.has_cache(&self.paths.thumbs) {
                continue;
            }
            let marker = markers.entry(pack.clone()).or_insert_with(|| read_marker(&anim_marker(&self.paths.thumbs, &pack)));
            let animated = marker.as_ref().is_some_and(|m| m.contains(&id));
            jobs.push(job);
            items.push(StickerDto { key, pack, id, emoji, animated });
        }
        (items, jobs)
    }

    /// A message-list sticker's frames (`key` is its thumbnail key).
    pub(super) fn sticker_frames(&self, key: String) {
        let source = self.sticker_source(&key);
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let f = match source.read() {
                Some(bytes) => Frames::decode(key, &bytes, view::media::sticker_size()),
                None => Frames::still(key),
            };
            debug!(ms = started.elapsed().as_millis() as u64, kb = f.png.len() / 1024, "sticker frames");
            emit(Event::Frames(f));
        });
    }

    /// Whether a message-list sticker is animated, for its mark while only the hovered one plays.
    pub(super) fn sticker_animated(&self, key: String) {
        let source = self.sticker_source(&key);
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            if source.read().is_some_and(|bytes| sigwin_thumb::is_animated(&bytes)) {
                emit(Event::StickerAnimated(key));
            }
        });
    }

    /// A sticker still being sent has no attachment file yet: its bytes are in the pending send.
    fn sticker_source(&self, key: &str) -> StickerSource {
        let sending = self.pending.iter().find(|p| p.image_key == key).and_then(|p| match &p.job {
            SendJob::Sticker(s) => Some(s.bytes.clone()),
            SendJob::Text { .. } => None,
        });
        match sending {
            Some(bytes) => StickerSource::Sending(bytes),
            None => StickerSource::File(attachment_path(&self.paths.attachments, key)),
        }
    }

    /// Picker stickers' frames, from the bytes `open_pack` kept for animated stickers. One at a
    /// time on one blocking thread, in the order asked: a screenful asks for tens at once, each
    /// tens of ms, and the grid's thumbnails come first. A new list replaces what's left of the old.
    pub(super) fn picker_frames(&self, items: Vec<(String, u32)>) {
        let mut q = self.picker_queue.lock().unwrap_or_else(|e| e.into_inner());
        // the one being made comes anyway
        let busy = q.busy.clone();
        q.todo = items
            .into_iter()
            .filter(|j| Some(j) != busy.as_ref())
            .map(|j| {
                // the previewed pack's stickers are only in memory
                let held = self.preview.as_ref().filter(|p| p.hex == j.0).and_then(|p| p.bytes.get(&j.1)).map(|(b, _)| b.clone());
                (j, held)
            })
            .collect();
        if q.running || q.todo.is_empty() {
            return;
        }
        q.running = true;
        let queue = self.picker_queue.clone();
        let dir = self.paths.thumbs.clone();
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || loop {
            let ((pack, id), held) = {
                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                let Some(j) = q.todo.pop_front() else {
                    q.running = false;
                    q.busy = None;
                    return;
                };
                q.busy = Some(j.0.clone());
                j
            };
            let key = sticker_thumb_key(&pack, id);
            let f = match held {
                Some(bytes) => Frames::decode(key, &bytes, PICKER_CELL),
                None => match sigwin_vault::read(&anim_source(&dir, &pack, id)) {
                    Ok(bytes) => Frames::decode(key, &bytes, PICKER_CELL),
                    Err(_) => Frames::still(key),
                },
            };
            emit(Event::Frames(f));
        });
    }

    /// The sticker the picker shows big, at its size in chats: the still first, then the frames
    /// of an animated one unless another was asked for meanwhile. One blocking thread works
    /// through only the newest asked, as the pointer may cross many cells.
    pub(super) fn peek_sticker(&self, pack: String, id: u32, animated: bool) {
        // the previewed pack's stickers are only in memory
        let held = self.preview.as_ref().filter(|p| p.hex == pack).and_then(|p| p.bytes.get(&id)).map(|(b, _)| b.clone());
        let mut q = self.peek_queue.lock().unwrap_or_else(|e| e.into_inner());
        q.next = Some(PeekJob { pack, id, animated, held });
        if q.running {
            return;
        }
        q.running = true;
        let queue = self.peek_queue.clone();
        let dir = self.paths.thumbs.clone();
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || loop {
            let Some(j) = ({
                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                q.running = q.next.is_some();
                q.next.take()
            }) else {
                return;
            };
            let key = sticker_thumb_key(&j.pack, j.id);
            let fit = view::media::sticker_size();
            let still = match &j.held {
                Some(b) => still_thumb(key.clone(), b, fit),
                // cached like the grid's, so resting on it again is quick
                None => StickerJob { base: key.clone(), key: key.clone(), bytes: None, fit }.make(&dir).ok(),
            };
            emit(Event::PeekStill(still.unwrap_or(Thumb { key: key.clone(), width: 0, height: 0, rgba: Vec::new() })));
            if !j.animated || queue.lock().unwrap_or_else(|e| e.into_inner()).next.is_some() {
                continue;
            }
            let f = match &j.held {
                Some(b) => Frames::decode(key, b, fit),
                None => match sigwin_vault::read(&anim_source(&dir, &j.pack, j.id)) {
                    Ok(bytes) => Frames::decode(key, &bytes, fit),
                    Err(_) => continue,
                },
            };
            if !f.png.is_empty() {
                emit(Event::PeekFrames(f));
            }
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
            self.notice(tr!("No sticker data"));
            return;
        };
        let content_type = sticker.content_type.filter(|c| !c.is_empty()).unwrap_or_else(|| "image/webp".into());
        let s = StickerSend { pack_id: pack.id, pack_key: pack.key, id: sticker_id, emoji: sticker.emoji, content_type, bytes };
        let ts = self.next_send_ts();
        let image_key = format!("pending-{ts}");
        let Some(thread_key) = self.current.as_ref().map(|o| o.key.clone()) else { return };
        if let Err(e) = self.index.use_sticker(&id, sticker_id, ts, RECENT_STICKERS).await {
            warn!("recent sticker: {e}");
        }
        // preview straight from the sticker bytes, no disk cache
        let bytes = s.bytes.clone();
        let emit = self.emit.clone();
        let key = image_key.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(t) = still_thumb(key, &bytes, view::media::sticker_size()) {
                emit(Event::Thumb(Arc::new(t)));
            }
        });
        self.queue_send(Pending {
            ts,
            thread_key,
            text: String::new(),
            quote: String::new(),
            quote_author: String::new(),
            quote_id: String::new(),
            attachment: String::new(),
            image_key,
            photos: Vec::new(),
            link: None,
            error: None,
            canceled: false,
            after_seq: None,
            job: SendJob::Sticker(s),
        })
        .await;
    }

    /// `have`: the pack's manifest and the stickers already downloaded (a preview's), if any.
    pub(super) fn install_pack(&self, id: Vec<u8>, key: Vec<u8>, have: Option<(StickerPackManifest, HashMap<u32, Arc<[u8]>>)>) {
        let Some(mut m) = self.manager.clone() else { return };
        let mut store = self.store.clone();
        let (tx, emit) = (self.tx.clone(), self.emit.clone());
        tokio::task::spawn_local(async move {
            let hex = hex::encode(&id);
            let progress = |done, total| emit(Event::PackAdding { pack: hex.clone(), done, total });
            let res = Box::pin(add_pack(&mut m, &mut store, &id, &key, have, progress)).await;
            let _ = tx.send(Internal::PackInstalled(hex, res));
        });
    }

    pub(super) async fn forget_recent_sticker(&self, hex_id: &str, sticker_id: u32) {
        let Ok(id) = hex::decode(hex_id) else { return };
        if let Err(e) = self.index.forget_recent_sticker(&id, sticker_id).await {
            warn!("forget recent sticker: {e}");
        }
    }

    pub(super) async fn set_pack_order(&self, ids: &[String]) {
        let ids: Vec<Vec<u8>> = ids.iter().filter_map(|id| hex::decode(id).ok()).collect();
        if let Err(e) = self.index.set_pack_order(&ids).await {
            warn!("pack order: {e}");
        }
    }

    /// The other devices hear of it first (presage removes it from the store after sending);
    /// the reloaded list then drops its caches and files.
    pub(super) async fn remove_pack(&self, hex_id: &str) {
        let Some(mut m) = self.manager.clone() else { return };
        let Ok(id) = hex::decode(hex_id) else { return };
        let Ok(Some(key)) = self.index.pack_key(&id).await else { return };
        let tx = self.tx.clone();
        self.progress(tr!("Removing sticker pack…"));
        tokio::task::spawn_local(async move {
            let res = Box::pin(m.remove_sticker_pack(&id, &key)).await;
            let _ = tx.send(Internal::PackRemoved(res.map_err(|e| e.to_string())));
        });
    }

    /// The panel opens on the sticker's pack: installed, on that sticker; otherwise a preview.
    pub(super) async fn sticker_clicked(&mut self, msg_id: &str) {
        let pack = self.current.as_ref().and_then(|open| {
            let ts = msg_id.parse::<u64>().ok()?;
            let (_, c) = open.window.iter().find(|(t, _)| *t == ts)?;
            let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return None };
            let s = dm.sticker.as_ref()?;
            Some((s.pack_id.clone()?, s.pack_key.clone(), s.sticker_id))
        });
        let Some((id, key, sticker)) = pack else { return };
        self.preview_pack(id, key, sticker).await;
    }
}

/// What presage's `install_sticker_pack` does, with progress (`done`, `total` stickers) and several
/// downloads at once: every
/// sticker stored, then the other devices told. Store the pack whole or not at all: an added pack is
/// never downloaded again, so a missing sticker would stay missing.
async fn add_pack(
    m: &mut Manager<SqliteStore, Registered>,
    store: &mut SqliteStore,
    id: &[u8],
    key: &[u8],
    have: Option<(StickerPackManifest, HashMap<u32, Arc<[u8]>>)>,
    progress: impl Fn(usize, usize),
) -> Result<(), String> {
    let (mut manifest, held) = match have {
        Some(h) => h,
        None => (m.sticker_pack_manifest(id, key).await.map_err(|e| e.to_string())?, HashMap::new()),
    };
    let total = manifest.stickers.len();
    let mut missing = Vec::new();
    for s in &mut manifest.stickers {
        match held.get(&s.id) {
            Some(b) => s.bytes = Some(b.to_vec()),
            None => missing.push(s.id),
        }
    }
    drop(held);
    let mut done = total - missing.len();
    progress(done, total);
    let mut got = HashMap::with_capacity(missing.len());
    {
        let m = &*m;
        let mut downloads = futures::stream::iter(missing)
            .map(|sid| async move { (sid, m.download_sticker(id, key, sid).await) })
            .buffer_unordered(super::packpreview::PARALLEL);
        while let Some((sid, res)) = downloads.next().await {
            got.insert(sid, res.map_err(|e| e.to_string())?);
            done += 1;
            progress(done, total);
        }
    }
    for s in &mut manifest.stickers {
        if let Some(b) = got.remove(&s.id) {
            s.bytes = Some(b);
        }
    }
    let pack = StickerPack { id: id.to_vec(), key: key.to_vec(), manifest };
    store.add_sticker_pack(&pack).await.map_err(|e| e.to_string())?;
    drop(pack);
    let op = StickerPackOperation {
        pack_id: Some(id.to_vec()),
        pack_key: Some(key.to_vec()),
        r#type: Some(sticker_pack_operation::Type::Install as i32),
    };
    let sync = SyncMessage { sticker_pack_operation: vec![op], ..Default::default() };
    let me = m.registration_data().service_ids.aci();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    Box::pin(m.send_message(me, sync, ts)).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// A sticker resampled to `logical` px from its bytes, with no disk cache.
pub(super) fn still_thumb(key: String, bytes: &[u8], logical: u32) -> Option<Thumb> {
    let i = sigwin_thumb::decode(bytes).ok()?;
    let rgba = sigwin_thumb::fit(sigwin_thumb::to_rgba(&i), crate::types::physical_px(logical));
    Some(Thumb { key, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

/// The thumbnail key of a pack's cover in its tab, from its sticker's key.
pub(super) fn cover_key(sticker_key: &str) -> String {
    format!("{sticker_key}-c")
}

enum StickerSource {
    File(PathBuf),
    Sending(Vec<u8>),
}

impl StickerSource {
    fn read(self) -> Option<Vec<u8>> {
        match self {
            StickerSource::File(path) => sigwin_vault::read(&path).ok(),
            StickerSource::Sending(bytes) => Some(bytes),
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
    let path = attachment_path(&attachments_dir, &view::media::attachment_key(&ptr, ts, 99));
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

/// Logical sizes the picker shows stickers at (ui/stickers.slint: `art`, `PackCover`
/// and its size in a selected tab).
pub(super) const PICKER_CELL: u32 = 80;

pub(super) const PACK_COVER: u32 = 32;

pub(super) const PACK_COVER_SMALL: u32 = 28;

/// Thumbnail keys of made packs' covers: drawn from `ls_made_packs`, never cached as files.
const MADE_COVER_PREFIX: &str = "mp_";

/// How many the recently used tab shows (Signal Desktop's count).
const RECENT_STICKERS: usize = 24;

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
        let target = Target::Fit(px);
        let t = target.draw(&self.key, thumb_source(|| bytes.context("no sticker bytes"), THUMB_MAX, &source, target, None)?);
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

/// `App::picker_frames`' work: (pack, sticker id) to make, the one being made, whether a thread runs.
#[derive(Default)]
pub(super) struct PickerQueue {
    /// with the bytes when they are held in memory (a previewed pack)
    todo: std::collections::VecDeque<((String, u32), Option<Arc<[u8]>>)>,
    busy: Option<(String, u32)>,
    running: bool,
}

/// `App::shared_want`'s work: packs to fetch (raw ids) in order, those under way, those that
/// failed or came since the pack list was last made (not asked for again until the next).
#[derive(Default)]
pub(super) struct SharedFetch {
    want: std::collections::VecDeque<Vec<u8>>,
    running: HashSet<Vec<u8>>,
    failed: HashSet<Vec<u8>>,
    done: HashSet<Vec<u8>>,
}

/// A shared pack's manifest, and its cover sticker's bytes if they came.
pub(super) struct SharedFetched {
    title: String,
    author: String,
    cover_id: Option<u32>,
    bytes: Option<Vec<u8>>,
}

/// `App::peek_sticker`'s work: the newest sticker asked for, whether a thread runs.
#[derive(Default)]
pub(super) struct PeekQueue {
    next: Option<PeekJob>,
    running: bool,
}

struct PeekJob {
    pack: String,
    id: u32,
    animated: bool,
    /// a previewed pack's sticker, held in memory
    held: Option<Arc<[u8]>>,
}

/// Every picker file of these packs: thumbs, covers, animated sources and the marker.
fn remove_pack_files(dir: &Path, packs: &[Vec<u8>]) {
    let prefixes: Vec<String> = packs.iter().map(|id| format!("{STICKER_THUMB_PREFIX}{}", hex::encode(id))).collect();
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut removed = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let ours = prefixes.iter().any(|p| name.strip_prefix(p.as_str()).is_some_and(|rest| rest.starts_with(['_', '-'])));
        if ours && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    debug!(packs = packs.len(), removed, "removed pack files");
}

/// An animated picker sticker's original bytes.
fn anim_source(dir: &Path, pack_hex: &str, id: u32) -> PathBuf {
    dir.join(format!("{}-anim.enc", sticker_thumb_key(pack_hex, id)))
}

fn read_marker(path: &Path) -> Option<HashSet<u32>> {
    let bytes = sigwin_vault::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).split(',').filter_map(|s| s.parse().ok()).collect())
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
