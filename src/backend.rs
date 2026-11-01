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
    parse_thread_key, thread_key, Command, Event, MsgDto, PackDto, PinDto, Scroll, StickerDto, Thumb, ThreadDto, STICKER_THUMB_PREFIX,
};
use crate::view::{self, Kind, THUMB_MAX};

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
}

impl Paths {
    pub fn new(dir: &Path) -> Self {
        Self {
            db: dir.join("signal.db3"),
            key: dir.join("db.key"),
            state: dir.join("state.json"),
            attachments: dir.join("attachments"),
            thumbs: dir.join("thumbs"),
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
    thumbs_requested: HashSet<String>,
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
}

/// Messages that produce a visible row.
fn displayable(window: &[(u64, Content)], me: Uuid) -> usize {
    window
        .iter()
        .filter(|(_, c)| matches!(view::classify(c, me), Kind::Msg { .. } | Kind::System(_) | Kind::Deleted | Kind::Pin { .. }))
        .count()
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

    let persist: Persist = std::fs::read(&paths.state)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

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
    };

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
    let mut memtick = tokio::time::interval(Duration::from_secs(10));
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
            _ = memtick.tick() => info!("mem: {}", crate::alloc::report()),
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

    /// One-time: index pin messages presage stored before we kept our own pin index.
    /// Walks every thread oldest first in pages, so pins and unpins apply in order.
    fn backfill_pins(&self) {
        let (store, index, me, tx) = (self.store.clone(), self.index.clone(), self.me, self.tx.clone());
        tokio::task::spawn_local(async move {
            let Ok(threads) = index.threads().await else { return };
            let mut n = 0usize;
            for row in threads {
                let key = thread_key(&row.thread);
                let mut after = 0u64;
                loop {
                    let Ok(tss) = index.page_after(&row.thread, after, 200).await else { break };
                    let (Some(&first), Some(&last)) = (tss.first(), tss.last()) else { break };
                    after = last;
                    if let Ok(msgs) = store.messages(&row.thread, first..=last).await {
                        let mut msgs: Vec<Content> = msgs.flatten().collect();
                        msgs.sort_by_key(|c| c.metadata.client_timestamp.timestamp_millis());
                        for c in &msgs {
                            match record_pin(&index, &key, c, me).await {
                                Ok(true) => n += 1,
                                Ok(false) => {}
                                Err(e) => warn!("index pins: {e:#}"),
                            }
                        }
                    }
                    if (tss.len() as u32) < 200 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
            info!("pin index backfilled: {n} pin messages");
            let _ = tx.send(Internal::PinsIndexed);
        });
    }

    /// One-time: index receipts presage stored before we kept our own receipt index.
    fn backfill_receipts(&self) {
        let (store, index, me, tx) = (self.store.clone(), self.index.clone(), self.me, self.tx.clone());
        tokio::task::spawn_local(async move {
            let Ok(threads) = index.threads().await else { return };
            let mut n = 0usize;
            for row in threads {
                // receipts are always filed under the sender's 1:1 thread
                if !matches!(row.thread, Thread::Contact(_)) {
                    continue;
                }
                let Ok(msgs) = store.messages(&row.thread, ..).await else { continue };
                for c in msgs.flatten() {
                    if let Kind::Receipt { read, targets } = view::classify(&c, me) {
                        let who = c.metadata.sender.raw_uuid();
                        n += targets.len();
                        if let Err(e) = index.record_receipts(who, targets, if read { 2 } else { 1 }).await {
                            warn!("index receipts: {e:#}");
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
            info!("receipt index backfilled: {n} receipts");
            let _ = tx.send(Internal::ReceiptsIndexed);
        });
    }

    fn start_receiving(&mut self) {
        let Some(m) = self.manager.clone() else { return };
        if let Some(t) = self.recv_task.take() {
            t.abort();
        }
        let tx = self.tx.clone();
        self.status("동기화 중…");
        self.recv_task = Some(tokio::task::spawn_local(async move {
            let mut m = m;
            let mut backoff = 2u64;
            loop {
                match Box::pin(m.receive_messages()).await {
                    Ok(stream) => {
                        backoff = 2;
                        pin_mut!(stream);
                        while let Some(r) = stream.next().await {
                            if tx.send(Internal::Received(r)).is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(Internal::ReceiveStopped("연결이 끊어졌습니다. 다시 연결 중…".into()));
                    }
                    Err(e) => {
                        let _ = tx.send(Internal::ReceiveStopped(format!("연결 실패 ({e}). {backoff}초 후 재시도")));
                    }
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(120);
            }
        }));
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
            Command::Search(s) => {
                self.search = s;
                self.emit_threads().await;
            }
            Command::WindowFocus(f) => {
                self.focused = f;
                if f {
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
            Command::React { id, emoji, remove } => self.react(&id, emoji, remove).await,
            Command::Edit { id, text, mentions } => self.edit(&id, text, &mentions).await,
            Command::Delete(id) => self.delete(&id).await,
            Command::Pin { id, secs } => self.pin(&id, secs).await,
            Command::Unpin(id) => self.unpin(&id).await,
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

    async fn received(&mut self, r: Received) {
        match r {
            Received::QueueEmpty => {
                if !self.synced {
                    self.sweep();
                }
                self.synced = true;
                self.status("");
                self.threads_dirty = true;
            }
            Received::Contacts => {
                self.names.clear();
                self.threads_dirty = true;
            }
            Received::DecryptionError(id) => debug!("decryption error from {}", id.service_id_string()),
            Received::Content(c) => self.content(*c).await,
        }
    }

    async fn content(&mut self, c: Content) {
        // read on another device -> clear our badge for that thread
        if let ContentBody::SynchronizeMessage(SyncMessage { read, .. }) = &c.body {
            for r in read {
                let (Some(aci), Some(ts)) = (r.sender_aci.as_deref(), r.timestamp) else { continue };
                let Some(sid) = ServiceId::parse_from_service_id_string(aci) else { continue };
                if let Ok(Some(t)) = self.store.thread_for_sender_and_timestamp(&sid, ts).await {
                    let key = thread_key(&t);
                    let upto = self.persist.read_upto.entry(key.clone()).or_default();
                    if ts > *upto {
                        *upto = ts;
                        self.persist_dirty = true;
                    }
                    if self.persist.unread.remove(&key).is_some() {
                        self.persist_dirty = true;
                        self.threads_dirty = true;
                    }
                }
            }
        }

        if let ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Configuration(cfg)), .. }) = &c.body {
            if let Some(on) = cfg.read_receipts {
                info!(on, "read receipts setting");
                self.persist.read_receipts = Some(on);
                self.persist_dirty = true;
            }
            if let Some(on) = cfg.typing_indicators {
                info!(on, "typing indicators setting");
                self.persist.typing_indicators = Some(on);
                self.persist_dirty = true;
                if !on {
                    self.typing.clear();
                    self.composing = None;
                    self.emit_typing().await;
                }
            }
        }

        // we deleted a message for everyone from another device
        if c.metadata.sender.raw_uuid() == self.me {
            if let Some(target) = view::data_message(&c).and_then(|dm| dm.delete.as_ref()?.target_sent_timestamp) {
                if let Err(e) = self.index.record_my_delete(target).await {
                    warn!("record delete: {e:#}");
                }
            }
        }

        if let ContentBody::TypingMessage(t) = &c.body {
            self.typing_received(c.metadata.sender.raw_uuid(), t).await;
            return;
        }

        if let Kind::Receipt { read, targets } = view::classify(&c, self.me) {
            debug!("receipt from {who} for {targets:?} read={read}", who = c.metadata.sender.raw_uuid());
            let who = c.metadata.sender.raw_uuid();
            if let Err(e) = self.index.record_receipts(who, targets, if read { 2 } else { 1 }).await {
                warn!("record receipts: {e:#}");
            }
            if let Some(open) = &self.current {
                if targets.iter().any(|t| open.window.iter().any(|(ts, _)| ts == t)) {
                    self.current_dirty = true;
                }
            }
            return;
        }

        let Ok(thread) = Thread::try_from(&c) else { return };
        let key = thread_key(&thread);
        let is_open = self.current.as_ref().is_some_and(|o| o.key == key);
        if let Err(e) = record_pin(&self.index, &key, &c, self.me).await {
            warn!("record pin: {e:#}");
        }
        let kind = view::classify(&c, self.me);
        match kind {
            Kind::Msg { outgoing: false, .. } => {
                // a message ends its sender's typing indicator
                if is_open && self.typing.remove(&c.metadata.sender.raw_uuid()).is_some() {
                    self.emit_typing().await;
                }
                if c.metadata.needs_receipt {
                    self.pending_receipts.entry(c.metadata.sender).or_default().push(c.timestamp());
                }
                if !(is_open && self.focused) {
                    *self.persist.unread.entry(key.clone()).or_default() += 1;
                    self.persist_dirty = true;
                }
                if self.synced && !(is_open && self.focused) && crate::settings::notify_enabled() {
                    let title = self.thread_name(&thread).await;
                    let body = self.snippet(&c).await.unwrap_or_default();
                    let title = match (&thread, &c.metadata.sender) {
                        (Thread::Group(_), s) => format!("{} — {}", self.name(s.raw_uuid()).await, title),
                        _ => title,
                    };
                    notify(title, body);
                }
            }
            Kind::Msg { outgoing: true, .. } => {
                // we replied from another device: consider the thread read
                if self.persist.unread.remove(&key).is_some() {
                    self.persist_dirty = true;
                }
            }
            _ => {}
        }
        if is_open {
            self.current_dirty = true;
        }
        self.threads_dirty = true;
    }

    /// Periodic work: coalesces bursts of incoming messages into one UI update.
    async fn flush(&mut self) {
        if self.next_expiry.is_some_and(|t| t <= view::now_ms()) {
            self.next_expiry = None;
            self.current_dirty = true;
            self.sweep();
        }
        if self.pin_expiry.is_some_and(|t| t <= view::now_ms()) {
            self.pin_expiry = None;
            self.current_dirty = true;
        }
        if self.composing.as_ref().is_some_and(|c| c.last_edit.elapsed() >= TYPING_IDLE) {
            if let Some(c) = self.composing.take() {
                self.send_typing(c.thread, false);
            }
        }
        let typists = self.typing.len();
        self.typing.retain(|_, t| t.elapsed() < TYPING_TIMEOUT);
        if self.typing.len() != typists {
            self.emit_typing().await;
        }
        if self.current_dirty {
            self.current_dirty = false;
            if let Err(e) = self.reload_window().await {
                warn!("reload window: {e:#}");
            }
        }
        if self.threads_dirty && self.manager.is_some() {
            self.threads_dirty = false;
            if let Err(e) = self.rebuild_threads().await {
                warn!("rebuild threads: {e:#}");
            }
        }
        if self.persist_dirty {
            self.persist_dirty = false;
            self.save_persist();
        }
        if !self.pending_receipts.is_empty() || !self.pending_reads.is_empty() {
            if let Some(m) = self.manager.clone() {
                let delivered = std::mem::take(&mut self.pending_receipts);
                let read = std::mem::take(&mut self.pending_reads);
                let receipts = self.persist.read_receipts != Some(false);
                tokio::task::spawn_local(send_receipts(m, delivered, read, receipts, self.me));
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

    async fn name(&mut self, uuid: Uuid) -> String {
        if let Some(n) = self.names.get(&uuid) {
            return n.clone();
        }
        let sid = ServiceId::Aci(uuid.into());
        let contact = self.store.contact_by_id(&sid).await.ok().flatten();
        let mut name = contact.as_ref().map(|c| c.name.trim().to_string()).unwrap_or_default();
        if name.is_empty() {
            if let Ok(Some(key)) = self.store.profile_key(&sid).await {
                if let Ok(Some(p)) = self.store.profile(uuid, key).await {
                    if let Some(n) = p.name {
                        name = combine_names(&n.given_name, n.family_name.as_deref().unwrap_or(""));
                    }
                }
            }
        }
        if name.is_empty() {
            name = contact
                .and_then(|c| c.phone_number)
                .map(|p| p.to_string())
                .unwrap_or_else(|| format!("알 수 없음 ({})", &uuid.to_string()[..8]));
        }
        self.names.insert(uuid, name.clone());
        name
    }

    async fn thread_name(&mut self, thread: &Thread) -> String {
        match thread {
            Thread::Contact(id) if id.raw_uuid() == self.me => "나에게 쓰는 메모".into(),
            Thread::Contact(id) => self.name(id.raw_uuid()).await,
            Thread::Group(key) => match self.store.group(*key).await {
                Ok(Some(g)) if !g.title.is_empty() => g.title,
                _ => "그룹".into(),
            },
        }
    }

    async fn rebuild_threads(&mut self) -> anyhow::Result<()> {
        let rows = self.index.threads().await?;
        let mut list: Vec<(u64, ThreadDto)> = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        for row in rows {
            let key = thread_key(&row.thread);
            seen.insert(key.clone());
            let (display_ts, snippet) = match self.persist.snippets.get(&key) {
                // cached before mentions were resolved: recompute
                Some((last, ts, s)) if *last == row.last_ts && !s.contains('\u{FFFC}') => (*ts, s.clone()),
                _ => {
                    let (ts, s) = self.compute_snippet(&row.thread, row.last_ts).await;
                    self.persist.snippets.insert(key.clone(), (row.last_ts, ts, s.clone()));
                    self.persist_dirty = true;
                    (ts, s)
                }
            };
            let name = self.thread_name(&row.thread).await;
            list.push((
                display_ts,
                ThreadDto {
                    unread: self.persist.unread.get(&key).copied().unwrap_or(0),
                    is_group: matches!(row.thread, Thread::Group(_)),
                    time: view::time_short(display_ts),
                    ts: display_ts,
                    key,
                    name,
                    snippet,
                },
            ));
        }
        // groups we're in but haven't talked in yet
        if let Ok(groups) = self.store.groups().await {
            for (k, g) in groups.flatten() {
                let key = thread_key(&Thread::Group(k));
                if seen.contains(&key) || self.store.is_group_left(k).await.unwrap_or(false) {
                    continue;
                }
                list.push((
                    0,
                    ThreadDto {
                        key,
                        name: if g.title.is_empty() { "그룹".into() } else { g.title },
                        snippet: String::new(),
                        time: String::new(),
                        ts: 0,
                        unread: 0,
                        is_group: true,
                    },
                ));
            }
        }
        list.sort_by(|a, b| b.0.cmp(&a.0));
        self.threads = list.into_iter().map(|(_, t)| t).collect();
        self.emit_threads().await;
        self.load_avatars();
        Ok(())
    }

    /// Queues the photos of listed threads not seen yet this session.
    fn load_avatars(&mut self) {
        let me = thread_key(&Thread::Contact(ServiceId::Aci(self.me.into())));
        let keys: Vec<String> =
            self.threads.iter().filter(|t| t.key != me && !self.avatars_done.contains(&t.key)).map(|t| t.key.clone()).collect();
        for key in keys {
            if let Some(thread) = parse_thread_key(&key) {
                self.avatars_done.insert(key.clone());
                self.avatar_queue.push_back(crate::avatar::Want { key, thread, size: crate::avatar::SIZE });
            }
        }
        self.run_avatars();
    }

    /// Photos of the senders shown next to group messages; ahead of the thread list.
    fn load_sender_avatars(&mut self, items: &[MsgDto]) {
        for m in items.iter().filter(|m| m.show_avatar) {
            let Ok(uuid) = m.sender_key.parse::<Uuid>() else { continue };
            let key = format!("{}{uuid}", crate::avatar::MSG_PREFIX);
            if self.avatars_done.insert(key.clone()) {
                let thread = Thread::Contact(ServiceId::Aci(uuid.into()));
                self.avatar_queue.push_front(crate::avatar::Want { key, thread, size: crate::avatar::MSG_SIZE });
            }
        }
        // readers' photos (the first MAX_READER_PHOTOS of each message are drawn)
        for (uuid, _) in items.iter().flat_map(|m| m.readers.iter().take(crate::types::MAX_READER_PHOTOS)) {
            let Ok(u) = uuid.parse::<Uuid>() else { continue };
            let key = format!("{}{u}", crate::avatar::READ_PREFIX);
            if self.avatars_done.insert(key.clone()) {
                let thread = Thread::Contact(ServiceId::Aci(u.into()));
                self.avatar_queue.push_front(crate::avatar::Want { key, thread, size: crate::avatar::READ_SIZE });
            }
        }
        self.run_avatars();
    }

    /// Loads queued photos in the background, a few at a time so new requests can cut in.
    fn run_avatars(&mut self) {
        if self.avatars_running || self.avatar_queue.is_empty() {
            return;
        }
        let Some(manager) = self.manager.clone() else { return };
        let n = self.avatar_queue.len().min(8);
        let todo: Vec<crate::avatar::Want> = self.avatar_queue.drain(..n).collect();
        self.avatars_running = true;
        let job = crate::avatar::Job {
            store: self.store.clone(),
            index: self.index.clone(),
            manager,
            emit: self.emit.clone(),
            dir: self.paths.thumbs.clone(),
            state: todo
                .iter()
                .map(|w| thread_key(&w.thread))
                .filter_map(|k| self.persist.avatars.get(&k).map(|s| (k, s.clone())))
                .collect(),
        };
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let done = Box::pin(job.run(todo)).await;
            let _ = tx.send(Internal::Avatars(done));
        });
    }

    /// `view::snippet` with @mention names resolved.
    async fn snippet(&mut self, c: &Content) -> Option<String> {
        for u in view::data_message(c).map(view::mentioned).unwrap_or_default() {
            self.name(u).await;
        }
        let names = &self.names;
        view::snippet(c, self.me, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into()))
    }

    async fn compute_snippet(&mut self, thread: &Thread, last_ts: u64) -> (u64, String) {
        let Ok(tss) = self.index.page(thread, last_ts.saturating_add(1), 20).await else {
            return (last_ts, String::new());
        };
        for ts in tss {
            if let Ok(Some(c)) = self.store.message(thread, ts).await {
                if let Some(s) = self.snippet(&c).await {
                    return (ts, s);
                }
            }
        }
        (0, String::new())
    }

    async fn emit_threads(&mut self) {
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            (self.emit)(Event::Threads(self.threads.clone()));
            return;
        }
        let mut out: Vec<ThreadDto> =
            self.threads.iter().filter(|t| t.name.to_lowercase().contains(&q)).cloned().collect();
        // contacts without a conversation yet, so new chats can be started
        if let Ok(contacts) = self.store.contacts().await {
            for c in contacts.flatten() {
                let key = format!("c:{}", c.uuid);
                if out.iter().any(|t| t.key == key) || self.threads.iter().any(|t| t.key == key) {
                    continue;
                }
                let phone = c.phone_number.as_ref().map(|p| p.to_string()).unwrap_or_default();
                if c.name.to_lowercase().contains(&q) || (!phone.is_empty() && phone.contains(&q)) {
                    out.push(ThreadDto {
                        key,
                        name: if c.name.is_empty() { phone } else { c.name },
                        snippet: "새 대화 시작".into(),
                        time: String::new(),
                        ts: 0,
                        unread: 0,
                        is_group: false,
                    });
                }
                if out.len() > 200 {
                    break;
                }
            }
        }
        (self.emit)(Event::Threads(out));
    }

    fn mark_read(&mut self, key: &str) {
        let unread = self.persist.unread.remove(key);
        if unread.is_some() {
            self.persist_dirty = true;
            self.threads_dirty = true;
        }
        if self.focused {
            self.queue_reads(key, unread.unwrap_or(0));
        }
    }

    /// Queues read receipts for incoming messages in the open window newer than the thread's
    /// watermark. Without a watermark (first time) only the last `unread` ones count.
    fn queue_reads(&mut self, key: &str, unread: u32) {
        let Some(open) = self.current.as_ref().filter(|o| o.key == key) else { return };
        let me = self.me;
        let incoming: Vec<(ServiceId, u64)> = open
            .window
            .iter()
            .filter(|(_, c)| {
                let s = c.metadata.sender.raw_uuid();
                s != me && !s.is_nil() && matches!(view::classify(c, me), Kind::Msg { outgoing: false, .. })
            })
            .map(|(_, c)| (c.metadata.sender, c.timestamp()))
            .collect();
        let Some(newest) = incoming.iter().map(|(_, ts)| *ts).max() else { return };
        let upto = self.persist.read_upto.get(key).copied();
        if upto.is_some_and(|u| u >= newest) {
            return;
        }
        match upto {
            Some(u) => self.pending_reads.extend(incoming.into_iter().filter(|(_, ts)| *ts > u)),
            None => {
                let skip = incoming.len().saturating_sub(unread as usize);
                self.pending_reads.extend(incoming.into_iter().skip(skip));
            }
        }
        self.persist.read_upto.insert(key.to_string(), newest);
        self.persist_dirty = true;
    }

    async fn fetch_range(&self, thread: &Thread, tss: &[u64]) -> anyhow::Result<Vec<(u64, Content)>> {
        let (Some(&max), Some(&min)) = (tss.first(), tss.last()) else { return Ok(Vec::new()) };
        let mut v: Vec<(u64, Content)> = self
            .store
            .messages(thread, min..=max)
            .await?
            .filter_map(Result::ok)
            .map(|c| (c.metadata.client_timestamp.timestamp_millis() as u64, c))
            .collect();
        v.sort_by_key(|(ts, _)| *ts);
        Ok(v)
    }

    async fn open_thread(&mut self, key: &str) -> anyhow::Result<()> {
        let thread = parse_thread_key(key).context("bad thread key")?;
        let mut tss = self.index.page(&thread, u64::MAX >> 1, PAGE).await?;
        let mut window = self.fetch_range(&thread, &tss).await?;
        // receipts etc. take rows without producing bubbles: fetch more until the view can scroll
        while tss.len() as u32 == PAGE && window.len() < MAX_WINDOW && displayable(&window, self.me) < 30 {
            let oldest = window.first().map(|(t, _)| *t).unwrap_or(0);
            tss = self.index.page(&thread, oldest, PAGE).await?;
            let mut older = self.fetch_range(&thread, &tss).await?;
            older.append(&mut window);
            window = older;
        }
        let is_group = matches!(thread, Thread::Group(_));
        let members = self.emit_header(&thread).await;
        self.thumbs_requested.clear();
        // the UI drops message-list avatars on thread switch
        let in_list = |k: &str| k.starts_with(crate::avatar::MSG_PREFIX) || k.starts_with(crate::avatar::READ_PREFIX);
        self.avatars_done.retain(|k| !in_list(k));
        self.avatar_queue.retain(|w| !in_list(&w.key));
        // leaving a thread we were typing in
        if let Some(c) = self.composing.take() {
            self.send_typing(c.thread, false);
        }
        self.typing.clear();
        self.emit_typing().await;
        let group_id = match &thread {
            Thread::Group(k) => Some(group_identifier(k)),
            _ => None,
        };
        self.current = Some(Open {
            thread,
            key: key.to_string(),
            is_group,
            members,
            group_id,
            has_more: tss.len() as u32 == PAGE,
            window,
        });
        self.mark_read(key);
        self.emit_messages(Scroll::Bottom).await;
        Ok(())
    }

    async fn load_older(&mut self) -> anyhow::Result<()> {
        if self.extend_older().await? {
            self.emit_messages(Scroll::Older).await;
        }
        Ok(())
    }

    /// Prepends older rows to the open window; false when there is nothing more to load.
    async fn extend_older(&mut self) -> anyhow::Result<bool> {
        let Some(open) = self.current.as_ref() else { return Ok(false) };
        if !open.has_more || open.window.len() >= MAX_WINDOW {
            return Ok(false);
        }
        let oldest = open.window.first().map(|(t, _)| *t).unwrap_or(u64::MAX >> 1);
        let thread = open.thread.clone();
        let mut tss = self.index.page(&thread, oldest, PAGE).await?;
        let mut older = self.fetch_range(&thread, &tss).await?;
        while tss.len() as u32 == PAGE && older.len() < MAX_WINDOW / 2 && displayable(&older, self.me) < 20 {
            let o = older.first().map(|(t, _)| *t).unwrap_or(0);
            tss = self.index.page(&thread, o, PAGE).await?;
            let mut more = self.fetch_range(&thread, &tss).await?;
            more.append(&mut older);
            older = more;
        }
        let Some(open) = self.current.as_mut() else { return Ok(false) };
        open.has_more = tss.len() as u32 == PAGE;
        let mut w = older;
        w.append(&mut open.window);
        open.window = w;
        Ok(true)
    }

    /// Scrolls to the message with sent timestamp `id`, loading older rows until it is in the window.
    async fn jump_to(&mut self, id: &str) -> anyhow::Result<()> {
        let target: u64 = id.parse()?;
        let mut loaded = false;
        loop {
            let Some(open) = self.current.as_ref() else { return Ok(()) };
            if let Some((row, _)) = open.window.iter().find(|(_, c)| c.timestamp() == target) {
                let row = row.to_string();
                if loaded {
                    self.emit_messages(Scroll::Keep).await;
                }
                (self.emit)(Event::JumpTo(row));
                return Ok(());
            }
            let passed = open.window.first().is_some_and(|(t, _)| *t < target);
            if passed || !self.extend_older().await? {
                if loaded {
                    self.emit_messages(Scroll::Keep).await;
                }
                self.status(if passed { "메시지를 찾을 수 없습니다" } else { "너무 오래된 메시지라 이동할 수 없습니다" });
                return Ok(());
            }
            loaded = true;
        }
    }

    async fn reload_window(&mut self) -> anyhow::Result<()> {
        let Some(open) = self.current.as_ref() else { return Ok(()) };
        let thread = open.thread.clone();
        let oldest = open.window.first().map(|(t, _)| *t);
        let window: Vec<(u64, Content)> = match oldest {
            Some(oldest) => {
                let mut v: Vec<_> = self
                    .store
                    .messages(&thread, oldest..)
                    .await?
                    .filter_map(Result::ok)
                    .map(|c| (c.metadata.client_timestamp.timestamp_millis() as u64, c))
                    .collect();
                v.sort_by_key(|(ts, _)| *ts);
                v
            }
            None => {
                let tss = self.index.page(&thread, u64::MAX >> 1, PAGE).await?;
                self.fetch_range(&thread, &tss).await?
            }
        };
        // the header carries the disappearing-message timer, which may just have changed
        let members = self.emit_header(&thread).await;
        if let Some(open) = self.current.as_mut() {
            open.window = window;
            open.members = members;
        }
        if self.focused {
            if let Some(key) = self.current.as_ref().map(|o| o.key.clone()) {
                self.mark_read(&key);
            }
        }
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
        Ok(())
    }

    async fn emit_messages(&mut self, scroll: Scroll) {
        let Some(open) = self.current.as_ref() else { return };
        let (from, to) = match (open.window.first(), open.window.last()) {
            (Some((a, _)), Some((b, _))) => (*a, *b),
            _ => (0, 0),
        };
        let mut receipts: HashMap<u64, HashMap<Uuid, u8>> = HashMap::new();
        match self.index.receipts_between(from, to).await {
            Ok(rows) => {
                for (ts, who, level) in rows {
                    receipts.entry(ts).or_default().insert(Uuid::from_bytes(*who.as_bytes()), level);
                }
            }
            Err(e) => warn!("receipts: {e:#}"),
        }
        let my_deletes = self.index.my_deletes_between(from, to).await.unwrap_or_else(|e| {
            warn!("deletes: {e:#}");
            Default::default()
        });
        let Some(open) = self.current.as_ref() else { return };
        // resolve names first (async), then build synchronously
        let mut senders: Vec<Uuid> = open.window.iter().map(|(_, c)| c.metadata.sender.raw_uuid()).collect();
        senders.extend(open.members.iter().copied());
        senders.extend(receipts.values().flat_map(|m| m.keys().copied()));
        let mentioned: Vec<Uuid> =
            open.window.iter().filter_map(|(_, c)| view::data_message(c)).flat_map(view::mentioned).collect();
        senders.sort();
        senders.dedup();
        for s in mentioned {
            self.name(s).await;
        }
        for s in senders {
            if s != self.me && !s.is_nil() {
                self.name(s).await;
            }
        }
        let Some(open) = self.current.as_ref() else { return };
        let names = &self.names;
        let built = view::build(&open.window, self.me, open.is_group, &open.members, &receipts, &my_deletes, &|u| {
            names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into())
        });
        let key = open.key.clone();
        let can_older = open.has_more && open.window.len() < MAX_WINDOW;
        let next_expiry = built.next_expiry;
        let mut items = built.items;
        // rows by sent timestamp, for the pin marks
        let rows: HashMap<u64, String> = open.window.iter().map(|(t, c)| (c.timestamp(), t.to_string())).collect();
        let pins = self.load_pins().await;
        for p in &pins {
            let Some(row) = p.id.parse().ok().and_then(|t: u64| rows.get(&t)) else { continue };
            if let Some(m) = items.iter_mut().find(|m| &m.id == row) {
                m.pinned = true;
            }
        }
        (self.emit)(Event::Pins(pins));
        let Some(open) = self.current.as_ref() else { return };
        for op in self.ops.iter().filter(|o| o.thread_key == key) {
            let row = op.row.to_string();
            if let Some(m) = items.iter_mut().find(|m| m.id == row) {
                match &op.op {
                    Op::React(e) => view::set_my_reaction(m, e.as_deref()),
                    Op::Edit(t) => view::set_edited_text(m, t),
                    Op::Delete(_) => view::set_deleted(m),
                    Op::Pin { .. } | Op::Unpin { .. } => {}
                }
            }
        }
        let mut prev = open.window.last().map(|(t, _)| *t);
        for p in self.pending.iter().filter(|p| p.thread_key == key) {
            if open.window.iter().any(|(t, _)| *t == p.ts) {
                continue; // already stored, the real row is shown
            }
            items.push(pending_row(p, prev));
            prev = Some(p.ts);
        }
        for (k, ptr, fit) in built.wanted_thumbs {
            if self.thumbs_requested.insert(k.clone()) {
                self.request_thumb(k, ptr, fit);
            }
        }
        self.next_expiry = next_expiry;
        self.load_sender_avatars(&items);
        (self.emit)(Event::Messages { key, items, can_older, scroll });
    }

    /// Emits the conversation header; returns the group members (empty for 1:1).
    async fn emit_header(&mut self, thread: &Thread) -> Vec<Uuid> {
        let name = self.thread_name(thread).await;
        let mut members = Vec::new();
        let mut parts = Vec::new();
        match thread {
            Thread::Group(k) => {
                if let Ok(Some(g)) = self.store.group(*k).await {
                    members = g.members.iter().map(|m| Uuid::from(m.aci)).collect();
                    parts.push(format!("멤버 {}명", g.members.len()));
                }
            }
            Thread::Contact(id) => {
                if let Some(p) = self.store.contact_by_id(id).await.ok().flatten().and_then(|c| c.phone_number) {
                    parts.push(p.to_string());
                }
            }
        }
        if let Ok(Some((t, _))) = self.store.expire_timer(thread).await {
            if t > 0 {
                parts.push(format!("사라지는 메시지 {}", view::timer_label(t)));
            }
        }
        (self.emit)(Event::Header { name, sub: parts.join(" · ") });
        let mut mentionable = Vec::new();
        let me = self.me;
        for &u in members.iter().filter(|u| **u != me) {
            mentionable.push((u.to_string(), self.name(u).await));
        }
        (self.emit)(Event::Members(mentionable));
        members
    }

    // ---- typing indicators ----

    fn typing_enabled(&self) -> bool {
        self.persist.typing_indicators != Some(false)
    }

    async fn typing_received(&mut self, who: Uuid, t: &TypingMessage) {
        if who == self.me || !self.typing_enabled() {
            return;
        }
        let Some(open) = self.current.as_ref() else { return };
        let here = match (&t.group_id, &open.group_id) {
            (Some(g), Some(o)) => g.as_slice() == o.as_slice(),
            (None, None) => matches!(&open.thread, Thread::Contact(id) if id.raw_uuid() == who),
            _ => false,
        };
        if !here {
            return;
        }
        // presage can't send "online only": stale indicators arrive when a device reconnects
        let fresh = t.timestamp.is_none_or(|ts| view::now_ms().saturating_sub(ts) < TYPING_TIMEOUT.as_millis() as u64);
        if t.action == Some(typing_message::Action::Started as i32) && fresh {
            self.typing.insert(who, Instant::now());
        } else {
            self.typing.remove(&who);
        }
        self.emit_typing().await;
    }

    async fn emit_typing(&mut self) {
        let mut ids: Vec<Uuid> = self.typing.keys().copied().collect();
        ids.sort();
        let group = self.current.as_ref().is_some_and(|o| o.is_group);
        let key = ids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        if key == self.typing_shown {
            return;
        }
        self.typing_shown = key;
        let mut who = Vec::with_capacity(ids.len());
        for u in ids {
            who.push((u.to_string(), self.name(u).await));
        }
        (self.emit)(Event::Typing { group, who });
    }

    /// The composer changed: tell the open thread we are typing (or stopped).
    fn composing(&mut self, active: bool) {
        if !active {
            if let Some(c) = self.composing.take() {
                self.send_typing(c.thread, false);
            }
            return;
        }
        if !self.typing_enabled() {
            return;
        }
        let Some(open) = self.current.as_ref() else { return };
        if open.members.len() > TYPING_MAX_GROUP || matches!(&open.thread, Thread::Contact(id) if id.raw_uuid() == self.me) {
            return;
        }
        let now = Instant::now();
        if let Some(c) = self.composing.as_mut().filter(|c| c.key == open.key) {
            c.last_edit = now;
            if c.last_sent.elapsed() >= TYPING_RESEND {
                c.last_sent = now;
                let thread = c.thread.clone();
                self.send_typing(thread, true);
            }
            return;
        }
        if let Some(old) = self.composing.take() {
            self.send_typing(old.thread, false);
        }
        let Some(open) = self.current.as_ref() else { return };
        let thread = open.thread.clone();
        self.composing = Some(Composing { key: open.key.clone(), thread: thread.clone(), last_edit: now, last_sent: now });
        self.send_typing(thread, true);
    }

    fn send_typing(&self, thread: Thread, started: bool) {
        let Some(mut m) = self.manager.clone() else { return };
        let ts = view::now_ms();
        let action = if started { typing_message::Action::Started } else { typing_message::Action::Stopped };
        let group_id = match &thread {
            Thread::Group(k) => Some(group_identifier(k).to_vec()),
            _ => None,
        };
        let msg = TypingMessage { timestamp: Some(ts), action: Some(action as i32), group_id };
        tokio::task::spawn_local(async move {
            let res = match thread {
                Thread::Contact(id) => Box::pin(m.send_message(id, msg, ts)).await,
                Thread::Group(key) => Box::pin(m.send_message_to_group(&key, msg, ts)).await,
            };
            if let Err(e) = res {
                debug!("typing indicator: {e}");
            }
        });
    }

    // ---- reactions, edits, deletes ----

    /// The stored message behind row `id` of the open window:
    /// (row, sent timestamp, author, outgoing, data message).
    fn target(&self, id: &str) -> Option<(u64, u64, Uuid, bool, DataMessage)> {
        let row = id.parse::<u64>().ok()?;
        let (_, c) = self.current.as_ref()?.window.iter().find(|(t, _)| *t == row)?;
        let Kind::Msg { outgoing, dm } = view::classify(c, self.me) else { return None };
        Some((row, c.timestamp(), c.metadata.sender.raw_uuid(), outgoing, dm.clone()))
    }

    async fn react(&mut self, id: &str, emoji: String, remove: bool) {
        let Some((row, target, author, _, _)) = self.target(id) else { return };
        let dm = DataMessage {
            reaction: Some(Reaction {
                emoji: Some(emoji.clone()),
                remove: Some(remove),
                target_author_aci: Some(author.to_string()),
                target_sent_timestamp: Some(target),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.send_op(row, Op::React((!remove).then_some(emoji)), dm.into()).await;
    }

    async fn edit(&mut self, id: &str, text: String, mentions: &[crate::mention::Member]) {
        let text = text.trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        let Some((row, target, _, true, orig)) = self.target(id) else {
            self.status("이 메시지는 수정할 수 없습니다");
            return;
        };
        if view::now_ms().saturating_sub(row) >= view::EDIT_WINDOW_MS {
            self.status("보낸 지 24시간이 지난 메시지는 수정할 수 없습니다");
            return;
        }
        let (body, body_ranges) = crate::mention::encode(&text, mentions);
        let dm = DataMessage {
            body: Some(body),
            body_ranges,
            quote: orig.quote,
            expire_timer: orig.expire_timer,
            expire_timer_version: orig.expire_timer_version,
            ..Default::default()
        };
        let edit = EditMessage { target_sent_timestamp: Some(target), data_message: Some(dm) };
        self.send_op(row, Op::Edit(text), edit.into()).await;
    }

    async fn delete(&mut self, id: &str) {
        let Some((row, target, _, true, _)) = self.target(id) else { return };
        if view::now_ms().saturating_sub(row) >= view::EDIT_WINDOW_MS {
            self.status("보낸 지 24시간이 지난 메시지는 모두에게서 삭제할 수 없습니다");
            return;
        }
        let dm = DataMessage { delete: Some(Delete { target_sent_timestamp: Some(target) }), ..Default::default() };
        self.send_op(row, Op::Delete(target), dm.into()).await;
    }

    async fn pin(&mut self, id: &str, secs: Option<u32>) {
        let Some((row, target, author, _, _)) = self.target(id) else { return };
        let Some(key) = self.current.as_ref().map(|o| o.key.clone()) else { return };
        let ts = self.next_send_ts();
        let expires = secs.map_or(0, |s| ts + s as u64 * 1000);
        if let Err(e) = self.index.pin(&key, target, author, ts, expires).await {
            warn!("pin: {e:#}");
            return;
        }
        let dm = DataMessage {
            pin_message: Some(PinMessage {
                target_author_aci_binary: Some(author.as_bytes().to_vec()),
                target_sent_timestamp: Some(target),
                pin_duration: Some(match secs {
                    Some(s) => PinDuration::PinDurationSeconds(s),
                    None => PinDuration::PinDurationForever(true),
                }),
            }),
            ..Default::default()
        };
        self.send_op(row, Op::Pin { target }, dm.into()).await;
    }

    /// `id`: sent timestamp of the pinned message (from the pin bar; it may be outside the window)
    /// or its row in the window (from the message menu).
    async fn unpin(&mut self, id: &str) {
        let Ok(id) = id.parse::<u64>() else { return };
        let Some(open) = self.current.as_ref() else { return };
        let key = open.key.clone();
        let by_row = open.window.iter().find(|(t, _)| *t == id).map(|(_, c)| c.timestamp());
        let pins = self.index.pins(&key).await.unwrap_or_default();
        let Some(&(target, author, at, expires)) =
            pins.iter().find(|p| p.0 == id).or_else(|| pins.iter().find(|p| Some(p.0) == by_row))
        else {
            return;
        };
        if let Err(e) = self.index.unpin(&key, target, u64::MAX >> 1).await {
            warn!("unpin: {e:#}");
            return;
        }
        let dm = DataMessage {
            unpin_message: Some(UnpinMessage {
                target_author_aci_binary: Some(author.as_bytes().to_vec()),
                target_sent_timestamp: Some(target),
            }),
            ..Default::default()
        };
        let row = self
            .current
            .as_ref()
            .and_then(|o| o.window.iter().find(|(_, c)| c.timestamp() == target).map(|(t, _)| *t))
            .unwrap_or(target);
        self.send_op(row, Op::Unpin { target, author, at, expires }, dm.into()).await;
    }

    /// Pins of the open thread that are still showable, newest first. Drops expired ones, pins of
    /// deleted or vanished messages, and everything past `MAX_PINS` from the index.
    async fn load_pins(&mut self) -> Vec<PinDto> {
        self.pin_expiry = None;
        let Some((key, thread)) = self.current.as_ref().map(|o| (o.key.clone(), o.thread.clone())) else { return Vec::new() };
        let rows = match self.index.pins(&key).await {
            Ok(r) => r,
            Err(e) => {
                warn!("pins: {e:#}");
                return Vec::new();
            }
        };
        let now = view::now_ms();
        let mut out = Vec::new();
        let mut kept = 0;
        for (target, author, at, expires) in rows {
            let gone = (expires != 0 && expires <= now) || kept >= MAX_PINS;
            let c = if gone { None } else { self.store.message(&thread, target).await.ok().flatten() };
            let gone = gone
                || c.as_ref().is_some_and(|c| {
                    matches!(view::classify(c, self.me), Kind::Deleted) || view::expires_at(c).is_some_and(|t| t <= now)
                });
            if gone {
                if let Err(e) = self.index.unpin(&key, target, at).await {
                    warn!("drop pin: {e:#}");
                }
                continue;
            }
            kept += 1;
            if expires != 0 {
                self.pin_expiry = Some(self.pin_expiry.map_or(expires, |t| t.min(expires)));
            }
            // pinned before this device was linked: nothing to show
            let Some(dm) = c.as_ref().and_then(|c| match view::classify(c, self.me) {
                Kind::Msg { dm, .. } => Some(dm.clone()),
                _ => None,
            }) else {
                continue;
            };
            for u in view::mentioned(&dm) {
                self.name(u).await;
            }
            let sender = if author == self.me { "나".to_string() } else { self.name(author).await };
            let names = &self.names;
            let text = view::one_line(&dm, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into()));
            out.push(PinDto { id: target.to_string(), sender, text });
        }
        out
    }

    async fn send_op(&mut self, row: u64, op: Op, mut body: ContentBody) {
        let Some(manager) = self.manager.clone() else { return };
        let Some(open) = self.current.as_ref() else { return };
        let (thread, thread_key) = (open.thread.clone(), open.key.clone());
        let group_revision = self.group_revision(&thread).await;
        let ts = self.next_send_ts();
        match &mut body {
            ContentBody::DataMessage(dm) => dm.timestamp = Some(ts),
            ContentBody::EditMessage(EditMessage { data_message: Some(dm), .. }) => dm.timestamp = Some(ts),
            _ => {}
        }
        self.ops.push(PendingOp { ts, thread_key, row, op });
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let mut m = manager;
            let res = Box::pin(deliver(&mut m, thread, body, ts, group_revision)).await;
            let _ = tx.send(Internal::Sent(ts, res.map_err(|e| format!("{e:#}"))));
        });
    }

    async fn group_revision(&self, thread: &Thread) -> u32 {
        match thread {
            Thread::Group(k) => self.store.group(*k).await.ok().flatten().map(|g| g.revision).unwrap_or(0),
            _ => 0,
        }
    }

    // ---- disappearing messages ----

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

    fn attachment_path(&self, key: &str, ptr: &AttachmentPointer) -> PathBuf {
        attachment_path(&self.paths.attachments, key, ptr)
    }

    // ---- stickers ----

    /// Installed packs come from the `ls_packs` cache. Only packs it doesn't know yet (or whose
    /// cover thumb is gone) are read from presage, one at a time: a pack row holds every sticker.
    async fn sticker_packs(&mut self) {
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let mut installed = HashSet::new();
        let ids = self.index.pack_ids().await.unwrap_or_default();
        let mut cached: HashMap<Vec<u8>, (String, Option<u32>)> = self
            .index
            .cached_packs()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, title, cover)| (id, (title, cover)))
            .collect();
        for id in cached.keys().filter(|k| !ids.contains(k)).cloned().collect::<Vec<_>>() {
            let _ = self.index.uncache_pack(&id).await;
            cached.remove(&id);
        }
        let mut loaded = 0;
        for raw in ids {
            let id = hex::encode(&raw);
            let hit = cached.get(&raw).filter(|(_, cover)| match cover {
                Some(c) => self.sticker_cover_path(&id, *c).exists(),
                None => true,
            });
            let (title, cover_id, bytes) = match hit {
                Some((title, cover)) => (title.clone(), *cover, None),
                None => {
                    let Ok(Some(p)) = self.store.sticker_pack(&raw).await else { continue };
                    loaded += 1;
                    let cover_id = p.manifest.cover.as_ref().map(|c| c.id);
                    let cover = cover_id
                        .and_then(|cid| p.manifest.stickers.iter().find(|s| s.id == cid && s.bytes.is_some()))
                        .or_else(|| p.manifest.stickers.iter().find(|s| s.bytes.is_some()));
                    let (cid, bytes) = match cover {
                        Some(s) => (Some(s.id), s.bytes.clone()),
                        None => (None, None),
                    };
                    if let Err(e) = self.index.cache_pack(&raw, &p.manifest.title, cid).await {
                        warn!("cache pack: {e}");
                    }
                    (p.manifest.title, cid, bytes)
                }
            };
            installed.insert(id.clone());
            let cover_key = match cover_id {
                Some(cid) => {
                    let base = sticker_thumb_key(&id, cid);
                    let key = format!("{base}-c");
                    self.sticker_thumb(&base, key.clone(), bytes, PACK_COVER);
                    key
                }
                None => String::new(),
            };
            out.push(PackDto { id, title, installed: true, cover_key });
        }
        debug!(ms = started.elapsed().as_millis() as u64, packs = out.len(), loaded, "sticker packs");
        for (id, _, title) in BLESSED_PACKS {
            if !installed.contains(*id) {
                out.push(PackDto { id: id.to_string(), title: title.to_string(), installed: false, cover_key: String::new() });
            }
        }
        (self.emit)(Event::StickerPacks(out));
    }

    /// The grid comes from `ls_stickers` and the thumb files when all are cached; otherwise the
    /// pack is read from presage once (it holds every sticker's bytes) and cached.
    async fn open_pack(&mut self, hex_id: &str) {
        let started = std::time::Instant::now();
        let Ok(id) = hex::decode(hex_id) else { return };
        let cached = self.index.cached_stickers(&id).await.unwrap_or_default();
        let all_thumbs = |me: &Self| cached.iter().all(|(sid, _)| me.sticker_cover_path(hex_id, *sid).exists());
        let mut items = Vec::new();
        let from_cache = !cached.is_empty() && all_thumbs(self);
        if from_cache {
            for (sid, emoji) in cached {
                let key = sticker_thumb_key(hex_id, sid);
                self.sticker_thumb(&key, key.clone(), None, PICKER_CELL);
                items.push(StickerDto { key, id: sid, emoji });
            }
        } else {
            let Ok(Some(pack)) = self.store.sticker_pack(&id).await else { return };
            let mut meta = Vec::with_capacity(pack.manifest.stickers.len());
            items.reserve(pack.manifest.stickers.len());
            for s in pack.manifest.stickers {
                let key = sticker_thumb_key(hex_id, s.id);
                self.sticker_thumb(&key, key.clone(), s.bytes, PICKER_CELL);
                let emoji = s.emoji.unwrap_or_default();
                meta.push((s.id, emoji.clone()));
                items.push(StickerDto { key, id: s.id, emoji });
            }
            if let Err(e) = self.index.cache_stickers(&id, &meta).await {
                warn!("cache stickers: {e}");
            }
        }
        debug!(ms = started.elapsed().as_millis() as u64, stickers = items.len(), from_cache, "open pack");
        (self.emit)(Event::StickerGrid { pack: hex_id.to_string(), items });
    }

    fn sticker_cover_path(&self, pack_hex: &str, id: u32) -> PathBuf {
        self.paths.thumbs.join(format!("{}-{THUMB_MAX}.png", sticker_thumb_key(pack_hex, id)))
    }

    /// `base` names the disk cache, `key` the emitted image (a pack cover and its grid cell differ in size).
    /// The resampled result is cached too (`-<px>px.png`): resampling costs ~40x the decode.
    fn sticker_thumb(&self, base: &str, key: String, bytes: Option<Vec<u8>>, fit: u32) {
        let thumb_path = self.paths.thumbs.join(format!("{base}-{THUMB_MAX}.png"));
        let fitted = self.paths.thumbs.join(format!("{base}-{}px.png", crate::types::physical_px(fit)));
        if bytes.is_none() && !fitted.exists() && !thumb_path.exists() {
            return;
        }
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let res = match sigwin_thumb::open_rgba(&fitted) {
                Ok(rgba) => Ok(Thumb { key: key.clone(), width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() }),
                Err(_) => make_thumb_from(&key, || bytes.context("no sticker bytes"), THUMB_MAX, &thumb_path, fit)
                    .inspect(|t| {
                        if let Some(img) = image::RgbaImage::from_raw(t.width, t.height, t.rgba.clone()) {
                            if let Err(e) = img.save(&fitted) {
                                warn!("save fitted thumb: {e}");
                            }
                        }
                    }),
            };
            match res {
                Ok(t) => emit(Event::Thumb(Arc::new(t))),
                Err(e) => debug!("sticker thumb {key}: {e}"),
            }
        });
    }

    async fn send_sticker(&mut self, hex_id: &str, sticker_id: u32) {
        let Some(manager) = self.manager.clone() else { return };
        let Some(thread) = self.current.as_ref().map(|o| o.thread.clone()) else { return };
        let Ok(id) = hex::decode(hex_id) else { return };
        let Ok(Some(pack)) = self.store.sticker_pack(&id).await else { return };
        let Some(sticker) = pack.manifest.stickers.into_iter().find(|s| s.id == sticker_id) else { return };
        let Some(bytes) = sticker.bytes else {
            self.status("스티커 데이터가 없습니다");
            return;
        };
        let content_type = sticker.content_type.filter(|c| !c.is_empty()).unwrap_or_else(|| "image/webp".into());
        let s = StickerSend { pack_id: pack.id, pack_key: pack.key, id: sticker_id, emoji: sticker.emoji, content_type, bytes };
        let group_revision = self.group_revision(&thread).await;
        let dir = self.paths.attachments.clone();
        let tx = self.tx.clone();
        let ts = self.next_send_ts();
        let image_key = format!("pending-{ts}");
        let Some(thread_key) = self.current.as_ref().map(|o| o.key.clone()) else { return };
        self.pending.push(Pending {
            ts,
            thread_key,
            text: String::new(),
            quote: String::new(),
            attachment: String::new(),
            image_key: image_key.clone(),
            error: None,
        });
        // preview straight from the sticker bytes, no disk cache
        let bytes = s.bytes.clone();
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let img = sigwin_thumb::decode(&bytes).map(|i| {
                sigwin_thumb::downscale(sigwin_thumb::to_rgba(&i), crate::types::physical_px(view::STICKER_SIZE))
            });
            if let Ok(rgba) = img {
                let t = Thumb { key: image_key, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() };
                emit(Event::Thumb(Arc::new(t)));
            }
        });
        self.emit_messages(Scroll::Bottom).await;
        tokio::task::spawn_local(async move {
            let started = std::time::Instant::now();
            let res = Box::pin(send_sticker_task(manager, thread, s, group_revision, dir, ts)).await;
            info!(ms = started.elapsed().as_millis() as u64, ok = res.is_ok(), "sticker sent");
            let _ = tx.send(Internal::Sent(ts, res.map_err(|e| format!("{e:#}"))));
        });
    }

    fn install_pack(&self, id: Vec<u8>, key: Vec<u8>) {
        let Some(mut m) = self.manager.clone() else { return };
        let tx = self.tx.clone();
        self.status("스티커 팩 내려받는 중…");
        tokio::task::spawn_local(async move {
            let res = Box::pin(m.install_sticker_pack(&id, &key)).await;
            let _ = tx.send(Internal::PackInstalled(res.map(|_| hex::encode(&id)).map_err(|e| e.to_string())));
        });
    }

    /// Installed pack: open the panel on it. Otherwise ask the user whether to install it.
    async fn sticker_clicked(&mut self, msg_id: &str) {
        let pack_id = self.current.as_ref().and_then(|open| {
            let ts = msg_id.parse::<u64>().ok()?;
            let (_, c) = open.window.iter().find(|(t, _)| *t == ts)?;
            let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return None };
            dm.sticker.as_ref()?.pack_id.clone()
        });
        let Some(pack_id) = pack_id else { return };
        match self.store.sticker_pack(&pack_id).await {
            Ok(Some(_)) => (self.emit)(Event::OpenStickerPanel(hex::encode(&pack_id))),
            _ => (self.emit)(Event::AskInstallPack(msg_id.to_string())),
        }
    }

