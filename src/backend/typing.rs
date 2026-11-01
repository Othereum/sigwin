//! Typing indicators, both ways.

use super::*;

impl App {
    pub(super) fn typing_enabled(&self) -> bool {
        self.persist.typing_indicators != Some(false)
    }

    pub(super) async fn typing_received(&mut self, who: Uuid, t: &TypingMessage) {
        if who == self.me || !self.typing_enabled() {
            return;
        }
        let Some(open) = self.current.as_ref() else { return };
        let here = match (&t.group_id, &open.group_id) {
            (Some(g), Some(o)) => g.as_slice() == o.as_slice(),
            (None, None) => matches!(&open.thread, Thread::Contact(id) if id.raw_uuid() == who),
            _ => false,
        };
        if !here {
            return;
        }
        // presage can't send "online only": stale indicators arrive when a device reconnects
        let fresh = t.timestamp.is_none_or(|ts| view::now_ms().saturating_sub(ts) < TYPING_TIMEOUT.as_millis() as u64);
        if t.action == Some(typing_message::Action::Started as i32) && fresh {
            self.typing.insert(who, Instant::now());
        } else {
            self.typing.remove(&who);
        }
        self.emit_typing().await;
    }

    pub(super) async fn emit_typing(&mut self) {
        let mut ids: Vec<Uuid> = self.typing.keys().copied().collect();
        ids.sort();
        let group = self.current.as_ref().is_some_and(|o| o.is_group);
        let key = ids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        if key == self.typing_shown {
            return;
        }
        self.typing_shown = key;
        let mut who = Vec::with_capacity(ids.len());
        for u in ids {
            who.push((u.to_string(), self.name(u).await));
        }
        (self.emit)(Event::Typing { group, who });
    }

    /// The composer changed: tell the open thread we are typing (or stopped).
    pub(super) fn composing(&mut self, active: bool) {
        if !active {
            if let Some(c) = self.composing.take() {
                self.send_typing(c.thread, false);
            }
            return;
        }
        if !self.typing_enabled() {
            return;
        }
        let Some(open) = self.current.as_ref() else { return };
        if open.members.len() > TYPING_MAX_GROUP || matches!(&open.thread, Thread::Contact(id) if id.raw_uuid() == self.me) {
            return;
        }
        let now = Instant::now();
        if let Some(c) = self.composing.as_mut().filter(|c| c.key == open.key) {
            c.last_edit = now;
            if c.last_sent.elapsed() >= TYPING_RESEND {
                c.last_sent = now;
                let thread = c.thread.clone();
                self.send_typing(thread, true);
            }
            return;
        }
        if let Some(old) = self.composing.take() {
            self.send_typing(old.thread, false);
        }
        let Some(open) = self.current.as_ref() else { return };
        let thread = open.thread.clone();
        self.composing = Some(Composing { key: open.key.clone(), thread: thread.clone(), last_edit: now, last_sent: now });
        self.send_typing(thread, true);
    }

    pub(super) fn send_typing(&self, thread: Thread, started: bool) {
        let Some(mut m) = self.manager.clone() else { return };
        let ts = view::now_ms();
        let action = if started { typing_message::Action::Started } else { typing_message::Action::Stopped };
        let group_id = match &thread {
            Thread::Group(k) => Some(group_identifier(k).to_vec()),
            _ => None,
        };
        let msg = TypingMessage { timestamp: Some(ts), action: Some(action as i32), group_id };
        tokio::task::spawn_local(async move {
            let res = match thread {
                Thread::Contact(id) => Box::pin(m.send_message(id, msg, ts)).await,
                Thread::Group(key) => Box::pin(m.send_message_to_group(&key, msg, ts)).await,
            };
            if let Err(e) = res {
                debug!("typing indicator: {e}");
            }
        });
    }

    // ---- reactions, edits, deletes ----
}
