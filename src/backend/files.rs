//! Attachment and thumbnail caches (encrypted on disk), opening attachments.

use super::*;

/// How a message's thumbnail is drawn, in logical px.
pub(super) enum ThumbShape {
    /// the longer side is this long
    Fit(u32),
    /// fills this size, cut off around its center
    Cover(u32, u32),
}

impl App {
    /// `key` is what the UI knows the result by, `file_key` the attachment's files on disk.
    pub(super) fn request_thumb(&self, key: String, file_key: String, ptr: AttachmentPointer, shape: ThumbShape) {
        let thumb_path = self.paths.thumbs.join(format!("{file_key}.enc"));
        let orig_path = attachment_path(&self.paths.attachments, &file_key);
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        let gate = self.thumb_gate.clone();
        tokio::task::spawn_local(async move {
            let mut labelled = false;
            if !thumb_path.exists() {
                if !orig_path.exists() {
                    labelled = true;
                    emit(Event::MediaLabel { key: key.clone(), label: view::undownloaded_label(&ptr) });
                    let Some(m) = manager else { return };
                    match m.get_attachment(&ptr).await {
                        Ok(data) => {
                            if let Err(e) = sigwin_vault::write(&orig_path, &data) {
                                warn!("save attachment: {e}");
                            }
                        }
                        Err(e) => {
                            warn!("download attachment: {e}");
                            let _ = tx.send(Internal::ThumbFailed(key));
                            return;
                        }
                    }
                }
            }
            let k = key.clone();
            // one at a time: an album would otherwise decode all its full-size originals at once
            let permit = gate.acquire_owned().await;
            let res = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                make_shaped_thumb(&k, &orig_path, &thumb_path, shape)
            })
            .await;
            if labelled {
                emit(Event::MediaLabel { key: key.clone(), label: Default::default() });
            }
            match res {
                Ok(Ok(t)) => emit(Event::Thumb(Arc::new(t))),
                _ => {
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
            }
        });
    }

    /// A video's picture, cut to `size` (logical px): its blurhash right away, its first frame once
    /// that can be had. Videos are downloaded one at a time, newest first, and only while the row
    /// is still in the window.
    pub(super) fn request_video_thumb(&self, key: String, file_key: String, ptr: AttachmentPointer, size: (u32, u32)) {
        let thumb_path = self.paths.thumbs.join(format!("{file_key}.enc"));
        let no_frame = no_frame_path(&self.paths.thumbs, &file_key);
        let length_path = length_path(&self.paths.thumbs, &file_key);
        let orig_path = attachment_path(&self.paths.attachments, &file_key);
        let (w, h) = (crate::types::physical_px(size.0), crate::types::physical_px(size.1));
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        let gate = self.thumb_gate.clone();
        let fetch_gate = self.video_gate.clone();
        let requested = self.thumbs_requested.clone();
        tokio::task::spawn_local(async move {
            if !thumb_path.exists() {
                if let Some(t) = ptr.blur_hash.as_deref().and_then(|b| blur_thumb(&key, b, w, h)) {
                    emit(Event::Thumb(Arc::new(t)));
                }
                if !orig_path.exists() {
                    emit(Event::MediaLabel { key: key.clone(), label: view::undownloaded_label(&ptr) });
                }
                if no_frame.exists() {
                    return;
                }
                if !orig_path.exists() {
                    if !view::auto_fetches_video(&ptr) {
                        return;
                    }
                    let _turn = fetch_gate.acquire_owned().await;
                    if !requested.borrow().contains(&key) {
                        return;
                    }
                    if !orig_path.exists() {
                        let Some(m) = manager else { return };
                        let data = match m.get_attachment(&ptr).await {
                            Ok(data) => data,
                            Err(e) => {
                                warn!("download video: {e}");
                                let _ = tx.send(Internal::ThumbFailed(key));
                                return;
                            }
                        };
                        let path = orig_path.clone();
                        let saved = tokio::task::spawn_blocking(move || sigwin_vault::write(&path, &data)).await;
                        if !matches!(saved, Ok(Ok(()))) {
                            warn!("save video failed");
                            let _ = tx.send(Internal::ThumbFailed(key));
                            return;
                        }
                    }
                }
            }
            let permit = gate.acquire_owned().await;
            let k = key.clone();
            let emit_done = emit.clone();
            let res = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let t = make_video_thumb(&k, &orig_path, &thumb_path, &length_path, &no_frame, w, h)?;
                emit_done(Event::Thumb(Arc::new(t)));
                // the picture doesn't wait for this: a thumbnail cached before lengths were kept
                // has to read its video again
                let text = view::length_text(video_length(&length_path, &orig_path));
                // having the frame doesn't mean having the video
                let undownloaded = !orig_path.exists();
                emit_done(Event::MediaLabel { key: k, label: crate::types::MediaLabel { text, undownloaded } });
                anyhow::Ok(())
            })
            .await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("video frame: {e:#}");
                    // it was downloaded, so its size no longer applies
                    emit(Event::MediaLabel { key: key.clone(), label: Default::default() });
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
                Err(_) => {
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
            }
        });
    }

    /// Video `index` of row `id`, in the default app. Once downloaded it can show its first frame.
    pub(super) fn open_video(&self, id: &str, index: usize) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let Some((i, a)) = view::shown_attachments(dm).filter(|(_, a)| view::is_video(a)).nth(index) else { return };
        let file_key = view::attachment_key(a, ts, i);
        let path = attachment_path(&self.paths.attachments, &file_key);
        let ptr = a.clone();
        let manager = self.manager.clone();
        let tx = self.tx.clone();
        let open_dir = self.paths.open.clone();
        tokio::task::spawn_local(async move {
            let had = path.exists();
            if fetch_attachment(&manager, &tx, &path, &ptr).await {
                if !had {
                    let _ = tx.send(Internal::VideoFetched(file_key));
                }
                open_outside(&tx, path, &open_dir, attachment_name(&ptr)).await;
            }
        });
    }

    /// Downloads (or reads from the cache) a long message's full text.
    pub(super) fn request_long_text(&self, key: String, ptr: AttachmentPointer) {
        let path = attachment_path(&self.paths.attachments, &key);
        let manager = self.manager.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let data = match sigwin_vault::read(&path) {
                Ok(d) => Ok(d),
                Err(_) => match manager {
                    Some(m) => m.get_attachment(&ptr).await.map_err(|e| e.to_string()).inspect(|d| {
                        if let Err(e) = sigwin_vault::write(&path, d) {
                            warn!("save long text: {e}");
                        }
                    }),
                    None => Err("offline".into()),
                },
            };
            match data {
                Ok(d) => {
                    let text = view::trim_body(&String::from_utf8_lossy(&d), view::MAX_LONG_TEXT_BYTES).to_string();
                    let _ = tx.send(Internal::LongText(key, text));
                }
                Err(e) => {
                    warn!("download long text: {e}");
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
            }
        });
    }

    /// The message's file line: its images have their own place, in the row and the viewer.
    pub(super) fn open_attachment(&self, id: &str) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let jobs: Vec<(PathBuf, AttachmentPointer)> = view::shown_attachments(dm)
            .filter(|(_, a)| !view::is_media(a))
            .map(|(i, a)| (attachment_path(&self.paths.attachments, &view::attachment_key(a, ts, i)), a.clone()))
            .collect();
        let manager = self.manager.clone();
        let tx = self.tx.clone();
        let open_dir = self.paths.open.clone();
        tokio::task::spawn_local(async move {
            for (path, ptr) in jobs {
                if fetch_attachment(&manager, &tx, &path, &ptr).await {
                    open_outside(&tx, path, &open_dir, attachment_name(&ptr)).await;
                }
            }
        });
    }

    /// The message of row `id` in the open window.
    fn row_message(&self, id: &str) -> Option<(u64, &DataMessage)> {
        let open = self.current.as_ref()?;
        let ts = id.parse::<u64>().ok()?;
        let (_, c) = open.window.iter().find(|(t, _)| *t == ts)?;
        match view::classify(c, self.me) {
            Kind::Msg { dm, .. } => Some((ts, dm)),
            _ => None,
        }
    }

    /// Image `index` of row `id`: cache path and pointer.
    fn viewed(&self, id: &str, index: usize) -> Option<(PathBuf, AttachmentPointer)> {
        let (ts, dm) = self.row_message(id)?;
        let (i, a) = view::shown_attachments(dm).filter(|(_, a)| view::is_image(a)).nth(index)?;
        Some((attachment_path(&self.paths.attachments, &view::attachment_key(a, ts, i)), a.clone()))
    }

    pub(super) fn view_image(&self, id: &str, index: usize, fit: (u32, u32), from_file: bool) {
        let Some((_, dm)) = self.row_message(id) else { return };
        let shown = view::shown_attachments(dm).count();
        let count = view::shown_attachments(dm).filter(|(_, a)| view::is_image(a)).count();
        if count == 0 || (from_file && count != shown) {
            return self.open_attachment(id);
        }
        let index = index.min(count - 1);
        let Some((path, ptr)) = self.viewed(id, index) else { return };
        let id = id.to_string();
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        let open_dir = self.paths.open.clone();
        tokio::task::spawn_local(async move {
            if !fetch_attachment(&manager, &tx, &path, &ptr).await {
                return;
            }
            let p = path.clone();
            let decoded = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let bytes = sigwin_vault::read(&p)?;
                let Some(view) = sigwin_thumb::decode_view(&bytes, fit.0, fit.1, VIEW_MAX_PIXELS)? else {
                    return Ok(None);
                };
                drop(bytes);
                Ok(Some(crate::types::ViewerPixels::from(view)))
            })
            .await;
            match decoded {
                Ok(Ok(Some(pixels))) => {
                    let _ = tx.send(Internal::ProgressDone);
                    emit(Event::ViewerImage(crate::types::ViewerImage { id, index, count, pixels }));
                }
                other => {
                    if let Ok(Err(e)) = other {
                        warn!("view image: {e:#}");
                    }
                    // animated, huge or undecodable: an outside app may do better
                    open_outside(&tx, path, &open_dir, attachment_name(&ptr)).await;
                }
            }
        });
    }

    /// One at a time, in order. Failures leave a blank cell: notices belong to the image being
    /// viewed.
    pub(super) fn viewer_thumbs(&self, id: &str, px: u32) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let images = view::shown_attachments(dm).filter(|(_, a)| view::is_image(a));
        let jobs: Vec<(String, AttachmentPointer)> = images.map(|(i, a)| (view::attachment_key(a, ts, i), a.clone())).collect();
        let id = id.to_string();
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let (attachments, thumbs) = (self.paths.attachments.clone(), self.paths.thumbs.clone());
        tokio::task::spawn_local(async move {
            for (index, (key, ptr)) in jobs.into_iter().enumerate() {
                let orig = attachment_path(&attachments, &key);
                let thumb = thumbs.join(format!("{key}.enc"));
                if !thumb.exists() && !orig.exists() {
                    let Some(m) = manager.as_ref() else { return };
                    let saved = m.get_attachment(&ptr).await.map_err(anyhow::Error::from);
                    if let Err(e) = saved.and_then(|data| Ok(sigwin_vault::write(&orig, &data)?)) {
                        warn!("viewer thumbnail: {e:#}");
                        continue;
                    }
                }
                let made = tokio::task::spawn_blocking(move || -> anyhow::Result<Thumb> {
                    let t = make_thumb(&key, &orig, &thumb, 0)?;
                    let img = image::RgbaImage::from_raw(t.width, t.height, t.rgba).context("thumbnail size")?;
                    let square = sigwin_thumb::square(&image::DynamicImage::ImageRgba8(img), px);
                    Ok(Thumb { key, width: square.width(), height: square.height(), rgba: square.into_raw() })
                })
                .await;
                match made {
                    Ok(Ok(t)) => emit(Event::ViewerThumb(id.clone(), index, t)),
                    Ok(Err(e)) => warn!("viewer thumbnail: {e:#}"),
                    Err(e) => warn!("viewer thumbnail: {e}"),
                }
            }
        });
    }

    pub(super) fn open_viewed(&self, id: &str, index: usize) {
        let Some((path, ptr)) = self.viewed(id, index) else { return };
        let tx = self.tx.clone();
        let open_dir = self.paths.open.clone();
        tokio::task::spawn_local(async move {
            open_outside(&tx, path, &open_dir, attachment_name(&ptr)).await;
        });
    }

    pub(super) fn save_viewed(&self, id: &str, index: usize, owner: isize) {
        let Some((path, ptr)) = self.viewed(id, index) else { return };
        let tx = self.tx.clone();
        crate::picker::spawn_save(owner, attachment_name(&ptr), move |dst| {
            let Some(dst) = dst else { return };
            let status = match sigwin_vault::decrypt_file(&path, &dst) {
                Ok(()) => "저장했습니다.".to_string(),
                Err(e) => format!("저장 실패: {e}"),
            };
            let _ = tx.send(Internal::Notice(status));
        });
    }
}