    fn install_pack_from_message(&self, msg_id: &str) {
        let Some(open) = self.current.as_ref() else { return };
        let Ok(ts) = msg_id.parse::<u64>() else { return };
        let Some((_, c)) = open.window.iter().find(|(t, _)| *t == ts) else { return };
        let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return };
        let Some(s) = &dm.sticker else { return };
        if let (Some(id), Some(key)) = (s.pack_id.clone(), s.pack_key.clone()) {
            self.install_pack(id, key);
        }
    }

    fn request_thumb(&self, key: String, ptr: AttachmentPointer, fit: u32) {
        let thumb_path = self.paths.thumbs.join(format!("{key}.png"));
        let orig_path = self.attachment_path(&key, &ptr);
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            if !thumb_path.exists() {
                if !orig_path.exists() {
                    let Some(m) = manager else { return };
                    match m.get_attachment(&ptr).await {
                        Ok(data) => {
                            if let Some(p) = orig_path.parent() {
                                let _ = std::fs::create_dir_all(p);
                            }
                            if let Err(e) = std::fs::write(&orig_path, &data) {
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

    fn open_attachment(&self, id: &str) {
        let Some(open) = self.current.as_ref() else { return };
        let Ok(ts) = id.parse::<u64>() else { return };
        let Some((_, c)) = open.window.iter().find(|(t, _)| *t == ts) else { return };
        let dm = match view::classify(c, self.me) {
            Kind::Msg { dm, .. } => dm.clone(),
            _ => return,
        };
        let jobs: Vec<(PathBuf, AttachmentPointer)> = dm
            .attachments
            .iter()
            .enumerate()
            .map(|(i, a)| (self.attachment_path(&view::attachment_key(a, ts, i), a), a.clone()))
            .collect();
        let manager = self.manager.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            for (path, ptr) in jobs {
                if !path.exists() {
                    let _ = tx.send(Internal::Status("첨부파일 다운로드 중…".into()));
                    let Some(m) = manager.as_ref() else { return };
                    match m.get_attachment(&ptr).await {
                        Ok(data) => {
                            if let Some(p) = path.parent() {
                                let _ = std::fs::create_dir_all(p);
                            }
                            if let Err(e) = std::fs::write(&path, &data) {
                                let _ = tx.send(Internal::Status(format!("저장 실패: {e}")));
                                continue;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Internal::Status(format!("다운로드 실패: {e}")));
                            continue;
                        }
                    }
                }
                let _ = tx.send(Internal::Status(String::new()));
                open_with_default_app(&path);
            }
        });
    }

    async fn send(&mut self, text: String, files: Vec<PathBuf>, quote_id: Option<String>, mentions: Vec<crate::mention::Member>) {
        let Some(manager) = self.manager.clone() else { return };
        let Some(open) = self.current.as_ref() else { return };
        let text = text.trim_end().to_string();
        if text.trim().is_empty() && files.is_empty() {
            return;
        }
        let thread = open.thread.clone();
        let quote = quote_id.and_then(|id| id.parse::<u64>().ok()).and_then(|qts| {
            let (_, c) = open.window.iter().find(|(t, _)| *t == qts)?;
            let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return None };
            Some(Quote {
                id: Some(qts),
                author_aci: Some(c.metadata.sender.raw_uuid().to_string()),
                text: dm.body.clone(),
                // keeps the quoted @mentions readable on the other side
                body_ranges: dm.body_ranges.clone(),
                ..Default::default()
            })
        });
        let names = &self.names;
        let quote_label = quote
            .as_ref()
            .and_then(|q| {
                let t = q.text.as_deref().filter(|t| !t.is_empty())?;
                Some(view::with_mentions(t, &q.body_ranges, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into())))
            })
            .unwrap_or_default();
        let (body, body_ranges) = crate::mention::encode(&text, &mentions);
        let group_revision = self.group_revision(&thread).await;
        // the message itself ends our typing indicator on the other side
        self.composing = None;
        let ts = self.next_send_ts();
        let attachment = match files.as_slice() {
            [] => String::new(),
            [one] => file_label(one),
            [first, rest @ ..] => format!("{} 외 {}개", file_label(first), rest.len()),
        };
        self.pending.push(Pending {
            ts,
            thread_key: open.key.clone(),
            text: text.clone(),
            quote: quote_label,
            attachment,
            image_key: String::new(),
            error: None,
        });
        self.emit_messages(Scroll::Bottom).await;
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let started = std::time::Instant::now();
            let res = Box::pin(send_task(manager, thread, body, body_ranges, files, quote, group_revision, ts)).await;
            info!(ms = started.elapsed().as_millis() as u64, ok = res.is_ok(), "message sent");
            let _ = tx.send(Internal::Sent(ts, res.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Unique, increasing send timestamps (they are the message ids).
    fn next_send_ts(&self) -> u64 {
        let last = self.pending.iter().map(|p| p.ts).chain(self.ops.iter().map(|o| o.ts)).max().unwrap_or(0);
        now_ms().max(last + 1)
    }

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
        let _ = std::fs::create_dir_all(&self.paths.attachments);
        let _ = std::fs::create_dir_all(&self.paths.thumbs);
        (self.emit)(Event::Threads(Vec::new()));
        (self.emit)(Event::NeedLink);
        (self.emit)(Event::LinkStatus("연결이 해제되었습니다.".into()));
    }
}

/// Delivery receipts, then read receipts (unless turned off on the phone) and one read sync
/// to our other devices so they clear their unread state too.
/// Applies a stored pin or unpin message to the pin index; true if `c` was one.
async fn record_pin(index: &Index, key: &str, c: &Content, me: Uuid) -> anyhow::Result<bool> {
    let at = c.timestamp();
    match view::classify(c, me) {
        Kind::Pin { target, author, secs } => {
            let expires = secs.map_or(0, |s| at + s as u64 * 1000);
            index.pin(key, target, author, at, expires).await?;
        }
        Kind::Unpin { target } => index.unpin(key, target, at).await?,
        _ => return Ok(false),
    }
    Ok(true)
}

async fn send_receipts(
    mut m: Mgr,
    delivered: HashMap<ServiceId, Vec<u64>>,
    read: Vec<(ServiceId, u64)>,
    read_receipts: bool,
    me: Uuid,
) {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    for (to, timestamps) in delivered {
        let receipt = ReceiptMessage {
            r#type: Some(receipt_message::Type::Delivery as i32),
            timestamp: timestamps,
        };
        if let Err(e) = Box::pin(m.send_message(to, receipt, now())).await {
            debug!("delivery receipt to {}: {e}", to.service_id_string());
        }
    }
    if read.is_empty() {
        return;
    }
    if read_receipts {
        let mut by_sender: HashMap<ServiceId, Vec<u64>> = HashMap::new();
        for (to, ts) in &read {
            by_sender.entry(*to).or_default().push(*ts);
        }
        for (to, timestamps) in by_sender {
            debug!("read receipt to {} for {timestamps:?}", to.service_id_string());
            let receipt = ReceiptMessage {
                r#type: Some(receipt_message::Type::Read as i32),
                timestamp: timestamps,
            };
            if let Err(e) = Box::pin(m.send_message(to, receipt, now())).await {
                debug!("read receipt to {}: {e}", to.service_id_string());
            }
        }
    }
    let sync = SyncMessage {
        read: read
            .iter()
            .map(|(sender, ts)| presage::proto::sync_message::Read {
                sender_aci: Some(sender.service_id_string()),
                timestamp: Some(*ts),
                sender_aci_binary: Some(sender.raw_uuid().as_bytes().to_vec()),
            })
            .collect(),
        ..Default::default()
    };
    if let Err(e) = Box::pin(m.send_message(ServiceId::Aci(me.into()), sync, now())).await {
        debug!("read sync: {e}");
    }
}

async fn send_task(
    mut m: Mgr,
    thread: Thread,
    text: String,
    body_ranges: Vec<BodyRange>,
    files: Vec<PathBuf>,
    quote: Option<Quote>,
    group_revision: u32,
    ts: u64,
) -> anyhow::Result<()> {
    let mut specs = Vec::new();
    for path in &files {
        let data = std::fs::read(path).with_context(|| format!("{} 읽기 실패", path.display()))?;
        let ct = mime_guess::from_path(path).first_or_octet_stream().to_string();
        let (width, height) = if ct.starts_with("image/") {
            image::image_dimensions(path).map(|(w, h)| (Some(w), Some(h))).unwrap_or((None, None))
        } else {
            (None, None)
        };
        specs.push((
            AttachmentSpec {
                content_type: ct,
                length: data.len(),
                file_name: path.file_name().map(|s| s.to_string_lossy().to_string()),
                preview: None,
                voice_note: None,
                borderless: None,
                width,
                height,
                caption: None,
                blur_hash: None,
            },
            data,
        ));
    }
    let attachments = if specs.is_empty() {
        Vec::new()
    } else {
        m.upload_attachments(specs).await?.into_iter().collect::<Result<Vec<_>, _>>()?
    };
    let dm = DataMessage {
        body: (!text.is_empty()).then_some(text),
        body_ranges,
        attachments,
        quote,
        timestamp: Some(ts),
        ..Default::default()
    };
    deliver(&mut m, thread, dm.into(), ts, group_revision).await
}

fn file_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn pending_row(p: &Pending, prev_ts: Option<u64>) -> MsgDto {
    let mut m = view::outgoing_stub(p.ts, prev_ts);
    (m.text, m.styles) = crate::format::parse(&p.text);
    m.quote = p.quote.clone();
    m.attachment = p.attachment.clone();
    if !p.image_key.is_empty() {
        m.image_key = p.image_key.clone();
        m.img_w = view::STICKER_SIZE;
        m.img_h = view::STICKER_SIZE;
        m.sticker = true;
    }
    m.big_emoji = !m.sticker
        && m.attachment.is_empty()
        && m.quote.is_empty()
        && crate::emoji::only_emoji(&m.text, 5).is_some();
    match &p.error {
        None => {
            m.status = "전송 중".into();
            m.receipt_info = "서버로 보내는 중입니다.".into();
        }
        Some(e) => {
            m.status = "실패".into();
            m.receipt_info = format!("보내지 못했습니다.\n{e}");
        }
    }
    m
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct StickerSend {
    pack_id: Vec<u8>,
    pack_key: Vec<u8>,
    id: u32,
    emoji: Option<String>,
    content_type: String,
    bytes: Vec<u8>,
}

async fn send_sticker_task(
    mut m: Mgr,
    thread: Thread,
    s: StickerSend,
    group_revision: u32,
    attachments_dir: PathBuf,
    ts: u64,
) -> anyhow::Result<()> {
    let spec = AttachmentSpec {
        content_type: s.content_type.clone(),
        length: s.bytes.len(),
        file_name: None,
        preview: None,
        voice_note: None,
        borderless: Some(true),
        width: None,
        height: None,
        caption: None,
        blur_hash: None,
    };
    let ptr = m
        .upload_attachments(vec![(spec, s.bytes.clone())])
        .await?
        .pop()
        .context("upload returned nothing")??;
    // keep a local copy so our own sticker renders without downloading it back
    let path = attachment_path(&attachments_dir, &view::attachment_key(&ptr, ts, 99), &ptr);
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&path, &s.bytes);
    let dm = DataMessage {
        sticker: Some(presage::proto::data_message::Sticker {
            pack_id: Some(s.pack_id),
            pack_key: Some(s.pack_key),
            sticker_id: Some(s.id),
            data: Some(ptr),
            emoji: s.emoji,
        }),
        timestamp: Some(ts),
        ..Default::default()
    };
    deliver(&mut m, thread, dm.into(), ts, group_revision).await
}

async fn deliver(m: &mut Mgr, thread: Thread, mut body: ContentBody, ts: u64, group_revision: u32) -> anyhow::Result<()> {
    match thread {
        Thread::Contact(id) => m.send_message(id, body, ts).await?,
        Thread::Group(key) => {
            let ctx = GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(group_revision), ..Default::default() };
            match &mut body {
                ContentBody::DataMessage(dm) => dm.group_v2 = Some(ctx),
                ContentBody::EditMessage(EditMessage { data_message: Some(dm), .. }) => dm.group_v2 = Some(ctx),
                _ => {}
            }
            let started = std::time::Instant::now();
            m.send_message_to_group(&key, body, ts).await?;
            debug!(ms = started.elapsed().as_millis() as u64, "group send done");
        }
    }
    Ok(())
}

/// Identifier of a group as typing messages carry it.
fn group_identifier(master_key: &[u8; 32]) -> [u8; 32] {
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key)).get_group_identifier()
}

