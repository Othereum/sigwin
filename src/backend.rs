//! Backend thread: owns the presage `Manager` (which is `!Send`) on a single-threaded
//! tokio runtime, talks to the UI only through `Command`s in and `Event`s out.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::{channel::oneshot, pin_mut, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
use presage::proto::data_message::{pin_message::PinDuration, Delete, PinMessage, Quote, Reaction, UnpinMessage};
use presage::proto::sync_message::Content as SyncContent;
use presage::proto::{
    receipt_message, typing_message, AttachmentPointer, BodyRange, EditMessage, ReceiptMessage, SyncMessage, TypingMessage,
};
use presage::store::{ContentExt, ContentsStore, StateStore, Store, Thread};
use presage::Manager;
use presage_store_sqlite::{OnNewIdentity, SqliteConnectOptions, SqliteStore};
use serde::{Deserialize, Serialize};
use sqlx::ConnectOptions;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::db::Index;
use crate::types::{
    parse_thread_key, thread_key, Command, Event, Frames, MsgDto, PackDto, Paging, PinDto, ReplyDto, Scroll, StickerDto, Thumb, ThreadDto,
    ANIM_BUDGET, STICKER_THUMB_PREFIX,
};
use crate::view::media::THUMB_MAX;
use crate::view::{self, Kind};

pub(crate) mod files;
pub(crate) mod fixture;
pub(crate) mod linkpreview;
mod gif;
mod mute;
pub(crate) mod notify;
mod ops;
mod receive;
#[cfg(test)]
mod scenarios;
mod send;
mod stickers;
mod threads;
mod typing;
mod window;

use files::*;
use linkpreview::*;
use ops::*;
use send::*;
use stickers::*;

pub(crate) type Mgr = Manager<SqliteStore, Registered>;
pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

const PAGE: u32 = 60;
/// shown rows kept loaded: the window moves through the thread, so this bounds memory, not history
const MAX_WINDOW: usize = 600;
/// shown rows kept above a jump target (`App::jump_to`): well past the older-page prefetch
/// distance (two screens)
const JUMP_MARGIN: usize = 30;
/// presage sends to each group member one by one: skip typing indicators in larger groups
const TYPING_MAX_GROUP: usize = 12;
/// Signal clients resend "typing" every few seconds and give up on silent typists after 15s
const TYPING_RESEND: Duration = Duration::from_secs(10);
const TYPING_IDLE: Duration = Duration::from_secs(5);
const TYPING_TIMEOUT: Duration = Duration::from_secs(15);
/// back at the newest messages this long, the window lets go of what scrolling up loaded
const SETTLE_AFTER: Duration = Duration::from_secs(30);
/// how often to look for expired disappearing messages
const SWEEP_EVERY: Duration = Duration::from_secs(300);
/// Signal keeps at most this many pinned messages per chat; a newer pin pushes out the oldest
const MAX_PINS: usize = 3;
/// most replies the list of a message shows (the newest); a few messages draw hundreds
const MAX_REPLIES: u32 = 50;

pub struct Paths {
    pub db: PathBuf,
    pub key: PathBuf,
    pub state: PathBuf,
    pub attachments: PathBuf,
    pub thumbs: PathBuf,
    /// plain copies of attachments handed to other apps
    pub open: PathBuf,
}

