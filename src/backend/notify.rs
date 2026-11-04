//! Notifications the way Signal Desktop does them (`ts/services/notifications.preload.ts`,
//! `ts/messages/maybeNotify.preload.ts`):
//! - one slot: a new toast replaces the last one; queued work runs 1s after the last request
//!   (debounce), so bursts and read syncs collapse into one update
//! - nothing shows while the window is focused, and focusing it clears the toast
//! - reading the chat (here or on the phone), deleting the message for everyone, or taking
//!   back / changing the reaction removes it
//! - reactions to our own messages from others notify too
//! - Windows: disappearing messages never show their text

use crate::toast::{self, Picture, Toast};

use super::*;

/// lodash `debounce(..., 1000)` in Signal: one update this long after the last request
const DEBOUNCE: Duration = Duration::from_secs(1);
const NEW_MESSAGE: &str = "새 메시지";

/// Someone's reaction to one of our messages.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct ReactionRef {
    pub from: Uuid,
    pub from_name: String,
    pub emoji: String,
}

/// What a toast is about (Signal's `QueuedNotificationData`).
pub(crate) struct Queued {
    pub key: String,
    /// sent timestamp of the message (reactions: the reacted-to message)
    pub msg: u64,
    /// sender, or "{group}의 {sender} 님"
    pub sender_title: String,
    pub message: String,
    pub expiring: bool,
    pub reaction: Option<ReactionRef>,
    pub picture: Option<Picture>,
}

struct Shown {
    key: String,
    msg: u64,
    reaction: Option<ReactionRef>,
}

#[derive(Default)]
pub(crate) struct Notifier {
    queued: Option<Queued>,
    shown: Option<Shown>,
    clear_shown: bool,
    due: Option<Instant>,
}

/// What to take back (Signal's `removeBy`).
pub(crate) enum By<'a> {
    Thread(&'a str),
    Message(u64),
    /// `from`'s reaction to message `msg` (any emoji: a new one replaces it)
    Reaction { msg: u64, from: Uuid },
}

impl By<'_> {
    fn matches(&self, key: &str, msg: u64, reaction: Option<&ReactionRef>) -> bool {
        match self {
            By::Thread(k) => *k == key,
            By::Message(m) => *m == msg,
            By::Reaction { msg: m, from } => *m == msg && reaction.is_some_and(|r| r.from == *from),
        }
    }
}

impl Notifier {
    fn update(&mut self) {
        self.due = Some(Instant::now() + DEBOUNCE);
    }

    pub fn add(&mut self, q: Queued) {
        self.queued = Some(q);
        self.update();
    }

    pub fn remove(&mut self, by: By) {
        if self.queued.as_ref().is_some_and(|q| by.matches(&q.key, q.msg, q.reaction.as_ref())) {
            self.queued = None;
        }
        if self.shown.as_ref().is_some_and(|s| by.matches(&s.key, s.msg, s.reaction.as_ref())) {
            self.clear_shown = true;
            self.update();
        }
    }

    /// The window was focused (`activeWindowService.registerForActive`).
    pub fn clear(&mut self) {
        self.clear_shown = true;
        self.queued = None;
        self.update();
    }

    fn close_last_shown(&mut self) {
        self.clear_shown = false;
        if self.shown.take().is_some() {
            toast::clear();
        }
    }

    /// Runs the debounced update once it is due (`#fastUpdate`).
    pub fn tick(&mut self, focused: bool) {
        if !self.due.is_some_and(|d| d <= Instant::now()) {
            return;
        }
        self.due = None;
        if self.clear_shown {
            self.close_last_shown();
        }
        if focused {
            self.queued = None;
            return;
        }
        let Some(q) = self.queued.take() else { return };
        let Some(level) = crate::settings::preview() else { return };
        let (title, body) = content(&q, level);
        let picture = if level < 2 { q.picture } else { None };
        info!("notification: showing one");
        // showing replaces the previous toast
        self.clear_shown = false;
        self.shown = Some(Shown { key: q.key.clone(), msg: q.msg, reaction: q.reaction.clone() });
        toast::show(Toast { title, body, picture, key: q.key, msg: Some(q.msg) });
    }
}

