//! Plain `Send` data exchanged between the backend thread and the UI thread.

use std::path::PathBuf;
use std::sync::Arc;

use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::libsignal_service::zkgroup::GroupMasterKeyBytes;
use presage::store::Thread;

/// Stable string form of a [`Thread`], used as UI key and in persisted state.
pub fn thread_key(thread: &Thread) -> String {
    match thread {
        Thread::Contact(id) => format!("c:{}", id.raw_uuid()),
        Thread::Group(key) => format!("g:{}", hex::encode(key)),
    }
}

pub fn parse_thread_key(key: &str) -> Option<Thread> {
    if let Some(uuid) = key.strip_prefix("c:") {
        let uuid = uuid.parse().ok()?;
        Some(Thread::Contact(ServiceId::Aci(Aci::from_uuid_bytes(
            *uuid::Uuid::as_bytes(&uuid),
        ))))
    } else if let Some(hexkey) = key.strip_prefix("g:") {
        let bytes: GroupMasterKeyBytes = hex::decode(hexkey).ok()?.try_into().ok()?;
        Some(Thread::Group(bytes))
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ThreadDto {
    pub key: String,
    pub name: String,
    pub snippet: String,
    pub time: String,
    /// last activity (ms), 0 = none; `time` is derived from it
    pub ts: u64,
    pub unread: u32,
    pub is_group: bool,
    pub mute: Mute,
    /// Sigwin's per-thread notification choices (None = the default)
    pub prefs: ThreadPrefs,
    /// the phone's "notify for mentions while muted" (groups only; None = not set there)
    pub phone_mentions: Option<bool>,
}

/// A thread's own notification settings; each None follows the next layer (the phone's
/// value where it has one, else the app setting).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ThreadPrefs {
    /// what toasts show, like `Settings::preview`
    pub preview: Option<u8>,
    /// toasts play a sound, like `Settings::sound`
    pub sound: Option<bool>,
    /// while notifications are off: @mentions of us still notify
    pub mentions: Option<bool>,
    /// ... replies to our messages still notify
    pub replies: Option<bool>,
    /// ... the chat counts in the badge
    pub badge: Option<bool>,
}

impl ThreadPrefs {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One of `ThreadPrefs` changed in the thread's settings dialog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pref {
    Preview(Option<u8>),
    Mentions(Option<bool>),
    Replies(Option<bool>),
    Badge(Option<bool>),
    Sound(Option<bool>),
}

impl ThreadPrefs {
    pub fn set(&mut self, p: Pref) {
        match p {
            Pref::Preview(v) => self.preview = v.map(|v| v.min(2)),
            Pref::Mentions(v) => self.mentions = v,
            Pref::Replies(v) => self.replies = v,
            Pref::Badge(v) => self.badge = v,
            Pref::Sound(v) => self.sound = v,
        }
    }
}

/// Notifications off for a thread until (ms since the epoch); 0 = on.
pub const MUTE_ALWAYS: u64 = u64::MAX;

/// A thread's "notifications off" state: Sigwin's own choice, else the phone's.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Mute {
    /// in effect: off until this (`MUTE_ALWAYS`: until turned back on), 0 = on
    pub until: u64,
    /// set in Sigwin; otherwise `until` is the phone's
    pub overridden: bool,
    /// the phone's value (Storage Service)
    pub phone: u64,
}

impl Mute {
    pub fn active(&self, now: u64) -> bool {
        self.until > now
    }
}

/// What "notifications off" a thread gets from Sigwin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MuteSet {
    /// drop Sigwin's choice, use the phone's
    Phone,
    On,
    /// off for this many seconds
    For(u64),
    Always,
}

/// "오후 3:00까지 알림 꺼짐", for the thread menu. Android sends `i64::MAX` for "always".
pub fn mute_text(until: u64, now: u64) -> String {
    if until <= now {
        "알림 켜짐".into()
    } else if until >= i64::MAX as u64 || until - now > 100 * 365 * 86_400_000 {
        "알림 꺼짐".into()
    } else {
        format!("{}까지 알림 꺼짐", crate::view::time::time_until(until))
    }
}

/// reader photos drawn next to a message; the rest is a "+N"
pub const MAX_READER_PHOTOS: usize = 3;

