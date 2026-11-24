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

/// Where a message's pictures sit, which decides how wide they may be: alone (`bare` in
/// `ui/bubble.slint`), on top of a caption's bubble (`flush`), or inside a bubble's padding (a
/// reply, a file line, a link preview's image), held to that bubble's width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    Bare,
    Captioned,
    Boxed,
}

impl Place {
    /// As the UI tells them apart.
    pub fn of(text: &str, quote: &str, attachment: &str) -> Place {
        if !quote.is_empty() {
            Place::Boxed
        } else if !text.is_empty() {
            Place::Captioned
        } else if !attachment.is_empty() {
            Place::Boxed
        } else {
            Place::Bare
        }
    }
}

/// Where the pictures of `m` sit.
pub fn place(m: &MsgDto) -> Place {
    Place::of(&m.text, &m.quote, &m.attachment)
}

/// The width a picture may take in the chat, logical px (`set_room`): a narrow chat shrinks its
/// pictures rather than cutting them. Alone or captioned, and inside a bubble. 0 until the UI
/// says, no limit.
static ROOM: AtomicU32 = AtomicU32::new(0);
static ROOM_BOXED: AtomicU32 = AtomicU32::new(0);

/// Sets the widths pictures may take in the chat. Whether any picture's size changes: past the
/// widest box at the chosen size, the room makes no difference.
pub fn set_room(px: u32, boxed: u32) -> bool {
    let widest = photo_px(PHOTO_BOX_WIDE.0, photo_choice()).max(LINK_IMAGE_WIDE.0);
    let bound = |r: u32| if r == 0 { widest } else { r.min(widest) };
    // the UI's least pane leaves more; a caption's bubble is never narrower anyway
    let set = |room: &AtomicU32, px: u32| {
        let px = px.max(CAPTIONED_MIN_WIDTH);
        bound(room.swap(px, Ordering::Relaxed)) != bound(px)
    };
    // both, not the first to differ only
    let changed = set(&ROOM, px);
    set(&ROOM_BOXED, boxed) || changed
}

/// Whether the UI has said how wide the chat is.
pub fn has_room() -> bool {
    ROOM.load(Ordering::Relaxed) != 0
}

/// The room set for pictures in `place`, as set: 0 for none.
fn room_set(place: Place) -> u32 {
    match place {
        Place::Boxed => ROOM_BOXED.load(Ordering::Relaxed),
        _ => ROOM.load(Ordering::Relaxed),
    }
}

/// The room for pictures in `place`, or no limit.
fn room(place: Place) -> u32 {
    match room_set(place) {
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

/// An image too small or of unknown size stays small (site logos), and so does a sticker pack's
/// cover, which a wide crop would cut. A wide one is no wider than its bubble's room, keeping
/// its shape.
pub fn link_image_size(w: Option<u32>, h: Option<u32>, sticker_pack: bool) -> (u32, u32) {
    if sticker_pack {
        return LINK_IMAGE_SMALL;
    }
    link_image_size_in(w, h, room(Place::Boxed))
}

fn link_image_size_in(w: Option<u32>, h: Option<u32>, room: u32) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w >= 200 && h >= 100 && w >= h => narrowed(LINK_IMAGE_WIDE, room),
        _ => LINK_IMAGE_SMALL,
    }
}

/// The width a link preview's image of `w` x `h` (logical px, on the card) has with no room
/// limit: the UI follows a resize to it (`Bubble.link-k`).
pub fn link_image_full_width(w: u32) -> u32 {
    if w > LINK_IMAGE_SMALL.0 { LINK_IMAGE_WIDE.0 } else { w }
}