impl Paths {
    pub fn new(dir: &Path) -> Self {
        Self {
            db: dir.join("signal.db3"),
            key: dir.join("db.key"),
            state: dir.join("state.enc"),
            attachments: dir.join("attachments"),
            thumbs: dir.join("thumbs"),
            open: dir.join("open"),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Persist {
    unread: HashMap<String, u32>,
    /// key -> (db last_ts, display ts, text)
    snippets: HashMap<String, (u64, u64, String)>,
    /// receipts already stored before the receipt index existed have been indexed
    #[serde(default)]
    receipts_indexed: bool,
    /// the phone's "typing indicators" setting (configuration sync); None = not received yet
    #[serde(default)]
    typing_indicators: Option<bool>,
    /// the phone's "read receipts" setting (configuration sync); None = not received yet
    #[serde(default)]
    read_receipts: Option<bool>,
    /// thread key -> newest incoming message (sent ts) already marked read
    #[serde(default)]
    read_upto: HashMap<String, u64>,
    /// disappearing-message sweep progress per thread
    #[serde(default)]
    sweep: HashMap<String, SweepState>,
    /// thread key -> what we know about its photo
    #[serde(default)]
    avatars: HashMap<String, crate::avatar::State>,
    /// pin messages stored before the pin index existed have been indexed
    #[serde(default)]
    pins_indexed: bool,
    /// edit messages stored before the edit index existed have been indexed
    #[serde(default)]
    edits_indexed: bool,
    /// reactions stored before the reaction index existed have been indexed
    #[serde(default)]
    reactions_indexed: bool,
    /// replies stored before the reply index existed have been indexed
    #[serde(default)]
    replies_indexed: bool,
    /// layout of the attachment and thumbnail caches on disk (`CACHE_FORMAT` once converted)
    #[serde(default)]
    cache_format: u32,
    /// `snippets` holds nothing from when only the newest rows were searched and edits were ignored
    #[serde(default)]
    snippets_namedle: bool,
    /// `snippets` of groups lead with the sender's name (cleared once when that began)
    #[serde(default)]
    snippets_named: bool,
    /// thread key -> notifications off until (ms; 0 = on, `MUTE_ALWAYS`), picked in Sigwin
    #[serde(default)]
    mute_override: HashMap<String, u64>,
    /// thread key -> the phone's "notifications off until" (Storage Service), nonzero only
    #[serde(default)]
    mute_phone: HashMap<String, u64>,
    /// Storage Service manifest version `mute_phone` was read from
    #[serde(default)]
    storage_version: u64,
    /// groups whose mentions don't notify while muted, on the phone (`dontNotifyForMentionsIfMuted`)
    #[serde(default)]
    phone_no_mentions: HashSet<String>,
    /// what the last Storage Service read collected (`mute::STORAGE_FORMAT`); older reads
    /// are redone even when the manifest is unchanged
    #[serde(default)]
    storage_format: u32,
    /// thread key -> its own notification settings (only threads with any)
    #[serde(default)]
    thread_prefs: HashMap<String, crate::types::ThreadPrefs>,
    /// emoji we reacted with, newest first (the reaction picker's recent row)
    #[serde(default)]
    recent_reactions: Vec<String>,
}

#[derive(Default, Clone, Copy, Serialize, Deserialize)]
struct SweepState {
    /// every message up to here has been looked at
    scanned: u64,
    /// oldest looked-at message whose timer still runs (rescan from here once `due`)
    hold: Option<u64>,
    /// earliest expiry among the held messages
    due: u64,
}

enum Internal {
    Received(Received),
    /// the connection is back: the queued messages come next
    Reconnected,
    ReceiveStopped(String),
    Linked(Result<Mgr, String>),
    Sent(u64, Result<(), String>),
    LinkFetched(String, Option<Box<(Fetched, Vec<crate::types::Thumb>)>>),
    ThumbFailed(String),
    /// a video the user opened has arrived, by its file key: it can show its frame now
    VideoFetched(String),
    /// a long message's full text: (attachment key, text)
    LongText(String, String),
    Notice(String),
    /// the user chose where to save a message's files: download and write them
    SaveFiles(Vec<(PathBuf, AttachmentPointer)>, files::SaveTo),
    /// the files were saved (the last one's path)
    Saved(String, PathBuf),
    Progress(String),
    ProgressDone,
    PackInstalled(Result<String, String>),
    ReceiptsIndexed,
    PinsIndexed,
    Swept { sweep: HashMap<String, SweepState>, changed: Vec<String> },
    Avatars(crate::avatar::Done),
    PhoneMutes(Result<Option<mute::PhoneMutes>, String>),
    /// a fixture operation (`fixture.rs`); the sender hears back once it and the flush after it are done
    Fixture(fixture::FxOp, std::sync::mpsc::Sender<String>),
}

/// A reaction, edit or delete we sent that presage has not stored yet. Shown optimistically.
struct PendingOp {
    ts: u64,
    thread_key: String,
    /// row (window timestamp) of the target message
    row: u64,
    op: Op,
}

enum Op {
    /// `emoji` None: reaction taken back; `target`: sent timestamp of the message
    React { emoji: Option<String>, target: u64 },
    /// `target`: sent timestamp of the edited message
    Edit { text: String, target: u64 },
    /// sent timestamp of the target
    Delete(u64),
    /// already written to the pin index; undone if sending fails
    Pin { target: u64 },
    Unpin { target: u64, author: Uuid, at: u64, expires: u64 },
}

/// We are typing in this thread (typing indicator sent).
struct Composing {
    key: String,
    thread: Thread,
    last_edit: Instant,
    last_sent: Instant,
}

/// A message we sent that presage has not stored yet (it stores only after the server accepts it).
struct Pending {
    ts: u64,
    thread_key: String,
    text: String,
    quote: String,
    quote_author: String,
    quote_id: String,
    attachment: String,
    /// sticker preview: thumbnail key, already emitted to the UI
    image_key: String,
    /// photos being sent (their size as shown, 0 if unknown): shown as photos, not as a file
    photos: Vec<(u32, u32)>,
    /// the link preview going with it; its thumbnail is the composer's, already emitted
    link: Option<crate::types::LinkCard>,
    error: Option<String>,
    /// failed: last message stored at that moment, the row stays right below it
    after_seq: Option<i64>,
    /// what to send, again if it fails
    job: SendJob,
}

struct Open {
    thread: Thread,
    key: String,
    is_group: bool,
    /// group members (ACI uuids), empty for 1:1
    members: Vec<Uuid>,
    /// group identifier (what typing messages carry), None for 1:1
    group_id: Option<[u8; 32]>,
    /// Only rows that show. Receipts and the like are most of a 1:1 thread (and all of some
    /// long stretches), so they would crowd the messages out of a window of any size.
    window: Vec<(u64, Content)>,
    /// where older pages continue, at or above the window's first row; `i64::MAX` when empty
    oldest_seq: i64,
    has_more: bool,
    /// display position of the window's last row when it stops short of the thread's newest
    /// message; None: the window reaches it, and new messages join it
    end_seq: Option<i64>,
    /// newest display position read, shown or not; rows after it are new
    tail_seq: i64,
    /// the first message that was unread when the thread was opened, or when news came in
    /// after everything was read (a window key): the list draws a line above it. It stays put
    /// while the user reads on.
    divider: Option<u64>,
    /// the user has been at the divider's row (read on from it): the jump button goes to the
    /// bottom then
    divider_reached: bool,
    /// the thread just got unread messages: the divider goes where the window shows them
    divider_due: bool,
}

/// A stretch of a thread read into memory: what `Open` keeps about its window.
struct Span {
    window: Vec<(u64, Content)>,
    oldest_seq: i64,
    has_more: bool,
    end_seq: Option<i64>,
    tail_seq: i64,
}

struct App {
    /// set in fixtures only: no `manager`, and what would go out is kept here instead
    offline: Option<fixture::Offline>,
    emit: Emit,
    paths: Paths,
    store: SqliteStore,
    index: Index,
    manager: Option<Mgr>,
    me: Uuid,
    persist: Persist,
    persist_dirty: bool,
    names: HashMap<Uuid, String>,
    threads: Vec<ThreadDto>,
    search: String,
    current: Option<Open>,
    current_dirty: bool,
    threads_dirty: bool,
    /// thumbnails and long texts asked for in the open thread
    /// (shared with the tasks: a video waiting for its turn is dropped once it's no longer in here)
    thumbs_requested: Rc<RefCell<HashSet<String>>>,
    attachment_downloads: files::AttachmentDownloads,
    /// held while making a message's thumbnail
    thumb_gate: Arc<tokio::sync::Semaphore>,
    /// held while downloading a video nobody asked for
    video_gate: Arc<tokio::sync::Semaphore>,
    long: view::text::LongTexts,
    pending: Vec<Pending>,
    /// the composer's link preview
    link_draft: Option<LinkDraft>,
    /// makes each one's thumbnail keys unique
    link_seq: u64,
    /// per thread: resolves when the last message queued for sending is done
    send_tail: HashMap<String, oneshot::Receiver<()>>,
    ops: Vec<PendingOp>,
    /// who is typing in the open thread
    typing: HashMap<Uuid, Instant>,
    typing_shown: String,
    composing: Option<Composing>,
    /// a disappearing message in the open window expires then
    next_expiry: Option<u64>,
    /// a pin of the open thread runs out then
    pin_expiry: Option<u64>,
    sweeping: bool,
    focused: bool,
    /// the window shows without the focus (`Command::WindowWatching`)
    watching: bool,
    /// the window lost rows since the last `emit_messages`
    moved: bool,
    /// when a window that grew lets go again, while the user stays at the newest messages
    settle_at: Option<Instant>,
    /// the message list is at its bottom (always, until the UI says otherwise)
    at_bottom: bool,
    /// row the next `open_thread` opens around (`Command::RestoreAt`)
    restore_at: Option<u64>,
    synced: bool,
    tx: UnboundedSender<Internal>,
    link_task: Option<JoinHandle<()>>,
    recv_task: Option<JoinHandle<()>>,
    /// delivery receipts to send, batched per sender
    pending_receipts: HashMap<ServiceId, Vec<u64>>,
    /// incoming messages seen in the focused open thread: read receipts + read sync to our devices
    pending_reads: Vec<(ServiceId, u64)>,
    /// threads whose photo was loaded (or is queued) this session
    avatars_done: HashSet<String>,
    /// thread key -> (sent ts of the thread list's message, whether it is ours), so the list
    /// reads a message from the store once
    last_outgoing: HashMap<String, (u64, bool)>,
    avatars_running: bool,
    avatar_queue: std::collections::VecDeque<crate::avatar::Want>,
    /// bumped per opened sticker pack: image work for an older grid stops
    grid_gen: Arc<std::sync::atomic::AtomicU64>,
    /// the toast slot (`notify.rs`)
    notes: notify::Notifier,
    /// a thread's "notifications off" runs out then (the list shows it)
    mute_due: Option<u64>,
    mutes_syncing: bool,
    /// stops the GIF that plays (`gif.rs`)
    gif_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
}

pub fn spawn(dir: PathBuf, cmd_rx: UnboundedReceiver<Command>, emit: Emit) -> std::thread::JoinHandle<()> {
    spawn_with(dir, cmd_rx, emit, false)
}

/// A backend with no account and no network, on a scratch data dir (`fixture.rs`).
pub fn spawn_offline(dir: PathBuf, cmd_rx: UnboundedReceiver<Command>, emit: Emit) -> std::thread::JoinHandle<()> {
    spawn_with(dir, cmd_rx, emit, true)
}

fn spawn_with(dir: PathBuf, cmd_rx: UnboundedReceiver<Command>, emit: Emit, offline: bool) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("backend".into())
        // libsignal + presage futures are deep; give them room
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(2)
                .build()
                .expect("tokio runtime");
            let local = tokio::task::LocalSet::new();
            let emit2 = emit.clone();
            local.block_on(&rt, async move {
                if let Err(e) = Box::pin(run(dir, cmd_rx, emit, offline)).await {
                    error!("backend failed: {e:#}");
                    emit2(Event::Loading(format!("오류: {e:#}")));
                }
            });
        })
        .expect("spawn backend thread")
}

