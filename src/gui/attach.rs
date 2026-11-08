//! The attachments waiting in the composer: a tile per file, with a picture for the ones that
//! have one. Pictures are cut off the UI thread, small, and kept only in memory.

use std::path::Path;

use super::*;

/// Tile size in logical px.
const TILE: u32 = 120;
/// Past this a file isn't opened for a tile.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

fn ext(p: &Path) -> SharedString {
    let e = p.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
    e.chars().take(4).collect::<String>().into()
}

/// Whether a tile shows the file's picture: GIFs and the rest are left to decode, like any image,
/// while videos and other files get their extension.
fn has_picture(p: &Path) -> bool {
    mime_guess::from_path(p).first_or_octet_stream().type_() == "image"
}

/// Pictures and videos can go together; any other file goes alone.
fn is_media(p: &Path) -> bool {
    matches!(mime_guess::from_path(p).first_or_octet_stream().type_().as_str(), "image" | "video")
}

/// Which of `new` may join `have`, in order, and why the first refused one was refused.
pub(crate) fn admit(have: &[PathBuf], new: Vec<PathBuf>) -> (Vec<PathBuf>, Option<&'static str>) {
    let mut all = have.to_vec();
    let mut why = None;
    for p in new {
        if all.iter().any(|f| !is_media(f)) {
            why.get_or_insert("파일은 한 번에 하나만 보낼 수 있습니다.");
        } else if !is_media(&p) && !all.is_empty() {
            why.get_or_insert("사진·동영상과 파일은 함께 보낼 수 없습니다.");
        } else {
            all.push(p);
        }
    }
    (all.split_off(have.len()), why)
}

fn file_name(p: &Path) -> SharedString {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into()
}

/// Shows `files` as the composer's attachments.
pub(crate) fn show(app: &AppWindow, files: &[PathBuf]) {
    let photo = files.iter().any(|p| {
        let t = mime_guess::from_path(p).first_or_octet_stream();
        t.type_() == "image" && t.subtype() != "gif"
    });
    app.set_pending_has_photo(photo);
    // the quality button takes the sticker button's place
    if photo && app.get_sticker_open() {
        stickers::close(app);
    }
    let (rows, wanted) = UI.with_borrow_mut(|ui| {
        ui.pending_thumbs.retain(|p, _| files.contains(p));
        let mut wanted = Vec::new();
        let rows: Vec<PendingFile> = files
            .iter()
            .map(|p| {
                let thumb = match ui.pending_thumbs.get(p) {
                    Some(t) => t.clone(),
                    None if has_picture(p) => {
                        ui.pending_thumbs.insert(p.clone(), None);
                        wanted.push(p.clone());
                        None
                    }
                    None => None,
                };
                let is_file = !is_media(p);
                let size = if is_file {
                    std::fs::metadata(p).map(|m| crate::view::media::size_text(m.len())).unwrap_or_default()
                } else {
                    String::new()
                };
                PendingFile {
                    ext: ext(p),
                    has_thumb: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    is_file,
                    name: if is_file { file_name(p) } else { SharedString::new() },
                    size: size.into(),
                }
            })
            .collect();
        (rows, wanted)
    });
    app.set_pending_files(ModelRc::new(VecModel::from(rows)));
    for path in wanted {
        let px = types::physical_px(TILE);
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let pixels = cut(&path, px);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    arrived(&app, path, pixels);
                }
            });
        });
    }
}

/// The file's picture cropped to a `px` square. `None` when it can't be decoded.
fn cut(path: &Path, px: u32) -> Option<(Vec<u8>, u32)> {
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let img = sigwin_thumb::decode_thumb(&bytes, px * 2).ok()?;
    let square = sigwin_thumb::cover(&sigwin_thumb::to_rgba(&img), px, px);
    Some((square.into_raw(), px))
}

fn arrived(app: &AppWindow, path: PathBuf, pixels: Option<(Vec<u8>, u32)>) {
    let at = UI.with_borrow_mut(|ui| {
        // removed while it was being cut
        let slot = ui.pending_thumbs.get_mut(&path)?;
        let (raw, px) = pixels?;
        *slot = Some(Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&raw, px, px)));
        ui.files.iter().position(|p| *p == path).map(|i| (i, slot.clone()))
    });
    let Some((i, Some(img))) = at else { return };
    let model = app.get_pending_files();
    if let Some(mut row) = slint::Model::row_data(&model, i) {
        row.thumb = img;
        row.has_thumb = true;
        slint::Model::set_row_data(&model, i, row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        s.into()
    }

    #[test]
    fn media_go_together() {
        let (ok, why) = admit(&[p("a.jpg")], vec![p("b.mp4"), p("c.gif")]);
        assert_eq!(ok.len(), 2);
        assert!(why.is_none());
    }

    #[test]
    fn file_refused_after_media() {
        let (ok, why) = admit(&[p("a.jpg")], vec![p("b.pdf")]);
        assert!(ok.is_empty());
        assert!(why.unwrap().contains("함께"));
    }

    #[test]
    fn media_refused_after_file() {
        let (ok, why) = admit(&[p("a.pdf")], vec![p("b.jpg"), p("c.zip")]);
        assert!(ok.is_empty());
        assert!(why.unwrap().contains("하나만"));
    }

    #[test]
    fn first_of_a_mixed_batch_decides() {
        let (ok, why) = admit(&[], vec![p("a.pdf"), p("b.jpg")]);
        assert_eq!(ok, vec![p("a.pdf")]);
        assert!(why.is_some());
        let (ok, _) = admit(&[], vec![p("a.jpg"), p("b.pdf"), p("c.png")]);
        assert_eq!(ok, vec![p("a.jpg"), p("c.png")]);
    }
}

/// Takes the `i`th attachment off.
pub(crate) fn remove(app: &AppWindow, i: usize) {
    let files = UI.with_borrow_mut(|ui| {
        if i < ui.files.len() {
            ui.files.remove(i);
        }
        ui.files.clone()
    });
    show(app, &files);
    drafts::drop_if_empty(app);
}
