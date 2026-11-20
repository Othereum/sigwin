//! The sidebar's search: threads and contacts by name or number, and the contact list the
//! ✎ button opens to start a conversation.

use presage::libsignal_service::prelude::phonenumber::{self, country, Mode, PhoneNumber};
use presage::model::contacts::Contact;

use std::cell::Cell;

use super::threads::{note_to_self, show_number, unknown_name};
use super::*;
use crate::types::{Find, ResultDto, SearchFilter};

/// What the sidebar searches: everything from the thread list, or only people (the ✎ list).
#[derive(Default)]
pub(super) struct Search {
    pub query: String,
    pub contacts: bool,
    pub find: Find,
    /// the thread whose messages alone are searched ("search in this chat")
    pub scope: Option<String>,
    pub filter: SearchFilter,
    /// the list is who sent messages (in the scope), to pick the sender filter from; `query`
    /// is their name, the search's own words wait in the UI
    pub pick_sender: bool,
    /// who sent messages in the scope (key, name), while picking; read once per picking
    senders: Option<Vec<(String, String)>>,
    pub hits: Hits,
    /// how far the one-time text backfill is (%), while it runs
    pub indexing: Option<u8>,
}

impl Search {
    fn active(&self) -> bool {
        self.contacts || self.pick_sender || self.scope.is_some() || !self.filter.is_empty() || !self.query.trim().is_empty()
    }
}

/// Whether messages are looked for: words to look for, or no words and a filter. A lone
/// consonant or vowel would match nearly every message, so it waits for more.
fn looks(q: &str, filter: &SearchFilter) -> bool {
    if q.trim().is_empty() { !filter.is_empty() } else { Terms::new(q).needle().is_some() }
}

/// The words of a message search: a message must hold all of them, in any order. A quoted
/// part is one, spaces and all; an unclosed quote runs to the end.
pub(crate) struct Terms {
    words: Vec<String>,
    /// the last word may still be in composition (`hangul::find_typed`): not a closed quote
    typing: bool,
}

impl Terms {
    /// `q` is lowercase.
    pub(crate) fn new(q: &str) -> Self {
        let (mut words, mut typing) = (Vec::new(), false);
        let mut rest = q;
        loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                break;
            }
            let word = if let Some(r) = rest.strip_prefix('"') {
                match r.find('"') {
                    Some(end) => {
                        rest = &r[end + 1..];
                        typing = false;
                        &r[..end]
                    }
                    None => {
                        rest = "";
                        typing = true;
                        r
                    }
                }
            } else {
                let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
                let w = &rest[..end];
                rest = &rest[end..];
                typing = true;
                w
            };
            if !word.trim().is_empty() {
                words.push(word.to_string());
            }
        }
        Self { words, typing }
    }

    fn typed(&self, i: usize) -> bool {
        self.typing && i + 1 == self.words.len()
    }

    /// What the index is scanned for (`instr`): the longest word, of the last only what's
    /// surely typed (`needle`). A last word of one syllable, which may still grow a final
    /// consonant, only when it's alone. None: nothing to look for.
    fn needle(&self) -> Option<String> {
        let alone = self.words.len() == 1;
        self.words
            .iter()
            .enumerate()
            .filter_map(|(i, w)| match self.typed(i) {
                true if !alone && w.chars().count() == 1 => None,
                true => needle(w),
                false => Some(w.clone()),
            })
            .max_by_key(|w| w.chars().count())
    }

    /// Whether `text` (lowercase) holds every word.
    pub(crate) fn keep(&self, text: &str) -> bool {
        self.words.iter().enumerate().all(|(i, w)| if self.typed(i) { crate::hangul::find_typed(text, w).is_some() } else { text.contains(w.as_str()) })
    }

    /// Where in `text` (lowercase) the first word found starts, in chars.
    fn first_at(&self, text: &str) -> Option<usize> {
        self.words
            .iter()
            .enumerate()
            .filter_map(|(i, w)| if self.typed(i) { crate::hangul::find_typed(text, w) } else { text.find(w.as_str()).map(|b| text[..b].chars().count()) })
            .min()
    }
}

/// The messages found for the search: all of them as ids, the rows for those shown so far.
/// The rows of the last search stay until the next one's arrive, so typing doesn't blink.
#[derive(Default)]
pub(super) struct Hits {
    /// what was searched for: the typed text, lowercase, the scope and the filter
    asked: Option<(String, Option<String>, SearchFilter)>,
    /// bumped by every new search: a scan still running for an older one stops
    turn: Rc<Cell<u64>>,
    all: Vec<(Thread, u64)>,
    rows: Vec<ResultDto>,
    /// how many of `all` `rows` went through
    shown: usize,
}

