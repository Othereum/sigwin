//! Attachments, stickers and link previews on a row: what they are, their labels, their size on screen.

use presage::libsignal_service::content::DataMessage;
use presage::proto::AttachmentPointer;

use super::text::is_long_text;
use crate::types::MsgDto;

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

/// Decode size of stickers (also part of their cache file names).
pub const THUMB_MAX: u32 = 400;
/// Photo boxes in the chat at the medium size, logical px (w, h): landscape and square vs
/// portrait. A square comes out 300x300.
const PHOTO_BOX_WIDE: (u32, u32) = (400, 300);
const PHOTO_BOX_TALL: (u32, u32) = (300, 450);
/// Least longer side of photo thumbnail caches: a medium photo's longest. Larger ones are made
/// for what is drawn bigger (`backend::files::thumb_source`).
pub const PHOTO_THUMB_MAX: u32 = PHOTO_BOX_TALL.1;

/// The sizes chosen for photos and stickers in chats (`Settings::photo_size`, `sticker_size`):
/// 0 small, 1 medium, 2 large.
static PHOTO_SIZE: AtomicU8 = AtomicU8::new(1);
static STICKER_CHOICE: AtomicU8 = AtomicU8::new(1);

/// Sets the sizes. Whether they changed.
pub fn set_sizes(photo: u8, sticker: u8) -> bool {
    let old = (PHOTO_SIZE.swap(photo.min(2), Ordering::Relaxed), STICKER_CHOICE.swap(sticker.min(2), Ordering::Relaxed));
    old != (photo.min(2), sticker.min(2))
}

fn photo_choice() -> u8 {
    PHOTO_SIZE.load(Ordering::Relaxed)
}

/// The width a picture may take in the chat, logical px (`set_room`): a narrow chat shrinks its
/// pictures rather than cutting them. 0 until the UI says, no limit.
static ROOM: AtomicU32 = AtomicU32::new(0);

/// Sets the width pictures may take in the chat. Whether any picture's size changes: past the
/// widest box at the chosen size, the room makes no difference.
pub fn set_room(px: u32) -> bool {
    // the UI's least pane leaves more; a caption's bubble is never narrower anyway
    let px = px.max(CAPTIONED_MIN_WIDTH);
    let old = ROOM.swap(px, Ordering::Relaxed);
    let widest = photo_px(PHOTO_BOX_WIDE.0, photo_choice());
    let bound = |r: u32| if r == 0 { widest } else { r.min(widest) };
    bound(old) != bound(px)
}

/// Whether the UI has said how wide the chat is.
pub fn has_room() -> bool {
    ROOM.load(Ordering::Relaxed) != 0
}

/// The room set, or no limit.
fn room() -> u32 {
    match ROOM.load(Ordering::Relaxed) {
        0 => u32::MAX,
        r => r,
    }
}

/// `v` logical px at the medium photo size, at size `choice`.
fn photo_px(v: u32, choice: u8) -> u32 {
    match choice {
        0 => (v * 3).div_ceil(4),
        2 => (v * 4).div_ceil(3),
        _ => v,
    }
}

/// A sticker's longer side in chats, logical px.
pub fn sticker_size() -> u32 {
    [120, 180, 240][STICKER_CHOICE.load(Ordering::Relaxed).min(2) as usize]
}

/// A cell of a chat's media page, square, logical px (`GalleryData.cell` in the UI).
pub const GALLERY_CELL: u32 = 120;

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

/// Attachments shown as such (the long-text one is part of the body). None of a view-once
/// message: it is seen once, through `view_once_photo`, and must not show, download, save or be
/// quoted as an ordinary picture.
pub fn shown_attachments(dm: &DataMessage) -> impl Iterator<Item = (usize, &AttachmentPointer)> {
    let once = dm.is_view_once.unwrap_or(false);
    dm.attachments.iter().enumerate().filter(move |(_, a)| !once && !is_long_text(a))
}

/// How long a view-once message can be opened, from its send time: Signal's clients erase it
/// then, opened or not (`global.messageQueueTimeInSeconds`, a month by default).
pub const VIEW_ONCE_KEPT_MS: u64 = 30 * 24 * 3600 * 1000;

