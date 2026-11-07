//! Reactions, edits, deletes and pins on existing messages.

use super::*;

impl App {
    /// The stored message behind row `id` of the open window:
    /// (row, sent timestamp, author, outgoing, data message).
    pub(super) fn target(&self, id: &str) -> Option<(u64, u64, Uuid, bool, DataMessage)> {
        let row = id.parse::<u64>().ok()?;
        let (_, c) = self.current.as_ref()?.window.iter().find(|(t, _)| *t == row)?;
        let Kind::Msg { outgoing, dm } = view::classify(c, self.me) else { return None };
        Some((row, c.timestamp(), c.metadata.sender.raw_uuid(), outgoing, dm.clone()))
    }

    pub(super) async fn react(&mut self, id: &str, emoji: String, remove: bool) {
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
        self.send_op(row, Op::React { emoji: (!remove).then_some(emoji), target }, dm.into()).await;
    }

    pub(super) async fn edit(&mut self, id: &str, text: String, mentions: &[crate::mention::Member]) {
        let text = text.trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        let Some((row, target, _, true, orig)) = self.target(id) else {
            self.notice("이 메시지는 수정할 수 없습니다");
            return;
        };
        if view::now_ms().saturating_sub(row) >= view::EDIT_WINDOW_MS {
            self.notice("보낸 지 24시간이 지난 메시지는 수정할 수 없습니다");
            return;
        }
        let (mut body, body_ranges) = crate::mention::encode(&text, mentions);
        if body.len() > view::MAX_LONG_TEXT_BYTES {
            self.notice("메시지가 너무 깁니다 (최대 64KB)");
            return;
        }
        let Some(mut m) = self.manager.clone() else { return };
        let long = match long_body(&mut m, &mut body, &self.paths.attachments).await {
            Ok(l) => l,
            Err(e) => {
                self.notice(format!("수정 실패: {e:#}"));
                return;
            }
        };
        let dm = DataMessage {
            body: Some(body),
            body_ranges,
            attachments: long.into_iter().collect(),
            quote: orig.quote,
            expire_timer: orig.expire_timer,
            expire_timer_version: orig.expire_timer_version,
            ..Default::default()
        };
        let edit = EditMessage { target_sent_timestamp: Some(target), data_message: Some(dm) };
        self.send_op(row, Op::Edit { text, target }, edit.into()).await;
    }

    pub(super) async fn delete(&mut self, id: &str) {
        let Some((row, target, _, true, _)) = self.target(id) else { return };
        if view::now_ms().saturating_sub(row) >= view::EDIT_WINDOW_MS {
            self.notice("보낸 지 24시간이 지난 메시지는 모두에게서 삭제할 수 없습니다");
            return;
        }
        let dm = DataMessage { delete: Some(Delete { target_sent_timestamp: Some(target) }), ..Default::default() };
        self.send_op(row, Op::Delete(target), dm.into()).await;
    }

    pub(super) async fn pin(&mut self, id: &str, secs: Option<u32>) {
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
    pub(super) async fn unpin(&mut self, id: &str) {
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
    pub(super) async fn load_pins(&mut self) -> Vec<PinDto> {
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
            let Some((dm, by)) = c.as_ref().and_then(|c| match view::classify(c, self.me) {
                Kind::Msg { dm, .. } => Some((dm.clone(), c.metadata.sender.raw_uuid())),
                _ => None,
            }) else {
                continue;
            };
            let dm = self.latest_edit(&key, &thread, target, by).await.unwrap_or(dm);
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

    /// The newest text `author` gave message `target`, if they edited it.
    pub(super) async fn latest_edit(&self, key: &str, thread: &Thread, target: u64, author: Uuid) -> Option<DataMessage> {
        let at = match self.index.latest_edit(key, target, author).await {
            Ok(at) => at?,
            Err(e) => {
                warn!("latest edit: {e:#}");
                return None;
            }
        };
        let c = self.store.message(thread, at).await.ok().flatten()?;
        match view::classify(&c, self.me) {
            Kind::Edit { dm, .. } => Some(dm.clone()),
            _ => None,
        }
    }

    pub(super) async fn send_op(&mut self, row: u64, op: Op, mut body: ContentBody) {
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

    pub(super) async fn group_revision(&self, thread: &Thread) -> u32 {
        match thread {
            Thread::Group(k) => self.store.group(*k).await.ok().flatten().map(|g| g.revision).unwrap_or(0),
            _ => 0,
        }
    }

    // ---- disappearing messages ----
}

/// Applies a stored pin or unpin message to the pin index; true if `c` was one.
pub(super) async fn record_pin(index: &Index, key: &str, c: &Content, me: Uuid) -> anyhow::Result<bool> {
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

/// Applies a stored reaction to the reaction index; true if `c` was one.
pub(super) async fn record_reaction(index: &Index, key: &str, c: &Content, me: Uuid) -> anyhow::Result<bool> {
    let Kind::Reaction { target, emoji, remove } = view::classify(c, me) else { return Ok(false) };
    let at = c.metadata.client_timestamp.timestamp_millis() as u64;
    index.record_reaction(key, target, c.metadata.sender.raw_uuid(), if remove { "" } else { &emoji }, at).await?;
    Ok(true)
}

/// Notes where a stored edit is, for showing it without the window; true if `c` was one.
pub(super) async fn record_edit(index: &Index, key: &str, c: &Content, me: Uuid) -> anyhow::Result<bool> {
    let Kind::Edit { target, .. } = view::classify(c, me) else { return Ok(false) };
    // not `c.timestamp()`: for an edit that's the target's
    let at = c.metadata.client_timestamp.timestamp_millis() as u64;
    index.record_edit(key, target, c.metadata.sender.raw_uuid(), at).await?;
    Ok(true)
}