/// Downloaded attachments and thumbnails of an expired message.
fn remove_files(c: &Content, ts: u64, attachments: &Path, thumbs: &Path) {
    let Some(dm) = view::data_message(c) else { return };
    let mut keys: Vec<String> = dm.attachments.iter().enumerate().map(|(i, a)| view::attachment_key(a, ts, i)).collect();
    if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.as_ref()) {
        keys.push(view::attachment_key(data, ts, 99));
    }
    for k in keys {
        let _ = std::fs::remove_dir_all(attachments.join(&k));
        let _ = std::fs::remove_file(thumbs.join(format!("{k}.png")));
    }
}

fn make_thumb(key: &str, orig: &Path, thumb: &Path, fit: u32) -> anyhow::Result<Thumb> {
    make_thumb_from(key, || Ok(std::fs::read(orig)?), THUMB_MAX, thumb, fit)
}

/// Decodes (sniffing the format, file names may lie) and downsizes to `max`, caching the result as PNG.
/// With `fit` > 0 the result is then resampled so its longer side is exactly `fit` logical px on screen:
/// the software renderer scales images nearest-neighbor, which shreds downscaled stickers.
fn make_thumb_from(
    key: &str,
    bytes: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    max: u32,
    thumb: &Path,
    fit: u32,
) -> anyhow::Result<Thumb> {
    let mut rgba = if thumb.exists() {
        sigwin_thumb::open_rgba(thumb)?
    } else {
        let img = sigwin_thumb::decode_thumb(&bytes()?, max)?;
        if let Err(e) = img.save(thumb) {
            warn!("save thumb: {e}");
        }
        sigwin_thumb::to_rgba(&img)
    };
    if fit > 0 {
        rgba = sigwin_thumb::downscale(rgba, crate::types::physical_px(fit));
    }
    Ok(Thumb { key: key.to_string(), width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

fn attachment_path(dir: &Path, key: &str, ptr: &AttachmentPointer) -> PathBuf {
    let name = ptr.file_name.as_deref().filter(|n| !n.is_empty()).map(sanitize).unwrap_or_else(|| {
        let ext = ptr
            .content_type
            .as_deref()
            .and_then(mime_guess::get_mime_extensions_str)
            .and_then(|e| e.first().copied())
            .unwrap_or("bin");
        format!("attachment.{ext}")
    });
    dir.join(key).join(name)
}

/// Logical sizes the picker shows stickers at (ui/app.slint: `cell - 12px`, pack tab cover).
const PICKER_CELL: u32 = 72;
const PACK_COVER: u32 = 32;

/// Picker thumbs used to be cached at 96px as `sp_<pack>_<id>.png`; they are `...-260.png` now.
fn remove_legacy_sticker_thumbs(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(STICKER_THUMB_PREFIX) && !name.contains('-') {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn sticker_thumb_key(pack_hex: &str, id: u32) -> String {
    format!("{STICKER_THUMB_PREFIX}{pack_hex}_{id}")
}

/// Signal's built-in packs (Signal-Desktop `BLESSED_PACKS`): hex id, base64 key, title.
const BLESSED_PACKS: &[(&str, &str, &str)] = &[
    ("42fb75e1827c0c945cfb5ca0975db03c", "7uJ+K593PgpV6iTDQLe+hYcRpuK9m27nBEND4OQovmU=", "Rocky Talk"),
    ("ccc89a05dc077856b57351e90697976c", "RXMOYPCdVWYRUiN0RTemt9nqmc7qy3eh+9aAG5YH+88=", "My Daily Life 1"),
    ("fb535407d2f6497ec074df8b9c51dd1d", "F+lxwTQDViJ4HS7iSeZHO3dFg3ULaMEbuCt1CcaLbf0=", "Zozo the French Bulldog"),
    ("3044281a51307306e5442f2e9070953a", "xMqqhDl+GmMKWWD1SguCdTyIpeUuDe/mFbpN2A8TDL8=", "Croco's Feelings"),
    ("a2414255948558316f37c1d36c64cd28", "/aEpNxltI28cqeEZalZULh0c72/4TivgOChxf6IK02Y=", "My Daily Life 2"),
    ("684d2b7bcfc2eec6f57f2e7be0078e0f", "hm4Ny0obJfKwTfJwzXQnI+SmVVwKGrw/PzDcxaIBDFU=", "Cozy Season"),
    ("f19548e5afa38d1ce4f5c3191eba5e30", "LLMHZ0D2aapExsBjKQskmn0ApLAu2PnppbkCo38bvEE=", "Chug the Mouse"),
    ("9acc9e8aba563d26a4994e69263e3b25", "Wm3/OUjCjvubeq+T7MN1xp/DFueAd+0mhnoU0QoPahI=", "Bandit the Cat"),
];

fn blessed_key(hex_id: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let (_, b64, _) = BLESSED_PACKS.iter().find(|(id, _, _)| *id == hex_id)?;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

/// Accepts `https://signal.art/addstickers/#pack_id=<hex>&pack_key=<hex>` (or a bare query string).
fn parse_pack_url(url: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut id = None;
    let mut key = None;
    for part in url.split(['#', '&', '?']) {
        if let Some(v) = part.trim().strip_prefix("pack_id=") {
            id = hex::decode(v.trim()).ok();
        } else if let Some(v) = part.trim().strip_prefix("pack_key=") {
            key = hex::decode(v.trim()).ok();
        }
    }
    Some((id?, key?))
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "attachment".into() } else { s }
}

fn open_with_default_app(path: &Path) {
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

fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Profile display name like the official apps: CJK names are family first with no space
/// ("홍길동"), everything else "given family".
fn combine_names(given: &str, family: &str) -> String {
    let (given, family) = (given.trim(), family.trim());
    if given.is_empty() || family.is_empty() {
        return format!("{given}{family}");
    }
    if is_cjk(given) && is_cjk(family) {
        format!("{family}{given}")
    } else {
        format!("{given} {family}")
    }
}

fn is_cjk(s: &str) -> bool {
    s.chars().all(|c| {
        matches!(c as u32,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF // Hangul
            | 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F // kana
            | 0x3005 | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x3FFFF) // Han
    })
}

/// Removes the folder's contents, keeping the folder.
fn clear_dir(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
}

fn notify(title: String, body: String) {
    let Some((title, body)) = crate::settings::toast_text(title, body) else { return };
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crate::toast::notification().summary(&title).body(&body).show() {
            debug!("notification: {e}");
        }
    });
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_profile_names() {
        assert_eq!(combine_names("길동", "홍"), "홍길동");
        assert_eq!(combine_names("太郎", "山田"), "山田太郎");
        assert_eq!(combine_names("John", "Smith"), "John Smith");
        assert_eq!(combine_names("길동", "Hong"), "길동 Hong");
        assert_eq!(combine_names(" 홍길동 ", ""), "홍길동");
        assert_eq!(combine_names("", "홍"), "홍");
    }

    #[test]
    fn blessed_keys_are_32_bytes() {
        for (id, _, title) in BLESSED_PACKS {
            assert_eq!(hex::decode(id).unwrap().len(), 16, "{title} id");
            assert_eq!(blessed_key(id).unwrap().len(), 32, "{title} key");
        }
    }

    #[test]
    fn parses_signal_art_links() {
        let (id, key) = parse_pack_url(
            "https://signal.art/addstickers/#pack_id=9acc9e8aba563d26a4994e69263e3b25&pack_key=5a6dff3948c28efb9b7aaf93ecc375c69fc316e78077ed26867a14d10a0f6a12",
        )
        .unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(key.len(), 32);
        assert!(parse_pack_url("https://signal.art/addstickers/").is_none());
        assert!(parse_pack_url("pack_id=zz&pack_key=00").is_none());
    }
}