/// The picture of a view-once message that can still be opened here. Videos and other kinds are
/// left to the phone: they'd play in an outside app, which could keep a copy.
pub fn view_once_photo(dm: &DataMessage, ts: u64, now: u64) -> Option<(usize, &AttachmentPointer)> {
    if !dm.is_view_once.unwrap_or(false) || ts.saturating_add(VIEW_ONCE_KEPT_MS) <= now {
        return None;
    }
    dm.attachments.iter().enumerate().find(|(_, a)| !is_long_text(a)).filter(|(_, a)| is_image(a))
}

/// What a view-once message's card says, `None` for any other message.
pub fn view_once_state(dm: &DataMessage, outgoing: bool, ts: u64, now: u64) -> crate::types::ViewOnce {
    use crate::types::ViewOnce;
    if !dm.is_view_once.unwrap_or(false) {
        ViewOnce::None
    } else if outgoing {
        ViewOnce::Sent
    } else if view_once_photo(dm, ts, now).is_some() {
        ViewOnce::Photo
    } else if dm.attachments.iter().any(|a| !is_long_text(a)) && ts.saturating_add(VIEW_ONCE_KEPT_MS) > now {
        ViewOnce::Phone
    } else {
        ViewOnce::Viewed
    }
}

/// What a message adds to its chat's media and files: (index in `attachments`, kind). Not voice
/// messages, which aren't files to the user.
pub fn gallery_items(dm: &DataMessage) -> Vec<(usize, crate::db::MediaKind)> {
    shown_attachments(dm)
        .filter(|(_, a)| !is_voice(a))
        .map(|(i, a)| (i, if is_media(a) { crate::db::MediaKind::Media } else { crate::db::MediaKind::Files }))
        .collect()
}

pub fn is_voice(ptr: &AttachmentPointer) -> bool {
    ptr.flags.unwrap_or(0) & 1 == 1
}

pub fn is_image(ptr: &AttachmentPointer) -> bool {
    matches!(
        ptr.content_type.as_deref(),
        Some("image/jpeg" | "image/png" | "image/apng" | "image/gif" | "image/webp" | "image/avif")
    )
}

/// Largest picture downloaded just to show it: `get_attachment` holds the whole file in memory
/// while it arrives, and nobody asked for this one. A bigger one shows its blurhash and size
/// until it is clicked, which is asking.
const IMAGE_AUTO_MAX: u32 = 15 * 1024 * 1024;