/// Safety limit, not a budget: more pixels than this are abnormal input and go to an outside app.
/// The peak is 7 bytes per pixel for photos, 8 with transparency; the viewer holds the original at 4
/// until it closes.
const VIEW_MAX_PIXELS: u64 = 100_000_000;

/// Downloads an attachment unless it is cached; false (notice shown) if that failed.
async fn fetch_attachment(manager: &Option<Mgr>, tx: &UnboundedSender<Internal>, path: &Path, ptr: &AttachmentPointer) -> bool {
    if path.exists() {
        return true;
    }
    let Some(m) = manager.as_ref() else { return false };
    let _ = tx.send(Internal::Progress("첨부파일 다운로드 중…".into()));
    match m.get_attachment(ptr).await {
        Ok(data) => match sigwin_vault::write(path, &data) {
            Ok(()) => true,
            Err(e) => {
                let _ = tx.send(Internal::Notice(format!("저장 실패: {e}")));
                false
            }
        },
        Err(e) => {
            let _ = tx.send(Internal::Notice(format!("다운로드 실패: {e}")));
            false
        }
    }
}

/// Decrypts a cached attachment to `open/` and opens it with the default app.
async fn open_outside(tx: &UnboundedSender<Internal>, path: PathBuf, open_dir: &Path, name: String) {
    let dir = open_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || decrypt_for_open(&path, &dir, &name)).await {
        Ok(Ok(plain)) => {
            let _ = tx.send(Internal::ProgressDone);
            open_with_default_app(&plain);
        }
        Ok(Err(e)) => {
            let _ = tx.send(Internal::Notice(format!("첨부파일을 열 수 없습니다: {e}")));
        }
        Err(_) => {}
    }
}

