//! Thread list: names, snippets, unread counts, read receipts, avatars.

use super::*;

impl App {
    pub(super) async fn name(&mut self, uuid: Uuid) -> String {
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
                .map(|p| p.to_string())
                .unwrap_or_else(|| format!("알 수 없음 ({})", &uuid.to_string()[..8]));
        }
        self.names.insert(uuid, name.clone());
        name
    }

    pub(super) async fn thread_name(&mut self, thread: &Thread) -> String {
        match thread {
            Thread::Contact(id) if id.raw_uuid() == self.me => "나에게 쓰는 메모".into(),
            Thread::Contact(id) => self.name(id.raw_uuid()).await,
            Thread::Group(key) => match self.store.group(*key).await {
                Ok(Some(g)) if !g.title.is_empty() => g.title,
                _ => "그룹".into(),
            },
        }
    }

    pub(super) async fn rebuild_threads(&mut self) -> anyhow::Result<()> {
        let rows = self.index.threads().await?;
        // empty snippets cached while only the last 20 rows were looked at
        if !self.persist.snippets_deep {
            self.persist.snippets.retain(|_, (_, _, s)| !s.is_empty());
            self.persist.snippets_deep = true;
            self.persist_dirty = true;
        }
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
                    mute: self.mute(&key),
                    prefs: self.prefs(&key),
                    phone_mentions: self.phone_mentions(&key),
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
                        mute: self.mute(&key),
                        prefs: self.prefs(&key),
                        phone_mentions: self.phone_mentions(&key),
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
        self.mute_due = self.next_mute_change();
        self.emit_threads().await;
        self.load_avatars();
        Ok(())
    }

    /// Queues the photos of listed threads not seen yet this session.
    pub(super) fn load_avatars(&mut self) {
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
    pub(super) async fn snippet(&mut self, c: &Content) -> Option<String> {
        for u in view::data_message(c).map(view::mentioned).unwrap_or_default() {
            self.name(u).await;
        }
        let names = &self.names;
        view::snippet(c, self.me, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into()))
    }

    /// Newest message that makes a snippet. 1:1 threads collect receipts (also for our group
    /// messages), so look past them page by page, as far back as opening the thread would.
    pub(super) async fn compute_snippet(&mut self, thread: &Thread, last_ts: u64) -> (u64, String) {
        let mut before = last_ts.saturating_add(1);
        let mut scanned = 0usize;
        while scanned < MAX_WINDOW {
            let Ok(tss) = self.index.page(thread, before, PAGE).await else {
                return (last_ts, String::new());
            };
            let Some(&oldest) = tss.last() else { break };
            let Ok(rows) = self.fetch_range(thread, &tss).await else {
                return (last_ts, String::new());
            };
            for (ts, c) in rows.iter().rev() {
                if let Some(s) = self.snippet(c).await {
                    return (*ts, s);
                }
            }
            if (tss.len() as u32) < PAGE {
                break;
            }
            scanned += tss.len();
            before = oldest;
        }
        (0, String::new())
    }

    pub(super) async fn emit_threads(&mut self) {
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
                        mute: self.mute(&key),
                        prefs: self.prefs(&key),
                        phone_mentions: None,
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

    pub(super) fn mark_read(&mut self, key: &str) {
        self.notes.remove(notify::By::Thread(key));
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
    pub(super) fn queue_reads(&mut self, key: &str, unread: u32) {
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
}

/// Profile display name like the official apps: CJK names are family first with no space
/// ("홍길동"), everything else "given family".
pub(super) fn combine_names(given: &str, family: &str) -> String {
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
}