/// Loads the DB key (made on first use) and opens the vault: encrypted files use keys derived from
/// it. Also called before the UI reads its drafts, and before this thread starts: no race to make it.
pub fn unlock(dir: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir)?;
    let pass = crate::secret::load_or_create(&Paths::new(dir).key)?;
    let root: [u8; 32] = hex::decode(&pass).ok().and_then(|k| k.try_into().ok()).context("db key is not 32 bytes")?;
    sigwin_vault::init(root);
    Ok(pass)
}

/// Opens the data dir (store, index, state) and builds the `App`, not yet registered or online.
async fn open_app(dir: &Path, emit: Emit) -> anyhow::Result<(App, UnboundedReceiver<Internal>)> {
    std::fs::create_dir_all(dir)?;
    let paths = Paths::new(dir);
    std::fs::create_dir_all(&paths.attachments)?;
    std::fs::create_dir_all(&paths.thumbs)?;
    // PNG cache of the former DirectWrite emoji path; emoji are plain text now
    let _ = std::fs::remove_dir_all(dir.join("emoji"));
    remove_legacy_sticker_thumbs(&paths.thumbs);
    emit(Event::Loading("저장소 여는 중…".into()));

    let pass = unlock(dir)?;
    clear_dir(&paths.open);
    let options = SqliteConnectOptions::new()
        .filename(&paths.db)
        .create_if_missing(true)
        .foreign_keys(true)
        // raw 256-bit key (x'..') skips sqlcipher's PBKDF2, which costs seconds per connection
        .pragma("key", format!("\"x'{pass}'\""))
        // never let sqlx echo statements (they would include the key) into logs
        .disable_statement_logging();
    let store = SqliteStore::open_with_options(options.clone(), OnNewIdentity::Trust)
        .await
        .context("opening database")?;
    let index = Index::open(options).await.context("opening index")?;

    let mut persist: Persist = crate::sealed::read(&paths.state, &dir.join("state.json"))
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let sealed_now = persist.cache_format < CACHE_FORMAT && {
        emit(Event::Loading("첨부파일 캐시 암호화 중…".into()));
        let (att, thumbs) = (paths.attachments.clone(), paths.thumbs.clone());
        let started = std::time::Instant::now();
        match tokio::task::spawn_blocking(move || seal_legacy_files(&att, &thumbs)).await {
            Ok(Ok(())) => {
                info!(ms = started.elapsed().as_millis() as u64, "cache files sealed");
                true
            }
            Ok(Err(e)) => {
                warn!("seal cache files: {e:#}");
                false
            }
            Err(_) => false,
        }
    };
    if sealed_now {
        persist.cache_format = CACHE_FORMAT;
    }

    let (tx, rx) = unbounded_channel();
    let app = App {
        offline: None,
        emit,
        paths,
        store,
        index,
        manager: None,
        me: Uuid::nil(),
        persist,
        persist_dirty: false,
        names: HashMap::new(),
        threads: Vec::new(),
        search: String::new(),
        current: None,
        current_dirty: false,
        threads_dirty: false,
        thumbs_requested: Default::default(),
        attachment_downloads: Default::default(),
        thumb_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        video_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        long: Default::default(),
        pending: Vec::new(),
        link_draft: None,
        link_seq: 0,
        send_tail: HashMap::new(),
        ops: Vec::new(),
        typing: HashMap::new(),
        typing_shown: String::new(),
        composing: None,
        next_expiry: None,
        pin_expiry: None,
        sweeping: false,
        focused: true,
        watching: false,
        moved: false,
        settle_at: None,
        at_bottom: true,
        restore_at: None,
        synced: false,
        tx,
        link_task: None,
        recv_task: None,
        pending_receipts: HashMap::new(),
        pending_reads: Vec::new(),
        avatars_done: HashSet::new(),
        last_outgoing: HashMap::new(),
        avatars_running: false,
        avatar_queue: Default::default(),
        grid_gen: Default::default(),
        notes: Default::default(),
        mute_due: None,
        mutes_syncing: false,
        gif_stop: None,
    };
    if sealed_now {
        app.save_persist();
    }
    Ok((app, rx))
}

