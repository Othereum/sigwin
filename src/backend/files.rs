//! Attachment and thumbnail caches (encrypted on disk), opening attachments.

use super::*;

impl App {
    pub(super) fn request_thumb(&self, key: String, ptr: AttachmentPointer, fit: u32) {
        let thumb_path = self.paths.thumbs.join(format!("{key}.enc"));
        let orig_path = attachment_path(&self.paths.attachments, &key);
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            if !thumb_path.exists() {
                if !orig_path.exists() {
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
            let res = tokio::task::spawn_blocking(move || make_thumb(&k, &orig_path, &thumb_path, fit)).await;
            match res {
                Ok(Ok(t)) => emit(Event::Thumb(Arc::new(t))),
                _ => {
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
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

    pub(super) fn open_attachment(&self, id: &str) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let jobs: Vec<(PathBuf, AttachmentPointer)> = view::shown_attachments(dm)
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
                let Some((img, full, opaque)) = sigwin_thumb::decode_view(&bytes, fit.0, fit.1, VIEW_MAX_PIXELS)? else {
                    return Ok(None);
                };
                drop(bytes);
                let pixels = slint::SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height());
                Ok(Some((pixels, full, opaque)))
            })
            .await;
            match decoded {
                Ok(Ok(Some((pixels, full, opaque)))) => {
                    let _ = tx.send(Internal::Status(String::new()));
                    emit(Event::ViewerImage(crate::types::ViewerImage { id, index, count, full, opaque, pixels }));
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
            let _ = tx.send(Internal::Status(status));
        });
    }
}

/// Safety limit, not a budget: more pixels than this are abnormal input and go to an outside app.
/// Decoding takes up to 4 bytes per pixel, returned when it ends.
const VIEW_MAX_PIXELS: u64 = 100_000_000;

/// Downloads an attachment unless it is cached; false (status set) if that failed.
async fn fetch_attachment(manager: &Option<Mgr>, tx: &UnboundedSender<Internal>, path: &Path, ptr: &AttachmentPointer) -> bool {
    if path.exists() {
        return true;
    }
    let _ = tx.send(Internal::Status("첨부파일 다운로드 중…".into()));
    let Some(m) = manager.as_ref() else { return false };
    match m.get_attachment(ptr).await {
        Ok(data) => match sigwin_vault::write(path, &data) {
            Ok(()) => true,
            Err(e) => {
                let _ = tx.send(Internal::Status(format!("저장 실패: {e}")));
                false
            }
        },
        Err(e) => {
            let _ = tx.send(Internal::Status(format!("다운로드 실패: {e}")));
            false
        }
    }
}

/// Decrypts a cached attachment to `open/` and opens it with the default app.
async fn open_outside(tx: &UnboundedSender<Internal>, path: PathBuf, open_dir: &Path, name: String) {
    let dir = open_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || decrypt_for_open(&path, &dir, &name)).await {
        Ok(Ok(plain)) => {
            let _ = tx.send(Internal::Status(String::new()));
            open_with_default_app(&plain);
        }
        Ok(Err(e)) => {
            let _ = tx.send(Internal::Status(format!("첨부파일을 열 수 없습니다: {e}")));
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
    for k in keys {
        let _ = std::fs::remove_file(attachment_path(attachments, &k));
        let _ = std::fs::remove_file(thumbs.join(format!("{k}.enc")));
    }
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
