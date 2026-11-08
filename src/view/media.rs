//! Attachments, stickers and link previews on a row: what they are, their labels, their size on screen.

use presage::libsignal_service::content::DataMessage;
use presage::proto::AttachmentPointer;

use super::text::is_long_text;
use crate::types::MsgDto;

/// Decode size of stickers (also part of their cache file names).
pub const THUMB_MAX: u32 = 400;
/// Photo boxes in the chat, logical px (w, h): landscape and square vs portrait. A square comes out 300x300.
const PHOTO_BOX_WIDE: (u32, u32) = (400, 300);
const PHOTO_BOX_TALL: (u32, u32) = (300, 450);
/// Decode size of photo thumbnails: the longest side a photo can show.
pub const PHOTO_THUMB_MAX: u32 = PHOTO_BOX_TALL.1;

pub const STICKER_SIZE: u32 = 180;

pub fn attachment_key(ptr: &AttachmentPointer, ts: u64, idx: usize) -> String {
    match &ptr.digest {
        Some(d) if d.len() >= 12 => hex::encode(&d[..12]),
        _ => format!("{ts}-{idx}"),
    }
}

/// `attachment_key` index of a link preview's image (not in `attachments`, like a sticker's).
pub const LINK_IMAGE_INDEX: usize = 98;
/// Size of that image on the card (logical px): a wide one on top, others beside the text.
const LINK_IMAGE_WIDE: (u32, u32) = (300, 158);
const LINK_IMAGE_SMALL: (u32, u32) = (64, 64);

/// Cut-offs for what a preview can say (characters).
pub const LINK_TITLE_MAX: usize = 200;
pub const LINK_DESC_MAX: usize = 300;

/// An image too small or of unknown size stays small (site logos).
pub fn link_image_size(w: Option<u32>, h: Option<u32>) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w >= 200 && h >= 100 && w >= h => LINK_IMAGE_WIDE,
        _ => LINK_IMAGE_SMALL,
    }
}

/// The preview to show on a message, and its image to fetch. Like Signal's clients, it counts
/// only if its url really is a link in the text.
pub fn link_card(dm: &DataMessage, ts: u64) -> Option<(crate::types::LinkCard, Option<WantedCell>)> {
    let p = dm.preview.first()?;
    let url = p.url.as_deref()?;
    if !crate::link::preview_allowed(url, dm.body.as_deref()?) {
        return None;
    }
    let title = crate::link::clean_preview_text(p.title.as_deref().unwrap_or_default(), LINK_TITLE_MAX);
    let description = crate::link::clean_preview_text(p.description.as_deref().unwrap_or_default(), LINK_DESC_MAX);
    if title.is_empty() && p.image.is_none() {
        return None;
    }
    let mut card = crate::types::LinkCard { url: url.to_string(), title, description, domain: crate::link::domain(url), ..Default::default() };
    let wanted = p.image.as_ref().map(|a| {
        let size = link_image_size(a.width, a.height);
        let file_key = attachment_key(a, ts, LINK_IMAGE_INDEX);
        card.image_key = format!("{file_key}@{}x{}", size.0, size.1);
        (card.img_w, card.img_h) = size;
        WantedCell { key: card.image_key.clone(), file_key, ptr: a.clone(), size }
    });
    Some((card, wanted))
}

/// Attachments shown as such (the long-text one is part of the body).
pub fn shown_attachments(dm: &DataMessage) -> impl Iterator<Item = (usize, &AttachmentPointer)> {
    dm.attachments.iter().enumerate().filter(|(_, a)| !is_long_text(a))
}

pub fn is_image(ptr: &AttachmentPointer) -> bool {
    matches!(
        ptr.content_type.as_deref(),
        Some("image/jpeg" | "image/png" | "image/apng" | "image/gif" | "image/webp")
    ) && ptr.size.unwrap_or(0) < 15 * 1024 * 1024
}

pub fn is_video(ptr: &AttachmentPointer) -> bool {
    ptr.content_type.as_deref().is_some_and(|c| c.starts_with("video/"))
}

/// Drawn in the message itself, as a picture or a grid cell.
pub fn is_media(ptr: &AttachmentPointer) -> bool {
    is_image(ptr) || is_video(ptr)
}

