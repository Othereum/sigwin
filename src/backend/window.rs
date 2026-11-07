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

    /// The rows of `rows` that show in the list, in display order.
    async fn fetch_shown(&self, thread: &Thread, rows: &[(i64, u64)]) -> anyhow::Result<Vec<(u64, Content)>> {
        let mut found = self.fetch_range(thread, rows).await?;
        found.retain(|(_, c)| shows(c, self.me));
        Ok(found)
    }

    /// The thread's newest rows, enough for the view to scroll.
    async fn latest_rows(&self, thread: &Thread) -> anyhow::Result<Span> {
        let mut rows = self.index.page(thread, i64::MAX, PAGE).await?;
        let tail_seq = rows.first().map_or(0, |r| r.0);
        let mut window = self.fetch_shown(thread, &rows).await?;
        let mut oldest_seq = rows.last().map_or(i64::MAX, |r| r.0);
        while rows.len() as u32 == PAGE && window.len() < 30 {
            rows = self.index.page(thread, oldest_seq, PAGE).await?;
            oldest_seq = rows.last().map_or(oldest_seq, |r| r.0);
            let mut older = self.fetch_shown(thread, &rows).await?;
            older.append(&mut window);
            window = older;
        }
        Ok(Span { window, oldest_seq, has_more: rows.len() as u32 == PAGE, end_seq: None, tail_seq })
    }

    /// The user is looking at the thread's newest messages.
    pub(super) fn sees_latest(&self) -> bool {
        self.at_bottom && self.current.as_ref().is_some_and(|o| o.end_seq.is_none())
    }

    /// Marks the open thread read if the user can see its newest messages.
    pub(super) fn read_if_seen(&mut self) {
        if self.focused && self.sees_latest() {
            if let Some(key) = self.current.as_ref().map(|o| o.key.clone()) {
                self.mark_read(&key);
            }
        }
    }

    /// Back to the thread's newest messages.
    async fn to_latest(&mut self) -> anyhow::Result<()> {
        let Some(thread) = self.current.as_ref().map(|o| o.thread.clone()) else { return Ok(()) };
        let span = self.latest_rows(&thread).await?;
        let Some(open) = self.current.as_mut() else { return Ok(()) };
        open.show(span);
        self.moved = true;
        self.at_bottom = true;
        self.read_if_seen();
        Ok(())
    }

    /// Keeps the window within `MAX_WINDOW` by dropping its newest rows.
    async fn trim_newest(&mut self) -> anyhow::Result<()> {
        let Some(open) = self.current.as_mut().filter(|o| o.window.len() > MAX_WINDOW) else { return Ok(()) };
        open.window.truncate(MAX_WINDOW);
        let last = open.window[MAX_WINDOW - 1].0;
        // gone since it was read: reloading then reaches the end, which is harmless
        open.end_seq = Some(self.index.seq_of(&open.thread, last).await?.unwrap_or(i64::MAX));
        self.moved = true;
        Ok(())
    }

    /// Keeps the newest `keep` rows of the window at most.
    async fn trim_oldest(&mut self, keep: usize) -> anyhow::Result<()> {
        let Some(open) = self.current.as_mut().filter(|o| o.window.len() > keep) else { return Ok(()) };
        let extra = open.window.len() - keep;
        open.window.drain(..extra);
        if let Some(seq) = self.index.seq_of(&open.thread, open.window[0].0).await? {
            open.oldest_seq = seq;
        }
        open.has_more = true;
        self.moved = true;
        Ok(())
    }

    /// Scrolling up is over once the user has stayed at the newest messages for a while: what it
    /// loaded goes, and scrolling up again loads it back like any older page.
    pub(super) async fn settle_window(&mut self) -> anyhow::Result<()> {
        if !self.sees_latest() || self.current.as_ref().is_none_or(|o| o.window.len() <= PAGE as usize) {
            self.settle_at = None;
            return Ok(());
        }
        let due = *self.settle_at.get_or_insert_with(|| Instant::now() + SETTLE_AFTER);
        if Instant::now() < due {
            return Ok(());
        }
        self.settle_at = None;
        self.trim_oldest(PAGE as usize).await?;
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
        Ok(())
    }

    pub(super) async fn open_thread(&mut self, key: &str) -> anyhow::Result<()> {
        let thread = parse_thread_key(key).context("bad thread key")?;
        let span = self.latest_rows(&thread).await?;
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
            has_more: span.has_more,
            window: span.window,
            oldest_seq: span.oldest_seq,
            end_seq: span.end_seq,
            tail_seq: span.tail_seq,
        });
        self.at_bottom = true;
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

    pub(super) async fn load_newer(&mut self) -> anyhow::Result<()> {
        if self.extend_newer().await? {
            self.emit_messages(Scroll::Keep).await;
            self.read_if_seen();
        }
        Ok(())
    }

    /// Appends newer rows to the open window, dropping its oldest past `MAX_WINDOW`; false when
    /// it already reaches the thread's newest message.
    async fn extend_newer(&mut self) -> anyhow::Result<bool> {
        let Some(open) = self.current.as_ref() else { return Ok(false) };
        let Some(mut end) = open.end_seq else { return Ok(false) };
        let thread = open.thread.clone();
        let mut rows = self.index.page_newer(&thread, end, PAGE).await?;
        end = rows.last().map_or(end, |r| r.0);
        let mut newer = self.fetch_shown(&thread, &rows).await?;
        while rows.len() as u32 == PAGE && newer.len() < 20 {
            rows = self.index.page_newer(&thread, end, PAGE).await?;
            end = rows.last().map_or(end, |r| r.0);
            newer.append(&mut self.fetch_shown(&thread, &rows).await?);
        }
        let Some(open) = self.current.as_mut() else { return Ok(false) };
        open.end_seq = (rows.len() as u32 == PAGE).then_some(end);
        open.tail_seq = end;
        open.window.append(&mut newer);
        self.trim_oldest(MAX_WINDOW).await?;
        Ok(true)
    }

    /// Prepends older rows to the open window, dropping its newest past `MAX_WINDOW`; false when
    /// there is nothing more to load.
    pub(super) async fn extend_older(&mut self) -> anyhow::Result<bool> {
        let Some(open) = self.current.as_ref() else { return Ok(false) };
        if !open.has_more {
            return Ok(false);
        }
        let mut oldest_seq = open.oldest_seq;
        let thread = open.thread.clone();
        let mut rows = self.index.page(&thread, oldest_seq, PAGE).await?;
        oldest_seq = rows.last().map_or(oldest_seq, |r| r.0);
        let mut older = self.fetch_shown(&thread, &rows).await?;
        while rows.len() as u32 == PAGE && older.len() < 20 {
            rows = self.index.page(&thread, oldest_seq, PAGE).await?;
            oldest_seq = rows.last().map_or(oldest_seq, |r| r.0);
            let mut more = self.fetch_shown(&thread, &rows).await?;
            more.append(&mut older);
            older = more;
        }
        let Some(open) = self.current.as_mut() else { return Ok(false) };
        open.oldest_seq = oldest_seq;
        open.has_more = rows.len() as u32 == PAGE;
        let mut w = older;
        w.append(&mut open.window);
        open.window = w;
        self.trim_newest().await?;
        Ok(true)
    }

    /// Reads the rows around display position `seq` (either side a page), without touching the
    /// window.
    async fn rows_around(&self, thread: &Thread, seq: i64) -> anyhow::Result<Span> {
        let older = self.index.page(thread, seq + 1, PAGE).await?;
        let newer = self.index.page_newer(thread, seq, PAGE).await?;
        let oldest_seq = older.last().map_or(seq, |r| r.0);
        let tail_seq = newer.last().map_or(seq, |r| r.0);
        let end_seq = (newer.len() as u32 == PAGE).then_some(tail_seq);
        let has_more = older.len() as u32 == PAGE;
        let rows: Vec<(i64, u64)> = older.into_iter().chain(newer).collect();
        Ok(Span { window: self.fetch_shown(thread, &rows).await?, oldest_seq, has_more, end_seq, tail_seq })
    }

    /// Scrolls to the message with sent timestamp `id`, moving the window to it if needed.
    ///
    /// The target also gets `JUMP_MARGIN` rows either side when history allows. Landing near the
    /// window's first row would make the next scroll fetch an older page while that row is on
    /// screen, and anchoring it (`gui::scroll::hold_older`) fights the wheel: the view shakes.
    /// With a margin, more pages come in through the usual prefetch, off screen.
    pub(super) async fn jump_to(&mut self, id: &str) -> anyhow::Result<()> {
        let target: u64 = id.parse()?;
        let is_target = |c: &Content| c.timestamp() == target;
        let mut loaded = false;
        let mut searched = false;
        // each side's page can trim what the other side loaded: a row-poor stretch mustn't loop
        let mut margin_pages = 8;
        loop {
            let Some(open) = self.current.as_ref() else { return Ok(()) };
            if let Some(pos) = open.window.iter().position(|(_, c)| is_target(c)) {
                let (above, below) = (pos, open.window.len() - pos - 1);
                margin_pages -= 1;
                if margin_pages > 0 && above < JUMP_MARGIN && self.extend_older().await? {
                    loaded = true;
                    continue;
                }
                if margin_pages > 0 && below < JUMP_MARGIN && self.extend_newer().await? {
                    loaded = true;
                    continue;
                }
                // nothing was added (no more history): `pos` still holds
                let Some(open) = self.current.as_ref() else { return Ok(()) };
                let row = open.window[pos].0.to_string();
                if loaded {
                    self.emit_messages(Scroll::Keep).await;
                }
                (self.emit)(Event::JumpTo(row));
                return Ok(());
            }
            let (thread, oldest_seq, len) = (open.thread.clone(), open.oldest_seq, open.window.len());
            // a synced message's row can have another timestamp than the one it's quoted by:
            // it's then around the first row sent after that
            let seq = match self.index.seq_of(&thread, target).await? {
                Some(seq) => Some(seq),
                None => self.index.seq_from_ts(&thread, target).await?,
            };
            let Some(seq) = seq.filter(|_| !searched) else {
                if loaded {
                    self.emit_messages(Scroll::Keep).await;
                }
                self.notice("메시지를 찾을 수 없습니다");
                return Ok(());
            };
            searched = true;
            // just above the window and within its budget: keep what's loaded
            let near = seq < oldest_seq && len + self.index.count_rows(&thread, seq, oldest_seq).await? as usize + 2 * PAGE as usize <= MAX_WINDOW;
            if near {
                while self.current.as_ref().is_some_and(|o| o.oldest_seq > seq) && self.extend_older().await? {}
                loaded = true;
                continue;
            }
            // nothing changes for a message that isn't there
            let span = self.rows_around(&thread, seq).await?;
            if !span.window.iter().any(|(_, c)| is_target(c)) {
                continue;
            }
            let Some(open) = self.current.as_mut() else { return Ok(()) };
            open.show(span);
            self.moved = true;
            loaded = true;
        }
    }

    pub(super) async fn reload_window(&mut self) -> anyhow::Result<()> {
        let Some(open) = self.current.as_ref() else { return Ok(()) };
        let thread = open.thread.clone();
        // the rows it has, and what's new: not the stretch they span, which can be mostly receipts
        let tss: Vec<u64> = open.window.iter().map(|(t, _)| *t).collect();
        let mut rows = self.index.rows_at(&thread, &tss).await?;
        if open.end_seq.is_none() {
            rows.append(&mut self.index.rows_from(&thread, open.tail_seq + 1).await?);
        }
        let window = self.fetch_shown(&thread, &rows).await?;
        let oldest_seq = rows.iter().map(|r| r.0).min();
        let tail_seq = rows.iter().map(|r| r.0).max();
        // the header carries the disappearing-message timer, which may just have changed
        let members = self.emit_header(&thread).await;
        if let Some(open) = self.current.as_mut() {
            open.window = window;
            open.members = members;
            open.oldest_seq = oldest_seq.unwrap_or(open.oldest_seq).min(open.oldest_seq);
            open.tail_seq = tail_seq.unwrap_or(open.tail_seq).max(open.tail_seq);
        }
        // new messages grow it: past the budget (with some slack, to trim in batches) it lets go
        // of the rows further from the view
        if self.current.as_ref().is_some_and(|o| o.window.len() > MAX_WINDOW + PAGE as usize) {
            if self.at_bottom {
                self.trim_oldest(MAX_WINDOW).await?;
            } else {
                self.trim_newest().await?;
            }
        }
        self.read_if_seen();
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
        Ok(())
    }

    pub(super) async fn emit_messages(&mut self, scroll: Scroll) {
        // whatever puts the view at the bottom means the thread's real bottom
        if scroll == Scroll::Bottom && self.current.as_ref().is_some_and(|o| o.end_seq.is_some()) {
            if let Err(e) = self.to_latest().await {
                warn!("to latest: {e:#}");
            }
        }
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
        let reactions = self.index.reactions_for(&open.key, &tss).await.unwrap_or_else(|e| {
            warn!("reactions: {e:#}");
            Default::default()
        });
        let mut edits = Vec::new();
        match self.index.edits_for(&open.key, &tss).await {
            Ok(rows) => {
                for (_, _, at) in rows {
                    if let Ok(Some(c)) = self.store.message(&open.thread, at).await {
                        edits.push((at, c));
                    }
                }
            }
            Err(e) => warn!("edits: {e:#}"),
        }
        let indexed = view::Indexed { status: receipts, my_deletes, reactions, edits };
        // resolve names first (async), then build synchronously
        let mut senders: Vec<Uuid> = open.window.iter().map(|(_, c)| c.metadata.sender.raw_uuid()).collect();
        senders.extend(open.members.iter().copied());
        senders.extend(indexed.status.values().flat_map(|m| m.keys().copied()));
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
        let built = view::build(&open.window, self.me, &open.members, &indexed, &self.long, &|u| {
            names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into())
        });
        let key = open.key.clone();
        let paging = Paging { older: open.has_more, newer: open.end_seq.is_some() };
        let next_expiry = built.next_expiry;
        let mut items = built.items;
        // rows by sent timestamp, for the pin marks
        let rows: HashMap<u64, String> = open.window.iter().map(|(t, c)| (c.timestamp(), t.to_string())).collect();
        let mut pins = self.load_pins().await;
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
                    Op::React { emoji, .. } => view::set_my_reaction(m, emoji.as_deref()),
                    Op::Edit { text, target } => {
                        view::set_edited_text(m, text);
                        let id = target.to_string();
                        if let Some(p) = pins.iter_mut().find(|p| p.id == id) {
                            p.text = text.replace('\n', " ");
                        }
                    }
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
                .rows_at(&open.thread, &tss)
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
                // below what's loaded: the same
                if open.end_seq.is_some_and(|end| after > end) {
                    continue;
                }
                let i = failed_row_index(&items, &seqs, after);
                let prev = i.checked_sub(1).map(|i| items[i].ts);
                items.insert(i, pending_row(p, prev));
            }
        }
        // on their way: at the bottom
        let mut prev = items.last().map(|m| m.ts);
        for p in stubs.iter().filter(|p| p.after_seq.is_none() && open.end_seq.is_none()) {
            items.push(pending_row(p, prev));
            prev = Some(p.ts);
        }
        view::mark_runs(&mut items, open.is_group);
        // rows that left the window let go of what they asked for, so coming back asks again
        if std::mem::take(&mut self.moved) {
            let keep: HashSet<&str> = built
                .wanted_thumbs
                .iter()
                .map(|t| t.0.as_str())
                .chain(built.wanted_cells.iter().map(|c| c.key.as_str()))
                .chain(built.long_keys.iter().map(String::as_str))
                .collect();
            self.thumbs_requested.retain(|k| keep.contains(k.as_str()));
            self.long.full.retain(|k, _| keep.contains(k.as_str()));
            self.long.shown.retain(|row, _| open.window.iter().any(|(t, _)| t == row));
        }
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
        (self.emit)(Event::Messages { key, items, paging, scroll });
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

impl Open {
    fn show(&mut self, span: Span) {
        self.window = span.window;
        self.oldest_seq = span.oldest_seq;
        self.has_more = span.has_more;
        self.end_seq = span.end_seq;
        self.tail_seq = span.tail_seq;
    }
}

/// A stored message that produces a visible row.
fn shows(c: &Content, me: Uuid) -> bool {
    matches!(view::classify(c, me), Kind::Msg { .. } | Kind::System(_) | Kind::Deleted | Kind::Pin { .. })
}
