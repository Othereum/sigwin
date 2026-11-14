//! Thread list: names, snippets, unread counts, read receipts, avatars.

use super::window::{fetch_rows, is_incoming, newly_read};
use super::*;

pub(super) const NOTE_TO_SELF: &str = "나에게 쓰는 메모";

/// A number as people write it: "+82 10-1234-5678".
pub(super) fn show_number(p: &presage::libsignal_service::prelude::phonenumber::PhoneNumber) -> String {
    p.format().mode(presage::libsignal_service::prelude::phonenumber::Mode::International).to_string()
}

/// A group's title as shown; "그룹" without one.
pub(super) fn group_name(title: &str) -> String {
    let t = crate::link::one_line(title);
    if t.is_empty() { "그룹".into() } else { t }
}

/// What someone is called when there's no name, profile or number to go by.
pub(super) fn unknown_name(uuid: Uuid) -> String {
    format!("알 수 없음 ({})", &uuid.to_string()[..8])
}

impl App {
    pub(super) async fn name(&mut self, uuid: Uuid) -> String {
        if let Some(n) = self.names.get(&uuid) {
            return n.clone();
        }
        let sid = ServiceId::Aci(uuid.into());
        let contact = self.store.contact_by_id(&sid).await.ok().flatten();
        let mut name = contact.as_ref().map(|c| crate::link::one_line(&c.name)).unwrap_or_default();
        if name.is_empty() {
            if let Ok(Some(key)) = self.store.profile_key(&sid).await {
                if let Ok(Some(p)) = self.store.profile(uuid, key).await {
                    if let Some(n) = p.name {
                        name = combine_names(&n.given_name, n.family_name.as_deref().unwrap_or(""));
                    }
                }
            }
        }
        // our own profile key isn't filed under our uuid in the store: use the registration's
        if name.is_empty() && uuid == self.me {
            if let Some(m) = self.manager.as_mut() {
                match Box::pin(m.retrieve_profile()).await {
                    Ok(p) => {
                        if let Some(n) = p.name {
                            name = combine_names(&n.given_name, n.family_name.as_deref().unwrap_or(""));
                        }
                    }
                    Err(e) => warn!("own profile: {e:#}"),
                }
            }
        }
        if name.is_empty() {
            name = contact
                .and_then(|c| c.phone_number)
                .map(|p| show_number(&p))
                .unwrap_or_else(|| unknown_name(uuid));
        }
        self.names.insert(uuid, name.clone());
        name
    }

    pub(super) async fn thread_name(&mut self, thread: &Thread) -> String {
        match thread {
            Thread::Contact(id) if id.raw_uuid() == self.me => NOTE_TO_SELF.into(),
            Thread::Contact(ServiceId::Pni(p)) => self.pni_name((*p).into()).await,
            Thread::Contact(id) => self.name(id.raw_uuid()).await,
            Thread::Group(key) => match self.store.group(*key).await {
                Ok(Some(g)) => group_name(&g.title),
                _ => group_name(""),
            },
        }
    }

