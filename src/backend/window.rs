//! The open thread's message window: paging, jumps, rows and header.

use super::*;

impl App {
    /// The messages of `rows` (`(seq, ts)`, any order), in display order.
    pub(super) async fn fetch_range(&self, thread: &Thread, rows: &[(i64, u64)]) -> anyhow::Result<Vec<(u64, Content)>> {
        let (Some(min), Some(max)) = (rows.iter().map(|r| r.1).min(), rows.iter().map(|r| r.1).max()) else {
            return Ok(Vec::new());
        };
        let mut rows = rows.to_vec();
        rows.sort_by_key(|r| r.0);
        // the store only reads timestamp ranges, and one message from a wrong clock can stretch
        // the range over the whole thread: then read the rows one by one
        let mut found: HashMap<u64, Content> = if self.index.count_between(thread, min, max).await? <= rows.len() as u64 * 2 {
            self.store
                .messages(thread, min..=max)
                .await?
                .filter_map(Result::ok)
                .map(|c| (c.metadata.client_timestamp.timestamp_millis() as u64, c))
                .collect()
        } else {
            let mut m = HashMap::with_capacity(rows.len());
            for (_, ts) in &rows {
                if let Ok(Some(c)) = self.store.message(thread, *ts).await {
                    m.insert(*ts, c);
                }
            }
            m
        };
        Ok(rows.into_iter().filter_map(|(_, ts)| Some((ts, found.remove(&ts)?))).collect())
    }