/// One image of a message's grid; position and size in logical px within the grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellDto {
    /// key into the UI thumbnail cache
    pub key: String,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// images not shown, covered by this cell's "+N"
    pub more: u32,
    pub video: bool,
}

/// What a click on a message's image or grid cell landed on.
#[derive(Debug, PartialEq)]
pub enum ClickedMedia {
    /// its place among the message's images, as the viewer counts them
    Image(usize),
    /// its place among the message's videos
    Video(usize),
}

/// A link preview: on a message, or in the composer before sending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkCard {
    pub url: String,
    pub title: String,
    pub description: String,
    pub domain: String,
    /// key into the UI thumbnail cache, empty when there is no image
    pub image_key: String,
    pub img_w: u32,
    pub img_h: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MsgDto {
    pub id: String,
    pub sender: String,
    pub sender_key: String,
    pub text: String,
    pub time: String,
    pub outgoing: bool,
    pub show_sender: bool,
    /// incoming in a group: the row keeps a column for the sender's photo
    pub avatar_col: bool,
    /// last of a run from one sender: the photo is drawn on this row
    pub show_avatar: bool,
    pub status: String,
    pub quote: String,
    /// who wrote the quoted message ("나" for us), empty if unknown
    pub quote_author: String,
    /// sent timestamp of the quoted message, for jumping to it
    pub quote_id: String,
    /// key into the UI thumbnail cache for the quoted message's picture, empty if none
    pub quote_image_key: String,
    /// (emoji, count, who: names joined by ", ", "나" for us)
    pub reactions: Vec<(String, u32, String)>,
    /// the message is only 1..=5 emoji: shown large, without a bubble
    pub big_emoji: bool,
    pub attachment: String,
    /// key into the UI thumbnail cache, empty when the message has no inline image
    pub image_key: String,
    pub img_w: u32,
    pub img_h: u32,
    /// the preview of a link in the text, as the sender's app made it
    pub link: Option<LinkCard>,
    /// several images: the grid drawn instead of `image_key`, which is its first cell's
    pub cells: Vec<CellDto>,
    /// `image_key` is the picture of a video, not a grid
    pub video: bool,
    pub deleted: bool,
    pub day_sep: String,
    /// the first message that was unread when the thread was opened (or when news came in
    /// after everything was read): a line above it says so
    pub unread_divider: bool,
    pub system: bool,
    pub sticker: bool,
    /// who has the message (outgoing only), shown when the status is clicked
    pub receipt_info: String,
    /// sent by us, not yet confirmed by the server (or failed)
    pub pending: bool,
    /// pending and the send failed: can be sent again or discarded
    pub failed: bool,
    /// our own reaction on this message, empty if none
    pub my_reaction: String,
    /// ours, text only, young enough to edit / delete for everyone
    pub can_edit: bool,
    pub can_delete: bool,
    /// "@name" byte ranges in `text` (drawn highlighted) and who they mention (ACI uuid)
    pub mentions: Vec<(usize, usize, String)>,
    /// text style byte ranges in `text`: (start, end, `view::text::STYLE_*`)
    pub styles: Vec<(usize, usize, u8)>,
    /// pinned in this thread (a pin marker next to the time)
    pub pinned: bool,
    /// collapsed with the next row (`view::mark_runs`): the footer's time / status is redundant
    pub hide_time: bool,
    /// same run as the row above / below (Signal's grouping): flatter corners, tighter spacing
    pub collapse_above: bool,
    pub collapse_below: bool,
    /// sort timestamp (ms), what runs are measured by
    pub ts: u64,
    pub edited: bool,
    /// who has read it (outgoing only): (uuid, name), by name
    pub readers: Vec<(String, String)>,
    /// the text is cut short: "더 읽기" shows more
    pub more: bool,
}

impl MsgDto {
    /// What a click on its picture, or on cell `index` of its grid, landed on.
    pub fn clicked_media(&self, index: usize) -> ClickedMedia {
        if self.cells.is_empty() {
            return if self.video { ClickedMedia::Video(0) } else { ClickedMedia::Image(index) };
        }
        let before = self.cells.iter().take(index).filter(|c| c.video).count();
        if self.cells.get(index).is_some_and(|c| c.video) { ClickedMedia::Video(before) } else { ClickedMedia::Image(index - before) }
    }
}