/// Messages found at most: a page of rows at a time is built from them.
const MAX_HITS: usize = 2000;
const HITS_PAGE: usize = 30;
/// a search waits this long for the next keystroke before scanning
const TYPING_PAUSE: Duration = Duration::from_millis(120);

/// What the index is scanned for (`instr`), the rest being checked by `hangul::find_typed`:
/// a last Hangul character may still be in composition, so all but it. None: nothing to look
/// for, or a lone consonant or vowel, which would match nearly every message.
fn needle(q: &str) -> Option<String> {
    let (last, head) = {
        let mut cs = q.chars();
        let last = cs.next_back()?;
        (last, cs.as_str())
    };
    let hangul = ('\u{AC00}'..='\u{D7A3}').contains(&last) || ('\u{3131}'..='\u{318E}').contains(&last);
    if !hangul {
        return Some(q.to_string());
    }
    if !head.is_empty() {
        return Some(head.to_string());
    }
    // a syllable alone: the messages that hold it (one still growing a final consonant is missed)
    (!('\u{3131}'..='\u{318E}').contains(&last)).then(|| q.to_string())
}

/// The part of a one-line `text` to show for a match of `q`: from a little before it, so that
/// it shows in a row that cuts the end off.
pub(crate) fn excerpt(text: &str, q: &str) -> String {
    const BEFORE: usize = 10;
    let chars: Vec<char> = text.chars().collect();
    let lower: String = text.to_lowercase();
    // lowercasing can change the count of a few chars: then it's only near the match
    let at = Terms::new(q).first_at(&lower).unwrap_or(0).min(chars.len());
    if at <= BEFORE + 4 {
        return text.to_string();
    }
    let from = at - BEFORE;
    format!("…{}", chars[from..].iter().collect::<String>())
}

/// Whether `name` or `phone` (E.164, as stored) matches the query. A number matches however it
/// is typed: "010-1234" finds "+821012…", the leading 0 standing for the country code. A name
/// matches as Korean is typed (`hangul::find`).
fn matches(q: &Query, name: &str, phone: &str) -> bool {
    if q.text.is_empty() || crate::hangul::find(name, &q.text).is_some() {
        return true;
    }
    if q.digits.len() < 3 || phone.is_empty() {
        return false;
    }
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    digits.contains(&q.digits) || q.digits.strip_prefix('0').is_some_and(|d| d.len() >= 2 && digits.contains(d))
}

struct Query {
    text: String,
    /// the digits, when the query is a number ("010-1234 5678", "+82 10 …")
    digits: String,
}

impl Query {
    fn new(q: &str) -> Self {
        let text = q.trim().to_lowercase();
        let number = !text.is_empty() && text.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c));
        let digits = if number { text.chars().filter(char::is_ascii_digit).collect() } else { String::new() };
        Self { text, digits }
    }
}

/// Someone the search box could name, to start a conversation with.
#[derive(Debug, PartialEq)]
pub(crate) enum Start {
    Phone(PhoneNumber),
    /// a Signal username, "name.42"
    Username(String),
}

impl Start {
    /// What `q` names, if it is a whole number (in `country` when it has no "+") or a username.
    pub(crate) fn of(q: &str, country: Option<country::Id>) -> Option<Self> {
        let q = q.trim();
        let digits = q.chars().filter(char::is_ascii_digit).count();
        if digits >= 7 && q.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c)) {
            let p = phonenumber::parse(country, q).ok()?;
            return phonenumber::is_valid(&p).then_some(Self::Phone(p));
        }
        let name = q.strip_prefix('@').unwrap_or(q);
        let (nick, number) = name.rsplit_once('.')?;
        let nick_ok = (3..=32).contains(&nick.len())
            && nick.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !nick.starts_with(|c: char| c.is_ascii_digit());
        let number_ok = (2..=9).contains(&number.len()) && number.chars().all(|c| c.is_ascii_digit());
        (nick_ok && number_ok).then(|| Self::Username(name.to_string()))
    }

    /// The row that starts it: what it shows, and what `Command::StartChat` gets back (an
    /// E.164 number, which starts with "+", or the username).
    pub(crate) fn row(&self) -> ResultDto {
        let (label, sub, target, phone) = match self {
            Self::Phone(p) => (show_number(p), tr!("Start a chat with this number"), p.format().mode(Mode::E164).to_string(), true),
            Self::Username(u) => (u.clone(), tr!("Start a chat with this username"), u.clone(), false),
        };
        ResultDto::Start { label, sub: sub.into(), target, ready: true, phone }
    }
}