/// A box `w` x `h` no wider than `room`, keeping its shape.
fn narrowed((w, h): (u32, u32), room: u32) -> (u32, u32) {
    if w <= room { (w, h) } else { (room, ((u64::from(h) * u64::from(room) * 2 + u64::from(w)) / (2 * u64::from(w))) as u32) }
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
        let size = link_image_size(a.width, a.height, crate::stickerpack::is_link(url));
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

/// Kinds the official desktop app refuses to send or save.
const SIGNAL_REFUSES: &[&str] = &[
    "ade", "adp", "apk", "bat", "cab", "chm", "cmd", "com", "cpl", "diagcab", "dll", "dmg", "exe", "hta", "inf", "ins", "isp",
    "jar", "js", "jse", "lib", "lnk", "mde", "mht", "msc", "msi", "msp", "mst", "nsh", "pif", "ps1", "psc1", "psm1", "psrc",
    "reg", "scr", "sct", "settingcontent-ms", "shb", "sys", "vb", "vbe", "vbs", "vxd", "wsc", "wsf", "wsh",
];

/// Further kinds the shell runs, installs or mounts when opened, or that carry macros.
const ALSO_DANGEROUS: &[&str] = &[
    "appinstaller", "application", "appref-ms", "appx", "appxbundle", "docm", "gadget", "hlp", "img", "iso", "library-ms",
    "msix", "msixbundle", "ocx", "pptm", "ps1xml", "psd1", "scf", "searchconnector-ms", "url", "vhd", "vhdx", "wsb", "xbap",
    "xll", "xlsm",
];

fn kind_of(name: &str) -> Option<String> {
    sanitize(name).rsplit_once('.').map(|(_, e)| e.to_lowercase())
}

/// A file that runs or changes the system when opened. We only warn. Judged on the name it
/// would be saved under: Windows drops what `sanitize` drops too (trailing dots and spaces).
pub fn is_dangerous_file(name: &str) -> bool {
    kind_of(name).is_some_and(|e| SIGNAL_REFUSES.contains(&e.as_str()) || ALSO_DANGEROUS.contains(&e.as_str()))
}

/// A file the official app won't take from us.
pub fn signal_refuses(name: &str) -> bool {
    kind_of(name).is_some_and(|e| SIGNAL_REFUSES.contains(&e.as_str()))
}

/// Whether saving attachment `ptr` is worth a warning first.
pub fn is_dangerous(ptr: &AttachmentPointer) -> bool {
    is_dangerous_file(&saved_name(ptr))
}

/// A name Windows will write as given: no reserved or invisible characters, no trailing dots
/// or spaces (Windows drops them, so the name would end up different), no device name.
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || crate::link::is_format_mark(c) || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let s = s.trim_matches(|c: char| c.is_whitespace() || c == '.');
    if s.is_empty() {
        return "attachment".into();
    }
    if is_device_name(s) { format!("_{s}") } else { s.to_string() }
}

/// `CON`, `nul.txt`, `COM¹ .log`…: Windows opens the device instead of a file of that name,
/// whatever the extension.
fn is_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    match base.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        b => {
            let mut c = b.chars();
            let head: String = c.by_ref().take(3).collect();
            matches!(head.as_str(), "COM" | "LPT")
                && matches!((c.next(), c.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
        }
    }
}

/// The extension of a picture or video we show as one, from what it says it is: an outside app
/// gets it only under a kind its default app plays, never under the sender's name for it (a
/// "video" named `clip.exe` would run). `None`: not a kind we let the shell open.
fn media_ext(ptr: &AttachmentPointer) -> Option<&'static str> {
    if !is_media(ptr) {
        return None;
    }
    let ct = ptr.content_type.as_deref()?.split(';').next()?.trim().to_ascii_lowercase();
    Some(match ct.as_str() {
        "image/jpeg" => "jpg",
        "image/png" | "image/apng" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/bmp" => "bmp",
        "image/tiff" => "tif",
        "video/mp4" => "mp4",
        "video/x-m4v" => "m4v",
        "video/quicktime" => "mov",
        "video/3gpp" => "3gp",
        "video/webm" => "webm",
        "video/x-msvideo" | "video/avi" => "avi",
        "video/x-matroska" => "mkv",
        _ => return None,
    })
}