/// Downloaded attachments and thumbnails of an expired message.
pub(super) fn remove_files(c: &Content, ts: u64, attachments: &Path, thumbs: &Path) {
    let Some(dm) = view::data_message(c) else { return };
    let mut keys: Vec<String> = dm.attachments.iter().enumerate().map(|(i, a)| view::attachment_key(a, ts, i)).collect();
    if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.as_ref()) {
        keys.push(view::attachment_key(data, ts, 99));
    }
    keys.extend(dm.preview.iter().filter_map(|p| p.image.as_ref()).map(|a| view::attachment_key(a, ts, view::LINK_IMAGE_INDEX)));
    for k in keys {
        let _ = std::fs::remove_file(attachment_path(attachments, &k));
        let _ = std::fs::remove_file(thumbs.join(format!("{k}.enc")));
        let _ = std::fs::remove_file(no_frame_path(thumbs, &k));
        let _ = std::fs::remove_file(length_path(thumbs, &k));
    }
}

/// A video's seconds, encrypted like everything else derived from an attachment. Kept next to its
/// frame, which is what the cache is emptied of.
fn length_path(thumbs: &Path, key: &str) -> PathBuf {
    thumbs.join(format!("{key}.len"))
}

fn save_length(path: &Path, seconds: u32) {
    if let Err(e) = sigwin_vault::write_cache(path, &seconds.to_le_bytes()) {
        warn!("save video length: {e}");
    }
}