/// (title, body) under the preview setting (`redactMessageNotificationContent`).
fn content(q: &Queued, level: u8) -> (String, String) {
    if level >= 2 {
        return ("Sigwin".into(), NEW_MESSAGE.into());
    }
    let body = if q.expiring || level == 1 {
        NEW_MESSAGE.into()
    } else if let Some(r) = &q.reaction {
        // Signal puts the sender title here; the plain name avoids "…의 철수 님 님이"
        format!("{} 님이 \"{}\" 메시지에 {} 이모지로 공감했어요", r.from_name, q.message, r.emoji)
    } else {
        q.message.clone()
    };
    (q.sender_title.clone(), body)
}

impl App {
    /// An incoming message (`maybeNotify` for a normal message).
    pub(super) async fn notify_message(&mut self, thread: &Thread, key: &str, c: &Content) {
        if !self.synced || !crate::settings::notify_enabled() {
            return;
        }
        let Some(dm) = view::data_message(c) else { return };
        if self.mute(key).active(view::now_ms()) && !mute::breaks_mute(dm, self.me) {
            return;
        }
        let expiring = dm.expire_timer.unwrap_or(0) > 0;
        let message = self.snippet(c).await.unwrap_or_default();
        let sender_title = self.sender_title(thread, c.metadata.sender.raw_uuid()).await;
        let picture = self.toast_picture(thread, key).await;
        self.notes.add(Queued { key: key.into(), msg: c.timestamp(), sender_title, message, expiring, reaction: None, picture });
    }

    /// Someone else reacted (or took a reaction back): a reaction to one of our messages
    /// notifies, and replaces that person's earlier one.
    pub(super) async fn notify_reaction(&mut self, thread: &Thread, key: &str, c: &Content, target: u64, emoji: &str, remove: bool) {
        let from = c.metadata.sender.raw_uuid();
        if from == self.me {
            return;
        }
        self.notes.remove(By::Reaction { msg: target, from });
        if remove || !self.synced || !crate::settings::notify_enabled() || self.mute(key).active(view::now_ms()) {
            return;
        }
        let ours = view::data_message(c)
            .and_then(|dm| dm.reaction.as_ref())
            .and_then(|r| r.parse_target_author_aci())
            .is_some_and(|a| Uuid::from(a) == self.me);
        if !ours {
            return;
        }
        let Ok(found) = self.store.messages(thread, target..=target).await else { return };
        let Some(msg) = found.flatten().find(|m| m.timestamp() == target && m.metadata.sender.raw_uuid() == self.me) else {
            return;
        };
        let expiring = view::data_message(&msg).is_some_and(|dm| dm.expire_timer.unwrap_or(0) > 0);
        let message = self.snippet(&msg).await.unwrap_or_default();
        let from_name = self.name(from).await;
        let sender_title = self.sender_title(thread, from).await;
        let picture = self.toast_picture(thread, key).await;
        let reaction = Some(ReactionRef { from, from_name, emoji: emoji.into() });
        self.notes.add(Queued { key: key.into(), msg: target, sender_title, message, expiring, reaction, picture });
    }

    /// 1:1: the sender's name; groups: "{group}의 {sender} 님" (`icu:notificationSenderInGroup`).
    async fn sender_title(&mut self, thread: &Thread, sender: Uuid) -> String {
        let name = self.name(sender).await;
        match thread {
            Thread::Group(_) => format!("{}의 {name} 님", self.thread_name(thread).await),
            Thread::Contact(_) => name,
        }
    }