/// What leads the results, before the threads and contacts.
#[derive(Debug, PartialEq)]
pub(crate) enum Lead {
    None,
    /// the contact list's "찾기" rows, while nothing is typed
    Finds,
    Start(Start),
    /// on its way to a number or username, not whole yet
    Partial { phone: bool },
}

impl Lead {
    /// Started conversations are found only once what's typed is whole; in the contact list,
    /// what is on its way there already shows, dim, so that it's clear it can be done.
    pub(crate) fn of(query: &str, find: Find, contacts: bool, country: Option<country::Id>) -> Self {
        let q = query.trim();
        if q.is_empty() {
            return if contacts && find == Find::Any { Self::Finds } else { Self::None };
        }
        match (Start::of(q, country), find) {
            (Some(s @ Start::Phone(_)), Find::Any | Find::Phone) | (Some(s @ Start::Username(_)), Find::Any | Find::Username) => {
                return Self::Start(s);
            }
            _ => {}
        }
        let numberish = Query::new(q).digits.len() >= 3;
        match find {
            Find::Phone => Self::Partial { phone: true },
            Find::Username => Self::Partial { phone: false },
            Find::Any if contacts && numberish => Self::Partial { phone: true },
            Find::Any => Self::None,
        }
    }

    /// Its rows, a number taken as not known yet (the backend first looks for it among contacts).
    pub(crate) fn rows(self) -> Vec<ResultDto> {
        match self {
            Self::None => Vec::new(),
            Self::Finds => vec![ResultDto::Find(Find::Phone), ResultDto::Find(Find::Username)],
            Self::Start(s) => vec![s.row()],
            Self::Partial { phone } => vec![ResultDto::Start {
                label: if phone { tr!("Start a chat with this number") } else { tr!("Start a chat with this username") }.into(),
                sub: if phone { tr!("You can search once the number is complete") } else { tr!("You can enter it like name.42") }.into(),
                target: String::new(),
                ready: false,
                phone,
            }],
        }
    }
}

/// What looking someone up came to.
pub(super) enum Found {
    Aci { uuid: Uuid, phone: Option<PhoneNumber>, username: Option<String> },
    /// the number is on Signal, but only its PNI is given: we've never talked
    PniOnly { pni: Uuid, phone: PhoneNumber },
    Nobody { phone: bool },
    Failed(String),
}

/// Hangul names first, then the rest, each in character order: how a Korean contact list reads.
fn sort_key(name: &str) -> (bool, String) {
    let hangul = name.chars().next().is_some_and(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c) || ('\u{3131}'..='\u{318E}').contains(&c));
    (!hangul, name.to_lowercase())
}

impl App {
    pub(super) async fn set_search(&mut self, query: String, contacts: bool, find: Find, scope: Option<String>, filter: SearchFilter, pick_sender: bool) {
        let s = &mut self.search;
        if !pick_sender || s.scope != scope {
            s.senders = None;
        }
        (s.query, s.contacts, s.find, s.scope, s.filter, s.pick_sender) = (query, contacts, find, scope, filter, pick_sender);
        // while picking, the query is a name: the messages found stay for when it's done
        if !pick_sender {
            self.search_messages();
        }
        self.emit_results().await;
    }

