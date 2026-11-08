//! Backend without an account, for scenarios: the real `App` runs on a scratch data dir with
//! no `Manager`, so nothing can reach the network. Messages come from here instead of the
//! websocket (through the same `content()` as real ones), and what would go out (receipts,
//! toasts) is kept for inspection. Used by the in-process `Harness` (tests) and by the headless
//! `SIGWIN_FIXTURE` mode, whose steps send these ops one by one.
//!
//! Safety: a data dir that already holds an account is refused, since seeding writes into the
//! store.

use super::receive::Outgoing;
use super::*;

/// The fake account's own id.
const ME: u128 = 0xF1_0000;

/// A fixture contact's id: `who` 1, 2, ...
pub(crate) fn contact(who: u32) -> Uuid {
    Uuid::from_u128(0xC0_0000 + who as u128)
}

/// What an offline `App` keeps instead of sending.
pub(crate) struct Offline {
    pub sent: Vec<Outgoing>,
    /// sent timestamp of the latest fixture message
    next_ts: u64,
}

/// One step of a scenario, run by the backend.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FxOp {
    /// `n` older messages from contact `who`, already stored and synced, the newest `unread` of them unread
    Seed { who: u32, name: String, n: u32, unread: u32 },
    /// `n` new messages from `who` arriving now (the receive path)
    Recv { who: u32, n: u32 },
    /// the phone read `who`'s thread up to its newest message
    PhoneRead { who: u32 },
    /// a value to check: `unread`, `read_upto`, `sent`, `toasts`, `window`
    Query { what: String, who: u32 },
}

impl FxOp {
    /// `seed who=1 name=철수 n=100 unread=30`, `recv who=1 n=2`, `phone-read who=1`, `query unread who=1`.
    pub fn parse(line: &str) -> anyhow::Result<Self> {
        let mut words = line.split_whitespace();
        let cmd = words.next().ok_or_else(|| anyhow::anyhow!("empty fixture op"))?;
        let mut what = String::new();
        let (mut who, mut n, mut unread, mut name) = (1u32, 1u32, 0u32, String::new());
        for w in words {
            match w.split_once('=') {
                Some(("who", v)) => who = v.parse()?,
                Some(("n", v)) => n = v.parse()?,
                Some(("unread", v)) => unread = v.parse()?,
                Some(("name", v)) => name = v.to_string(),
                Some((k, _)) => anyhow::bail!("unknown fixture option: {k}"),
                None => what = w.to_string(),
            }
        }
        Ok(match cmd {
            "seed" => Self::Seed { who, name, n, unread },
            "recv" => Self::Recv { who, n },
            "phone-read" => Self::PhoneRead { who },
            "query" => Self::Query { what, who },
            _ => anyhow::bail!("unknown fixture op: {cmd}"),
        })
    }
}

/// A message from `who` to us.
fn incoming(who: u32, ts: u64, body: &str, me: Uuid) -> Content {
    let sender = ServiceId::Aci(contact(who).into());
    let at = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap_or_default();
    let dm = DataMessage { body: Some(body.into()), timestamp: Some(ts), ..Default::default() };
    Content::from_body(
        dm,
        presage::libsignal_service::content::Metadata {
            sender,
            destination: ServiceId::Aci(me.into()),
            sender_device: 1u32.try_into().unwrap(),
            pni_verified: None,
            client_timestamp: at,
            server_timestamp: at,
            needs_receipt: true,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        },
    )
}

/// Our phone telling us it read `who`'s message `ts` (a read sync).
fn read_sync(who: u32, ts: u64, me: Uuid) -> Content {
    let at = chrono::DateTime::from_timestamp_millis(view::now_ms() as i64).unwrap_or_default();
    let sync = SyncMessage {
        read: vec![presage::proto::sync_message::Read {
            sender_aci: Some(contact(who).to_string()),
            timestamp: Some(ts),
            sender_aci_binary: Some(contact(who).as_bytes().to_vec()),
        }],
        ..Default::default()
    };
    Content::from_body(
        sync,
        presage::libsignal_service::content::Metadata {
            sender: ServiceId::Aci(me.into()),
            destination: ServiceId::Aci(me.into()),
            sender_device: 2u32.try_into().unwrap(),
            pni_verified: None,
            client_timestamp: at,
            server_timestamp: at,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        },
    )
}