    /// The chat's photo as stored (no network), else its initial (`getAvatarOrIdenticon`).
    async fn toast_picture(&mut self, thread: &Thread, key: &str) -> Option<Picture> {
        if crate::settings::preview().is_none_or(|p| p >= 2) {
            return None;
        }
        let photo = match thread {
            Thread::Contact(sid) => {
                let uuid = sid.raw_uuid();
                let profile = match self.store.profile_key(sid).await {
                    Ok(Some(k)) => self.store.profile_avatar(uuid, k).await.ok().flatten(),
                    _ => None,
                };
                match profile {
                    Some(b) => Some(b.to_vec()),
                    None => self.store.contact_by_id(sid).await.ok().flatten().and_then(|c| c.avatar).map(|a| a.reader.to_vec()),
                }
            }
            Thread::Group(master) => self.store.group_avatar(*master).await.ok().flatten().map(|b| b.to_vec()),
        };
        Some(match photo {
            Some(b) => Picture::Photo(b),
            None => {
                let name = self.thread_name(thread).await;
                Picture::Initial { text: crate::gui::initial(&name), rgb: crate::gui::tint_rgb(key) }
            }
        })
    }

    /// A toast was clicked: open its chat and show the message (`showConversationViaNotification`).
    pub(super) async fn open_from_toast(&mut self, key: &str, msg: Option<u64>) {
        if self.current.as_ref().map(|c| c.key.as_str()) != Some(key) {
            if let Err(e) = self.open_thread(key).await {
                self.status(format!("대화를 열지 못했습니다: {e:#}"));
                return;
            }
        }
        let Some(msg) = msg else { return };
        // the newest message is already in view at the bottom
        let me = self.me;
        let newest = self.current.as_ref().and_then(|o| {
            o.window.iter().rev().find(|(_, c)| matches!(view::classify(c, me), Kind::Msg { .. })).map(|(_, c)| c.timestamp())
        });
        if newest != Some(msg) {
            if let Err(e) = self.jump_to(&msg.to_string()).await {
                warn!("jump to {msg}: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(reaction: Option<ReactionRef>, expiring: bool) -> Queued {
        Queued {
            key: "c:1".into(),
            msg: 5,
            sender_title: "그룹의 철수 님".into(),
            message: "안녕".into(),
            expiring,
            reaction,
            picture: None,
        }
    }

    #[test]
    fn content_follows_preview_and_kind() {
        assert_eq!(content(&q(None, false), 0), ("그룹의 철수 님".into(), "안녕".into()));
        assert_eq!(content(&q(None, false), 1).1, "새 메시지");
        assert_eq!(content(&q(None, false), 2), ("Sigwin".into(), "새 메시지".into()));
        assert_eq!(content(&q(None, true), 0).1, "새 메시지");
        let r = ReactionRef { from: Uuid::nil(), from_name: "철수".into(), emoji: "👍".into() };
        assert_eq!(content(&q(Some(r), false), 0).1, "철수 님이 \"안녕\" 메시지에 👍 이모지로 공감했어요");
    }

    #[test]
    fn remove_matches_thread_message_and_reactor() {
        let r = ReactionRef { from: Uuid::from_u128(7), from_name: String::new(), emoji: "👍".into() };
        assert!(By::Thread("c:1").matches("c:1", 5, None));
        assert!(!By::Thread("c:2").matches("c:1", 5, None));
        assert!(By::Message(5).matches("c:1", 5, Some(&r)));
        assert!(By::Reaction { msg: 5, from: Uuid::from_u128(7) }.matches("c:1", 5, Some(&r)));
        assert!(!By::Reaction { msg: 5, from: Uuid::from_u128(8) }.matches("c:1", 5, Some(&r)));
        assert!(!By::Reaction { msg: 5, from: Uuid::from_u128(7) }.matches("c:1", 5, None));
    }

    #[test]
    fn focus_drops_queued_and_debounces() {
        let mut n = Notifier::default();
        n.add(q(None, false));
        n.tick(false);
        assert!(n.queued.is_some(), "not due yet");
        n.due = Some(Instant::now());
        n.tick(true);
        assert!(n.queued.is_none() && n.shown.is_none());
    }
}
