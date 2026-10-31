//! Backend thread: owns the presage `Manager` (which is `!Send`) on a single-threaded
//! tokio runtime, talks to the UI only through `Command`s in and `Event`s out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::{channel::oneshot, pin_mut, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::proto::data_message::Quote;
use presage::proto::{receipt_message, AttachmentPointer, ReceiptMessage, SyncMessage};
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
    parse_thread_key, thread_key, Command, Event, PackDto, Scroll, StickerDto, Thumb, ThreadDto, STICKER_THUMB_PREFIX,
};
use crate::view::{self, Kind, THUMB_MAX};

type Mgr = Manager<SqliteStore, Registered>;
pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

const PAGE: u32 = 60;
const MAX_WINDOW: usize = 600;

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
}

enum Internal {
    Received(Received),
    ReceiveStopped(String),
    Linked(Result<Mgr, String>),
    Sent(Result<(), String>),
    ThumbFailed(String),
    Status(String),
    PackInstalled(Result<String, String>),
    ReceiptsIndexed,
}

struct Open {
    thread: Thread,
    key: String,
    is_group: bool,
    /// group members (ACI uuids), empty for 1:1
    members: Vec<Uuid>,
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
    focused: bool,
    synced: bool,
    tx: UnboundedSender<Internal>,
    link_task: Option<JoinHandle<()>>,
    recv_task: Option<JoinHandle<()>>,
    /// delivery receipts to send, batched per sender
    pending_receipts: HashMap<ServiceId, Vec<u64>>,
}