/// `offline`: no account and no network, for fixtures (`fixture.rs`).
async fn run(dir: PathBuf, mut cmd_rx: UnboundedReceiver<Command>, emit: Emit, offline: bool) -> anyhow::Result<()> {
    let (mut app, mut rx) = open_app(&dir, emit).await?;

    if offline {
        app.start_offline().await?;
    } else if app.store.is_registered().await {
        match Manager::load_registered(app.store.clone()).await {
            Ok(m) => app.on_registered(m, false).await,
            Err(e) => {
                warn!("load_registered failed: {e}");
                (app.emit)(Event::NeedLink);
                (app.emit)(Event::LinkStatus(format!("저장된 계정을 불러오지 못했습니다: {e}")));
            }
        }
    } else {
        (app.emit)(Event::NeedLink);
        app.start_link("Sigwin".into());
    }

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // every 10s at debug (development); every 10 min at info, so the user's log file keeps a
    // memory trend without being flooded
    let mut memtick = tokio::time::interval(Duration::from_secs(10));
    let mut memticks = 0u32;
    let mut sweeptick = tokio::time::interval(SWEEP_EVERY);
    sweeptick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // relative times ("3분") in the thread list age
    let mut agetick = tokio::time::interval(Duration::from_secs(30));
    agetick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => Box::pin(app.command(cmd)).await,
                None => break,
            },
            Some(ev) = rx.recv() => Box::pin(app.internal(ev)).await,
            _ = tick.tick() => app.flush().await,
            _ = memtick.tick() => {
                if memticks % 60 == 0 {
                    info!("mem: {}", crate::alloc::report());
                } else {
                    debug!("mem: {}", crate::alloc::report());
                }
                memticks = memticks.wrapping_add(1);
            }
            _ = sweeptick.tick() => app.sweep(),
            _ = agetick.tick() => {
                let mut changed = false;
                for t in app.threads.iter_mut().filter(|t| t.ts != 0) {
                    let s = view::time::time_short(t.ts);
                    if s != t.time {
                        t.time = s;
                        changed = true;
                    }
                }
                if changed {
                    app.emit_threads().await;
                }
            }
        }
    }
    app.save_persist();
    Ok(())
}

