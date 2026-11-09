//! The sidebar's search: threads and contacts by name or number, and the contact list the
//! ✎ button opens to start a conversation.

use super::threads::{show_number, unknown_name, NOTE_TO_SELF};
use super::*;
use crate::types::ResultDto;

/// What the sidebar searches: everything from the thread list, or only people (the ✎ list).
#[derive(Default)]
pub(super) struct Search {
    pub query: String,
    pub contacts: bool,
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

/// Hangul names first, then the rest, each in character order: how a Korean contact list reads.
fn sort_key(name: &str) -> (bool, String) {
    let hangul = name.chars().next().is_some_and(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c) || ('\u{3131}'..='\u{318E}').contains(&c));
    (!hangul, name.to_lowercase())
}

impl App {
    pub(super) async fn set_search(&mut self, query: String, contacts: bool) {
        self.search = Search { query, contacts };
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
        if !self.search.contacts {
            let threads: Vec<ThreadDto> = self.threads.iter().filter(|t| matches(&q, &t.name, "")).cloned().collect();
            if !threads.is_empty() {
                rows.push(ResultDto::Header("대화".into()));
                rows.extend(threads.into_iter().map(ResultDto::Thread));
            }
        }
        let people = self.people(&q).await;
        if !people.is_empty() {
            if !self.search.contacts {
                rows.push(ResultDto::Header("연락처".into()));
            }
            rows.extend(people.into_iter().map(ResultDto::Contact));
        }
        (self.emit)(Event::Results(rows));
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
        found
            .into_iter()
            .map(|(key, name, phone)| ThreadDto {
                mute: self.mute(&key),
                prefs: self.prefs(&key),
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
            })
            .collect()
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
    fn hangul_names_come_first() {
        let mut names = vec!["Alice", "나영", "bob", "가람"];
        names.sort_by_cached_key(|n| sort_key(n));
        assert_eq!(names, ["가람", "나영", "Alice", "bob"]);
    }
}