    /// Starts looking for the messages the search names, if that changed, once typing pauses.
    fn search_messages(&mut self) {
        let s = &mut self.search;
        let q = s.query.trim().to_lowercase();
        let asked = (!s.contacts).then(|| (q.clone(), s.scope.clone(), s.filter.clone()));
        if asked == s.hits.asked {
            return;
        }
        s.hits.asked = asked;
        let turn = s.hits.turn.get() + 1;
        s.hits.turn.set(turn);
        // while older texts are still being indexed, nothing: part of the results could pass
        // for all of them. The search runs once they're in (`texts_indexed`)
        if s.contacts || s.indexing.is_some() || !looks(&q, &s.filter) {
            (s.hits.all, s.hits.rows, s.hits.shown) = (Vec::new(), Vec::new(), 0);
            return;
        }
        let scope = s.scope.as_deref().and_then(parse_thread_key);
        let filter = s.filter.clone();
        let (index, tx, current) = (self.index.clone(), self.tx.clone(), s.hits.turn.clone());
        tokio::task::spawn_local(async move {
            tokio::time::sleep(TYPING_PAUSE).await;
            if current.get() != turn {
                return;
            }
            let started = Instant::now();
            let terms = Terms::new(&q);
            let needle = terms.needle();
            let keep = |text: &str| terms.keep(text);
            let gone = || current.get() != turn;
            let sender = filter.sender.as_deref().and_then(parse_thread_key).and_then(|t| match t {
                Thread::Contact(id @ ServiceId::Aci(_)) => Some(id.service_id_string()),
                _ => None,
            });
            let searched = crate::db::Searched {
                thread: scope.as_ref(),
                needle: needle.as_deref(),
                keep: &keep,
                sender: sender.as_deref(),
                until: filter.until,
                kind: filter.kind,
            };
            match index.search(&searched, MAX_HITS, &gone).await {
                Ok(list) if !gone() => {
                    debug!("message search: {} found in {:?}", list.len(), started.elapsed());
                    let _ = tx.send(Internal::Hits { turn, list });
                }
                Ok(_) => {}
                Err(e) => warn!("message search: {e:#}"),
            }
        });
    }

    /// The messages search `turn` found arrived.
    pub(super) async fn hits(&mut self, turn: u64, list: Vec<(Thread, u64)>) {
        let h = &mut self.search.hits;
        if h.turn.get() != turn {
            return;
        }
        (h.all, h.shown) = (list, 0);
        // built aside: the old rows stay until the new ones are all there
        let rows = self.hit_rows(HITS_PAGE).await;
        self.search.hits.rows = rows;
        self.emit_results().await;
    }

    /// Builds the rows of the next page of found messages, and sends the results.
    pub(super) async fn more_hits(&mut self) {
        let rows = self.hit_rows(HITS_PAGE).await;
        self.search.hits.rows.extend(rows);
        self.emit_results().await;
    }

    /// The rows of the next `n` found messages.
    async fn hit_rows(&mut self, n: usize) -> Vec<ResultDto> {
        let q = self.search.query.trim().to_lowercase();
        let next: Vec<(Thread, u64)> = self.search.hits.all.iter().skip(self.search.hits.shown).take(n).cloned().collect();
        self.search.hits.shown += next.len();
        let mut rows = Vec::with_capacity(next.len());
        for (thread, ts) in next {
            rows.extend(self.hit_row(&thread, ts, &q).await);
        }
        rows
    }

    /// A found message as a row: its chat, and its text from around the match.
    async fn hit_row(&mut self, thread: &Thread, ts: u64, q: &str) -> Option<ResultDto> {
        let key = thread_key(thread);
        // what the list leaves out (blocked people) or doesn't show yet (requests) isn't found
        if self.is_request(&key) {
            return None;
        }
        let chat = self.threads.iter().find(|t| t.key == key)?.clone();
        let c = self.store.message(thread, ts).await.ok().flatten()?;
        let Kind::Msg { dm, outgoing } = view::classify(&c, self.me) else { return None };
        let sender = c.metadata.sender.raw_uuid();
        let dm = self.latest_edit(&key, thread, ts, sender).await.unwrap_or_else(|| dm.clone());
        for u in view::text::mentioned(&dm) {
            self.name(u).await;
        }
        let names = &self.names;
        let text = excerpt(&view::text::one_line(&dm, &|u| names.get(u).cloned().unwrap_or_else(|| tr!("Unknown").into())), q);
        let snippet = if outgoing {
            tr!("You: {}", text).to_string()
        } else if chat.is_group {
            format!("{}: {text}", self.name(sender).await)
        } else {
            text
        };
        Some(ResultDto::Message {
            id: ts.to_string(),
            thread: ThreadDto {
                snippet,
                time: view::time::time_short(ts),
                ts,
                unread: 0,
                readers: Vec::new(),
                readers_more: 0,
                ..chat
            },
        })
    }