    pub(super) async fn rebuild_threads(&mut self) -> anyhow::Result<()> {
        let rows = self.index.threads().await?;
        // cached while only the newest rows were searched, before edits counted, before videos
        // had their mark
        if !self.persist.snippets_namedle || !self.persist.snippets_named || !self.persist.snippets_video {
            self.persist.snippets.clear();
            self.persist.snippets_namedle = true;
            self.persist.snippets_named = true;
            self.persist.snippets_video = true;
            self.persist_dirty = true;
        }
        let mut list: Vec<(i64, ThreadDto)> = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        let open_was_request = self.current.as_ref().is_some_and(|o| self.is_request(&o.key));
        for row in rows {
            let key = thread_key(&row.thread);
            seen.insert(key.clone());
            // a blocked person's chat leaves the list (its messages stay, unblocking brings it back)
            if self.is_blocked(&key) {
                continue;
            }
            let (display_ts, snippet) = match self.persist.snippets.get(&key) {
                // cached before mentions were resolved: recompute
                Some((last, ts, s)) if *last == row.tail_ts && !s.contains('\u{FFFC}') => (*ts, s.clone()),
                _ => {
                    let (ts, s) = self.refresh_snippet(&key, &row.thread, row.tail_ts).await;
                    self.persist.snippets.insert(key.clone(), (row.tail_ts, ts, s.clone()));
                    self.persist_dirty = true;
                    (ts, s)
                }
            };
            // nothing to show: someone we never talked with (whose receipts were stored here
            // before they were moved out), or whose messages have all gone
            if display_ts == 0 && matches!(row.thread, Thread::Contact(_)) {
                continue;
            }
            let name = self.thread_name(&row.thread).await;
            // what a stranger wrote doesn't show before it's accepted
            let snippet = if self.check_request(&row.thread).await { "메시지 요청".to_string() } else { snippet };
            list.push((
                self.index.seq_of(&row.thread, display_ts).await.ok().flatten().unwrap_or(0),
                ThreadDto {
                    unread: self.persist.unread.get(&key).copied().unwrap_or(0),
                    is_group: matches!(row.thread, Thread::Group(_)),
                    mute: self.mute(&key),
                    prefs: self.prefs(&key),
                    phone_mentions: self.phone_mentions(&key),
                    time: view::time::time_short(display_ts),
                    ts: display_ts,
                    key,
                    name,
                    snippet,
                    readers: Vec::new(),
                    readers_more: 0,
                },
            ));
        }
        self.fill_readers(&mut list).await;
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
                        mute: self.mute(&key),
                        prefs: self.prefs(&key),
                        phone_mentions: self.phone_mentions(&key),
                        key,
                        name: group_name(&g.title),
                        snippet: String::new(),
                        time: String::new(),
                        ts: 0,
                        unread: 0,
                        is_group: true,
                        readers: Vec::new(),
                        readers_more: 0,
                    },
                ));
            }
        }
        list.sort_by(|a, b| b.0.cmp(&a.0));
        // the UI re-sorts by `ts` when there are drafts: a late message's old timestamp
        // mustn't sink its thread there. `time` keeps the message's own
        let mut floor = 0;
        for (_, t) in list.iter_mut().rev() {
            floor = floor.max(t.ts);
            t.ts = floor;
        }
        self.threads = list.into_iter().map(|(_, t)| t).collect();
        let requests = self.requests.len();
        if self.logged_requests != Some(requests) {
            info!(requests, "message requests");
            self.logged_requests = Some(requests);
        }
        // the phone accepted or blocked the open chat meanwhile
        if let Some(key) = self.current.as_ref().map(|o| o.key.clone()) {
            if self.is_blocked(&key) {
                self.close_thread();
            } else if open_was_request != self.is_request(&key) {
                self.request_changed().await;
            }
        }
        self.mute_due = self.next_mute_change();
        self.emit_threads().await;
        self.load_avatars();
        self.load_list_reader_avatars();
        Ok(())
    }

    /// Whoever read the message each thread shows, when it is ours (`ThreadDto::readers`). The
    /// rows' `ts` is the message's own here, before the list floors it.
    async fn fill_readers(&mut self, list: &mut [(i64, ThreadDto)]) {
        let mut mine = Vec::new();
        for (_, t) in list.iter() {
            if t.ts == 0 {
                continue;
            }
            let known = self.last_outgoing.get(&t.key).filter(|(ts, _)| *ts == t.ts).map(|(_, out)| *out);
            let outgoing = match known {
                Some(out) => out,
                None => {
                    let out = match parse_thread_key(&t.key) {
                        Some(thread) => match self.store.message(&thread, t.ts).await {
                            Ok(Some(c)) => matches!(view::classify(&c, self.me), Kind::Msg { outgoing: true, .. }),
                            _ => false,
                        },
                        None => false,
                    };
                    self.last_outgoing.insert(t.key.clone(), (t.ts, out));
                    out
                }
            };
            if outgoing {
                mine.push(t.ts);
            }
        }
        if mine.is_empty() {
            return;
        }
        let Ok(receipts) = self.index.receipts_for(&mine).await else { return };
        let mut by_ts: HashMap<u64, Vec<Uuid>> = HashMap::new();
        for (ts, who, level) in receipts {
            if level >= 2 && who != self.me {
                by_ts.entry(ts).or_default().push(who);
            }
        }
        for (_, t) in list.iter_mut() {
            let Some(who) = by_ts.remove(&t.ts).filter(|_| mine.contains(&t.ts)) else { continue };
            let mut named = Vec::with_capacity(who.len());
            for u in who {
                named.push((self.name(u).await, u.to_string()));
            }
            named.sort();
            t.readers_more = named.len().saturating_sub(crate::types::MAX_READER_PHOTOS) as u32;
            t.readers = named.into_iter().take(crate::types::MAX_READER_PHOTOS).map(|(n, u)| (u, n)).collect();
        }
    }

    /// Photos of those shown as readers in the thread list, after the threads' own.
    fn load_list_reader_avatars(&mut self) {
        let uuids: Vec<Uuid> = self.threads.iter().flat_map(|t| t.readers.iter()).filter_map(|(u, _)| u.parse().ok()).collect();
        for u in uuids {
            let key = format!("{}{u}", crate::avatar::LIST_READ_PREFIX);
            if self.avatars_done.insert(key.clone()) {
                let thread = Thread::Contact(ServiceId::Aci(u.into()));
                self.avatar_queue.push_back(crate::avatar::Want { key, thread, size: crate::avatar::LIST_READ_SIZE });
            }
        }
        self.run_avatars();
    }

    /// Queues the photos of listed threads not seen yet this session.
    pub(super) fn load_avatars(&mut self) {
        let me = Thread::Contact(ServiceId::Aci(self.me.into()));
        for (key, size) in [(crate::avatar::ME_SIDEBAR, crate::avatar::ME_SIDEBAR_SIZE), (crate::avatar::ME_CARD, crate::avatar::ME_CARD_SIZE)] {
            if self.avatars_done.insert(key.to_string()) {
                self.avatar_queue.push_front(crate::avatar::Want { key: key.into(), thread: me.clone(), size });
            }
        }
        let keys: Vec<String> = self.threads.iter().filter(|t| !self.avatars_done.contains(&t.key)).map(|t| t.key.clone()).collect();
        for key in keys {
            if let Some(thread) = parse_thread_key(&key) {
                self.avatars_done.insert(key.clone());
                self.avatar_queue.push_back(crate::avatar::Want { key, thread, size: crate::avatar::SIZE });
            }
        }
        self.run_avatars();
    }

    /// The scale changed: photos and pictures on screen are asked for again, resampled to the new
    /// physical size (the renderer would scale the old ones nearest-neighbor). Disk caches are
    /// size-independent or keyed by size, so this costs no downloads.
    pub(super) async fn rescale(&mut self) {
        self.avatars_done.clear();
        self.avatar_queue.clear();
        self.load_avatars();
        self.load_list_reader_avatars();
        if self.current.is_some() {
            self.thumbs_requested.borrow_mut().clear();
            // also asks for the message list's avatars again
            self.emit_messages(crate::types::Scroll::Keep).await;
        }
    }

    /// Photos of the senders shown next to group messages; ahead of the thread list.
    pub(super) fn load_sender_avatars(&mut self, items: &[MsgDto]) {
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
    pub(super) fn run_avatars(&mut self) {
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
            own: (self.me, manager.registration_data().profile_key()),
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

    /// `view::text::snippet` with @mention names resolved.
    pub(super) async fn snippet(&mut self, c: &Content) -> Option<String> {
        for u in view::data_message(c).map(view::text::mentioned).unwrap_or_default() {
            self.name(u).await;
        }
        let names = &self.names;
        view::text::snippet(c, self.me, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into()))
    }

    /// `snippet` with the author's newest edit of the message.
    /// In a group, a received message is prefixed with its sender's name.
    async fn edited_snippet(&mut self, key: &str, thread: &Thread, c: &Content) -> Option<String> {
        let Kind::Msg { outgoing, .. } = view::classify(c, self.me) else { return self.snippet(c).await };
        if view::text::snippet(c, self.me, &|_| String::new()).is_none() {
            return None;
        }
        let sender = c.metadata.sender.raw_uuid();
        let text = match self.latest_edit(key, thread, c.timestamp(), sender).await {
            None => self.snippet(c).await?,
            Some(dm) => {
                for u in view::text::mentioned(&dm) {
                    self.name(u).await;
                }
                let names = &self.names;
                view::text::message_snippet(&dm, outgoing, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into()))
            }
        };
        if outgoing || !matches!(thread, Thread::Group(_)) {
            return Some(text);
        }
        let who = self.name(sender).await;
        Some(format!("{who}: {text}"))
    }

    /// What the thread list shows for a thread whose last stored row is now `tail_ts`:
    /// (sent timestamp, text). Rows that don't show (reactions, edits) move that row too, so this
    /// looks only at the rows since the last time. Without any it searches the whole thread, once.
    pub(super) async fn refresh_snippet(&mut self, key: &str, thread: &Thread, tail_ts: u64) -> (u64, String) {
        let cached = self.persist.snippets.get(key).filter(|(_, _, s)| !s.contains('\u{FFFC}')).cloned();
        let after = match &cached {
            Some((last, _, _)) => self.index.seq_of(thread, *last).await.ok().flatten(),
            None => None,
        };
        let me = self.me;
        let found = newest_shown(&self.index, &self.store, thread, me, after).await;
        match found {
            Err(_) => return cached.map_or((tail_ts, String::new()), |(_, ts, s)| (ts, s)),
            Ok(Some((ts, c))) => {
                let text = self.edited_snippet(key, thread, &c).await.unwrap_or_default();
                return (ts, text);
            }
            Ok(None) => {}
        }
        let (Some(_), Some((_, ts, text))) = (after, cached) else { return (0, String::new()) };
        if ts == 0 {
            return (0, text);
        }
        // the same message, which the new rows may have edited
        match self.store.message(thread, ts).await {
            Err(_) => return (ts, text),
            Ok(Some(c)) => {
                if let Some(text) = self.edited_snippet(key, thread, &c).await {
                    return (ts, text);
                }
            }
            Ok(None) => {}
        }
        // it's gone
        match newest_shown(&self.index, &self.store, thread, me, None).await {
            Ok(Some((ts, c))) => {
                let text = self.edited_snippet(key, thread, &c).await.unwrap_or_default();
                (ts, text)
            }
            _ => (0, String::new()),
        }
    }

    /// The message deleted for everyone may be the one the thread list shows.
    pub(super) fn forget_snippet(&mut self, key: &str, target: u64) {
        if self.persist.snippets.get(key).is_some_and(|(_, ts, _)| *ts == target) {
            self.persist.snippets.remove(key);
            self.persist_dirty = true;
        }
    }

    pub(super) async fn emit_threads(&mut self) {
        (self.emit)(Event::Threads(self.threads.clone()));
        self.emit_results().await;
    }

    pub(super) fn mark_read(&mut self, key: &str) {
        self.notes.remove(notify::By::Thread(key));
        let unread = self.persist.unread.remove(key);
        if unread.is_some() {
            self.persist_dirty = true;
            self.threads_dirty = true;
        }
        if self.reads() {
            self.queue_reads(key, unread.unwrap_or(0));
        }
    }

    /// Queues read receipts for incoming messages in the open window newer than the thread's
    /// watermark. Without a watermark (first time) only the last `unread` ones count.
    pub(super) fn queue_reads(&mut self, key: &str, unread: u32) {
        self.queue_reads_to(key, unread, usize::MAX);
    }

    /// `queue_reads` for the window rows before index `end` only; returns how many it queued.
    fn queue_reads_to(&mut self, key: &str, unread: u32, end: usize) -> u32 {
        let Some(open) = self.current.as_ref().filter(|o| o.key == key) else { return 0 };
        let me = self.me;
        let incoming: Vec<(usize, ServiceId, u64)> = open
            .window
            .iter()
            .enumerate()
            .filter(|(_, (_, c))| is_incoming(c, me))
            .map(|(i, (_, c))| (i, c.metadata.sender, c.timestamp()))
            .collect();
        let Some(newest) = incoming.iter().filter(|m| m.0 < end).map(|m| m.2).max() else { return 0 };
        let upto = self.persist.read_upto.get(key).copied();
        if upto.is_some_and(|u| u >= newest) {
            return 0;
        }
        let positions: Vec<(usize, u64)> = incoming.iter().map(|m| (m.0, m.2)).collect();
        let picked = newly_read(&positions, end, upto, unread);
        self.pending_reads.extend(picked.iter().map(|&i| (incoming[i].1, incoming[i].2)));
        self.persist.read_upto.insert(key.to_string(), newest);
        self.persist_dirty = true;
        picked.len() as u32
    }

    /// The user has seen the open thread's rows up to window row `row`: reads what is unread
    /// there (receipts, read sync, the counts) and no more. Seeing the window's last row, when
    /// it is the thread's newest, reads it all.
    pub(super) fn read_through(&mut self, row: u64) {
        if !self.reads() {
            return;
        }
        let Some(open) = self.current.as_mut() else { return };
        let Some(pos) = open.window.iter().position(|(t, _)| *t == row) else { return };
        if open.divider.is_some_and(|d| open.window[..=pos].iter().any(|(t, _)| *t == d)) {
            open.divider_reached = true;
        }
        let key = open.key.clone();
        if open.end_seq.is_none() && pos + 1 == open.window.len() {
            self.mark_read(&key);
            return;
        }
        let unread = self.persist.unread.get(&key).copied().unwrap_or(0);
        let n = self.queue_reads_to(&key, unread, pos + 1);
        if n > 0 {
            match unread.saturating_sub(n) {
                0 => {
                    self.persist.unread.remove(&key);
                }
                left => {
                    self.persist.unread.insert(key, left);
                }
            }
            self.persist_dirty = true;
            self.threads_dirty = true;
        }
    }
}

