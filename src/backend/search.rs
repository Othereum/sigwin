//! The sidebar's search: threads and contacts by name or number, and the contact list the
//! ✎ button opens to start a conversation.

use presage::libsignal_service::prelude::phonenumber::{self, country, Mode, PhoneNumber};
use presage::model::contacts::Contact;

use super::threads::{show_number, unknown_name, NOTE_TO_SELF};
use super::*;
use crate::types::{Find, ResultDto};

/// What the sidebar searches: everything from the thread list, or only people (the ✎ list).
#[derive(Default)]
pub(super) struct Search {
    pub query: String,
    pub contacts: bool,
    pub find: Find,
}

impl Search {
    fn active(&self) -> bool {
        self.contacts || !self.query.trim().is_empty()
    }
}

/// Whether `name` or `phone` (E.164, as stored) matches the query. A number matches however it
/// is typed: "010-1234" finds "+821012…", the leading 0 standing for the country code.
fn matches(q: &Query, name: &str, phone: &str) -> bool {
    if q.text.is_empty() || name.to_lowercase().contains(&q.text) {
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
            Self::Phone(p) => (show_number(p), "번호로 대화 시작", p.format().mode(Mode::E164).to_string(), true),
            Self::Username(u) => (u.clone(), "사용자 이름으로 대화 시작", u.clone(), false),
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
                label: if phone { "번호로 대화 시작" } else { "사용자 이름으로 대화 시작" }.into(),
                sub: if phone { "끝까지 입력하면 찾을 수 있습니다" } else { "name.42처럼 입력할 수 있습니다" }.into(),
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
    PniOnly,
    Nobody { phone: bool },
    Failed(String),
}

/// Hangul names first, then the rest, each in character order: how a Korean contact list reads.
fn sort_key(name: &str) -> (bool, String) {
    let hangul = name.chars().next().is_some_and(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c) || ('\u{3131}'..='\u{318E}').contains(&c));
    (!hangul, name.to_lowercase())
}

impl App {
    pub(super) async fn set_search(&mut self, query: String, contacts: bool, find: Find) {
        self.search = Search { query, contacts, find };
        self.emit_results().await;
    }

    /// Sends the rows for the current search, if one is on. Called again when the thread list
    /// changes, so they follow it.
    pub(super) async fn emit_results(&mut self) {
        if !self.search.active() {
            return;
        }
        let q = Query::new(&self.search.query);
        let mut rows = Vec::new();
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
                rows.push(ResultDto::Header("대화".into()));
                rows.extend(threads.into_iter().map(ResultDto::Thread));
            }
        }
        let people = self.people(&q).await;
        if !people.is_empty() {
            // in the contact list, apart from the rows above them
            if !self.search.contacts || !rows.is_empty() {
                rows.push(ResultDto::Header("연락처".into()));
            }
            rows.extend(people.into_iter().map(ResultDto::Contact));
        }
        (self.emit)(Event::Results(rows));
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
        let name = if mine { NOTE_TO_SELF.into() } else { self.name(uuid).await };
        Some(person_row(self, key, name, show_number(p)))
    }

    /// Looks up whom `target` names (`Start::row`), once, in the background.
    pub(super) fn start_chat(&mut self, target: String) {
        let Some(m) = self.manager.clone() else {
            self.notice("연결된 뒤에 찾을 수 있습니다");
            return;
        };
        self.progress("찾는 중…");
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let mut m = m;
            let found = if target.starts_with('+') {
                match phonenumber::parse(None, &target) {
                    Err(_) => Found::Nobody { phone: true },
                    Ok(p) => match Box::pin(m.discover_contacts_by_phone_number([p.clone()])).await {
                        Ok(v) => match v.into_iter().find_map(|(_, sid)| sid) {
                            Some(ServiceId::Aci(a)) => Found::Aci { uuid: a.into(), phone: Some(p), username: None },
                            Some(ServiceId::Pni(_)) => Found::PniOnly,
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
            Found::PniOnly => {
                info!("lookup: only a PNI");
                return self.notice("이 번호로는 아직 대화를 시작할 수 없습니다. 휴대폰에서 먼저 대화를 시작하면 여기서도 이어갈 수 있습니다");
            }
            Found::Nobody { phone: true } => return self.notice("이 번호로 찾을 수 있는 Signal 사용자가 없습니다"),
            Found::Nobody { phone: false } => return self.notice("이 사용자 이름을 찾을 수 없습니다"),
            Found::Failed(e) => {
                warn!("lookup: {e}");
                return self.notice("찾지 못했습니다. 잠시 후 다시 시도할 수 있습니다");
            }
        };
        if uuid != self.me {
            if let Err(e) = self.file_found(uuid, phone, username).await {
                warn!("file a found contact: {e:#}");
            }
        }
        let key = format!("c:{uuid}");
        if let Err(e) = self.open_thread(&key).await {
            self.notice(format!("대화를 열지 못했습니다: {e:#}"));
        }
    }

    async fn file_found(&mut self, uuid: Uuid, phone: Option<PhoneNumber>, username: Option<String>) -> anyhow::Result<()> {
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
        if all && matches(q, NOTE_TO_SELF, "") {
            found.insert(0, (me, NOTE_TO_SELF.into(), String::new()));
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
    fn hangul_names_come_first() {
        let mut names = vec!["Alice", "나영", "bob", "가람"];
        names.sort_by_cached_key(|n| sort_key(n));
        assert_eq!(names, ["가람", "나영", "Alice", "bob"]);
    }
}