impl App {
    fn notice(&self, s: impl Into<String>) {
        (self.emit)(Event::Notice(s.into()));
    }
    fn progress(&self, s: impl Into<String>) {
        (self.emit)(Event::Progress(s.into()));
    }
    fn connection(&self, s: impl Into<String>, at_once: bool) {
        (self.emit)(Event::Connection { text: s.into(), at_once });
    }

    async fn on_registered(&mut self, m: Mgr, fresh: bool) {
        self.me = m.registration_data().service_ids.aci;
        self.manager = Some(m);
        (self.emit)(Event::Ready);
        (self.emit)(Event::RecentReactions(self.persist.recent_reactions.clone()));
        self.threads_dirty = true;
        if fresh {
            if let Some(m) = self.manager.as_mut() {
                if let Err(e) = m.request_contacts().await {
                    warn!("request_contacts: {e}");
                }
            }
        }
        self.start_receiving();
        if !self.persist.receipts_indexed {
            self.backfill_receipts();
        }
        if !self.persist.pins_indexed
            || !self.persist.edits_indexed
            || !self.persist.reactions_indexed
            || !self.persist.replies_indexed
        {
            self.backfill_pins();
        }
    }

    async fn command(&mut self, cmd: Command) {
        match cmd {
            Command::StartLink(name) => self.start_link(name),
            Command::LinkPreview(url) => self.link_preview(url),
            Command::OpenThread(key) => {
                self.link_preview(None);
                if let Err(e) = self.open_thread(&key).await {
                    self.notice(format!("대화를 열지 못했습니다: {e:#}"));
                }
            }
            Command::RestoreAt(id) => self.restore_at = id.parse().ok(),
            Command::LoadOlder => {
                if let Err(e) = self.load_older().await {
                    warn!("load_older: {e:#}");
                }
            }
            Command::LoadNewer => {
                if let Err(e) = self.load_newer().await {
                    warn!("load_newer: {e:#}");
                }
            }
            Command::ToLatest => self.emit_messages(Scroll::Bottom).await,
            Command::AtBottom(at) => {
                self.at_bottom = at;
                self.read_if_seen();
            }
            Command::ReadUpTo(id) => {
                if let Ok(row) = id.parse::<u64>() {
                    self.read_through(row);
                }
            }
            Command::UnreadButton => {
                if let Err(e) = self.unread_button().await {
                    warn!("unread button: {e:#}");
                }
            }
            Command::Send { text, files, quote, mentions, link, quality } => self.send(text, files, quote, mentions, link, quality).await,
            Command::ViewImage { id, index, fit } => self.view_image(&id, index, fit),
            Command::SaveFiles { id, owner, all } => self.save_files(&id, owner, all),
            Command::OpenVideo { id, index } => self.open_video(&id, index),
            Command::ToggleImageDownload { id, index } => self.toggle_image_download(&id, index).await,
            Command::PlayGif { id, seq, px } => self.play_gif(&id, seq, px),
            Command::StopGif => self.stop_gif(),
            Command::PlayPendingGif { path, seq, tile } => self.play_pending_gif(path, seq, tile),
            Command::ViewerThumbs { id, px } => self.viewer_thumbs(&id, px),
            Command::OpenViewed { id, index } => self.open_viewed(&id, index),
            Command::SaveViewed { id, index, owner } => self.save_viewed(&id, index, owner),
            Command::ReadMore(id) => {
                if let Ok(row) = id.parse::<u64>() {
                    *self.long.shown.entry(row).or_insert(view::text::READ_MORE_AT) += view::text::READ_MORE_STEP;
                    self.emit_messages(Scroll::Keep).await;
                }
            }
            Command::Search(s) => {
                self.search = s;
                self.emit_threads().await;
            }
            Command::WindowFocus(f) => {
                self.focused = f;
                if f {
                    self.notes.clear();
                    self.read_if_seen();
                }
            }
            Command::WindowWatching(w) => self.watching = w,
            Command::Unlink => self.unlink().await,
            Command::StickerPanel => self.sticker_packs().await,
            Command::OpenPack(id) => self.open_pack(&id).await,
            Command::SendSticker { pack, id } => self.send_sticker(&pack, id).await,
            Command::InstallPack { id, key } => match (hex::decode(&id), blessed_key(&id).or_else(|| hex::decode(&key).ok())) {
                (Ok(id), Some(key)) => self.install_pack(id, key),
                _ => self.notice("잘못된 스티커 팩입니다"),
            },
            Command::InstallPackUrl(url) => match parse_pack_url(&url) {
                Some((id, key)) => self.install_pack(id, key),
                None => self.notice("스티커 링크를 인식하지 못했습니다 (signal.art/addstickers 링크)"),
            },
            Command::InstallPackFromMessage(id) => self.install_pack_from_message(&id),
            Command::StickerClicked(id) => self.sticker_clicked(&id).await,
            Command::StickerFrames(key) => self.sticker_frames(key),
            Command::PickerFrames { pack, id } => self.picker_frames(&pack, id),
            Command::React { id, emoji, remove } => self.react(&id, emoji, remove).await,
            Command::Edit { id, text, mentions } => self.edit(&id, text, &mentions).await,
            Command::Delete(id) => self.delete(&id).await,
            Command::RetrySend(id) => self.retry_send(&id).await,
            Command::DiscardFailed(id) => self.discard_failed(&id).await,
            Command::Pin { id, secs } => self.pin(&id, secs).await,
            Command::Unpin(id) => self.unpin(&id).await,
            Command::Replies(id) => self.replies(&id).await,
            Command::OpenFromToast { key, msg } => self.open_from_toast(&key, msg).await,
            Command::JumpTo(id) => {
                if let Err(e) = self.jump_to(&id).await {
                    warn!("jump to {id}: {e:#}");
                }
            }
            Command::JumpToDay(day) => {
                if let Err(e) = self.jump_to_day(day).await {
                    warn!("jump to day: {e:#}");
                }
            }
            Command::Composing(active) => self.composing(active),
            Command::SettingsOpened => {
                self.emit_account().await;
                self.emit_storage();
            }
            Command::ClearThumbs => {
                let dir = self.paths.thumbs.clone();
                let _ = tokio::task::spawn_blocking(move || clear_dir(&dir)).await;
                self.thumbs_requested.borrow_mut().clear();
                self.emit_storage();
            }
            Command::SetMute { key, set } => self.set_mute(&key, set),
            Command::SetPref { key, pref } => self.set_pref(&key, pref),
        }
    }