impl App {
    /// Starts without an account, like `on_registered` minus everything that needs the network.
    pub(super) async fn start_offline(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.store.is_registered().await, "fixture mode refuses a data dir that holds an account");
        // nothing a fixture does may reach the screen
        crate::toast::capture();
        self.me = Uuid::from_u128(ME);
        let _ = TX.set(self.tx.clone());
        // a fresh dir has nothing to index
        self.persist.receipts_indexed = true;
        self.persist.pins_indexed = true;
        self.persist.edits_indexed = true;
        self.persist.reactions_indexed = true;
        self.offline = Some(Offline { sent: Vec::new(), next_ts: view::now_ms() - 2 * 86_400_000 });
        (self.emit)(Event::Ready);
        self.threads_dirty = true;
        Ok(())
    }

    fn fx_ts(&mut self) -> u64 {
        let o = self.offline.as_mut().expect("fixture op without a fixture");
        o.next_ts += 1_000;
        o.next_ts
    }

    /// Runs one op and answers (queries: the value; the rest: "ok").
    pub(super) async fn fixture_op(&mut self, op: FxOp) -> String {
        if self.offline.is_none() {
            return "error: not a fixture".into();
        }
        match self.fixture_run(op).await {
            Ok(s) => s,
            Err(e) => format!("error: {e:#}"),
        }
    }

    async fn fixture_run(&mut self, op: FxOp) -> anyhow::Result<String> {
        let me = self.me;
        match op {
            FxOp::Seed { who, name, n, unread } => {
                let thread = Thread::Contact(ServiceId::Aci(contact(who).into()));
                if !name.is_empty() {
                    self.names.insert(contact(who), name);
                }
                let mut last = 0;
                for i in 0..n {
                    last = self.fx_ts();
                    self.store.save_message(&thread, incoming(who, last, &format!("메시지 {}", i + 1), me)).await?;
                }
                let key = thread_key(&thread);
                if unread > 0 {
                    self.persist.unread.insert(key, unread.min(n));
                }
                self.persist_dirty = true;
                self.threads_dirty = true;
                let _ = last;
            }
            FxOp::Recv { who, n } => {
                let thread = Thread::Contact(ServiceId::Aci(contact(who).into()));
                for i in 0..n {
                    let ts = self.fx_ts();
                    let c = incoming(who, ts, &format!("새 메시지 {}", i + 1), me);
                    self.store.save_message(&thread, c.clone()).await?;
                    self.content(c).await;
                }
            }
            FxOp::PhoneRead { who } => {
                let thread = Thread::Contact(ServiceId::Aci(contact(who).into()));
                let rows = self.index.page(&thread, i64::MAX, 1).await?;
                let ts = rows.first().map(|r| r.1).ok_or_else(|| anyhow::anyhow!("thread {who} is empty"))?;
                self.content(read_sync(who, ts, me)).await;
            }
            FxOp::Query { what, who } => return Ok(self.fixture_query(&what, who)),
        }
        Ok("ok".into())
    }

    fn fixture_query(&self, what: &str, who: u32) -> String {
        let key = thread_key(&Thread::Contact(ServiceId::Aci(contact(who).into())));
        match what {
            "unread" => self.persist.unread.get(&key).copied().unwrap_or(0).to_string(),
            "read_upto" => self.persist.read_upto.get(&key).map_or("none".into(), |t| t.to_string()),
            "sent" => {
                let (mut delivery, mut read, mut sync) = (0, 0, 0);
                for o in self.offline.iter().flat_map(|o| &o.sent) {
                    match o {
                        Outgoing::Delivery { timestamps, .. } => delivery += timestamps.len(),
                        Outgoing::Read { timestamps, .. } => read += timestamps.len(),
                        Outgoing::ReadSync(v) => sync += v.len(),
                    }
                }
                format!("delivery={delivery} read={read} sync={sync}")
            }
            "toasts" => crate::toast::shown().join(","),
            "window" => match &self.current {
                None => "closed".into(),
                Some(o) => {
                    let divider = o.divider.and_then(|d| o.window.iter().position(|(t, _)| *t == d));
                    format!(
                        "len={} divider={} reached={} end={}",
                        o.window.len(),
                        divider.map_or("none".into(), |i| i.to_string()),
                        o.divider_reached,
                        o.end_seq.is_some()
                    )
                }
            },
            _ => format!("error: unknown query {what}"),
        }
    }
}