/// The saved length, or else the one read from the video if that is here. 0: not known. A video
/// that doesn't say isn't asked again.
fn video_length(path: &Path, orig: &Path) -> u32 {
    if let Some(saved) = sigwin_vault::read(path).ok().and_then(|b| <[u8; 4]>::try_from(b).ok()) {
        return u32::from_le_bytes(saved);
    }
    if !orig.exists() {
        return 0;
    }
    match crate::videoframe::first_frame(orig) {
        Ok(v) => {
            let seconds = v.map_or(0, |v| v.seconds);
            save_length(path, seconds);
            seconds
        }
        Err(e) => {
            warn!("video length: {e:#}");
            0
        }
    }
}

/// Empty file saying a video's frame can't be decoded here, so it isn't tried on every visit.
/// Emptying the thumbnail cache forgets it (a codec may have been installed since).
fn no_frame_path(thumbs: &Path, key: &str) -> PathBuf {
    thumbs.join(format!("{key}.none"))
}

/// The blurhash is in the message and is never saved. Drawn small, then enlarged smoothly: it has
/// no detail to lose, and the cost of drawing grows with every pixel.
fn blur_thumb(key: &str, hash: &str, w: u32, h: u32) -> Option<Thumb> {
    const DRAWN: u32 = 32;
    let (bw, bh) = if w >= h { (DRAWN, (DRAWN * h / w.max(1)).max(1)) } else { ((DRAWN * w / h.max(1)).max(1), DRAWN) };
    let cell = sigwin_thumb::cover(&sigwin_thumb::blurhash(hash, bw, bh)?, w, h);
    Some(Thumb { key: key.to_string(), width: cell.width(), height: cell.height(), rgba: cell.into_raw() })
}

