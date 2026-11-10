//! Message requests: a 1:1 chat someone started whom the user hasn't accepted. As in Signal
//! Desktop (`isConversationAccepted`), accepted are the people the phone accepts (in the address
//! book or sharing our profile, from the Storage Service), the chats accepted here and the ones
//! we wrote in, and an empty chat. Until it is accepted nothing tells the sender we got or read
//! anything (no receipts), pictures wait for a click and toasts don't show the text. Groups are
//! never requests here: accepting an invitation is a group change.
//!
//! Answers go to our other devices as a sync message, like Signal Desktop's; the phone files them
//! in its storage, which we only read. Blocked people's messages are dropped as they come.

use presage::proto::sync_message::message_request_response::Type as Answered;
use presage::proto::sync_message::MessageRequestResponse;

use super::receive::Outgoing;
use super::*;
use crate::types::RequestAnswer;

impl App {
    /// The phone's storage has been read with what requests go by. Until then nothing is a
    /// request: a read that fails mustn't take every chat's composer away.
    fn requests_known(&self) -> bool {
        self.persist.storage_format >= storage::STORAGE_FORMAT
    }

    /// Someone whose messages are dropped (by the thread key of their 1:1 chat).
    pub(super) fn is_blocked(&self, key: &str) -> bool {
        self.persist.phone_blocked.contains(key) || self.persist.blocked_here.contains(key)
    }

    /// As last worked out by `check_request`.
    pub(super) fn is_request(&self, key: &str) -> bool {
        self.requests.contains(key)
    }

    /// Works out whether `thread` is a message request now, and files it in `requests`.
    pub(super) async fn check_request(&mut self, thread: &Thread) -> bool {
        let key = thread_key(thread);
        let request = self.judge(thread, &key).await;
        if request {
            self.requests.insert(key);
        } else {
            self.requests.remove(&key);
        }
        request
    }

    async fn judge(&mut self, thread: &Thread, key: &str) -> bool {
        // a PNI thread is one we started by number
        let Thread::Contact(ServiceId::Aci(aci)) = thread else { return false };
        if !self.requests_known()
            || Uuid::from(*aci) == self.me
            || self.persist.phone_accepted.contains(key)
            || self.persist.accepted_here.contains(key)
        {
            return false;
        }
        if self.not_written.contains(key) {
            return true;
        }
        match self.index.wrote_in(thread, self.me).await {
            // ours from before requests were kept track of: no need to ask again
            Ok(Some(true)) => {
                self.persist.accepted_here.insert(key.to_string());
                self.persist_dirty = true;
                false
            }
            Ok(Some(false)) => {
                self.not_written.insert(key.to_string());
                true
            }
            // empty: a chat we're starting
            Ok(None) => false,
            // can't tell: as before requests existed
            Err(e) => {
                warn!("request check: {e:#}");
                false
            }
        }
    }

    /// We wrote in `key`, here or on another device: it's no request any more.
    pub(super) fn wrote(&mut self, key: &str) {
        self.not_written.remove(key);
        if self.requests.remove(key) {
            self.threads_dirty = true;
        }
    }

    /// What the phone's storage says about whom it accepts and blocks.
    pub(super) fn phone_answers(&mut self, accepted: HashSet<String>, blocked: HashSet<String>) {
        // a block from here counts until the phone shows it; after that the phone's word goes,
        // so unblocking there works
        self.persist.blocked_here.retain(|k| !blocked.contains(k));
        self.persist.phone_accepted = accepted;
        self.persist.phone_blocked = blocked;
        self.persist_dirty = true;
        self.threads_dirty = true;
    }

    /// The request bar: the answer applies here and goes to our other devices.
    pub(super) async fn answer_request(&mut self, key: &str, answer: RequestAnswer) {
        let Some(thread @ Thread::Contact(ServiceId::Aci(aci))) = parse_thread_key(key) else { return };
        let uuid = Uuid::from(aci);
        info!(?answer, "message request answered");
        let mut out = vec![Outgoing::RequestAnswer { uuid, answer }];
        match answer {
            RequestAnswer::Accept => {
                // they see who we are, and that what is on screen was read
                out.push(Outgoing::ProfileKey(uuid));
                if self.persist.read_receipts != Some(false) {
                    let me = self.me;
                    let seen: Vec<u64> = self
                        .current
                        .iter()
                        .filter(|o| o.key == key)
                        .flat_map(|o| o.window.iter())
                        .filter(|(_, c)| super::window::is_incoming(c, me))
                        .map(|(_, c)| c.timestamp())
                        .collect();
                    if !seen.is_empty() {
                        out.push(Outgoing::Read { to: ServiceId::Aci(aci), timestamps: seen });
                    }
                }
                self.accept(key).await;
            }
            RequestAnswer::Block => self.block(key),
            RequestAnswer::Delete => self.delete_thread(&thread).await,
        }
        self.send_now(out);
    }