    async fn emit_account(&mut self) {
        let Some(m) = self.manager.as_ref() else { return };
        let reg = m.registration_data();
        let number = reg.phone_number.to_string();
        let device = reg.device_name().unwrap_or_default().to_string();
        let mut name = self.name(self.me).await;
        if name == number || name.starts_with("알 수 없음") {
            name.clear();
        }
        (self.emit)(Event::Account {
            name,
            number,
            device,
            receipts: self.persist.read_receipts,
            typing: self.persist.typing_indicators,
        });
    }

    fn emit_storage(&self) {
        let emit = self.emit.clone();
        let db = self.paths.db.clone();
        let (att, thumbs) = (self.paths.attachments.clone(), self.paths.thumbs.clone());
        tokio::task::spawn_blocking(move || {
            // the WAL and other sidecars belong to the DB too
            let db_size = ["", "-wal", "-shm"]
                .iter()
                .filter_map(|s| std::fs::metadata(format!("{}{s}", db.display())).ok())
                .map(|m| m.len())
                .sum();
            emit(Event::Storage { db: db_size, attachments: dir_size(&att), thumbs: dir_size(&thumbs) });
        });
    }

    fn start_link(&mut self, name: String) {
        if let Some(t) = self.link_task.take() {
            t.abort();
        }
        let store = self.store.clone();
        let tx = self.tx.clone();
        let emit = self.emit.clone();
        let name = if name.trim().is_empty() { "Sigwin".to_string() } else { name.trim().to_string() };
        (self.emit)(Event::LinkStatus("QR 코드 생성 중…".into()));
        self.link_task = Some(tokio::task::spawn_local(async move {
            let (url_tx, url_rx) = oneshot::channel::<url::Url>();
            let show = async move {
                if let Ok(url) = url_rx.await {
                    emit(Event::LinkQr(url.to_string()));
                    emit(Event::LinkStatus("휴대폰으로 QR 코드를 스캔하세요.".into()));
                }
            };
            let link = Manager::link_secondary_device(store, SignalServers::Production, name, url_tx);
            let (res, _) = futures::join!(Box::pin(link), show);
            let _ = tx.send(Internal::Linked(res.map_err(|e| e.to_string())));
        }));
    }