/// Messages that produce a visible row.
fn displayable(window: &[(u64, Content)], me: Uuid) -> usize {
    window
        .iter()
        .filter(|(_, c)| matches!(view::classify(c, me), Kind::Msg { .. } | Kind::System(_) | Kind::Deleted))
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
        focused: true,
        synced: false,
        tx,
        link_task: None,
        recv_task: None,
        pending_receipts: HashMap::new(),
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
        app.start_link("lite-signal".into());
    }

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut memtick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => Box::pin(app.command(cmd)).await,
                None => break,
            },
            Some(ev) = rx.recv() => Box::pin(app.internal(ev)).await,
            _ = tick.tick() => app.flush().await,
            _ = memtick.tick() => info!("mem: {}", crate::alloc::report()),
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
            Command::Send { text, files, quote } => self.send(text, files, quote).await,
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
        }
    }

    fn start_link(&mut self, name: String) {
        if let Some(t) = self.link_task.take() {
            t.abort();
        }
        let store = self.store.clone();
        let tx = self.tx.clone();
        let emit = self.emit.clone();
        let name = if name.trim().is_empty() { "lite-signal".to_string() } else { name.trim().to_string() };
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
            Internal::Sent(Ok(())) => {
                self.status(if self.synced { "" } else { "동기화 중…" });
                self.current_dirty = true;
                self.threads_dirty = true;
            }
            Internal::Sent(Err(e)) => self.status(format!("전송 실패: {e}")),
            Internal::ThumbFailed(key) => {
                // allow a retry next time the thread is opened
                self.thumbs_requested.remove(&key);
            }
            Internal::Status(s) => self.status(s),
            Internal::PackInstalled(Ok(id)) => {
                self.status("스티커 팩을 추가했습니다");
                (self.emit)(Event::OpenStickerPanel(id));
            }
            Internal::ReceiptsIndexed => {
                self.persist.receipts_indexed = true;
                self.persist_dirty = true;
                self.current_dirty = true;
            }
            Internal::PackInstalled(Err(e)) => self.status(format!("스티커 팩 추가 실패: {e}")),
        }
    }

    async fn received(&mut self, r: Received) {
        match r {
            Received::QueueEmpty => {
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
                    if self.persist.unread.remove(&key).is_some() {
                        self.persist_dirty = true;
                        self.threads_dirty = true;
                    }
                }
            }
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
        let kind = view::classify(&c, self.me);
        match kind {
            Kind::Msg { outgoing: false, .. } => {
                if c.metadata.needs_receipt {
                    self.pending_receipts.entry(c.metadata.sender).or_default().push(c.timestamp());
                }
                if !(is_open && self.focused) {
                    *self.persist.unread.entry(key.clone()).or_default() += 1;
                    self.persist_dirty = true;
                }
                if self.synced && !(is_open && self.focused) {
                    let title = self.thread_name(&thread).await;
                    let body = view::snippet(&c, self.me).unwrap_or_default();
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
        if !self.pending_receipts.is_empty() {
            if let Some(m) = self.manager.clone() {
                let batch = std::mem::take(&mut self.pending_receipts);
                tokio::task::spawn_local(send_receipts(m, batch));
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
                        name = match n.family_name.filter(|f| !f.is_empty()) {
                            Some(f) => format!("{} {}", n.given_name, f),
                            None => n.given_name,
                        }
                        .trim()
                        .to_string();
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
                Some((last, ts, s)) if *last == row.last_ts => (*ts, s.clone()),
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
                        unread: 0,
                        is_group: true,
                    },
                ));
            }
        }
        list.sort_by(|a, b| b.0.cmp(&a.0));
        self.threads = list.into_iter().map(|(_, t)| t).collect();
        self.emit_threads().await;
        Ok(())
    }

    async fn compute_snippet(&self, thread: &Thread, last_ts: u64) -> (u64, String) {
        let Ok(tss) = self.index.page(thread, last_ts.saturating_add(1), 20).await else {
            return (last_ts, String::new());
        };
        for ts in tss {
            if let Ok(Some(c)) = self.store.message(thread, ts).await {
                if let Some(s) = view::snippet(&c, self.me) {
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
        if self.persist.unread.remove(key).is_some() {
            self.persist_dirty = true;
            self.threads_dirty = true;
        }
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
        let name = self.thread_name(&thread).await;
        let mut members = Vec::new();
        let sub = match &thread {
            Thread::Group(k) => match self.store.group(*k).await {
                Ok(Some(g)) => {
                    members = g.members.iter().map(|m| Uuid::from(m.aci)).collect();
                    format!("멤버 {}명", g.members.len())
                }
                _ => String::new(),
            },
            Thread::Contact(id) => self
                .store
                .contact_by_id(id)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.phone_number)
                .map(|p| p.to_string())
                .unwrap_or_default(),
        };
        (self.emit)(Event::Header { name, sub });
        self.thumbs_requested.clear();
        self.current = Some(Open {
            thread,
            key: key.to_string(),
            is_group,
            members,
            has_more: tss.len() as u32 == PAGE,
            window,
        });
        self.mark_read(key);
        self.emit_messages(Scroll::Bottom).await;
        Ok(())
    }

    async fn load_older(&mut self) -> anyhow::Result<()> {
        let Some(open) = self.current.as_ref() else { return Ok(()) };
        if !open.has_more || open.window.len() >= MAX_WINDOW {
            return Ok(());
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
        let Some(open) = self.current.as_mut() else { return Ok(()) };
        open.has_more = tss.len() as u32 == PAGE;
        let mut w = older;
        w.append(&mut open.window);
        open.window = w;
        self.emit_messages(Scroll::Older).await;
        Ok(())
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
        if let Some(open) = self.current.as_mut() {
            open.window = window;
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
        let Some(open) = self.current.as_ref() else { return };
        // resolve names first (async), then build synchronously
        let mut senders: Vec<Uuid> = open.window.iter().map(|(_, c)| c.metadata.sender.raw_uuid()).collect();
        senders.extend(open.members.iter().copied());
        senders.extend(receipts.values().flat_map(|m| m.keys().copied()));
        senders.sort();
        senders.dedup();
        for s in senders {
            if s != self.me && !s.is_nil() {
                self.name(s).await;
            }
        }
        let Some(open) = self.current.as_ref() else { return };
        let names = &self.names;
        let built = view::build(&open.window, self.me, open.is_group, &open.members, &receipts, &|u| {
            names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into())
        });
        let key = open.key.clone();
        let can_older = open.has_more && open.window.len() < MAX_WINDOW;
        for (k, ptr) in built.wanted_thumbs {
            if self.thumbs_requested.insert(k.clone()) {
                self.request_thumb(k, ptr);
            }
        }
        (self.emit)(Event::Messages { key, items: built.items, can_older, scroll });
    }

    fn attachment_path(&self, key: &str, ptr: &AttachmentPointer) -> PathBuf {
        attachment_path(&self.paths.attachments, key, ptr)
    }

    // ---- stickers ----

    async fn sticker_packs(&mut self) {
        let mut out = Vec::new();
        let mut installed = HashSet::new();
        if let Ok(packs) = self.store.sticker_packs().await {
            // packs carry every sticker's bytes: handle one at a time, keep only metadata
            for p in packs.flatten() {
                let id = hex::encode(&p.id);
                installed.insert(id.clone());
                let cover_id = p.manifest.cover.as_ref().map(|c| c.id);
                let cover = cover_id
                    .and_then(|cid| p.manifest.stickers.iter().find(|s| s.id == cid && s.bytes.is_some()))
                    .or_else(|| p.manifest.stickers.iter().find(|s| s.bytes.is_some()));
                let cover_key = match cover {
                    Some(s) => {
                        let key = sticker_thumb_key(&id, s.id);
                        self.sticker_thumb(key.clone(), s.bytes.clone());
                        key
                    }
                    None => String::new(),
                };
                out.push(PackDto { id, title: p.manifest.title, installed: true, cover_key });
            }
        }
        for (id, _, title) in BLESSED_PACKS {
            if !installed.contains(*id) {
                out.push(PackDto { id: id.to_string(), title: title.to_string(), installed: false, cover_key: String::new() });
            }
        }
        (self.emit)(Event::StickerPacks(out));
    }

    async fn open_pack(&mut self, hex_id: &str) {
        let Ok(id) = hex::decode(hex_id) else { return };
        let Ok(Some(pack)) = self.store.sticker_pack(&id).await else { return };
        let mut items = Vec::with_capacity(pack.manifest.stickers.len());
        for s in pack.manifest.stickers {
            let key = sticker_thumb_key(hex_id, s.id);
            self.sticker_thumb(key.clone(), s.bytes);
            items.push(StickerDto { key, id: s.id, emoji: s.emoji.unwrap_or_default() });
        }
        (self.emit)(Event::StickerGrid { pack: hex_id.to_string(), items });
    }

    fn sticker_thumb(&self, key: String, bytes: Option<Vec<u8>>) {
        let thumb_path = self.paths.thumbs.join(format!("{key}.png"));
        if bytes.is_none() && !thumb_path.exists() {
            return;
        }
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let k = key.clone();
            match make_thumb_from(&k, || bytes.context("no sticker bytes"), PICKER_THUMB, &thumb_path) {
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
        let group_revision = match &thread {
            Thread::Group(k) => self.store.group(*k).await.ok().flatten().map(|g| g.revision).unwrap_or(0),
            _ => 0,
        };
        let dir = self.paths.attachments.clone();
        let tx = self.tx.clone();
        self.status("전송 중…");
        tokio::task::spawn_local(async move {
            let res = Box::pin(send_sticker_task(manager, thread, s, group_revision, dir)).await;
            let _ = tx.send(Internal::Sent(res.map_err(|e| format!("{e:#}"))));
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

    fn request_thumb(&self, key: String, ptr: AttachmentPointer) {
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
            let res = tokio::task::spawn_blocking(move || make_thumb(&k, &orig_path, &thumb_path)).await;
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

    async fn send(&mut self, text: String, files: Vec<PathBuf>, quote_id: Option<String>) {
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
                ..Default::default()
            })
        });
        let group_revision = match &thread {
            Thread::Group(k) => self.store.group(*k).await.ok().flatten().map(|g| g.revision).unwrap_or(0),
            _ => 0,
        };
        let tx = self.tx.clone();
        self.status("전송 중…");
        tokio::task::spawn_local(async move {
            let res = Box::pin(send_task(manager, thread, text, files, quote, group_revision)).await;
            let _ = tx.send(Internal::Sent(res.map_err(|e| format!("{e:#}"))));
        });
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

async fn send_receipts(mut m: Mgr, batch: HashMap<ServiceId, Vec<u64>>) {
    for (to, timestamps) in batch {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let receipt = ReceiptMessage {
            r#type: Some(receipt_message::Type::Delivery as i32),
            timestamp: timestamps,
        };
        if let Err(e) = Box::pin(m.send_message(to, receipt, now)).await {
            debug!("delivery receipt to {}: {e}", to.service_id_string());
        }
    }
}

async fn send_task(
    mut m: Mgr,
    thread: Thread,
    text: String,
    files: Vec<PathBuf>,
    quote: Option<Quote>,
    group_revision: u32,
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
    let ts = now_ms();
    let dm = DataMessage {
        body: (!text.is_empty()).then_some(text),
        attachments,
        quote,
        timestamp: Some(ts),
        ..Default::default()
    };
    deliver(&mut m, thread, dm, ts, group_revision).await
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
    let ts = now_ms();
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
    deliver(&mut m, thread, dm, ts, group_revision).await
}

async fn deliver(m: &mut Mgr, thread: Thread, mut dm: DataMessage, ts: u64, group_revision: u32) -> anyhow::Result<()> {
    match thread {
        Thread::Contact(id) => m.send_message(id, dm, ts).await?,
        Thread::Group(key) => {
            dm.group_v2 = Some(GroupContextV2 {
                master_key: Some(key.to_vec()),
                revision: Some(group_revision),
                ..Default::default()
            });
            m.send_message_to_group(&key, dm, ts).await?
        }
    }
    Ok(())
}

fn make_thumb(key: &str, orig: &Path, thumb: &Path) -> anyhow::Result<Thumb> {
    make_thumb_from(key, || Ok(std::fs::read(orig)?), THUMB_MAX, thumb)
}

/// Decodes (sniffing the format, file names may lie) and downsizes to `max`, caching the result as PNG.
fn make_thumb_from(
    key: &str,
    bytes: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    max: u32,
    thumb: &Path,
) -> anyhow::Result<Thumb> {
    let img = if thumb.exists() {
        image::open(thumb)?
    } else {
        let data = bytes()?;
        let img = image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()?
            .decode()?
            .thumbnail(max, max);
        if let Err(e) = img.save(thumb) {
            warn!("save thumb: {e}");
        }
        img
    };
    let rgba = img.to_rgba8();
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

const PICKER_THUMB: u32 = 96;

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

fn notify(title: String, body: String) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = notify_rust::Notification::new().summary(&title).body(&body).show() {
            debug!("notification: {e}");
        }
    });
}



#[cfg(test)]
mod tests {
    use super::*;

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