fn make_video_thumb(key: &str, orig: &Path, thumb: &Path, length: &Path, no_frame: &Path, w: u32, h: u32) -> anyhow::Result<Thumb> {
    let cached = if thumb.exists() { load_cached(thumb).ok() } else { None };
    let frame = match cached {
        Some(f) => f,
        None => {
            let Some(v) = crate::videoframe::first_frame(orig)? else {
                let _ = std::fs::write(no_frame, []);
                anyhow::bail!("not decodable here");
            };
            save_cached(thumb, sigwin_thumb::png_rgba(&v.frame));
            save_length(length, v.seconds);
            v.frame
        }
    };
    let cell = sigwin_thumb::cover(&frame, w, h);
    Ok(Thumb { key: key.to_string(), width: cell.width(), height: cell.height(), rgba: cell.into_raw() })
}

fn make_shaped_thumb(key: &str, orig: &Path, thumb: &Path, shape: ThumbShape) -> anyhow::Result<Thumb> {
    let (w, h) = match shape {
        ThumbShape::Fit(fit) => return make_thumb(key, orig, thumb, fit),
        ThumbShape::Cover(w, h) => (crate::types::physical_px(w), crate::types::physical_px(h)),
    };
    let t = make_thumb(key, orig, thumb, 0)?;
    let img = image::RgbaImage::from_raw(t.width, t.height, t.rgba).context("thumbnail size")?;
    let cell = sigwin_thumb::cover(&img, w, h);
    Ok(Thumb { key: t.key, width: cell.width(), height: cell.height(), rgba: cell.into_raw() })
}

pub(super) fn make_thumb(key: &str, orig: &Path, thumb: &Path, fit: u32) -> anyhow::Result<Thumb> {
    make_thumb_from(key, || Ok(sigwin_vault::read(orig)?), view::PHOTO_THUMB_MAX, thumb, fit)
}

/// A cached (encrypted) PNG.
pub(crate) fn load_cached(path: &Path) -> anyhow::Result<image::RgbaImage> {
    Ok(sigwin_thumb::decode_rgba(&sigwin_vault::read(path)?)?)
}

pub(crate) fn save_cached(path: &Path, png: image::ImageResult<Vec<u8>>) {
    if let Err(e) = png.map_err(anyhow::Error::from).and_then(|b| Ok(sigwin_vault::write_cache(path, &b)?)) {
        warn!("save {}: {e}", path.display());
    }
}