    async fn internal(&mut self, ev: Internal) {
        match ev {
            Internal::Linked(Ok(m)) => {
                info!("linked");
                self.link_task = None;
                (self.emit)(Event::Loading("연결 완료! 대화를 동기화하는 중…".into()));
                self.on_registered(m, true).await;
            }
            Internal::Linked(Err(e)) => {
                self.link_task = None;
                (self.emit)(Event::LinkStatus(format!("연결 실패: {e}. 다시 시도하세요.")));
            }
            Internal::Received(r) => self.received(r).await,
            Internal::Reconnected => self.connection("동기화 중…", false),
            Internal::ReceiveStopped(msg) => {
                self.synced = false;
                self.connection(msg, false);
            }
            Internal::Sent(ts, Ok(())) => self.sent(ts).await,
            Internal::Sent(ts, Err(e)) => self.send_failed(ts, e).await,
            Internal::LinkFetched(url, res) => self.link_fetched(url, res),
            Internal::ThumbFailed(key) => {
                // allow a retry next time the thread is opened
                self.thumbs_requested.borrow_mut().remove(&key);
            }
            Internal::VideoFetched(file_key) => {
                self.thumbs_requested.borrow_mut().retain(|k| !k.starts_with(&file_key));
                self.emit_messages(Scroll::Keep).await;
            }
            Internal::LongText(key, text) => {
                // still the thread it was asked for
                if self.thumbs_requested.borrow().contains(&key) {
                    self.long.full.insert(key, text);
                    self.emit_messages(Scroll::Keep).await;
                }
            }
            Internal::Notice(s) => self.notice(s),
            Internal::SaveFiles(jobs, to) => self.save_files_to(jobs, to),
            Internal::Saved(text, path) => {
                (self.emit)(Event::ProgressDone);
                (self.emit)(Event::Saved(text, path));
            }
            Internal::Progress(s) => self.progress(s),
            Internal::ProgressDone => (self.emit)(Event::ProgressDone),
            Internal::PackInstalled(Ok(id)) => {
                (self.emit)(Event::ProgressDone);
                (self.emit)(Event::OpenStickerPanel(id));
            }
            Internal::PinsIndexed => {
                self.persist.pins_indexed = true;
                self.persist.edits_indexed = true;
                self.persist.reactions_indexed = true;
                self.persist.replies_indexed = true;
                self.persist_dirty = true;
                self.current_dirty = true;
            }
            Internal::ReceiptsIndexed => {
                self.persist.receipts_indexed = true;
                self.persist_dirty = true;
                self.current_dirty = true;
            }
            Internal::PackInstalled(Err(e)) => self.notice(format!("스티커 팩 추가 실패: {e}")),
            Internal::Avatars(done) => {
                self.avatars_running = false;
                self.persist.avatars.extend(done.state);
                self.persist_dirty = true;
                if done.profiles_refreshed {
                    // refetched profiles may carry new names
                    self.names.clear();
                    self.threads_dirty = true;
                    self.current_dirty = true;
                }
                self.run_avatars();
            }
            Internal::PhoneMutes(res) => self.phone_mutes(res),
            Internal::Fixture(op, ack) => {
                let out = self.fixture_op(op).await;
                // what the op queued goes out now, so the ack means "done and shown"
                self.flush().await;
                let _ = ack.send(out);
            }
            Internal::Swept { sweep, changed } => {
                self.sweeping = false;
                self.persist.sweep = sweep;
                self.persist_dirty = true;
                for key in &changed {
                    self.persist.snippets.remove(key);
                    if self.current.as_ref().is_some_and(|o| &o.key == key) {
                        self.current_dirty = true;
                    }
                }
                if !changed.is_empty() {
                    info!("disappearing messages removed in {} threads", changed.len());
                    self.threads_dirty = true;
                }
            }
        }
    }