/// A pinned message of the open thread, for the bar above the message list.
#[derive(Debug, Clone, Default)]
pub struct PinDto {
    /// sent timestamp of the pinned message (may be outside the loaded window)
    pub id: String,
    pub sender: String,
    pub text: String,
}

/// Thumbnail keys with this prefix belong to the sticker picker, not the message list.
pub const STICKER_THUMB_PREFIX: &str = "sp_";

/// Window scale factor x1000, set by the UI once the window exists.
static UI_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

pub fn set_ui_scale(scale: f32) {
    if scale.is_finite() && scale > 0.0 {
        UI_SCALE.store((scale * 1000.0).round() as u32, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Logical length -> physical pixels.
pub fn physical_px(logical: u32) -> u32 {
    let s = UI_SCALE.load(std::sync::atomic::Ordering::Relaxed);
    ((logical * s + 500) / 1000).max(1)
}

#[derive(Debug, Clone)]
pub struct PackDto {
    /// hex pack id
    pub id: String,
    pub title: String,
    pub installed: bool,
    /// thumbnail key of the cover sticker (installed packs only)
    pub cover_key: String,
}

#[derive(Debug, Clone)]
pub struct StickerDto {
    pub key: String,
    pub id: u32,
    pub emoji: String,
    /// plays on hover (`Command::PickerFrames`)
    pub animated: bool,
}

pub struct Thumb {
    pub key: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What a picture says about its file.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct MediaLabel {
    /// a video's length, or the size of what isn't downloaded yet. Empty: none.
    pub text: String,
    /// a click downloads it first, which a video's mark shows
    pub undownloaded: bool,
    /// percent, while it is on its way and that's being followed
    pub progress: Option<u8>,
}

/// An image for the viewer window.
pub struct ViewerImage {
    pub id: String,
    pub index: usize,
    /// images in the message
    pub count: usize,
    pub pixels: ViewerPixels,
}

/// A decoded image for the viewer window.
pub struct ViewerPixels {
    /// upright, at full size: opening one almost always leads to zooming it
    pub original: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    /// shrunk to the size asked for; `None` when the original already fits
    pub fitted: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    /// it can't have transparent pixels
    pub opaque: bool,
}

impl From<sigwin_thumb::ViewImage> for ViewerPixels {
    fn from(view: sigwin_thumb::ViewImage) -> Self {
        Self { fitted: view.fitted, original: view.original, opaque: view.opaque }
    }
}

impl ViewerPixels {
    pub fn full(&self) -> (u32, u32) {
        (self.original.width(), self.original.height())
    }

    /// What a newly shown image starts as. Takes the fitted one: only `source` holds it, so it
    /// goes when that shows the original.
    pub fn take_first(&mut self) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
        self.fitted.take().unwrap_or_else(|| self.original.clone())
    }
}

/// Frames of all playing stickers in the message list together; the least recently shown
/// sticker beyond this goes back to its still image.
pub const ANIM_BUDGET: usize = 24 << 20;

/// An animated sticker's frames (all `width` x `height` RGBA) and how long each shows (µs).
/// No frames: the image is a still.
pub struct Frames {
    pub key: String,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<Vec<u8>>,
    pub delays_us: Vec<u32>,
}

impl Frames {
    pub fn still(key: String) -> Self {
        Frames { key, width: 0, height: 0, frames: Vec::new(), delays_us: Vec::new() }
    }

    /// Decodes `bytes` with each frame fitted to `fit` logical px.
    pub fn decode(key: String, bytes: &[u8], fit: u32, max_bytes: usize) -> Self {
        match sigwin_thumb::animation(bytes, physical_px(fit), max_bytes) {
            Ok(Some(a)) => Frames {
                key,
                width: a.frames[0].width(),
                height: a.frames[0].height(),
                delays_us: a.delays_us,
                frames: a.frames.into_iter().map(|f| f.into_raw()).collect(),
            },
            Ok(None) => Frames::still(key),
            Err(e) => {
                tracing::debug!("frames {key}: {e}");
                Frames::still(key)
            }
        }
    }
}

/// Which way the message list can load more rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Paging {
    pub older: bool,
    /// the rows stop short of the thread's newest message
    pub newer: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scroll {
    Bottom,
    /// the first unread message (flagged `unread_divider`) near the top of the view, or the
    /// bottom if the rows end sooner
    Unread,
    KeepIfNotAtBottom,
    /// like `KeepIfNotAtBottom`, but news came in while the user was away: from the bottom the
    /// view goes to the unread line (flagged `unread_divider`) instead of past it
    News,
    Older,
    /// leave the scroll position alone (a `JumpTo` follows)
    Keep,
}

pub enum Event {
    /// the emoji we reacted with lately, newest first (the picker's recent row)
    RecentReactions(Vec<String>),
    /// the preview of the composer's link; None: there is none to show
    LinkPreview { url: String, card: Option<LinkCard> },
    Loading(String),
    NeedLink,
    LinkQr(String),
    LinkStatus(String),
    Ready,
    Threads(Vec<ThreadDto>),
    Header { name: String, sub: String },
    /// who can be @mentioned in the open thread: (uuid, name); empty outside groups
    Members(Vec<crate::mention::Member>),
    Messages { key: String, items: Vec<MsgDto>, paging: Paging, scroll: Scroll },
    /// pinned messages of the open thread, newest first
    Pins(Vec<PinDto>),
    /// scroll the message list to row `id` (already in the last `Messages`) and flash it
    JumpTo(String),
    /// scroll the message list to the first unread row `id` (already in the last `Messages`),
    /// its line near the top of the view
    JumpToUnread(String),
    Thumb(Arc<Thumb>),
    /// what the picture known by `key` says about its file
    MediaLabel { key: String, label: MediaLabel },
    /// answer to `ViewImage`
    ViewerImage(ViewerImage),
    /// one of the answers to `ViewerThumbs`: message id, image index, thumbnail
    ViewerThumb(String, usize, Thumb),
    /// answer to `StickerFrames` / `PickerFrames`
    Frames(Frames),
    /// sticker picker images (grid cells, pack covers), batched
    StickerThumbs(Vec<Thumb>),
    /// round photo of a thread (key = thread key); width 0 = none
    Avatar(Arc<Thumb>),
    /// a short message that goes away by itself
    Notice(String),
    /// work in progress: shown until `ProgressDone` or the next notice
    Progress(String),
    ProgressDone,
    /// what's going on with the connection, "" when all is well; not `at_once`: only if it lasts
    Connection { text: String, at_once: bool },
    StickerPacks(Vec<PackDto>),
    StickerGrid { pack: String, items: Vec<StickerDto> },
    /// open the sticker panel on this (installed) pack
    OpenStickerPanel(String),
    /// the clicked sticker's pack is not installed: ask, then InstallPackFromMessage(msg id)
    AskInstallPack(String),
    /// who is typing in the open thread as (uuid, name), sorted; empty when nobody.
    /// Shown as a "…" bubble at the end of the message list.
    Typing { group: bool, who: Vec<(String, String)> },
    /// for the settings page; `receipts` / `typing` are the phone's settings (None = not synced yet)
    Account { name: String, number: String, device: String, receipts: Option<bool>, typing: Option<bool> },
    /// bytes on disk
    Storage { db: u64, attachments: u64, thumbs: u64 },
}

pub enum Command {
    StartLink(String),
    OpenThread(String),
    LoadOlder,
    LoadNewer,
    /// back to the thread's newest messages
    ToLatest,
    /// the message list reached or left its bottom
    AtBottom(bool),
    /// row `id` is fully on screen: it and everything above it has been seen
    ReadUpTo(String),
    /// the jump button, while the thread has unread messages: to the first one, or on to the
    /// bottom once the user has been there
    UnreadButton,
    /// `mentions`: members picked in the composer, their "@name"s become mentions
    /// `link`: the url to preview on the message
    Send { text: String, files: Vec<PathBuf>, quote: Option<String>, mentions: Vec<crate::mention::Member>, link: Option<String> },
    /// the composer's link changed: fetch this one's preview (contacts its site), None cancels
    LinkPreview(Option<String>),
    /// show image `index` of row `id` in the viewer, shrunk to fit in `fit` (physical px).
    /// `from_file`: the file line was clicked; a row with other files then opens them outside.
    ViewImage { id: String, index: usize, fit: (u32, u32), from_file: bool },
    /// open video `index` of row `id` in the default app, downloading it first if needed
    OpenVideo { id: String, index: usize },
    /// Square thumbnails of `px` physical px of all images of a message for the viewer, downloading
    /// the ones still missing: whoever opens an album usually goes through it.
    ViewerThumbs { id: String, px: u32 },
    /// the viewer's "기본 앱으로 열기"
    OpenViewed { id: String, index: usize },
    /// the viewer's "저장"; `owner`: HWND for the file dialog
    SaveViewed { id: String, index: usize, owner: isize },
    /// "더 읽기" on a row: show more of its text
    ReadMore(String),
    Search(String),
    /// the user is working in the window: it has the focus and they are not away. Otherwise
    /// notifications show and nothing is read.
    WindowFocus(bool),
    /// the window isn't the one worked in but shows (not covered, user not away): its news is
    /// read after a while, with no toast for the open thread, and the list stays at the bottom
    WindowWatching(bool),
    Unlink,
    /// sticker panel opened: (re)load the pack list
    StickerPanel,
    OpenPack(String),
    SendSticker { pack: String, id: u32 },
    /// hex id + hex key
    InstallPack { id: String, key: String },
    /// https://signal.art/addstickers/#pack_id=..&pack_key=..
    InstallPackUrl(String),
    /// install the pack of the sticker in this message
    InstallPackFromMessage(String),
    /// a sticker in the message list was clicked
    StickerClicked(String),
    /// the frames of the sticker with this thumbnail key (message list), if it is animated
    StickerFrames(String),
    /// the frames of a picker sticker (hovered), if it is animated
    PickerFrames { pack: String, id: u32 },
    /// react to message `id`; `remove` takes back our reaction `emoji`
    React { id: String, emoji: String, remove: bool },
    /// replace the text of our message `id`
    Edit { id: String, text: String, mentions: Vec<crate::mention::Member> },
    /// delete our message `id` for everyone
    Delete(String),
    /// a failed row: send it again, with the same timestamp
    RetrySend(String),
    /// a failed row: forget it
    DiscardFailed(String),
    /// pin message `id` (row) for `secs` seconds, None = until unpinned
    Pin { id: String, secs: Option<u32> },
    /// unpin; `id` is the pinned message's sent timestamp (`PinDto::id`)
    Unpin(String),
    /// show the pinned message with this sent timestamp, loading older rows if needed
    JumpTo(String),
    /// a toast was clicked: open chat `key` at message `msg` (sent timestamp)
    OpenFromToast { key: String, msg: Option<u64> },
    /// the composer was edited; false when it became empty
    Composing(bool),
    /// the settings page opened: send Account and Storage
    SettingsOpened,
    /// empty the thumbnail cache (rebuilt from attachments and stored photos on demand)
    ClearThumbs,
    /// turn a thread's notifications off / on, or follow the phone again
    SetMute { key: String, set: MuteSet },
    /// change one of a thread's own notification settings
    SetPref { key: String, pref: Pref },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_count_images_and_videos_apart() {
        let cell = |video| CellDto { video, ..Default::default() };
        let grid = MsgDto { cells: vec![cell(true), cell(false), cell(true), cell(false)], ..Default::default() };
        assert_eq!(grid.clicked_media(0), ClickedMedia::Video(0));
        assert_eq!(grid.clicked_media(1), ClickedMedia::Image(0));
        assert_eq!(grid.clicked_media(2), ClickedMedia::Video(1));
        assert_eq!(grid.clicked_media(3), ClickedMedia::Image(1));
        assert_eq!(MsgDto { video: true, ..Default::default() }.clicked_media(0), ClickedMedia::Video(0));
        assert_eq!(MsgDto::default().clicked_media(0), ClickedMedia::Image(0));
    }

    #[test]
    fn mute_text_reads_on_off_and_always() {
        let now = 1_000_000;
        assert_eq!(mute_text(0, now), "알림 켜짐");
        assert_eq!(mute_text(now - 1, now), "알림 켜짐");
        assert_eq!(mute_text(MUTE_ALWAYS, now), "알림 꺼짐");
        // Android's "always"
        assert_eq!(mute_text(i64::MAX as u64, now), "알림 꺼짐");
        assert!(mute_text(now + 3_600_000, now).ends_with("까지 알림 꺼짐"));
        assert!(Mute { until: now + 1, overridden: false, phone: 0 }.active(now));
        assert!(!Mute::default().active(now));
    }
}
