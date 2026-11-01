//! Receive loop: incoming messages, receipts and notifications, index backfills.

use super::*;

impl App {
    /// One-time: index pin messages presage stored before we kept our own pin index.
    /// Walks every thread oldest first in pages, so pins and unpins apply in order.
    pub(super) fn backfill_pins(&self) {
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

    pub(super) async fn received(&mut self, r: Received) {
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

    pub(super) async fn content(&mut self, c: Content) {
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
                    notify(title, body, key.clone());
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
    pub(super) async fn flush(&mut self) {
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
}

/// Delivery receipts, then read receipts (unless turned off on the phone) and one read sync
/// to our other devices so they clear their unread state too.
pub(super) async fn send_receipts(
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

pub(super) fn notify(title: String, body: String, key: String) {
    let Some((title, body)) = crate::settings::toast_text(title, body) else { return };
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crate::toast::show(&title, &body, Some(key)) {
            debug!("notification: {e}");
        }
    });
}