    fn save_persist(&self) {
        if let Ok(b) = serde_json::to_vec(&self.persist) {
            if let Err(e) = crate::sealed::write(&self.paths.state, &b) {
                warn!("saving state: {e}");
            }
        }
    }

    /// Deletes expired disappearing messages (and their files) from the store, in the background.
    fn sweep(&mut self) {
        if self.sweeping || self.manager.is_none() {
            return;
        }
        self.sweeping = true;
        let (mut store, index, tx) = (self.store.clone(), self.index.clone(), self.tx.clone());
        let mut state = self.persist.sweep.clone();
        let (att_dir, thumb_dir) = (self.paths.attachments.clone(), self.paths.thumbs.clone());
        tokio::task::spawn_local(async move {
            let now = view::time::now_ms();
            let mut changed = Vec::new();
            let threads = index.threads().await.unwrap_or_default();
            for row in threads {
                let key = thread_key(&row.thread);
                let mut st = state.get(&key).copied().unwrap_or_default();
                let due = st.hold.is_some() && st.due <= now;
                if !due && row.last_ts <= st.scanned {
                    continue;
                }
                let mut cursor = st.scanned;
                if due {
                    // rescan from the oldest message whose timer was still running
                    cursor = st.hold.unwrap_or(0).saturating_sub(1);
                    st.hold = None;
                }
                if st.hold.is_none() {
                    st.due = u64::MAX;
                }
                let mut deleted = false;
                loop {
                    let Ok(tss) = index.page_after(&row.thread, cursor, 200).await else { break };
                    let (Some(&lo), Some(&hi)) = (tss.first(), tss.last()) else { break };
                    let msgs: Vec<Content> = match store.messages(&row.thread, lo..=hi).await {
                        Ok(it) => it.flatten().collect(),
                        Err(_) => break,
                    };
                    for c in msgs {
                        let Some(exp) = view::expires_at(&c) else { continue };
                        let ts = c.metadata.client_timestamp.timestamp_millis() as u64;
                        if exp <= now {
                            remove_files(&c, ts, &att_dir, &thumb_dir);
                            deleted |= store.delete_message(&row.thread, ts).await.unwrap_or(false);
                            if let Err(e) = index.forget_reactions(&key, c.timestamp()).await {
                                warn!("forget reactions: {e:#}");
                            }
                            if let Err(e) = index.forget_replies(&key, c.timestamp()).await {
                                warn!("forget replies: {e:#}");
                            }
                        } else {
                            st.hold = Some(st.hold.map_or(ts, |h| h.min(ts)));
                            st.due = st.due.min(exp);
                        }
                    }
                    cursor = cursor.max(hi);
                    if tss.len() < 200 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                st.scanned = st.scanned.max(cursor);
                state.insert(key.clone(), st);
                if deleted {
                    changed.push(key);
                }
                tokio::task::yield_now().await;
            }
            let _ = tx.send(Internal::Swept { sweep: state, changed });
        });
    }

    // ---- stickers ----

    async fn unlink(&mut self) {
        if let Some(t) = self.recv_task.take() {
            t.abort();
        }
        if let Some(m) = self.manager.take() {
            if let Err(e) = m.unlink_self().await {
                warn!("unlink_self: {e}");
            }
        }
        if let Err(e) = self.store.clear().await {
            warn!("clear store: {e}");
        }
        self.current = None;
        self.threads.clear();
        self.names.clear();
        self.avatars_done.clear();
        self.last_outgoing.clear();
        self.avatar_queue.clear();
        self.persist = Persist::default();
        self.save_persist();
        let _ = std::fs::remove_dir_all(&self.paths.attachments);
        let _ = std::fs::remove_dir_all(&self.paths.thumbs);
        clear_dir(&self.paths.open);
        let _ = std::fs::create_dir_all(&self.paths.attachments);
        let _ = std::fs::create_dir_all(&self.paths.thumbs);
        (self.emit)(Event::Threads(Vec::new()));
        (self.emit)(Event::NeedLink);
        (self.emit)(Event::LinkStatus("연결이 해제되었습니다.".into()));
    }
}