/// Largest video downloaded just to show its picture. `get_attachment` holds the whole file in
/// memory while it arrives, and nobody asked for this one.
const VIDEO_AUTO_MAX: u32 = 30 * 1024 * 1024;

/// Whether a video is downloaded without being asked for: the one place that decides. Others show
/// their blurhash until they are opened. A kind Windows can't usually decode isn't worth the traffic.
pub fn auto_fetches_video(ptr: &AttachmentPointer) -> bool {
    matches!(ptr.content_type.as_deref(), Some("video/mp4" | "video/quicktime" | "video/3gpp" | "video/x-m4v"))
        && ptr.size.is_some_and(|s| s <= VIDEO_AUTO_MAX)
}

pub fn size_text(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    match b as f64 {
        x if x >= 1024.0 * MB => format!("{:.1}GB", x / 1024.0 / MB),
        x if x >= 10.0 * MB => format!("{:.0}MB", x / MB),
        x if x >= 0.1 * MB => format!("{:.1}MB", x / MB),
        x => format!("{:.0}KB", (x / 1024.0).ceil()),
    }
}

/// What the picture of a file that isn't here yet says.
pub fn undownloaded_label(ptr: &AttachmentPointer) -> crate::types::MediaLabel {
    let text = ptr.size.filter(|s| *s > 0).map_or(String::new(), |s| size_text(s as u64));
    crate::types::MediaLabel { text, undownloaded: true, progress: None }
}

/// What the picture of a file on its way says: `got` bytes of it, and `percent` for its ring.
/// `got` counts what's on the wire, a little more than the file: it stops at the file's size.
pub fn downloading_label(ptr: &AttachmentPointer, got: u64, percent: u8) -> crate::types::MediaLabel {
    let size = ptr.size.filter(|s| *s > 0).map(u64::from);
    let text = match size {
        Some(size) => format!("{} / {}", size_text(got.min(size)), size_text(size)),
        None => size_text(got),
    };
    crate::types::MediaLabel { text, undownloaded: true, progress: Some(percent) }
}

/// The file line of a message: a name, how many more, and their size together. `bytes` is `None`
/// if a size isn't known: a total of some of them would be wrong.
pub fn file_line(first: &str, more: usize, bytes: Option<u64>) -> String {
    let names = if more > 0 { format!("{first} 외 {more}개") } else { first.to_string() };
    match bytes {
        Some(b) => format!("{names} · {}", size_text(b)),
        None => names,
    }
}

fn files_label(files: &[&AttachmentPointer]) -> String {
    let Some(first) = files.first() else { return String::new() };
    let bytes = files.iter().map(|a| a.size.filter(|s| *s > 0).map(|s| s as u64)).sum();
    file_line(&attachment_label(first), files.len() - 1, bytes)
}

/// A video's length as players show it. Empty when it isn't known.
pub fn length_text(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match (seconds, h) {
        (0, _) => String::new(),
        (_, 0) => format!("{m}:{s:02}"),
        _ => format!("{h}:{m:02}:{s:02}"),
    }
}

pub(super) fn attachment_label(ptr: &AttachmentPointer) -> String {
    let ct = ptr.content_type.as_deref().unwrap_or("");
    if let Some(name) = ptr.file_name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    if ct.starts_with("image/") {
        "사진".into()
    } else if ct.starts_with("video/") {
        "동영상".into()
    } else if ct.starts_with("audio/") {
        if ptr.flags.unwrap_or(0) & 1 == 1 { "음성 메시지".into() } else { "오디오".into() }
    } else {
        "파일".into()
    }
}

/// An image wanted for a cell of a message's grid.
pub struct WantedCell {
    /// what the UI knows it by
    pub key: String,
    /// its files on disk
    pub file_key: String,
    pub ptr: AttachmentPointer,
    /// logical px
    pub size: (u32, u32),
}

/// Images a grid shows; the rest are a "+N" on its last cell and wait for the viewer.
pub(super) const GRID_SHOWN: usize = 5;

