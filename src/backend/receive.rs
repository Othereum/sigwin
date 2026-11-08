//! Receive loop: incoming messages, receipts and notifications, index backfills.

use super::*;

impl App {
    /// One-time: index pin, edit and reaction messages presage stored before we kept our own indexes.
    /// Walks every thread oldest first in pages, so pins and unpins apply in order.
    pub(super) fn backfill_pins(&self) {
        let (store, index, me, tx) = (self.store.clone(), self.index.clone(), self.me, self.tx.clone());
        // a second replay could bring back a pin whose unpin is still being sent
        let pins = !self.persist.pins_indexed;
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
                            let pin = if pins { record_pin(&index, &key, c, me).await } else { Ok(false) };
                            for r in [pin, record_edit(&index, &key, c, me).await, record_reaction(&index, &key, c, me).await] {
                                match r {
                                    Ok(true) => n += 1,
                                    Ok(false) => {}
                                    Err(e) => warn!("index pins: {e:#}"),
                                }
                            }
                        }
                    }
                    if (tss.len() as u32) < 200 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
            info!("pin, edit and reaction index backfilled: {n} messages");
            let _ = tx.send(Internal::PinsIndexed);
        });
    }

    /// One-time: index receipts presage stored before we kept our own receipt index.
    pub(super) fn backfill_receipts(&self) {
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

    pub(super) fn start_receiving(&mut self) {
        let Some(m) = self.manager.clone() else { return };
        if let Some(t) = self.recv_task.take() {
            t.abort();
        }
        let tx = self.tx.clone();
        self.connection("동기화 중…", true);
        self.recv_task = Some(tokio::task::spawn_local(async move {
            let mut m = m;
            let mut backoff = 2u64;
            loop {
                match Box::pin(m.receive_messages()).await {
                    Ok(stream) => {
                        info!("receiving");
                        backoff = 2;
                        let _ = tx.send(Internal::Reconnected);
                        pin_mut!(stream);
                        while let Some(r) = stream.next().await {
                            if tx.send(Internal::Received(r)).is_err() {
                                return;
                            }
                        }
                        info!("receiving stopped");
                        let _ = tx.send(Internal::ReceiveStopped("연결이 끊어졌습니다. 다시 연결 중…".into()));
                    }
                    Err(e) => {
                        warn!("receive: {e}");
                        let _ = tx.send(Internal::ReceiveStopped(format!("연결 실패. {backoff}초 후 재시도")));
                    }
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(120);
            }
        }));
    }

    pub(super) async fn received(&mut self, r: Received) {
        match r {
            Received::QueueEmpty => {
                if !self.synced {
                    self.sweep();
                    crate::update::start();
                    self.sync_phone_mutes();
                }
                self.synced = true;
                self.connection("", true);
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

    pub(super) async fn content(&mut self, c: Content) {
        if view::data_message(&c).is_some() {
            // how long the sender took to get it to the server, then the server to get it here
            let (sent, server) = (c.metadata.client_timestamp.timestamp_millis(), c.metadata.server_timestamp.timestamp_millis());
            debug!(sent, sender_ms = server - sent, delivery_ms = now_ms() as i64 - server, "message arrived");
        }
        // read on another device -> clear our badge for that thread
        if let ContentBody::SynchronizeMessage(SyncMessage { read, .. }) = &c.body {
            for r in read {
                // newer clients send only the binary form of the sender
                let (Some(aci), Some(ts)) = (r.parse_sender_aci(), r.timestamp) else { continue };
                if let Ok(Some(t)) = self.store.thread_for_sender_and_timestamp(&ServiceId::Aci(aci), ts).await {
                    let key = thread_key(&t);
                    self.notes.remove(notify::By::Thread(&key));
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

        // the phone changed something in the Storage Service (e.g. a chat's notifications)
        if let ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::FetchLatest(f)), .. }) = &c.body {
            if f.r#type() == presage::proto::sync_message::fetch_latest::Type::StorageManifest {
                self.sync_phone_mutes();
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

        if let Some(target) = view::data_message(&c).and_then(|dm| dm.delete.as_ref()?.target_sent_timestamp) {
            self.notes.remove(notify::By::Message(target));
            // we deleted a message for everyone from another device
            if c.metadata.sender.raw_uuid() == self.me {
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
        // replaced in place, so the thread's last row doesn't say it changed
        if let Some(target) = view::data_message(&c).and_then(|dm| dm.delete.as_ref()?.target_sent_timestamp) {
            self.forget_snippet(&key, target);
        }
        if let Err(e) = record_pin(&self.index, &key, &c, self.me).await {
            warn!("record pin: {e:#}");
        }
        if let Err(e) = record_edit(&self.index, &key, &c, self.me).await {
            warn!("record edit: {e:#}");
        }
        if let Err(e) = record_reaction(&self.index, &key, &c, self.me).await {
            warn!("record reaction: {e:#}");
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
                if !(is_open && self.focused && self.sees_latest()) {
                    let n = self.persist.unread.entry(key.clone()).or_default();
                    // the first news since everything was read: the open list marks where it starts
                    if *n == 0 && !self.watching {
                        if let Some(open) = self.current.as_mut().filter(|o| o.key == key) {
                            open.divider_due = true;
                        }
                    }
                    *n += 1;
                    self.persist_dirty = true;
                }
                // watched: it is on screen already
                if !(is_open && self.watching && !self.focused) {
                    self.notify_message(&thread, &key, &c).await;
                }
            }
            Kind::Msg { outgoing: true, .. } => {
                // we replied from another device: consider the thread read
                self.notes.remove(notify::By::Thread(&key));
                if self.persist.unread.remove(&key).is_some() {
                    self.persist_dirty = true;
                }
            }
            Kind::Reaction { target, ref emoji, remove } => {
                self.notify_reaction(&thread, &key, &c, target, emoji, remove).await;
            }
            _ => {}
        }
        if is_open {
            self.current_dirty = true;
        }
        self.threads_dirty = true;
    }

    /// Periodic work: coalesces bursts of incoming messages into one UI update.
    pub(super) async fn flush(&mut self) {
        self.notes.tick(self.focused);
        if self.next_expiry.is_some_and(|t| t <= view::time::now_ms()) {
            self.next_expiry = None;
            self.current_dirty = true;
            self.sweep();
        }
        if self.pin_expiry.is_some_and(|t| t <= view::time::now_ms()) {
            self.pin_expiry = None;
            self.current_dirty = true;
        }
        if self.mute_due.is_some_and(|t| t <= view::time::now_ms()) {
            self.mute_due = None;
            self.expire_mutes();
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
        if let Err(e) = self.settle_window().await {
            warn!("settle window: {e:#}");
        }
        if self.threads_dirty && self.online() {
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
            if self.online() {
                let delivered = std::mem::take(&mut self.pending_receipts);
                let read = std::mem::take(&mut self.pending_reads);
                let out = receipts_to_send(delivered, read, self.persist.read_receipts != Some(false));
                match (self.manager.clone(), self.offline.as_mut()) {
                    (Some(m), _) => {
                        tokio::task::spawn_local(send_outgoing(m, out, self.me));
                    }
                    (None, Some(o)) => o.sent.extend(out),
                    (None, None) => {}
                }
            }
        }
    }

    /// There is somewhere to send to: an account, or a fixture standing in for one.
    pub(super) fn online(&self) -> bool {
        self.manager.is_some() || self.offline.is_some()
    }
}

/// What goes to the network after a flush.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outgoing {
    /// delivery receipt for these sent timestamps
    Delivery { to: ServiceId, timestamps: Vec<u64> },
    /// read receipt for these sent timestamps
    Read { to: ServiceId, timestamps: Vec<u64> },
    /// one sync to our other devices that these were read
    ReadSync(Vec<(ServiceId, u64)>),
}

/// Delivery receipts, then read receipts (unless turned off on the phone) and one read sync
/// to our other devices so they clear their unread state too.
pub(crate) fn receipts_to_send(
    delivered: HashMap<ServiceId, Vec<u64>>,
    read: Vec<(ServiceId, u64)>,
    read_receipts: bool,
) -> Vec<Outgoing> {
    let mut out: Vec<Outgoing> = delivered.into_iter().map(|(to, timestamps)| Outgoing::Delivery { to, timestamps }).collect();
    if read.is_empty() {
        return out;
    }
    if read_receipts {
        let mut by_sender: HashMap<ServiceId, Vec<u64>> = HashMap::new();
        for (to, ts) in &read {
            by_sender.entry(*to).or_default().push(*ts);
        }
        out.extend(by_sender.into_iter().map(|(to, timestamps)| Outgoing::Read { to, timestamps }));
    }
    out.push(Outgoing::ReadSync(read));
    out
}

pub(super) async fn send_outgoing(mut m: Mgr, out: Vec<Outgoing>, me: Uuid) {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    for o in out {
        match o {
            Outgoing::Delivery { to, timestamps } => {
                let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Delivery as i32), timestamp: timestamps };
                if let Err(e) = Box::pin(m.send_message(to, receipt, now())).await {
                    debug!("delivery receipt to {}: {e}", to.service_id_string());
                }
            }
            Outgoing::Read { to, timestamps } => {
                debug!("read receipt to {} for {timestamps:?}", to.service_id_string());
                let receipt = ReceiptMessage { r#type: Some(receipt_message::Type::Read as i32), timestamp: timestamps };
                if let Err(e) = Box::pin(m.send_message(to, receipt, now())).await {
                    debug!("read receipt to {}: {e}", to.service_id_string());
                }
            }
            Outgoing::ReadSync(read) => {
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
        }
    }
}