/// The name attachment `ptr` is saved or opened under: the sender's, cleaned, or one from its
/// type. A picture or video keeps the sender's stem with the extension of what it's shown as.
pub fn saved_name(ptr: &AttachmentPointer) -> String {
    let given = ptr.file_name.as_deref().filter(|n| !n.is_empty()).map(sanitize);
    if let Some(ext) = media_ext(ptr) {
        let stem = given.as_deref().map_or("attachment", |n| n.rsplit_once('.').map_or(n, |(s, _)| s));
        return sanitize(&format!("{stem}.{ext}"));
    }
    given.unwrap_or_else(|| {
        let ext = ptr
            .content_type
            .as_deref()
            .and_then(mime_guess::get_mime_extensions_str)
            .and_then(|e| e.first().copied())
            .unwrap_or("bin");
        format!("attachment.{ext}")
    })
}

/// The name of the plain copy an outside app opens picture or video `ptr` from, or `None` if
/// it's no kind we let the shell open.
pub fn open_name(ptr: &AttachmentPointer) -> Option<String> {
    media_ext(ptr).map(|_| saved_name(ptr))
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
    if let Some(name) = ptr.file_name.as_deref().map(crate::link::one_line).filter(|n| !n.is_empty()) {
        return name;
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
fn grid_cells(count: usize, place: Place) -> Vec<(u32, u32, u32, u32)> {
    grid_cells_at(count, photo_choice(), room(place))
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

/// Width and height of cell `n` in a grid of `count` images in `place` (logical px).
pub fn grid_cell_size(count: usize, n: usize, place: Place) -> (u32, u32) {
    grid_cells(count, place).get(n).map_or((0, 0), |c| (c.2, c.3))
}

/// Makes `m` show `count` images as a grid, if that is 2 or more. `key` gets the index of each
/// image shown and the size of its cell, and returns its key in the UI thumbnail cache.
pub fn set_photo_grid(m: &mut MsgDto, count: usize, mut key: impl FnMut(usize, u32, u32) -> String) {
    // no pictures: what else the row shows (a sticker) keeps its size
    if count == 0 {
        return;
    }
    let place = place(m);
    let cells = grid_cells(count, place);
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
    // a grid is its box
    let full = grid_cells_at(count, photo_choice(), u32::MAX).iter().map(|c| c.0 + c.2).max().unwrap_or(0);
    (m.img_full_w, m.img_box_w, m.img_room) = (full, full, room_set(place));
}

/// Marks the lone photo of `m`, `w` x `h` as the sender said and fitted by `fit`, for following
/// the chat's width (`MsgDto::img_full_w`).
pub fn follow_room(m: &mut MsgDto, w: Option<u32>, h: Option<u32>) {
    let (choice, place) = (photo_choice(), place(m));
    m.img_full_w = fit_at(w, h, place, choice, u32::MAX).0;
    m.img_box_w = photo_px(photo_box(w, h).0, choice);
    m.img_room = room_set(place);
}

/// The box at the medium size a photo of `w` x `h` is fitted into: by its orientation, landscape
/// when that isn't known.
fn photo_box(w: Option<u32>, h: Option<u32>) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > w => PHOTO_BOX_TALL,
        _ => PHOTO_BOX_WIDE,
    }
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

/// Size a lone photo of `w` x `h` in `place` is shown at (logical px): scaled into the box for
/// its orientation at the chosen size and no wider than the room, never up, unless it is smaller
/// than the least it is shown at. An extreme shape is cut to the box. With a caption, the bubble
/// is at least `CAPTIONED_MIN_WIDTH` wide and the picture as wide as the bubble.
pub fn fit(w: Option<u32>, h: Option<u32>, place: Place) -> (u32, u32) {
    fit_at(w, h, place, photo_choice(), room(place))
}

fn fit_at(w: Option<u32>, h: Option<u32>, place: Place, choice: u8, room: u32) -> (u32, u32) {
    let captioned = place == Place::Captioned;
    let sized = |(w, h): (u32, u32)| narrowed((photo_px(w, choice), photo_px(h, choice)), room);
    let (Some(w), Some(h)) = (w, h) else { return sized(PHOTO_BOX_WIDE) };
    if w == 0 || h == 0 {
        return sized(PHOTO_BOX_WIDE);
    }
    let (bw, bh) = sized(photo_box(Some(w), Some(h)));
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
    m.file_danger = files.iter().any(|a| is_dangerous(a));
    m.media_danger = photos.iter().any(|(_, a)| is_dangerous(a));
    if let [(i, a)] = photos[..] {
        let file_key = attachment_key(a, ts, i);
        // the sender's size may be missing or wrong: the picture is cut to fill the box
        let size = fit(a.width, a.height, place(m));
        // the same image may also be shown in a cell of another size
        let key = format!("{file_key}@{}x{}", size.0, size.1);
        set_single_photo(m, key.clone(), size);
        follow_room(m, a.width, a.height);
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
        // saved as `a.exe`: Windows and `sanitize` drop what trails
        assert!(is_dangerous_file("a.exe\u{3000}"));
        assert!(is_dangerous_file("a.exe . "));
        assert!(is_dangerous_file("Report.URL") && is_dangerous_file("disk.iso") && is_dangerous_file("book.xlsm"));
        assert!(!signal_refuses("disk.iso") && signal_refuses("setup.exe"));
    }

    #[test]
    fn sanitized_names_are_written_as_given() {
        assert_eq!(sanitize(" 보고서.pdf. "), "보고서.pdf");
        assert_eq!(sanitize("a:b/c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize("photo\u{202e}gpj.exe"), "photo_gpj.exe");
        assert_eq!(sanitize("\u{2066}x\u{2069}\u{200f}\u{061c}\u{feff}.pdf"), "_x____.pdf");
        assert_eq!(sanitize("a.exe\u{3000}"), "a.exe");
        assert_eq!(sanitize(" . "), "attachment");
        for dev in ["CON", "con.txt", "Nul.tar.gz", "aux .log", "COM1", "lpt9.doc", "COM0", "com¹.txt", "LPT³", "CONIN$", "conout$.x"] {
            assert_eq!(sanitize(dev), format!("_{dev}"), "{dev}");
        }
        for fine in ["CONSOLE.txt", "COM10", "LPT", "nul_x", "prnt.pdf", "COM¹0"] {
            assert_eq!(sanitize(fine), fine);
        }
    }

    fn ptr(content_type: &str, name: Option<&str>) -> AttachmentPointer {
        AttachmentPointer { content_type: Some(content_type.into()), file_name: name.map(Into::into), ..Default::default() }
    }

    #[test]
    fn media_opens_only_as_what_it_shows() {
        assert_eq!(open_name(&ptr("video/mp4", Some("clip.exe"))).as_deref(), Some("clip.mp4"));
        assert_eq!(open_name(&ptr("image/png", Some("cat.png"))).as_deref(), Some("cat.png"));
        assert_eq!(open_name(&ptr("image/jpeg", Some("a.b.scr"))).as_deref(), Some("a.b.jpg"));
        assert_eq!(open_name(&ptr("video/QuickTime; codecs=x", None)).as_deref(), Some("attachment.mov"));
        assert_eq!(open_name(&ptr("video/mp4", Some("con.exe"))).as_deref(), Some("_con.mp4"));
        assert_eq!(open_name(&ptr("video/mp4", Some("x\u{202e}4pm.exe"))).as_deref(), Some("x_4pm.mp4"));
        // a kind we don't let the shell open, and never a file
        assert_eq!(open_name(&ptr("video/x-ms-wmv", Some("a.wmv"))), None);
        assert_eq!(open_name(&ptr("application/pdf", Some("a.pdf"))), None);
        // saving keeps the shown kind too; what it can't be shown as is judged on its own name
        assert_eq!(saved_name(&ptr("video/mp4", Some("clip.exe"))), "clip.mp4");
        assert!(!is_dangerous(&ptr("video/mp4", Some("clip.exe"))));
        assert!(is_dangerous(&ptr("video/x-ms-wmv", Some("clip.exe"))));
        assert!(is_dangerous(&ptr("application/x-msdownload", None)));
        assert_eq!(saved_name(&ptr("application/pdf", Some("a.pdf"))), "a.pdf");
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

    /// With a caption or alone.
    fn at(captioned: bool) -> Place {
        if captioned { Place::Captioned } else { Place::Bare }
    }

    #[test]
    fn photo_size_caps() {
        let f = |w, h| fit_at(Some(w), Some(h), Place::Bare, 1, WIDE);
        assert_eq!(f(3000, 3000), (300, 300)); // square
        assert_eq!(f(4000, 3000), (400, 300)); // 4:3 unchanged
        assert_eq!(f(3000, 4000), (300, 400)); // 3:4 is width-limited
        assert_eq!(f(2000, 3000), (300, 450)); // 2:3
        assert_eq!(f(900, 1600), (253, 450)); // 9:16
        assert_eq!(f(1600, 900), (400, 225)); // 16:9 unchanged
        assert_eq!(f(150, 120), (150, 120)); // never upscaled
        assert_eq!(fit_at(None, None, Place::Bare, 1, WIDE), (400, 300));
    }

    #[test]
    fn a_narrow_chat_shrinks_photos() {
        let f = |w, h, c| fit_at(Some(w), Some(h), at(c), 2, 280);
        // the box narrows and keeps its shape: nothing is cut that a wide chat shows
        assert_eq!(f(4000, 3000, false), (280, 210));
        assert_eq!(f(2000, 3000, false), (280, 420));
        assert_eq!(fit_at(None, None, Place::Bare, 2, 280), (280, 210));
        // a photo that fits is left alone
        assert_eq!(f(200, 150, false), (200, 150));
        assert_eq!(f(16, 16, true), (200, 200));
        let cells = grid_cells_at(5, 2, 280);
        assert_eq!(cells.iter().map(|c| c.0 + c.2).max(), Some(280));
        // a link preview's wide image too; a small one stays as it is
        assert_eq!(link_image_size_in(Some(1200), Some(630), 240), (240, 126));
        assert_eq!(link_image_size_in(Some(1200), Some(630), WIDE), LINK_IMAGE_WIDE);
        assert_eq!(link_image_size_in(Some(64), Some(64), 240), LINK_IMAGE_SMALL);
        assert_eq!(link_image_size(Some(512), Some(512), true), LINK_IMAGE_SMALL);
    }

    #[test]
    fn pictures_sit_where_the_bubble_puts_them() {
        assert_eq!(Place::of("", "", ""), Place::Bare);
        assert_eq!(Place::of("caption", "", "a.zip"), Place::Captioned);
        assert_eq!(Place::of("caption", "quoted", ""), Place::Boxed);
        assert_eq!(Place::of("", "", "a.zip"), Place::Boxed);
    }

    #[test]
    fn small_photos_have_a_least_size() {
        let f = |w, h, c| fit_at(Some(w), Some(h), at(c), 1, WIDE);
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
        assert_eq!(fit_at(None, None, Place::Captioned, 1, WIDE), (400, 300));
    }

    #[test]
    fn photos_follow_the_chosen_size() {
        let f = |w, h, c, choice| fit_at(Some(w), Some(h), at(c), choice, WIDE);
        assert_eq!(f(4000, 3000, false, 0), (300, 225));
        assert_eq!(f(4000, 3000, false, 2), (533, 400));
        assert_eq!(f(3000, 4000, false, 2), (400, 533));
        assert_eq!(fit_at(None, None, Place::Bare, 0, WIDE), (300, 225));
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