/// The running offline backend's own queue, for ops from outside (the headless `fx` step).
static TX: std::sync::OnceLock<UnboundedSender<Internal>> = std::sync::OnceLock::new();

/// Runs one op line in the offline backend and waits for its answer: the op and the flush
/// after it are done then. Blocks the caller (the backend never waits for the UI).
pub fn run(line: &str) -> anyhow::Result<String> {
    let op = FxOp::parse(line)?;
    let tx = TX.get().ok_or_else(|| anyhow::anyhow!("no fixture backend is running"))?;
    let (ack, answer) = std::sync::mpsc::channel();
    tx.send(Internal::Fixture(op, ack)).map_err(|_| anyhow::anyhow!("fixture backend stopped"))?;
    answer.recv_timeout(Duration::from_secs(60)).map_err(|e| anyhow::anyhow!("fixture op `{line}`: {e}"))
}

/// An in-process backend for tests: the real `App`, driven step by step.
#[cfg(test)]
pub(crate) mod harness {
    use super::*;
    use std::sync::Mutex;

    pub(crate) struct Harness {
        pub app: App,
        rx: UnboundedReceiver<Internal>,
        events: Arc<Mutex<Vec<Event>>>,
        dir: PathBuf,
    }

    impl Harness {
        /// Must run inside a `LocalSet`: the backend spawns local tasks.
        pub(crate) async fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sigwin-fx-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let events: Arc<Mutex<Vec<Event>>> = Default::default();
            let sink = events.clone();
            let emit: Emit = Arc::new(move |ev| sink.lock().unwrap().push(ev));
            let (mut app, rx) = open_app(&dir, emit).await.unwrap();
            app.start_offline().await.unwrap();
            let mut h = Self { app, rx, events, dir };
            h.settle().await;
            h
        }

        /// Handles what the app queued for itself, then a flush; until nothing is left.
        pub(crate) async fn settle(&mut self) {
            for _ in 0..8 {
                while let Ok(ev) = self.rx.try_recv() {
                    Box::pin(self.app.internal(ev)).await;
                }
                self.app.flush().await;
                tokio::task::yield_now().await;
                if self.rx.is_empty() {
                    break;
                }
            }
        }

        pub(crate) async fn op(&mut self, line: &str) -> String {
            let out = self.app.fixture_op(FxOp::parse(line).unwrap()).await;
            self.settle().await;
            out
        }

        pub(crate) async fn cmd(&mut self, cmd: Command) {
            Box::pin(self.app.command(cmd)).await;
            self.settle().await;
        }

        /// `OpenThread` of fixture contact `who`.
        pub(crate) async fn open(&mut self, who: u32) {
            let key = thread_key(&Thread::Contact(ServiceId::Aci(contact(who).into())));
            self.cmd(Command::OpenThread(key)).await;
        }

        /// The app's window focus, as the UI reports it.
        pub(crate) async fn focus(&mut self, on: bool) {
            self.cmd(Command::WindowFocus(on)).await;
        }

        /// The window shows without the focus, as the UI reports it.
        pub(crate) async fn watch(&mut self, on: bool) {
            self.cmd(Command::WindowWatching(on)).await;
        }

        /// Everything emitted since the last call.
        pub(crate) fn take_events(&self) -> Vec<Event> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }

        pub(crate) fn query(&self, what: &str, who: u32) -> String {
            self.app.fixture_query(what, who)
        }

        /// The row id of window position `i` (what the UI sends back as a row id).
        pub(crate) fn row(&self, i: usize) -> String {
            self.app.current.as_ref().unwrap().window[i].0.to_string()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