    pub(super) async fn open_thread(&mut self, key: &str) -> anyhow::Result<()> {
        let thread = parse_thread_key(key).context("bad thread key")?;
        let mut rows = self.index.page(&thread, i64::MAX, PAGE).await?;
        let mut window = self.fetch_range(&thread, &rows).await?;
        let mut oldest_seq = rows.last().map_or(i64::MAX, |r| r.0);
        // receipts etc. take rows without producing bubbles: fetch more until the view can scroll
        while rows.len() as u32 == PAGE && window.len() < MAX_WINDOW && displayable(&window, self.me) < 30 {
            rows = self.index.page(&thread, oldest_seq, PAGE).await?;
            oldest_seq = rows.last().map_or(oldest_seq, |r| r.0);
            let mut older = self.fetch_range(&thread, &rows).await?;
            older.append(&mut window);
            window = older;
        }
        let is_group = matches!(thread, Thread::Group(_));
        let members = self.emit_header(&thread).await;
        self.thumbs_requested.clear();
        self.long = Default::default();
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
            has_more: rows.len() as u32 == PAGE,
            window,
            oldest_seq,
        });
        self.mark_read(key);
        self.emit_messages(Scroll::Bottom).await;
        Ok(())
    }

    pub(super) async fn load_older(&mut self) -> anyhow::Result<()> {
        if self.extend_older().await? {
            self.emit_messages(Scroll::Older).await;
        }
        Ok(())
    }

    /// Prepends older rows to the open window; false when there is nothing more to load.
    pub(super) async fn extend_older(&mut self) -> anyhow::Result<bool> {
        let Some(open) = self.current.as_ref() else { return Ok(false) };
        if !open.has_more || open.window.len() >= MAX_WINDOW {
            return Ok(false);
        }
        let mut oldest_seq = open.oldest_seq;
        let thread = open.thread.clone();
        let mut rows = self.index.page(&thread, oldest_seq, PAGE).await?;
        oldest_seq = rows.last().map_or(oldest_seq, |r| r.0);
        let mut older = self.fetch_range(&thread, &rows).await?;
        while rows.len() as u32 == PAGE && older.len() < MAX_WINDOW / 2 && displayable(&older, self.me) < 20 {
            rows = self.index.page(&thread, oldest_seq, PAGE).await?;
            oldest_seq = rows.last().map_or(oldest_seq, |r| r.0);
            let mut more = self.fetch_range(&thread, &rows).await?;
            more.append(&mut older);
            older = more;
        }
        let Some(open) = self.current.as_mut() else { return Ok(false) };
        open.oldest_seq = oldest_seq;
        open.has_more = rows.len() as u32 == PAGE;
        let mut w = older;
        w.append(&mut open.window);
        open.window = w;
        Ok(true)
    }

    /// Scrolls to the message with sent timestamp `id`, loading older rows until it is in the window.
    ///
    /// The target also gets `JUMP_MARGIN` rows above it when history allows. Landing near the
    /// window's first row would make the next scroll fetch an older page while that row is on
    /// screen, and anchoring it (`gui::scroll::hold_older`) fights the wheel: the view shakes.
    /// With a margin, older pages come in through the usual prefetch, off screen.
    pub(super) async fn jump_to(&mut self, id: &str) -> anyhow::Result<()> {
        let target: u64 = id.parse()?;
        let mut loaded = false;
        loop {
            let Some(open) = self.current.as_ref() else { return Ok(()) };
            if let Some(pos) = open.window.iter().position(|(_, c)| c.timestamp() == target) {
                if displayable(&open.window[..pos], self.me) < JUMP_MARGIN && self.extend_older().await? {
                    loaded = true;
                    continue;
                }
                // nothing was prepended (no more history): `pos` still holds
                let Some(open) = self.current.as_ref() else { return Ok(()) };
                let row = open.window[pos].0.to_string();
                if loaded {
                    self.emit_messages(Scroll::Keep).await;
                }
                (self.emit)(Event::JumpTo(row));
                return Ok(());
            }
            // a synced message's row can have another timestamp than the one it's quoted by:
            // not knowing the target doesn't rule it out unless the window is past it by time
            let oldest_ts = open.window.first().map(|(t, _)| *t);
            let passed = match self.index.seq_of(&open.thread, target).await? {
                Some(seq) => seq >= open.oldest_seq,
                None => oldest_ts.is_some_and(|t| t < target),
            };
            if passed || !self.extend_older().await? {
                if loaded {
                    self.emit_messages(Scroll::Keep).await;
                }
                self.notice(if passed { "메시지를 찾을 수 없습니다" } else { "너무 오래된 메시지라 이동할 수 없습니다" });
                return Ok(());
            }
            loaded = true;
        }
    }

    pub(super) async fn reload_window(&mut self) -> anyhow::Result<()> {
        let Some(open) = self.current.as_ref() else { return Ok(()) };
        let thread = open.thread.clone();
        let rows = if open.oldest_seq == i64::MAX {
            self.index.page(&thread, i64::MAX, PAGE).await?
        } else {
            self.index.rows_from(&thread, open.oldest_seq).await?
        };
        let window = self.fetch_range(&thread, &rows).await?;
        let oldest_seq = rows.iter().map(|r| r.0).min();
        // the header carries the disappearing-message timer, which may just have changed
        let members = self.emit_header(&thread).await;
        if let Some(open) = self.current.as_mut() {
            open.window = window;
            open.members = members;
            open.oldest_seq = oldest_seq.unwrap_or(open.oldest_seq).min(open.oldest_seq);
        }
        if self.focused {
            if let Some(key) = self.current.as_ref().map(|o| o.key.clone()) {
                self.mark_read(&key);
            }
        }
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
        Ok(())
    }

    pub(super) async fn emit_messages(&mut self, scroll: Scroll) {
        let Some(open) = self.current.as_ref() else { return };
        let tss: Vec<u64> = open.window.iter().map(|(t, _)| *t).collect();
        let mut receipts: HashMap<u64, HashMap<Uuid, u8>> = HashMap::new();
        match self.index.receipts_for(&tss).await {
            Ok(rows) => {
                for (ts, who, level) in rows {
                    receipts.entry(ts).or_default().insert(Uuid::from_bytes(*who.as_bytes()), level);
                }
            }
            Err(e) => warn!("receipts: {e:#}"),
        }
        let my_deletes = self.index.my_deletes_in(&tss).await.unwrap_or_else(|e| {
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
        let built = view::build(&open.window, self.me, &open.members, &receipts, &my_deletes, &self.long, &|u| {
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
        // not stored yet (once stored, the real row is shown)
        let stubs: Vec<&Pending> = self
            .pending
            .iter()
            .filter(|p| p.thread_key == key && !open.window.iter().any(|(t, _)| *t == p.ts))
            .collect();
        // failed rows stay where they failed, new messages go below them
        if stubs.iter().any(|p| p.after_seq.is_some()) {
            let seqs: HashMap<u64, i64> = self
                .index
                .rows_from(&open.thread, open.oldest_seq)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(seq, ts)| (ts, seq))
                .collect();
            for p in &stubs {
                let Some(after) = p.after_seq else { continue };
                // above what's loaded: it comes back with its older neighbours
                if after < open.oldest_seq && open.has_more {
                    continue;
                }
                let i = failed_row_index(&items, &seqs, after);
                let prev = i.checked_sub(1).map(|i| items[i].ts);
                items.insert(i, pending_row(p, prev));
            }
        }
        // on their way: at the bottom
        let mut prev = items.last().map(|m| m.ts);
        for p in stubs.iter().filter(|p| p.after_seq.is_none()) {
            items.push(pending_row(p, prev));
            prev = Some(p.ts);
        }
        view::mark_runs(&mut items, open.is_group);
        for (k, ptr, fit) in built.wanted_thumbs {
            if self.thumbs_requested.insert(k.clone()) {
                self.request_thumb(k.clone(), k, ptr, ThumbShape::Fit(fit));
            }
        }
        for c in built.wanted_cells {
            if self.thumbs_requested.insert(c.key.clone()) {
                self.request_thumb(c.key, c.file_key, c.ptr, ThumbShape::Cover(c.size.0, c.size.1));
            }
        }
        for (k, ptr) in built.wanted_texts {
            if self.thumbs_requested.insert(k.clone()) {
                self.request_long_text(k, ptr);
            }
        }
        self.next_expiry = next_expiry;
        self.load_sender_avatars(&items);
        (self.emit)(Event::Messages { key, items, can_older, scroll });
        // right after the rows (same UI turn): sent earlier, a thread switch showed the new
        // thread's pin bar over the old rows for a frame
        (self.emit)(Event::Pins(pins));
    }

    /// Emits the conversation header; returns the group members (empty for 1:1).
    pub(super) async fn emit_header(&mut self, thread: &Thread) -> Vec<Uuid> {
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
}

/// Messages that produce a visible row.
pub(super) fn displayable(window: &[(u64, Content)], me: Uuid) -> usize {
    window
        .iter()
        .filter(|(_, c)| matches!(view::classify(c, me), Kind::Msg { .. } | Kind::System(_) | Kind::Deleted | Kind::Pin { .. }))
        .count()
}
