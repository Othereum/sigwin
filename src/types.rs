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
}

/// reader photos drawn next to a message; the rest is a "+N"
pub const MAX_READER_PHOTOS: usize = 3;

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
    /// (emoji, count)
    pub reactions: Vec<(String, u32)>,
    /// the message is only 1..=5 emoji: shown large, without a bubble
    pub big_emoji: bool,
    pub attachment: String,
    /// key into the UI thumbnail cache, empty when the message has no inline image
    pub image_key: String,
    pub img_w: u32,
    pub img_h: u32,
    pub deleted: bool,
    pub day_sep: String,
    pub system: bool,
    pub sticker: bool,
    /// who has the message (outgoing only), shown when the status is clicked
    pub receipt_info: String,
    /// sent by us, not yet confirmed by the server (or failed)
    pub pending: bool,
    /// our own reaction on this message, empty if none
    pub my_reaction: String,
    /// ours, text only, young enough to edit / delete for everyone
    pub can_edit: bool,
    pub can_delete: bool,
    /// "@name" byte ranges in `text` (drawn highlighted) and who they mention (ACI uuid)
    pub mentions: Vec<(usize, usize, String)>,
    /// text style byte ranges in `text`: (start, end, `view::STYLE_*`)
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scroll {
    Bottom,
    KeepIfNotAtBottom,
    Older,
    /// leave the scroll position alone (a `JumpTo` follows)
    Keep,
}

pub enum Event {
    Loading(String),
    NeedLink,
    LinkQr(String),
    LinkStatus(String),
    Ready,
    Threads(Vec<ThreadDto>),
    Header { name: String, sub: String },
    /// who can be @mentioned in the open thread: (uuid, name); empty outside groups
    Members(Vec<crate::mention::Member>),
    Messages { key: String, items: Vec<MsgDto>, can_older: bool, scroll: Scroll },
    /// pinned messages of the open thread, newest first
    Pins(Vec<PinDto>),
    /// scroll the message list to row `id` (already in the last `Messages`) and flash it
    JumpTo(String),
    Thumb(Arc<Thumb>),
    /// answer to `StickerFrames` / `PickerFrames`
    Frames(Frames),
    /// sticker picker images (grid cells, pack covers), batched
    StickerThumbs(Vec<Thumb>),
    /// round photo of a thread (key = thread key); width 0 = none
    Avatar(Arc<Thumb>),
    Status(String),
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
    /// `mentions`: members picked in the composer, their "@name"s become mentions
    Send { text: String, files: Vec<PathBuf>, quote: Option<String>, mentions: Vec<crate::mention::Member> },
    OpenAttachment(String),
    Search(String),
    WindowFocus(bool),
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
    /// pin message `id` (row) for `secs` seconds, None = until unpinned
    Pin { id: String, secs: Option<u32> },
    /// unpin; `id` is the pinned message's sent timestamp (`PinDto::id`)
    Unpin(String),
    /// show the pinned message with this sent timestamp, loading older rows if needed
    JumpTo(String),
    /// the composer was edited; false when it became empty
    Composing(bool),
    /// the settings page opened: send Account and Storage
    SettingsOpened,
    /// empty the thumbnail cache (rebuilt from attachments and stored photos on demand)
    ClearThumbs,
}
