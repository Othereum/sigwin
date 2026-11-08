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
                PendingFile { ext: ext(p), has_thumb: thumb.is_some(), thumb: thumb.unwrap_or_default() }
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