/// The newest row after display position `after` (None: anywhere) that makes a snippet.
pub(super) async fn newest_shown(
    index: &Index,
    store: &SqliteStore,
    thread: &Thread,
    me: Uuid,
    after: Option<i64>,
) -> anyhow::Result<Option<(u64, Content)>> {
    let mut before = i64::MAX;
    loop {
        let page = index.page(thread, before, PAGE).await?;
        let Some(&(oldest, _)) = page.last() else { return Ok(None) };
        let wanted: Vec<(i64, u64)> = page.iter().copied().filter(|r| after.is_none_or(|a| r.0 > a)).collect();
        let mut rows = fetch_rows(index, store, thread, &wanted).await?;
        if let Some(i) = rows.iter().rposition(|(_, c)| view::text::snippet(c, me, &|_| String::new()).is_some()) {
            return Ok(Some(rows.swap_remove(i)));
        }
        if (page.len() as u32) < PAGE || after.is_some_and(|a| oldest <= a) {
            return Ok(None);
        }
        before = oldest;
    }
}

/// Profile display name like the official apps: CJK names are family first with no space
/// ("홍길동"), everything else "given family".
pub(super) fn combine_names(given: &str, family: &str) -> String {
    let (given, family) = (crate::link::one_line(given), crate::link::one_line(family));
    let (given, family) = (given.as_str(), family.as_str());
    if given.is_empty() || family.is_empty() {
        return format!("{given}{family}");
    }
    if is_cjk(given) && is_cjk(family) {
        format!("{family}{given}")
    } else {
        format!("{given} {family}")
    }
}