    /// Sends the rows for the current search, if one is on. Called again when the thread list
    /// changes, so they follow it.
    pub(super) async fn emit_results(&mut self) {
        if !self.search.active() {
            return;
        }
        if self.search.pick_sender {
            let rows = self.sender_rows().await;
            return (self.emit)(Event::Results(rows));
        }
        let more = self.search.hits.shown < self.search.hits.all.len();
        // older messages may be missing while their text is indexed: said first, before any
        // result could be taken for all there is
        let searching = self.search.hits.asked.as_ref().is_some_and(|(q, _, f)| looks(q, f));
        let note = self
            .search
            .indexing
            .filter(|_| searching)
            .map(|pct| ResultDto::Note(tr!("Preparing older messages for search ({}%)", pct).into()));
        let mut rows: Vec<ResultDto> = note.into_iter().collect();
        // a chat that left the list (deleted, blocked) takes its messages along
        let threads = &self.threads;
        let hits: Vec<ResultDto> = self
            .search
            .hits
            .rows
            .iter()
            .filter(|r| !matches!(r, ResultDto::Message { thread, .. } if !threads.iter().any(|t| t.key == thread.key)))
            .cloned()
            .collect();
        let found = self.search.hits.all.len();
        let count = if found >= MAX_HITS { format!("{MAX_HITS}+") } else { found.to_string() };
        let messages = |rows: &mut Vec<ResultDto>| {
            if !hits.is_empty() {
                rows.push(ResultDto::Header(tr!("Messages · {}", count).into()));
                rows.extend(hits);
                if more {
                    rows.push(ResultDto::More);
                }
            }
        };
        // filtered, or no words: only messages are looked for
        if self.search.scope.is_some() || !self.search.filter.is_empty() || self.search.query.trim().is_empty() && !self.search.contacts {
            messages(&mut rows);
            return (self.emit)(Event::Results(rows));
        }
        let q = Query::new(&self.search.query);
        // a whole number: someone already known by it, else a row to look it up
        let country = self.manager.as_ref().and_then(|m| m.registration_data().phone_number.country().id());
        match Lead::of(&self.search.query, self.search.find, self.search.contacts, country) {
            Lead::Start(Start::Phone(p)) => match self.contact_with_number(&p).await {
                Some(t) => rows.push(ResultDto::Contact(t)),
                None => rows.push(Start::Phone(p).row()),
            },
            lead => rows.extend(lead.rows()),
        }
        if !self.search.contacts {
            let threads: Vec<ThreadDto> = self.threads.iter().filter(|t| matches(&q, &t.name, "")).cloned().collect();
            if !threads.is_empty() {
                rows.push(ResultDto::Header(tr!("Chats").into()));
                rows.extend(threads.into_iter().map(ResultDto::Thread));
            }
        }
        let people = self.people(&q).await;
        if !people.is_empty() {
            // in the contact list, apart from the rows above them
            if !self.search.contacts || !rows.is_empty() {
                rows.push(ResultDto::Header(tr!("Contacts").into()));
            }
            rows.extend(people.into_iter().map(ResultDto::Contact));
        }
        if !self.search.contacts {
            messages(&mut rows);
        }
        (self.emit)(Event::Results(rows));
    }