/// Whether a picture is downloaded without being asked for.
pub fn auto_fetches_image(ptr: &AttachmentPointer) -> bool {
    ptr.size.is_none_or(|s| s < IMAGE_AUTO_MAX)
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

/// What the picture of a file the server no longer has says: nothing a click could get.
pub fn gone_label() -> crate::types::MediaLabel {
    crate::types::MediaLabel { text: tr!("Not available").into(),undownloaded: false, progress: None }
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
    let names = if more > 0 { tr!("{} and {} more", first, more) } else { first.to_string() };
    match bytes {
        Some(b) => format!("{names} · {}", size_text(b)),
        None => names,
    }
}

/// A file that runs or changes the system when opened: the official desktop app refuses to send
/// or save these. We only warn. Trailing dots and spaces don't change what Windows opens.
pub fn is_dangerous_file(name: &str) -> bool {
    const KINDS: &[&str] = &[
        "ade", "adp", "apk", "bat", "cab", "chm", "cmd", "com", "cpl", "diagcab", "dll", "dmg", "exe", "hta", "inf", "ins",
        "isp", "jar", "js", "jse", "lib", "lnk", "mde", "mht", "msc", "msi", "msp", "mst", "nsh", "pif", "ps1", "psc1",
        "psm1", "psrc", "reg", "scr", "sct", "settingcontent-ms", "shb", "sys", "vb", "vbe", "vbs", "vxd", "wsc", "wsf",
        "wsh",
    ];
    let name = name.trim_end_matches(['.', ' ']);
    name.rsplit_once('.').is_some_and(|(_, e)| KINDS.contains(&e.to_ascii_lowercase().as_str()))
}

/// What a message's file card shows, taken from its [`file_line`]: the name (with "and N more")
/// and its size.
pub fn file_card(line: &str) -> (String, String) {
    let sized = |s: &str| {
        let n = s.strip_suffix("KB").or_else(|| s.strip_suffix("MB")).or_else(|| s.strip_suffix("GB"));
        n.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    let (names, size) = match line.rsplit_once(" · ") {
        Some((n, s)) if sized(s) => (n, s),
        _ => (line, ""),
    };
    (names.to_string(), size.to_string())
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

pub fn attachment_label(ptr: &AttachmentPointer) -> String {
    let ct = ptr.content_type.as_deref().unwrap_or("");
    if let Some(name) = ptr.file_name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    if ct.starts_with("image/") {
        tr!("Photo").into()
    } else if ct.starts_with("video/") {
        tr!("Video").into()
    } else if ct.starts_with("audio/") {
        if is_voice(ptr) { tr!("Voice message").into() } else { tr!("Audio").into() }
    } else {
        tr!("File").into()
    }
}

/// What an attachment is, for one-line previews (thread list, notifications, pins, replies).
/// Pictures and videos go by their kind: their names are mostly camera numbers. Files keep
/// their name, which says what they are.
pub fn kind_label(ptr: &AttachmentPointer) -> String {
    let ct = ptr.content_type.as_deref().unwrap_or("");
    if ct == "image/gif" {
        "GIF".into()
    } else if ct.starts_with("image/") {
        tr!("Photo").into()
    } else if ct.starts_with("video/") {
        tr!("Video").into()
    } else if ct.starts_with("audio/") {
        if is_voice(ptr) { tr!("Voice message").into() } else { tr!("Audio").into() }
    } else {
        attachment_label(ptr)
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
pub const GRID_SHOWN: usize = 5;

/// Where a grid of `count` images (2 or more) puts them: x, y, width and height in logical px,
/// with a 1px gap. As in Signal's apps, the first ones are the largest.
fn grid_cells(count: usize) -> Vec<(u32, u32, u32, u32)> {
    grid_cells_at(count, photo_choice(), room())
}

fn grid_cells_at(count: usize, choice: u8, room: u32) -> Vec<(u32, u32, u32, u32)> {
    // 300 wide at the medium size, and its 1px gap
    let width = (photo_px(300, choice) + 1).min(room);
    match count {
        0 | 1 => Vec::new(),
        2 => split(width, &[1, 1]).into_iter().map(|(x, w)| (x, 0, w, w)).collect(),
        3 => {
            // a large one, and two stacked beside it as tall as it
            let [(bx, bw), (sx, sw)] = split(width, &[2, 1])[..] else { unreachable!() };
            let side = split(2 * sw + 1, &[1, 1]);
            let mut cells = vec![(bx, 0, bw, 2 * sw + 1)];
            cells.extend(side.into_iter().map(|(y, h)| (sx, y, sw, h)));
            cells
        }
        4 => {
            let half = split(width, &[1, 1]);
            half.iter().flat_map(|&(y, h)| half.iter().map(move |&(x, w)| (x, y, w, h))).collect()
        }
        _ => {
            let (top, bottom) = (split(width, &[1, 1]), split(width, &[1, 1, 1]));
            let (top_h, bottom_h) = (top[0].1, bottom[0].1);
            let mut cells: Vec<_> = top.into_iter().map(|(x, w)| (x, 0, w, top_h)).collect();
            cells.extend(bottom.into_iter().map(|(x, w)| (x, top_h + 1, w, bottom_h)));
            cells
        }
    }
}

/// `total` px cut into parts of these weights, with a 1px gap between them: (start, length).
fn split(total: u32, weights: &[u32]) -> Vec<(u32, u32)> {
    let free = u64::from(total - (weights.len() as u32 - 1));
    let sum: u64 = weights.iter().map(|&w| u64::from(w)).sum();
    let mut before = 0u64;
    let edge = |weight: u64| ((free * weight * 2 + sum) / (2 * sum)) as u32;
    weights
        .iter()
        .enumerate()
        .map(|(n, &w)| {
            let (start, end) = (edge(before), edge(before + u64::from(w)));
            before += u64::from(w);
            (start + n as u32, end - start)
        })
        .collect()
}

/// Width and height of cell `n` in a grid of `count` images (logical px).
pub fn grid_cell_size(count: usize, n: usize) -> (u32, u32) {
    grid_cells(count).get(n).map_or((0, 0), |c| (c.2, c.3))
}

/// Makes `m` show `count` images as a grid, if that is 2 or more. `key` gets the index of each
/// image shown and the size of its cell, and returns its key in the UI thumbnail cache.
pub fn set_photo_grid(m: &mut MsgDto, count: usize, mut key: impl FnMut(usize, u32, u32) -> String) {
    // no pictures: what else the row shows (a sticker) keeps its size
    if count == 0 {
        return;
    }
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
            gif: false,
        })
        .collect();
    m.img_w = cells.iter().map(|c| c.0 + c.2).max().unwrap_or(0);
    m.img_h = cells.iter().map(|c| c.1 + c.3).max().unwrap_or(0);
}

/// Makes `m` show one picture as a grid of one cell, `size` big (logical px). Everything a
/// picture does (marks, labels, playing, clicks) is the cell's, so a lone picture and a grid of
/// them can't differ.
pub fn set_single_photo(m: &mut MsgDto, key: String, size: (u32, u32)) {
    m.cells = vec![crate::types::CellDto { key, x: 0, y: 0, w: size.0, h: size.1, more: 0, video: false, gif: false }];
    (m.img_w, m.img_h) = size;
}

/// Whether the file may animate (the file says whether it does only once it's read).
fn may_animate(ptr: &AttachmentPointer) -> bool {
    matches!(ptr.content_type.as_deref(), Some("image/gif" | "image/apng" | "image/webp"))
}

/// The shorter side of a lone photo, at the least: a smaller one is easy to miss, and its marks
/// (download, size) need the room. As small as a grid's smallest cell.
const PHOTO_MIN: u32 = 100;
/// The narrowest bubble a photo with a caption gets (`flush` in `ui/bubble.slint`).
const CAPTIONED_MIN_WIDTH: u32 = 200;

/// Whether the picture of `m` shares its bubble with a caption: the bubble is then at least
/// `CAPTIONED_MIN_WIDTH` wide and the picture as wide as the bubble (`flush` in the UI).
pub fn captioned(m: &MsgDto) -> bool {
    !m.text.is_empty() && m.quote.is_empty()
}

/// Size a lone photo of `w` x `h` is shown at (logical px): scaled into the box for its
/// orientation at the chosen size and no wider than the room, never up, unless it is smaller
/// than the least it is shown at. An extreme shape is cut to the box.
pub fn fit(w: Option<u32>, h: Option<u32>, captioned: bool) -> (u32, u32) {
    fit_at(w, h, captioned, photo_choice(), room())
}

fn fit_at(w: Option<u32>, h: Option<u32>, captioned: bool, choice: u8, room: u32) -> (u32, u32) {
    // a box narrowed by the room keeps its shape
    let sized = |(w, h): (u32, u32)| {
        let (w, h) = (photo_px(w, choice), photo_px(h, choice));
        if w <= room { (w, h) } else { (room, ((u64::from(h) * u64::from(room) * 2 + u64::from(w)) / (2 * u64::from(w))) as u32) }
    };
    let (Some(w), Some(h)) = (w, h) else { return sized(PHOTO_BOX_WIDE) };
    if w == 0 || h == 0 {
        return sized(PHOTO_BOX_WIDE);
    }
    let (bw, bh) = sized(if h > w { PHOTO_BOX_TALL } else { PHOTO_BOX_WIDE });
    let (w, h) = (w as f64, h as f64);
    let into_box = (bw as f64 / w).min(bh as f64 / h).min(1.0);
    // the bubble's width is the UI's, at any size
    let min_w = if captioned { CAPTIONED_MIN_WIDTH } else { 0 };
    let least = (photo_px(PHOTO_MIN, choice) as f64 / w.min(h)).max(min_w as f64 / w);
    let s = into_box.max(least);
    let side = |v: f64, most: u32| ((v * s).round() as u32).clamp(1, most);
    let (w, h) = (side(w, bw), side(h, bh));
    (w.max(min_w), h)
}

/// Attachments: one photo or video alone, several in a grid, the rest as a file line.
pub(super) fn set_attachments(
    m: &mut MsgDto,
    dm: &DataMessage,
    ts: u64,
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
    m.file_danger = files.iter().any(|a| is_dangerous_file(&attachment_label(a)));
    if let [(i, a)] = photos[..] {
        let file_key = attachment_key(a, ts, i);
        // the sender's size may be missing or wrong: the picture is cut to fill the box
        let size = fit(a.width, a.height, captioned(m));
        // the same image may also be shown in a cell of another size
        let key = format!("{file_key}@{}x{}", size.0, size.1);
        set_single_photo(m, key.clone(), size);
        wanted_cells.push(WantedCell { key, file_key, ptr: a.clone(), size });
    } else {
        set_photo_grid(m, photos.len(), |n, w, h| {
            let (i, a) = photos[n];
            let file_key = attachment_key(a, ts, i);
            // the same image may also be shown alone, or in a cell of another size
            let key = format!("{file_key}@{w}x{h}");
            wanted_cells.push(WantedCell { key: key.clone(), file_key, ptr: a.clone(), size: (w, h) });
            key
        });
    }
    for (cell, (_, a)) in m.cells.iter_mut().zip(&photos) {
        cell.video = is_video(a);
        cell.gif = !cell.video && may_animate(a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_files() {
        assert!(is_dangerous_file("setup.EXE"));
        assert!(is_dangerous_file("run.bat."));
        assert!(is_dangerous_file("a.b.ps1 "));
        assert!(!is_dangerous_file("보고서.pdf"));
        assert!(!is_dangerous_file("exe"));
        assert!(!is_dangerous_file("a.exe.txt"));
    }

    #[test]
    fn file_card_takes_its_line_apart() {
        let card = |l: &str| file_card(l);
        assert_eq!(card("보고서.pdf · 2.4MB"), ("보고서.pdf".into(), "2.4MB".into()));
        assert_eq!(card("a.zip and 2 more · 13MB"), ("a.zip and 2 more".into(), "13MB".into()));
        // no size known; a " · " in the name isn't a size
        assert_eq!(card("a · b.txt"), ("a · b.txt".into(), String::new()));
        assert_eq!(card("File"), (tr!("File").into(), String::new()));
    }

    /// A chat wide enough for every box.
    const WIDE: u32 = u32::MAX;

    #[test]
    fn photo_size_caps() {
        let f = |w, h| fit_at(Some(w), Some(h), false, 1, WIDE);
        assert_eq!(f(3000, 3000), (300, 300)); // square
        assert_eq!(f(4000, 3000), (400, 300)); // 4:3 unchanged
        assert_eq!(f(3000, 4000), (300, 400)); // 3:4 is width-limited
        assert_eq!(f(2000, 3000), (300, 450)); // 2:3
        assert_eq!(f(900, 1600), (253, 450)); // 9:16
        assert_eq!(f(1600, 900), (400, 225)); // 16:9 unchanged
        assert_eq!(f(150, 120), (150, 120)); // never upscaled
        assert_eq!(fit_at(None, None, false, 1, WIDE), (400, 300));
    }

    #[test]
    fn a_narrow_chat_shrinks_photos() {
        let f = |w, h, c| fit_at(Some(w), Some(h), c, 2, 280);
        // the box narrows and keeps its shape: nothing is cut that a wide chat shows
        assert_eq!(f(4000, 3000, false), (280, 210));
        assert_eq!(f(2000, 3000, false), (280, 420));
        assert_eq!(fit_at(None, None, false, 2, 280), (280, 210));
        // a photo that fits is left alone
        assert_eq!(f(200, 150, false), (200, 150));
        assert_eq!(f(16, 16, true), (200, 200));
        let cells = grid_cells_at(5, 2, 280);
        assert_eq!(cells.iter().map(|c| c.0 + c.2).max(), Some(280));
    }

    #[test]
    fn small_photos_have_a_least_size() {
        let f = |w, h, c| fit_at(Some(w), Some(h), c, 1, WIDE);
        assert_eq!(f(1, 1, false), (100, 100));
        assert_eq!(f(60, 40, false), (150, 100)); // its shape kept
        // extreme shapes are cut to the box
        assert_eq!(f(1000, 10, false), (400, 100));
        assert_eq!(f(10, 1000, false), (100, 450));
        // a caption's bubble is wider: the picture fills it rather than being stretched to it
        assert_eq!(f(16, 16, true), (200, 200));
        assert_eq!(f(300, 100, true), (300, 100));
        assert_eq!(f(150, 120, true), (200, 160));
        assert_eq!(f(10, 1000, true), (200, 450));
        assert_eq!(fit_at(None, None, true, 1, WIDE), (400, 300));
    }

    #[test]
    fn photos_follow_the_chosen_size() {
        let f = |w, h, c, choice| fit_at(Some(w), Some(h), c, choice, WIDE);
        assert_eq!(f(4000, 3000, false, 0), (300, 225));
        assert_eq!(f(4000, 3000, false, 2), (533, 400));
        assert_eq!(f(3000, 4000, false, 2), (400, 533));
        assert_eq!(fit_at(None, None, false, 0, WIDE), (300, 225));
        // the least size follows; the caption's bubble doesn't
        assert_eq!(f(1, 1, false, 0), (75, 75));
        assert_eq!(f(1, 1, false, 2), (134, 134));
        assert_eq!(f(16, 16, true, 0), (200, 200));
        assert_eq!(f(16, 16, true, 2), (200, 200));
    }

    #[test]
    fn grid_cells_fill_their_box_without_touching() {
        // the medium size, as it always was
        assert_eq!(grid_cells_at(3, 1, WIDE), [(0, 0, 200, 201), (201, 0, 100, 100), (201, 101, 100, 100)]);
        assert_eq!(grid_cells_at(5, 1, WIDE), [(0, 0, 150, 150), (151, 0, 150, 150), (0, 151, 100, 100), (101, 151, 99, 100), (201, 151, 100, 100)]);
        for (count, choice) in (2..8).flat_map(|n| (0..3).map(move |c| (n, c))) {
            let cells = grid_cells_at(count, choice, WIDE);
            assert_eq!(cells.len(), count.min(GRID_SHOWN));
            let width = cells.iter().map(|c| c.0 + c.2).max().unwrap();
            let height = cells.iter().map(|c| c.1 + c.3).max().unwrap();
            assert_eq!(width, [226, 301, 401][choice as usize]);
            let area: u32 = cells.iter().map(|c| c.2 * c.3).sum();
            // what's left of the box is the 1px gaps
            assert!(area < width * height && width * height - area <= 2 * width + height, "{count} {choice}");
            for (i, a) in cells.iter().enumerate() {
                for b in &cells[i + 1..] {
                    let apart = a.0 + a.2 < b.0 + 1 || b.0 + b.2 < a.0 + 1 || a.1 + a.3 < b.1 + 1 || b.1 + b.3 < a.1 + 1;
                    assert!(apart, "{count} {choice}: {a:?} {b:?}");
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
    fn a_big_picture_is_a_picture_that_waits_to_be_asked_for() {
        let photo = |size| AttachmentPointer { content_type: Some("image/jpeg".into()), size, ..Default::default() };
        let big = photo(Some(IMAGE_AUTO_MAX + 1));
        assert!(is_image(&big) && is_media(&big));
        assert!(!auto_fetches_image(&big));
        assert!(auto_fetches_image(&photo(Some(IMAGE_AUTO_MAX - 1))));
        assert!(auto_fetches_image(&photo(None)));
    }

    #[test]
    fn file_lines_say_the_size_of_all_files_together() {
        let file = |name: &str, size| AttachmentPointer { file_name: Some(name.into()), size, ..Default::default() };
        let (a, b) = (file("a.pdf", Some(2 * 1024 * 1024)), file("b.pdf", Some(1024 * 1024)));
        assert_eq!(files_label(&[]), "");
        assert_eq!(files_label(&[&a]), "a.pdf · 2.0MB");
        assert_eq!(files_label(&[&a, &b]), "a.pdf and 1 more · 3.0MB");
        // one size missing: no total
        let unknown = file("c.pdf", None);
        assert_eq!(files_label(&[&a, &unknown]), "a.pdf and 1 more");
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
