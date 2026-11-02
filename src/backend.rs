//! Backend thread: owns the presage `Manager` (which is `!Send`) on a single-threaded
//! tokio runtime, talks to the UI only through `Command`s in and `Event`s out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    parse_thread_key, thread_key, Command, Event, Frames, MsgDto, PackDto, PinDto, Scroll, StickerDto, Thumb, ThreadDto, ANIM_BUDGET,
    STICKER_THUMB_PREFIX,
};
use crate::view::{self, Kind, THUMB_MAX};

pub(crate) mod files;
pub(crate) mod notify;
mod ops;
mod receive;
mod send;
mod stickers;
mod threads;
mod typing;
mod window;

use files::*;
use ops::*;
use send::*;
use stickers::*;

pub(crate) type Mgr = Manager<SqliteStore, Registered>;
pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

const PAGE: u32 = 60;
const MAX_WINDOW: usize = 600;
/// presage sends to each group member one by one: skip typing indicators in larger groups
const TYPING_MAX_GROUP: usize = 12;
/// Signal clients resend "typing" every few seconds and give up on silent typists after 15s
const TYPING_RESEND: Duration = Duration::from_secs(10);
const TYPING_IDLE: Duration = Duration::from_secs(5);
const TYPING_TIMEOUT: Duration = Duration::from_secs(15);
/// how often to look for expired disappearing messages
const SWEEP_EVERY: Duration = Duration::from_secs(300);
/// Signal keeps at most this many pinned messages per chat; a newer pin pushes out the oldest
const MAX_PINS: usize = 3;

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
            state: dir.join("state.json"),
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
    /// layout of the attachment and thumbnail caches on disk (`CACHE_FORMAT` once converted)
    #[serde(default)]
    cache_format: u32,
    /// `snippets` no longer holds misses from the old 20-row scan
    #[serde(default)]
    snippets_deep: bool,
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
    ReceiveStopped(String),
    Linked(Result<Mgr, String>),
    Sent(u64, Result<(), String>),
    ThumbFailed(String),
    /// a long message's full text: (attachment key, text)
    LongText(String, String),
    Status(String),
    PackInstalled(Result<String, String>),
    ReceiptsIndexed,
    PinsIndexed,
    Swept { sweep: HashMap<String, SweepState>, changed: Vec<String> },
    Avatars(crate::avatar::Done),
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
    /// None: reaction taken back
    React(Option<String>),
    Edit(String),
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
    attachment: String,
    /// sticker preview: thumbnail key, already emitted to the UI
    image_key: String,
    error: Option<String>,
}

struct Open {
    thread: Thread,
    key: String,
    is_group: bool,
    /// group members (ACI uuids), empty for 1:1
    members: Vec<Uuid>,
    /// group identifier (what typing messages carry), None for 1:1
    group_id: Option<[u8; 32]>,
    window: Vec<(u64, Content)>,
    has_more: bool,
}

struct App {
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
    thumbs_requested: HashSet<String>,
    long: view::LongTexts,
    pending: Vec<Pending>,
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
    avatars_running: bool,
    avatar_queue: std::collections::VecDeque<crate::avatar::Want>,
    /// bumped per opened sticker pack: image work for an older grid stops
    grid_gen: Arc<std::sync::atomic::AtomicU64>,
    /// the toast slot (`notify.rs`)
    notes: notify::Notifier,
}

pub fn spawn(dir: PathBuf, cmd_rx: UnboundedReceiver<Command>, emit: Emit) -> std::thread::JoinHandle<()> {
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
                if let Err(e) = Box::pin(run(dir, cmd_rx, emit)).await {
                    error!("backend failed: {e:#}");
                    emit2(Event::Loading(format!("오류: {e:#}")));
                }
            });
        })
        .expect("spawn backend thread")
}