    /// Who sent messages in the search's scope (everywhere, without one) and matches the
    /// name typed: us first, then the rest as a contact list reads.
    async fn sender_rows(&mut self) -> Vec<ResultDto> {
        if self.search.senders.is_none() {
            let scope = self.search.scope.as_deref().and_then(parse_thread_key);
            let stored = match self.index.senders(scope.as_ref()).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("senders: {e:#}");
                    Vec::new()
                }
            };
            let mut people = Vec::new();
            for s in stored {
                let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&s) else { continue };
                let uuid = Uuid::from(aci);
                if uuid == self.me || uuid.is_nil() {
                    continue;
                }
                let name = self.name(uuid).await;
                // nothing to call them by: not someone to pick from a list
                if name != unknown_name(uuid) {
                    people.push((format!("c:{uuid}"), name));
                }
            }
            people.sort_by_cached_key(|(_, name)| sort_key(name));
            people.insert(0, (format!("c:{}", self.me), tr!("You").into()));
            self.search.senders = Some(people);
        }
        let q = Query::new(&self.search.query);
        let list: Vec<(String, String)> =
            self.search.senders.iter().flatten().filter(|(_, name)| matches(&q, name, "")).cloned().collect();
        list.into_iter().map(|(key, name)| ResultDto::Contact(person_row(self, key, name, String::new()))).collect()
    }

    // Open results only ever change in place, so that no row moves under the pointer: a new
    // message isn't added, an edited one keeps its row even if it no longer matches, a
    // deleted one shows as deleted.

    /// Indexes the text of stored message (or edit) `c`; an edited message shown in the
    /// results shows its new text.
    pub(super) async fn note_text(&mut self, thread: &Thread, c: &Content) {
        if let Err(e) = record_text(&self.index, thread, c, self.me).await {
            warn!("record text: {e:#}");
        }
        let Kind::Edit { target, .. } = view::classify(c, self.me) else { return };
        let Some(i) = self.hit_at(thread, target) else { return };
        let q = self.search.query.trim().to_lowercase();
        // `hit_row` reads the newest edit by the author, so another's edit changes nothing
        if let Some(row) = self.hit_row(thread, target, &q).await {
            self.search.hits.rows[i] = row;
            self.emit_results().await;
        }
    }

    /// Message `ts` was deleted for everyone: out of the index, and shown as deleted in the
    /// results (where it can still be opened: the chat shows where it was).
    pub(super) async fn drop_text(&mut self, thread: &Thread, ts: u64) {
        if let Err(e) = self.index.forget_text(thread, ts).await {
            warn!("forget text: {e:#}");
        }
        let Some(i) = self.hit_at(thread, ts) else { return };
        if let ResultDto::Message { thread, .. } = &mut self.search.hits.rows[i] {
            thread.snippet = tr!("Deleted message").into();
        }
        self.emit_results().await;
    }

    /// Where message `ts` of `thread` is among the result rows.
    fn hit_at(&self, thread: &Thread, ts: u64) -> Option<usize> {
        if !self.search.active() {
            return None;
        }
        let (key, id) = (thread_key(thread), ts.to_string());
        self.search.hits.rows.iter().position(|r| matches!(r, ResultDto::Message { thread, id: i } if thread.key == key && *i == id))
    }

    /// The text backfill is `pct` % of the way.
    pub(super) async fn texts_indexing(&mut self, pct: u8) {
        if self.search.indexing.replace(pct) != Some(pct) && self.search.active() {
            self.emit_results().await;
        }
    }

    /// Every message's text is indexed: an open search, which showed none while they were
    /// on their way, looks now.
    pub(super) async fn texts_indexed(&mut self) {
        if self.search.indexing.take().is_none() {
            return;
        }
        self.search.hits.asked = None;
        self.search_messages();
        if self.search.active() {
            self.emit_results().await;
        }
    }

    /// The stored contact (or us) with number `p`, as a row.
    async fn contact_with_number(&mut self, p: &PhoneNumber) -> Option<ThreadDto> {
        let e164 = p.format().mode(Mode::E164).to_string();
        let mine = self.manager.as_ref().is_some_and(|m| m.registration_data().phone_number.format().mode(Mode::E164).to_string() == e164);
        let uuid = if mine {
            self.me
        } else {
            let contacts = self.store.contacts().await.ok()?;
            contacts.flatten().find(|c| c.phone_number.as_ref().is_some_and(|n| n.format().mode(Mode::E164).to_string() == e164))?.uuid
        };
        let key = format!("c:{uuid}");
        let name = if mine { note_to_self().into() } else { self.name(uuid).await };
        Some(person_row(self, key, name, show_number(p)))
    }

    /// Looks up whom `target` names (`Start::row`), once, in the background.
    pub(super) fn start_chat(&mut self, target: String) {
        let Some(m) = self.manager.clone() else {
            self.notice(tr!("You can search once connected"));
            return;
        };
        self.progress(tr!("Searching…"));
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let mut m = m;
            let found = if target.starts_with('+') {
                match phonenumber::parse(None, &target) {
                    Err(_) => Found::Nobody { phone: true },
                    Ok(p) => match Box::pin(m.discover_contacts_by_phone_number([p.clone()])).await {
                        Ok(v) => match v.into_iter().find_map(|(_, sid)| sid) {
                            Some(ServiceId::Aci(a)) => Found::Aci { uuid: a.into(), phone: Some(p), username: None },
                            Some(ServiceId::Pni(n)) => Found::PniOnly { pni: n.into(), phone: p },
                            None => Found::Nobody { phone: true },
                        },
                        Err(e) => Found::Failed(format!("{e}")),
                    },
                }
            } else {
                match Box::pin(m.lookup_username(&target)).await {
                    Ok(Some(a)) => Found::Aci { uuid: a.into(), phone: None, username: Some(target) },
                    Ok(None) => Found::Nobody { phone: false },
                    Err(e) => Found::Failed(format!("{e}")),
                }
            };
            let _ = tx.send(Internal::Found(found));
        });
    }

    /// Opens the conversation the lookup found, filing the number or username as their name
    /// until their profile tells us more.
    pub(super) async fn found(&mut self, found: Found) {
        (self.emit)(Event::ProgressDone);
        let (uuid, phone, username) = match found {
            Found::Aci { uuid, phone, username } => (uuid, phone, username),
            Found::PniOnly { pni, phone } => {
                info!("lookup: only a PNI");
                let e164 = phone.format().mode(Mode::E164).to_string();
                if let Err(e) = self.index.pni_put(pni, Some(&e164)).await {
                    return self.notice(tr!("Couldn't open the chat: {}", format!("{e:#}")));
                }
                self.names.remove(&pni);
                if let Err(e) = self.open_thread(&thread_key(&super::pni::pni_thread(pni))).await {
                    self.notice(tr!("Couldn't open the chat: {}", format!("{e:#}")));
                }
                return;
            }
            Found::Nobody { phone: true } => return self.notice(tr!("No Signal user can be found with this number")),
            Found::Nobody { phone: false } => return self.notice(tr!("This username couldn't be found")),
            Found::Failed(e) => {
                warn!("lookup: {e}");
                return self.notice(tr!("Couldn't search. You can try again in a moment"));
            }
        };
        if uuid != self.me {
            if let Err(e) = self.file_found(uuid, phone, username).await {
                warn!("file a found contact: {e:#}");
            }
        }
        let key = format!("c:{uuid}");
        if let Err(e) = self.open_thread(&key).await {
            self.notice(tr!("Couldn't open the chat: {}", format!("{e:#}")));
        }
    }

    pub(super) async fn file_found(&mut self, uuid: Uuid, phone: Option<PhoneNumber>, username: Option<String>) -> anyhow::Result<()> {
        let id = ServiceId::Aci(uuid.into());
        let mut c = match self.store.contact_by_id(&id).await? {
            Some(c) if c.phone_number.is_some() || phone.is_none() => return Ok(()),
            Some(c) => c,
            None => Contact {
                uuid,
                phone_number: None,
                name: username.unwrap_or_default(),
                verified: Default::default(),
                profile_key: Vec::new(),
                expire_timer: 0,
                expire_timer_version: 2,
                inbox_position: 0,
                avatar: None,
            },
        };
        c.phone_number = phone.or(c.phone_number);
        self.store.save_contact(&c).await?;
        self.names.remove(&uuid);
        Ok(())
    }

    /// People matching `q`, by name: from the thread list's search, those without a thread yet;
    /// in the contact list, everyone we know, us first.
    async fn people(&mut self, q: &Query) -> Vec<ThreadDto> {
        let all = self.search.contacts;
        let mut found: Vec<(String, String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let me = thread_key(&Thread::Contact(ServiceId::Aci(self.me.into())));
        seen.insert(me.clone());
        // whom the phone lists as contacts; before its storage was first read, everyone stored
        let listed = self.persist.listed.clone();
        let listed = |key: &str| listed.is_empty() || listed.contains(key);
        if let Ok(contacts) = self.store.contacts().await {
            for c in contacts.flatten() {
                let key = format!("c:{}", c.uuid);
                if !listed(&key) || !all && self.threads.iter().any(|t| t.key == key) || !seen.insert(key.clone()) {
                    continue;
                }
                let name = self.name(c.uuid).await;
                // nothing to call them by: not someone to pick from a list
                if name == unknown_name(c.uuid) {
                    continue;
                }
                let phone = c.phone_number.as_ref().map(|p| p.to_string()).unwrap_or_default();
                if matches(q, &name, &phone) {
                    found.push((key, name, c.phone_number.as_ref().map(show_number).unwrap_or_default()));
                }
            }
        }
        // people we talked with who aren't among the synced contacts
        if all {
            for t in self.threads.iter().filter(|t| !t.is_group && !seen.contains(&t.key)) {
                if matches(q, &t.name, "") {
                    found.push((t.key.clone(), t.name.clone(), String::new()));
                }
            }
        }
        found.sort_by_cached_key(|(_, name, _)| sort_key(name));
        if all && matches(q, note_to_self(), "") {
            found.insert(0, (me, note_to_self().into(), String::new()));
        }
        found.into_iter().map(|(key, name, phone)| person_row(self, key, name, phone)).collect()
    }
}