pub(super) fn is_cjk(s: &str) -> bool {
    s.chars().all(|c| {
        matches!(c as u32,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF // Hangul
            | 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F // kana
            | 0x3005 | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x3FFFF) // Han
    })
}

#[cfg(test)]
mod tests {
    /// Far past anything a page or a window would look at.
    #[tokio::test(flavor = "current_thread")]
    async fn the_newest_message_is_found_behind_any_number_of_receipts() {
        use crate::db::tests::{thread, Db};
        let db = Db::new().await;
        let index = db.index().await;
        let me = Uuid::from_u128(1);
        let search = |after| {
            let (index, store) = (index.clone(), db.store.clone());
            async move { newest_shown(&index, &store, &thread(), me, after).await.unwrap().map(|r| r.0) }
        };
        assert_eq!(search(None).await, None);
        db.save(100, "first").await;
        db.save(200, "last").await;
        for ts in 1000..1700 {
            db.save_receipt(ts).await;
        }
        assert_eq!(search(None).await, Some(200));
        // only what came after the given row counts
        let seen = index.seq_of(&thread(), 1699).await.unwrap();
        assert_eq!(search(seen).await, None);
        let partway = index.seq_of(&thread(), 150 + 1000).await.unwrap();
        assert_eq!(search(partway).await, None);
        db.save(2000, "new").await;
        db.save_receipt(2001).await;
        assert_eq!(search(seen).await, Some(2000));
    }

    use super::*;

    #[test]
    fn combines_profile_names() {
        assert_eq!(combine_names("길동", "홍"), "홍길동");
        assert_eq!(combine_names("太郎", "山田"), "山田太郎");
        assert_eq!(combine_names("John", "Smith"), "John Smith");
        assert_eq!(combine_names("길동", "Hong"), "길동 Hong");
        assert_eq!(combine_names(" 홍길동 ", ""), "홍길동");
        assert_eq!(combine_names("", "홍"), "홍");
        // synced names come wrapped in direction isolates
        assert_eq!(combine_names("\u{2068}길동\u{2069}", "\u{2068}홍\u{2069}"), "홍길동");
        assert_eq!(group_name(" \u{2068}\u{2069} "), "그룹");
    }
}