/// Where a grid of `count` images (2 or more) puts them: x, y, width and height in logical px,
/// with a 1px gap. As in Signal's apps, the first ones are the largest.
fn grid_cells(count: usize) -> &'static [(u32, u32, u32, u32)] {
    match count {
        0 | 1 => &[],
        2 => &[(0, 0, 150, 150), (151, 0, 150, 150)],
        3 => &[(0, 0, 200, 201), (201, 0, 100, 100), (201, 101, 100, 100)],
        4 => &[(0, 0, 150, 150), (151, 0, 150, 150), (0, 151, 150, 150), (151, 151, 150, 150)],
        _ => &[(0, 0, 150, 150), (151, 0, 150, 150), (0, 151, 100, 100), (101, 151, 99, 100), (201, 151, 100, 100)],
    }
}

/// Makes `m` show `count` images as a grid, if that is 2 or more. `key` gets the index of each
/// image shown and the size of its cell, and returns its key in the UI thumbnail cache.
pub fn set_photo_grid(m: &mut MsgDto, count: usize, mut key: impl FnMut(usize, u32, u32) -> String) {
    let cells = grid_cells(count);
    let more = count.saturating_sub(GRID_SHOWN) as u32;
    m.cells = cells
        .iter()
        .enumerate()
        .map(|(n, &(x, y, w, h))| crate::types::CellDto {
            key: key(n, w, h),
            x,
            y,
            w,
            h,
            more: if n + 1 == GRID_SHOWN { more } else { 0 },
            video: false,
        })
        .collect();
    let Some(first) = m.cells.first() else { return };
    m.image_key = first.key.clone();
    m.img_w = cells.iter().map(|c| c.0 + c.2).max().unwrap_or(0);
    m.img_h = cells.iter().map(|c| c.1 + c.3).max().unwrap_or(0);
}

fn fit(w: Option<u32>, h: Option<u32>) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            // scale into the box for its orientation, never up
            let (bw, bh) = if h > w { PHOTO_BOX_TALL } else { PHOTO_BOX_WIDE };
            let s = (bw as f32 / w as f32).min(bh as f32 / h as f32).min(1.0);
            (((w as f32 * s) as u32).max(40), ((h as f32 * s) as u32).max(40))
        }
        _ => PHOTO_BOX_WIDE,
    }
}