async fn run(dir: PathBuf, mut cmd_rx: UnboundedReceiver<Command>, emit: Emit) -> anyhow::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let paths = Paths::new(&dir);
    std::fs::create_dir_all(&paths.attachments)?;
    std::fs::create_dir_all(&paths.thumbs)?;
    // PNG cache of the former DirectWrite emoji path; emoji are plain text now
    let _ = std::fs::remove_dir_all(dir.join("emoji"));
    remove_legacy_sticker_thumbs(&paths.thumbs);
    emit(Event::Loading("저장소 여는 중…".into()));

    let pass = crate::secret::load_or_create(&paths.key)?;
    // the cache files are encrypted with keys derived from the DB key
    let root: [u8; 32] = hex::decode(&pass).ok().and_then(|k| k.try_into().ok()).context("db key is not 32 bytes")?;
    sigwin_vault::init(root);
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

    let mut persist: Persist = std::fs::read(&paths.state)
        .ok()
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

    let (tx, mut rx) = unbounded_channel();
    let mut app = App {
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
        thumbs_requested: HashSet::new(),
        long: Default::default(),
        pending: Vec::new(),
        ops: Vec::new(),
        typing: HashMap::new(),
        typing_shown: String::new(),
        composing: None,
        next_expiry: None,
        pin_expiry: None,
        sweeping: false,
        focused: true,
        synced: false,
        tx,
        link_task: None,
        recv_task: None,
        pending_receipts: HashMap::new(),
        pending_reads: Vec::new(),
        avatars_done: HashSet::new(),
        avatars_running: false,
        avatar_queue: Default::default(),
        grid_gen: Default::default(),
        notes: Default::default(),
    };
    if sealed_now {
        app.save_persist();
    }

    if app.store.is_registered().await {
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
                    let s = view::time_short(t.ts);
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
    fn status(&self, s: impl Into<String>) {
        (self.emit)(Event::Status(s.into()));
    }

    async fn on_registered(&mut self, m: Mgr, fresh: bool) {
        self.me = m.registration_data().service_ids.aci;
        self.manager = Some(m);
        (self.emit)(Event::Ready);
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
        if !self.persist.pins_indexed {
            self.backfill_pins();
        }
    }

    async fn command(&mut self, cmd: Command) {
        match cmd {
            Command::StartLink(name) => self.start_link(name),
            Command::OpenThread(key) => {
                if let Err(e) = self.open_thread(&key).await {
                    self.status(format!("대화를 열지 못했습니다: {e:#}"));
                }
            }
            Command::LoadOlder => {
                if let Err(e) = self.load_older().await {
                    warn!("load_older: {e:#}");
                }
            }
            Command::Send { text, files, quote, mentions } => self.send(text, files, quote, mentions).await,
            Command::OpenAttachment(id) => self.open_attachment(&id),
            Command::ReadMore(id) => {
                if let Ok(row) = id.parse::<u64>() {
                    *self.long.shown.entry(row).or_insert(view::READ_MORE_AT) += view::READ_MORE_STEP;
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
                    if let Some(key) = self.current.as_ref().map(|c| c.key.clone()) {
                        self.mark_read(&key);
                    }
                }
            }
            Command::Unlink => self.unlink().await,
            Command::StickerPanel => self.sticker_packs().await,
            Command::OpenPack(id) => self.open_pack(&id).await,
            Command::SendSticker { pack, id } => self.send_sticker(&pack, id).await,
            Command::InstallPack { id, key } => match (hex::decode(&id), blessed_key(&id).or_else(|| hex::decode(&key).ok())) {
                (Ok(id), Some(key)) => self.install_pack(id, key),
                _ => self.status("잘못된 스티커 팩입니다"),
            },
            Command::InstallPackUrl(url) => match parse_pack_url(&url) {
                Some((id, key)) => self.install_pack(id, key),
                None => self.status("스티커 링크를 인식하지 못했습니다 (signal.art/addstickers 링크)"),
            },
            Command::InstallPackFromMessage(id) => self.install_pack_from_message(&id),
            Command::StickerClicked(id) => self.sticker_clicked(&id).await,
            Command::StickerFrames(key) => self.sticker_frames(key),
            Command::PickerFrames { pack, id } => self.picker_frames(&pack, id),
            Command::React { id, emoji, remove } => self.react(&id, emoji, remove).await,
            Command::Edit { id, text, mentions } => self.edit(&id, text, &mentions).await,
            Command::Delete(id) => self.delete(&id).await,
            Command::Pin { id, secs } => self.pin(&id, secs).await,
            Command::Unpin(id) => self.unpin(&id).await,
            Command::OpenFromToast { key, msg } => self.open_from_toast(&key, msg).await,
            Command::JumpTo(id) => {
                if let Err(e) = self.jump_to(&id).await {
                    warn!("jump to {id}: {e:#}");
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
                self.thumbs_requested.clear();
                self.emit_storage();
            }
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
            Internal::ReceiveStopped(msg) => {
                self.synced = false;
                self.status(msg);
            }
            Internal::Sent(ts, Ok(())) => {
                // presage stored the message before returning: swap the stub for the real row now
                self.pending.retain(|p| p.ts != ts);
                if let Some(Op::Delete(target)) = self.ops.iter().find(|o| o.ts == ts).map(|o| &o.op) {
                    if let Err(e) = self.index.record_my_delete(*target).await {
                        warn!("record delete: {e:#}");
                    }
                }
                self.ops.retain(|o| o.ts != ts);
                if let Err(e) = self.reload_window().await {
                    warn!("reload window: {e:#}");
                }
                self.threads_dirty = true;
            }
            Internal::Sent(ts, Err(e)) => {
                warn!("send {ts} failed: {e}");
                if let Some(i) = self.ops.iter().position(|o| o.ts == ts) {
                    // nothing to retry from: drop the optimistic change
                    let op = self.ops.remove(i);
                    let what = match op.op {
                        Op::React(_) => "반응",
                        Op::Edit(_) => "수정",
                        Op::Delete(_) => "삭제",
                        Op::Pin { target } => {
                            if let Err(e) = self.index.unpin(&op.thread_key, target, ts).await {
                                warn!("undo pin: {e:#}");
                            }
                            "고정"
                        }
                        Op::Unpin { target, author, at, expires } => {
                            if let Err(e) = self.index.pin(&op.thread_key, target, author, at, expires).await {
                                warn!("undo unpin: {e:#}");
                            }
                            "고정 해제"
                        }
                    };
                    self.status(format!("{what} 실패: {e}"));
                    self.emit_messages(Scroll::KeepIfNotAtBottom).await;
                    return;
                }
                self.status(format!("전송 실패: {e}"));
                if let Some(p) = self.pending.iter_mut().find(|p| p.ts == ts) {
                    p.error = Some(e);
                }
                self.emit_messages(Scroll::KeepIfNotAtBottom).await;
            }
            Internal::ThumbFailed(key) => {
                // allow a retry next time the thread is opened
                self.thumbs_requested.remove(&key);
            }
            Internal::LongText(key, text) => {
                // still the thread it was asked for
                if self.thumbs_requested.contains(&key) {
                    self.long.full.insert(key, text);
                    self.emit_messages(Scroll::Keep).await;
                }
            }
            Internal::Status(s) => self.status(s),
            Internal::PackInstalled(Ok(id)) => {
                self.status("스티커 팩을 추가했습니다");
                (self.emit)(Event::OpenStickerPanel(id));
            }
            Internal::PinsIndexed => {
                self.persist.pins_indexed = true;
                self.persist_dirty = true;
                self.current_dirty = true;
            }
            Internal::ReceiptsIndexed => {
                self.persist.receipts_indexed = true;
                self.persist_dirty = true;
                self.current_dirty = true;
            }
            Internal::PackInstalled(Err(e)) => self.status(format!("스티커 팩 추가 실패: {e}")),
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
            let tmp = self.paths.state.with_extension("json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, &self.paths.state);
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
            let now = view::now_ms();
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