/// Decodes (sniffing the format, file names may lie) and downsizes to `max`, caching the result as PNG.
/// With `fit` > 0 the result is then resampled (down or up) so its longer side is exactly `fit` logical
/// px on screen: the software renderer scales images nearest-neighbor, which shreds stickers.
pub(super) fn make_thumb_from(
    key: &str,
    bytes: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    max: u32,
    thumb: &Path,
    fit: u32,
) -> anyhow::Result<Thumb> {
    let cached = if thumb.exists() { load_cached(thumb).ok() } else { None };
    let mut rgba = match cached {
        Some(rgba) => rgba,
        None => {
            let img = sigwin_thumb::decode_thumb(&bytes()?, max)?;
            save_cached(thumb, sigwin_thumb::png(&img));
            sigwin_thumb::to_rgba(&img)
        }
    };
    if fit > 0 {
        rgba = sigwin_thumb::fit(rgba, crate::types::physical_px(fit));
    }
    Ok(Thumb { key: key.to_string(), width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

/// Encrypted copy of a downloaded attachment. The name says nothing about the file: its real
/// name only appears on the plain copy made to open it (`attachment_name`).
pub(super) fn attachment_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.enc"))
}

pub(super) fn attachment_name(ptr: &AttachmentPointer) -> String {
    ptr.file_name.as_deref().filter(|n| !n.is_empty()).map(sanitize).unwrap_or_else(|| {
        let ext = ptr
            .content_type
            .as_deref()
            .and_then(mime_guess::get_mime_extensions_str)
            .and_then(|e| e.first().copied())
            .unwrap_or("bin");
        format!("attachment.{ext}")
    })
}

/// Plain copy of an attachment for an outside app, in its own folder under `open/` (emptied on
/// every start; files still open in another app then are left until the next one).
pub(super) fn decrypt_for_open(src: &Path, open_dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).map_err(|e| std::io::Error::other(e.to_string()))?;
    let dir = open_dir.join(hex::encode(r));
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join(name);
    sigwin_vault::decrypt_file(src, &dst)?;
    Ok(dst)
}

/// 1: everything encrypted, thumbnails still named `.png`. 2: thumbnails named `.enc`.
pub(super) const CACHE_FORMAT: u32 = 2;

/// Converts older caches: attachments lived in `<key>/<original name>`, thumbnails were plain
/// PNGs, then encrypted but still named `.png`. Idempotent; Err leaves the rest for the next start.
pub(super) fn seal_legacy_files(attachments: &Path, thumbs: &Path) -> anyhow::Result<()> {
    let mut failed = 0;
    for e in std::fs::read_dir(attachments)?.flatten() {
        let p = e.path();
        if p.is_dir() {
            let dst = attachment_path(attachments, &e.file_name().to_string_lossy());
            let src = std::fs::read_dir(&p)?.flatten().map(|f| f.path()).find(|f| f.is_file());
            if let Some(src) = src.filter(|_| !dst.exists()) {
                if let Err(e) = sigwin_vault::encrypt_file(&src, &dst, true) {
                    warn!("encrypt {}: {e}", src.display());
                    failed += 1;
                    continue;
                }
            }
            let _ = std::fs::remove_dir_all(&p);
        }
    }
    for e in std::fs::read_dir(thumbs)?.flatten() {
        let p = e.path();
        if !p.is_file() || p.extension().is_none_or(|x| x != "png") {
            continue;
        }
        let dst = p.with_extension("enc");
        let res = if sigwin_vault::is_sealed(&p) {
            std::fs::rename(&p, &dst)
        } else {
            sigwin_vault::encrypt_file(&p, &dst, false).and_then(|_| std::fs::remove_file(&p))
        };
        if let Err(e) = res {
            // only a cache: drop it rather than keep it plain
            warn!("encrypt {}: {e}", p.display());
            let _ = std::fs::remove_file(&p);
        }
    }
    anyhow::ensure!(failed == 0, "{failed} attachments left unencrypted");
    Ok(())
}

pub(super) fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "attachment".into() } else { s }
}

pub(super) fn open_with_default_app(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Deletes the plain copies of opened attachments under the data folder `dir`.
pub fn clear_open(dir: &Path) {
    clear_dir(&Paths::new(dir).open);
}

/// Removes the folder's contents, keeping the folder.
pub(super) fn clear_dir(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_lengths_are_sealed_and_read_back() {
        sigwin_vault::init([3; 32]);
        let dir = std::env::temp_dir().join(format!("sigwin-length-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (path, orig) = (length_path(&dir, "k"), dir.join("not-here.enc"));
        // nothing saved, and no video to read it from: unknown, and not remembered as unknown
        assert_eq!(video_length(&path, &orig), 0);
        assert!(!path.exists());

        save_length(&path, 75);
        assert_eq!(video_length(&path, &orig), 75);
        assert!(sigwin_vault::is_sealed(&path));
        // a broken file is read as missing
        std::fs::write(&path, b"junk").unwrap();
        assert_eq!(video_length(&path, &orig), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seals_legacy_cache_files() {
        sigwin_vault::init([3; 32]);
        let dir = std::env::temp_dir().join(format!("sigwin-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (att, thumbs) = (dir.join("attachments"), dir.join("thumbs"));
        std::fs::create_dir_all(att.join("abc")).unwrap();
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(att.join("abc").join("계약서.pdf"), b"pdf bytes").unwrap();
        std::fs::write(thumbs.join("abc.png"), b"png bytes").unwrap();
        sigwin_vault::write(&thumbs.join("new.png"), b"already sealed").unwrap();

        seal_legacy_files(&att, &thumbs).unwrap();
        assert!(!att.join("abc").exists());
        assert_eq!(sigwin_vault::read(&attachment_path(&att, "abc")).unwrap(), b"pdf bytes");
        assert_eq!(sigwin_vault::read(&thumbs.join("abc.enc")).unwrap(), b"png bytes");
        assert_eq!(sigwin_vault::read(&thumbs.join("new.enc")).unwrap(), b"already sealed");
        assert!(!thumbs.join("abc.png").exists() && !thumbs.join("new.png").exists());

        let open = dir.join("open");
        let plain = decrypt_for_open(&attachment_path(&att, "abc"), &open, "계약서.pdf").unwrap();
        assert_eq!(plain.file_name().unwrap(), "계약서.pdf");
        assert_eq!(std::fs::read(&plain).unwrap(), b"pdf bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