/// Attachments: one photo or video alone, several in a grid, the rest as a file line.
pub(super) fn set_attachments(
    m: &mut MsgDto,
    dm: &DataMessage,
    ts: u64,
    wanted: &mut Vec<(String, AttachmentPointer, u32)>,
    wanted_cells: &mut Vec<WantedCell>,
) {
    let mut files = Vec::new();
    let mut photos = Vec::new();
    for (i, a) in shown_attachments(dm) {
        if !m.sticker && is_media(a) {
            photos.push((i, a));
        } else {
            files.push(a);
        }
    }
    m.attachment = files_label(&files);
    if let [(i, a)] = photos[..] {
        let key = attachment_key(a, ts, i);
        let (w, h) = fit(a.width, a.height);
        m.img_w = w;
        m.img_h = h;
        if is_video(a) {
            // the sender's size may be missing or wrong: cut to fill the box
            m.video = true;
            m.image_key = format!("{key}@{w}x{h}");
            wanted_cells.push(WantedCell { key: m.image_key.clone(), file_key: key, ptr: a.clone(), size: (w, h) });
        } else {
            // pre-resample to the on-screen size (nearest-neighbor scaling is jagged)
            wanted.push((key.clone(), a.clone(), w.max(h)));
            m.image_key = key;
        }
    } else {
        set_photo_grid(m, photos.len(), |n, w, h| {
            let (i, a) = photos[n];
            let file_key = attachment_key(a, ts, i);
            // the same image may also be shown alone, or in a cell of another size
            let key = format!("{file_key}@{w}x{h}");
            wanted_cells.push(WantedCell { key: key.clone(), file_key, ptr: a.clone(), size: (w, h) });
            key
        });
        for (cell, (_, a)) in m.cells.iter_mut().zip(&photos) {
            cell.video = is_video(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_size_caps() {
        let f = |w, h| fit(Some(w), Some(h));
        assert_eq!(f(3000, 3000), (300, 300)); // square
        assert_eq!(f(4000, 3000), (400, 300)); // 4:3 unchanged
        assert_eq!(f(3000, 4000), (300, 400)); // 3:4 is width-limited
        assert_eq!(f(2000, 3000), (300, 450)); // 2:3
        assert_eq!(f(900, 1600), (253, 450)); // 9:16
        assert_eq!(f(1600, 900), (400, 225)); // 16:9 unchanged
        assert_eq!(f(100, 100), (100, 100)); // never upscaled
        assert_eq!(fit(None, None), (400, 300));
    }

    #[test]
    fn grid_cells_fill_their_box_without_touching() {
        for count in 2..8 {
            let cells = grid_cells(count);
            assert_eq!(cells.len(), count.min(GRID_SHOWN));
            let width = cells.iter().map(|c| c.0 + c.2).max().unwrap();
            let height = cells.iter().map(|c| c.1 + c.3).max().unwrap();
            assert_eq!(width, 301);
            let area: u32 = cells.iter().map(|c| c.2 * c.3).sum();
            // what's left of the box is the 1px gaps
            assert!(area < width * height && width * height - area <= 2 * width + height, "{count}");
            for (i, a) in cells.iter().enumerate() {
                for b in &cells[i + 1..] {
                    let apart = a.0 + a.2 < b.0 + 1 || b.0 + b.2 < a.0 + 1 || a.1 + a.3 < b.1 + 1 || b.1 + b.3 < a.1 + 1;
                    assert!(apart, "{count}: {a:?} {b:?}");
                }
            }
        }
    }

    #[test]
    fn labels_read_like_players_and_file_lists() {
        assert_eq!(length_text(0), "");
        assert_eq!(length_text(1), "0:01");
        assert_eq!(length_text(75), "1:15");
        assert_eq!(length_text(3600), "1:00:00");
        assert_eq!(length_text(3725), "1:02:05");
        let ptr = |size| AttachmentPointer { size, ..Default::default() };
        assert_eq!(undownloaded_label(&ptr(None)).text, "");
        assert_eq!(undownloaded_label(&ptr(Some(0))).text, "");
        assert_eq!(undownloaded_label(&ptr(Some(45 * 1024 * 1024))).text, "45MB");
        assert!(undownloaded_label(&ptr(None)).undownloaded);
        const MB: u64 = 1024 * 1024;
        let big = ptr(Some(45 * 1024 * 1024));
        assert_eq!(downloading_label(&big, 12 * MB, 26).text, "12MB / 45MB");
        assert_eq!(downloading_label(&big, 12 * MB, 26).progress, Some(26));
        // padding and the like: never past the file's own size
        assert_eq!(downloading_label(&big, 46 * MB, 100).text, "45MB / 45MB");
        assert_eq!(downloading_label(&ptr(None), 12 * MB, 0).text, "12MB");
    }

    #[test]
    fn file_lines_say_the_size_of_all_files_together() {
        let file = |name: &str, size| AttachmentPointer { file_name: Some(name.into()), size, ..Default::default() };
        let (a, b) = (file("a.pdf", Some(2 * 1024 * 1024)), file("b.pdf", Some(1024 * 1024)));
        assert_eq!(files_label(&[]), "");
        assert_eq!(files_label(&[&a]), "a.pdf · 2.0MB");
        assert_eq!(files_label(&[&a, &b]), "a.pdf 외 1개 · 3.0MB");
        // one size missing: no total
        let unknown = file("c.pdf", None);
        assert_eq!(files_label(&[&a, &unknown]), "a.pdf 외 1개");
        assert_eq!(files_label(&[&file("d.pdf", Some(0))]), "d.pdf");
    }

    #[test]
    fn only_small_videos_download_by_themselves() {
        let video = AttachmentPointer { content_type: Some("video/mp4".into()), size: Some(1024), ..Default::default() };
        assert!(auto_fetches_video(&video));
        assert!(auto_fetches_video(&AttachmentPointer { size: Some(VIDEO_AUTO_MAX), ..video.clone() }));
        assert!(!auto_fetches_video(&AttachmentPointer { size: Some(VIDEO_AUTO_MAX + 1), ..video.clone() }));
        assert!(!auto_fetches_video(&AttachmentPointer { size: None, ..video.clone() }));
        assert!(!auto_fetches_video(&AttachmentPointer { content_type: Some("video/webm".into()), ..video }));
    }
}
