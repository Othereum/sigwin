//! Per-thread notification settings. "Notifications off": the phone's value from the Storage
//! Service (read only, never written back), overridden by what is picked in Sigwin until that
//! runs out or is cleared ("휴대폰 설정 따르기"). The rest (preview, sound, what gets through while
//! off, badge) resolve the same way: the thread's own choice, else the phone's (group
//! mentions only), else the app setting.

use crate::types::{Mute, MuteSet, Pref, ThreadPrefs, MUTE_ALWAYS};

use super::*;

/// How a thread's notifications behave now, all layers resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Notify {
    pub muted: bool,
    /// what toasts show (`Settings::preview`); None = notifications are off in the app
    pub preview: Option<u8>,
    pub sound: bool,
    /// while muted
    pub mentions: bool,
    pub replies: bool,
}

impl App {
    pub(super) fn prefs(&self, key: &str) -> ThreadPrefs {
        self.persist.thread_prefs.get(key).copied().unwrap_or_default()
    }

    /// The phone's "mentions notify while muted" (groups that turned it off), else unset.
    pub(super) fn phone_mentions(&self, key: &str) -> Option<bool> {
        self.persist.phone_no_mentions.contains(key).then_some(false)
    }

    pub(super) fn notify_for(&self, key: &str) -> Notify {
        let p = self.prefs(key);
        let (mentions, replies) = crate::settings::muted_exceptions();
        Notify {
            muted: self.mute(key).active(view::time::now_ms()),
            preview: crate::settings::preview().map(|global| p.preview.unwrap_or(global)),
            sound: p.sound.unwrap_or_else(crate::settings::sound),
            mentions: p.mentions.or(self.phone_mentions(key)).unwrap_or(mentions),
            replies: p.replies.unwrap_or(replies),
        }
    }

    pub(super) fn set_pref(&mut self, key: &str, pref: Pref) {
        let mut p = self.prefs(key);
        p.set(pref);
        if p.is_empty() {
            self.persist.thread_prefs.remove(key);
        } else {
            self.persist.thread_prefs.insert(key.to_string(), p);
        }
        info!(?pref, "thread notification setting");
        self.persist_dirty = true;
        self.threads_dirty = true;
    }

    /// A thread's state now: Sigwin's choice if any, else the phone's.
    pub(super) fn mute(&self, key: &str) -> Mute {
        let phone = self.persist.mute_phone.get(key).copied().unwrap_or(0);
        match self.persist.mute_override.get(key) {
            Some(&until) => Mute { until, overridden: true, phone },
            None => Mute { until: phone, overridden: false, phone },
        }
    }

    pub(super) fn set_mute(&mut self, key: &str, set: MuteSet) {
        let now = view::time::now_ms();
        let phone_on = self.persist.mute_phone.get(key).is_none_or(|&u| u <= now);
        let until = match set {
            MuteSet::Phone => None,
            // the same as the phone's: nothing to override
            MuteSet::On if phone_on => None,
            MuteSet::On => Some(0),
            MuteSet::For(secs) => Some(now + secs * 1000),
            MuteSet::Always => Some(MUTE_ALWAYS),
        };
        match until {
            Some(u) => self.persist.mute_override.insert(key.to_string(), u),
            None => self.persist.mute_override.remove(key),
        };
        info!(?set, "notifications for a thread");
        self.persist_dirty = true;
        self.threads_dirty = true;
    }

    /// Sigwin's choices that ran out go back to the phone's; called when one is due.
    pub(super) fn expire_mutes(&mut self) {
        let now = view::time::now_ms();
        let before = self.persist.mute_override.len();
        self.persist.mute_override.retain(|_, u| *u == 0 || *u > now);
        if self.persist.mute_override.len() != before {
            self.persist_dirty = true;
        }
        self.threads_dirty = true;
    }

    /// The next moment a listed thread's state changes by itself.
    pub(super) fn next_mute_change(&self) -> Option<u64> {
        let now = view::time::now_ms();
        let phone = self.persist.mute_phone.values();
        let own = self.persist.mute_override.values();
        phone.chain(own).copied().filter(|&u| u > now && u < i64::MAX as u64).min()
    }
}

/// Whether a message gets through to a thread with notifications off: @mentions of us and
/// replies to our messages do, if the thread's settings say so (Signal's defaults: both).
pub(super) fn breaks_mute(dm: &DataMessage, me: Uuid, mentions: bool, replies: bool) -> bool {
    let mentioned = || dm.body_ranges.iter().filter_map(view::text::mention_uuid).any(|u| u == me);
    let replied = || dm.quote.as_ref().and_then(view::text::quote_author) == Some(me);
    (mentions && mentioned()) || (replies && replied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::proto::body_range::AssociatedValue;

    #[test]
    fn mentions_and_replies_break_through() {
        let me = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);
        let mention = |u: Uuid| BodyRange { start: Some(0), length: Some(1), associated_value: Some(AssociatedValue::MentionAci(u.to_string())), ..Default::default() };
        let quote = |u: Uuid| Quote { author_aci: Some(u.to_string()), ..Default::default() };
        let plain = DataMessage::default();
        let at_me = DataMessage { body_ranges: vec![mention(me)], ..Default::default() };
        let reply = DataMessage { quote: Some(quote(me)), ..Default::default() };
        assert!(!breaks_mute(&plain, me, true, true));
        assert!(breaks_mute(&at_me, me, true, false));
        assert!(!breaks_mute(&at_me, me, false, true));
        assert!(!breaks_mute(&DataMessage { body_ranges: vec![mention(other)], ..Default::default() }, me, true, true));
        assert!(breaks_mute(&reply, me, false, true));
        assert!(!breaks_mute(&reply, me, true, false));
        assert!(!breaks_mute(&DataMessage { quote: Some(quote(other)), ..Default::default() }, me, true, true));
    }
}