fn person_row(app: &App, key: String, name: String, phone: String) -> ThreadDto {
    ThreadDto {
        mute: app.mute(&key),
        prefs: app.prefs(&key),
        phone_mentions: None,
        key,
        name,
        snippet: phone,
        time: String::new(),
        ts: 0,
        unread: 0,
        is_group: false,
        readers: Vec::new(),
        readers_more: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_match_however_typed() {
        let phone = "+821012345678";
        for typed in ["010-1234-5678", "01012345678", "1234 5678", "+82 10-1234", "5678"] {
            assert!(matches(&Query::new(typed), "", phone), "{typed}");
        }
        for typed in ["010-9999", "12", "0"] {
            assert!(!matches(&Query::new(typed), "", phone), "{typed}");
        }
        assert!(matches(&Query::new("철수"), "김철수", ""));
        assert!(!matches(&Query::new("1234"), "김철수", ""), "a number doesn't match a name");
    }

    #[test]
    fn whole_numbers_and_usernames_start_a_chat() {
        let kr = Some(country::Id::KR);
        let phone = |q: &str| match Start::of(q, kr) {
            Some(Start::Phone(p)) => Some(p.format().mode(Mode::E164).to_string()),
            _ => None,
        };
        assert_eq!(phone("010-1234-5678").as_deref(), Some("+821012345678"));
        assert_eq!(phone("+82 10 1234 5678").as_deref(), Some("+821012345678"));
        assert_eq!(phone("+1 415-555-0123").as_deref(), Some("+14155550123"));
        // a part of a number is a search, not someone
        assert_eq!(Start::of("010-1234", kr), None);
        assert_eq!(Start::of("alice.42", kr), Some(Start::Username("alice.42".into())));
        assert_eq!(Start::of("@alice_b.07", kr), Some(Start::Username("alice_b.07".into())));
        for q in ["alice", "al.42", "1alice.42", "alice.4", "김철수.42", "alice.42x"] {
            assert_eq!(Start::of(q, kr), None, "{q}");
        }
    }

    #[test]
    fn the_contact_list_shows_how_to_start_before_it_is_whole() {
        let kr = Some(country::Id::KR);
        let lead = |q: &str, find, contacts| Lead::of(q, find, contacts, kr);
        assert_eq!(lead("", Find::Any, true), Lead::Finds);
        assert_eq!(lead("", Find::Any, false), Lead::None);
        assert_eq!(lead("", Find::Phone, true), Lead::None);
        assert_eq!(lead("010", Find::Any, true), Lead::Partial { phone: true });
        assert_eq!(lead("010", Find::Any, false), Lead::None, "the thread list's search waits for a whole one");
        assert_eq!(lead("alice", Find::Username, true), Lead::Partial { phone: false });
        assert_eq!(lead("alice", Find::Any, true), Lead::None, "a name is a search");
        assert!(matches!(lead("010-1234-5678", Find::Phone, true), Lead::Start(Start::Phone(_))));
        assert!(matches!(lead("alice.42", Find::Any, false), Lead::Start(Start::Username(_))));
        // looking for the other kind
        assert_eq!(lead("alice.42", Find::Phone, true), Lead::Partial { phone: true });
    }

    #[test]
    fn every_word_is_looked_for_in_any_order() {
        let keeps = |q: &str, text: &str| Terms::new(q).keep(text);
        assert!(keeps("회의 내일", "내일 회의 몇 시?"));
        assert!(!keeps("회의 모레", "내일 회의 몇 시?"));
        // quoted: as written
        assert!(keeps("\"내일 회의\"", "내일 회의 몇 시?"));
        assert!(!keeps("\"회의 내일\"", "내일 회의 몇 시?"));
        assert!(keeps("\"내일 회", "내일 회의"), "an unclosed quote is still being typed");
        // only the last word may still be in composition
        assert!(keeps("회의 냉", "내일 회의"));
        assert!(!keeps("횧 내일", "내일 회의"));
        // scanned for: the longest, of the last only what's surely typed
        assert_eq!(Terms::new("a 회의록 냉").needle().as_deref(), Some("회의록"));
        assert_eq!(Terms::new("ㅎ").needle(), None);
        assert_eq!(Terms::new("\"\"").needle(), None);
        assert_eq!(excerpt("앞에 긴 글이 한참 이어지고 나서야 내일 회의 얘기가 나온다", "회의 내일"), "… 이어지고 나서야 내일 회의 얘기가 나온다");
    }

    #[test]
    fn hangul_names_come_first() {
        let mut names = vec!["Alice", "나영", "bob", "가람"];
        names.sort_by_cached_key(|n| sort_key(n));
        assert_eq!(names, ["가람", "나영", "Alice", "bob"]);
    }
}