    /// One of our other devices answered a request (or blocked someone, accepted a chat).
    pub(super) async fn answered_elsewhere(&mut self, r: &MessageRequestResponse) {
        // groups aren't requests here
        let uuid = r.thread_aci_binary.as_deref().and_then(|b| Uuid::from_slice(b).ok()).or_else(|| r.thread_aci.as_deref()?.parse().ok());
        let Some(uuid) = uuid else { return };
        let thread = Thread::Contact(ServiceId::Aci(uuid.into()));
        let key = thread_key(&thread);
        let answer = r.r#type();
        info!(?answer, "message request answered on another device");
        match answer {
            Answered::Accept => self.accept(&key).await,
            Answered::Delete => self.delete_thread(&thread).await,
            Answered::Block | Answered::BlockAndSpam => self.block(&key),
            Answered::BlockAndDelete => {
                self.block(&key);
                self.delete_thread(&thread).await;
            }
            Answered::Spam | Answered::Unknown => {}
        }
    }

    async fn accept(&mut self, key: &str) {
        self.persist.accepted_here.insert(key.to_string());
        self.persist.blocked_here.remove(key);
        self.persist_dirty = true;
        self.not_written.remove(key);
        self.threads_dirty = true;
        if self.requests.remove(key) && self.current.as_ref().is_some_and(|o| o.key == key) {
            self.request_changed().await;
        }
    }

    /// The open thread became a request or stopped being one: the header says which (the
    /// composer or the request bar) and its pictures are asked for again.
    pub(super) async fn request_changed(&mut self) {
        let Some(thread) = self.current.as_ref().map(|o| o.thread.clone()) else { return };
        self.emit_header(&thread).await;
        // asked for under the old rule (waiting for a click, or not)
        self.thumbs_requested.borrow_mut().clear();
        self.current_dirty = true;
    }

    fn block(&mut self, key: &str) {
        if !self.persist.phone_blocked.contains(key) {
            self.persist.blocked_here.insert(key.to_string());
        }
        self.persist_dirty = true;
        self.requests.remove(key);
        self.notes.remove(notify::By::Thread(key));
        self.threads_dirty = true;
        if self.current.as_ref().is_some_and(|o| o.key == key) {
            self.close_thread();
        }
    }

    /// Deletes a chat: its messages, files and what was kept about it.
    pub(super) async fn delete_thread(&mut self, thread: &Thread) {
        let key = thread_key(thread);
        if self.current.as_ref().is_some_and(|o| o.key == key) {
            self.close_thread();
        }
        // the files are found through the messages, so they go first
        let mut after = 0u64;
        loop {
            let Ok(tss) = self.index.page_after(thread, after, 200).await else { break };
            let (Some(&lo), Some(&hi)) = (tss.first(), tss.last()) else { break };
            after = hi;
            if let Ok(msgs) = self.store.messages(thread, lo..=hi).await {
                for c in msgs.flatten() {
                    let ts = c.metadata.client_timestamp.timestamp_millis() as u64;
                    files::remove_files(&c, ts, &self.paths.attachments, &self.paths.thumbs);
                }
            }
            if tss.len() < 200 {
                break;
            }
        }
        if let Err(e) = self.store.clear_thread(thread).await {
            warn!("delete chat: {e:#}");
        }
        if let Err(e) = self.index.forget_thread(&key).await {
            warn!("delete chat index: {e:#}");
        }
        let p = &mut self.persist;
        p.unread.remove(&key);
        p.snippets.remove(&key);
        p.read_upto.remove(&key);
        p.sweep.remove(&key);
        p.thread_prefs.remove(&key);
        p.mute_override.remove(&key);
        self.persist_dirty = true;
        self.notes.remove(notify::By::Thread(&key));
        self.last_outgoing.remove(&key);
        self.requests.remove(&key);
        self.not_written.remove(&key);
        self.threads_dirty = true;
    }

    /// Nothing is open any more (the UI shows its empty page).
    pub(super) fn close_thread(&mut self) {
        self.current = None;
        self.typing.clear();
        (self.emit)(Event::Header { name: String::new(), sub: String::new(), request: false });
        (self.emit)(Event::Messages { key: String::new(), items: Vec::new(), paging: Default::default(), scroll: Scroll::Bottom });
        (self.emit)(Event::Pins(Vec::new()));
    }

    /// Sends what an answer or a flush has to send.
    pub(super) fn send_now(&mut self, out: Vec<Outgoing>) {
        if out.is_empty() {
            return;
        }
        match (self.manager.clone(), self.offline.as_mut()) {
            (Some(m), _) => {
                tokio::task::spawn_local(receive::send_outgoing(m, out, self.me, self.tx.clone()));
            }
            (None, Some(o)) => o.sent.extend(out),
            (None, None) => {}
        }
    }
}
