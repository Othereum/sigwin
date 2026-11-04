//! Per-thread notification settings. "Notifications off": the phone's value from the Storage
//! Service (read only, never written back), overridden by what is picked in Sigwin until that
//! runs out or is cleared ("휴대폰 설정 따르기"). The rest (preview, sound, what gets through while
//! off, badge) resolve the same way: the thread's own choice, else the phone's (group
//! mentions only), else the app setting.

use presage::libsignal_service::configuration::ServiceCredentials;
use presage::libsignal_service::master_key::StorageServiceKey;
use presage::libsignal_service::proto::{manifest_record::identifier::Type as IdType, storage_record::Record};
use presage::libsignal_service::protocol::E164;
use presage::libsignal_service::push_service::PushService;
use presage::libsignal_service::StorageService;
use presage::manager::RegistrationData;

use crate::types::{Mute, MuteSet, Pref, ThreadPrefs, MUTE_ALWAYS};

use super::*;

/// items per Storage Service read (presage reads groups 100 at a time too)
const READ_CHUNK: usize = 100;

/// bumped when a read collects more (2: group mention settings)
pub(super) const STORAGE_FORMAT: u32 = 2;

/// What the phone says about notifications, from one Storage Service manifest.
pub(super) struct PhoneMutes {
    version: u64,
    /// thread key -> muted until
    mutes: HashMap<String, u64>,
    /// groups whose mentions don't notify while muted
    no_mentions: HashSet<String>,
}

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
            muted: self.mute(key).active(view::now_ms()),
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
        let now = view::now_ms();
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
        let now = view::now_ms();
        let before = self.persist.mute_override.len();
        self.persist.mute_override.retain(|_, u| *u == 0 || *u > now);
        if self.persist.mute_override.len() != before {
            self.persist_dirty = true;
        }
        self.threads_dirty = true;
    }

    /// The next moment a listed thread's state changes by itself.
    pub(super) fn next_mute_change(&self) -> Option<u64> {
        let now = view::now_ms();
        let phone = self.persist.mute_phone.values();
        let own = self.persist.mute_override.values();
        phone.chain(own).copied().filter(|&u| u > now && u < i64::MAX as u64).min()
    }

    /// Reads the phone's muted threads in the background (skipped when the manifest is
    /// unchanged). Runs after each (re)connect and when the phone says storage changed.
    pub(super) fn sync_phone_mutes(&mut self) {
        if self.mutes_syncing {
            return;
        }
        let Some(m) = self.manager.as_ref() else { return };
        let reg = m.registration_data().clone();
        let (store, tx) = (self.store.clone(), self.tx.clone());
        let known = if self.persist.storage_format < STORAGE_FORMAT { 0 } else { self.persist.storage_version };
        self.mutes_syncing = true;
        tokio::task::spawn_local(async move {
            let res = Box::pin(read_phone_mutes(&store, &reg, known)).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::PhoneMutes(res));
        });
    }

    pub(super) fn phone_mutes(&mut self, res: Result<Option<PhoneMutes>, String>) {
        self.mutes_syncing = false;
        match res {
            Ok(Some(PhoneMutes { version, mutes, no_mentions })) => {
                info!(version, muted = mutes.len(), no_mentions = no_mentions.len(), "notification settings from the phone");
                self.persist.storage_version = version;
                self.persist.storage_format = STORAGE_FORMAT;
                self.persist.mute_phone = mutes;
                self.persist.phone_no_mentions = no_mentions;
                self.persist_dirty = true;
                self.threads_dirty = true;
            }
            Ok(None) => debug!("storage manifest unchanged"),
            // the threads keep the last known values (or Sigwin's own)
            Err(e) => warn!("notification settings from the phone: {e}"),
        }
    }
}

/// presage keeps its authenticated `PushService` crate-private, so one is built here from the
/// stored registration. The password is crate-private too, but `RegistrationData` serializes it.
fn push_service(reg: &RegistrationData) -> anyhow::Result<PushService> {
    let json = serde_json::to_value(reg)?;
    let password = json.get("password").and_then(|p| p.as_str()).context("no password in the registration")?.to_string();
    let phonenumber: E164 = reg.phone_number.to_string().parse().ok().context("phone number")?;
    let credentials = ServiceCredentials {
        aci: Some(reg.service_ids.aci),
        pni: Some(reg.service_ids.pni),
        phonenumber,
        password: Some(password),
        device_id: reg.device_id.and_then(|d| d.try_into().ok()),
    };
    Ok(PushService::new(reg.signal_servers, Some(credentials), concat!("sigwin/", env!("CARGO_PKG_VERSION"))))
}

/// Contact and group records with notifications off. None: the manifest is still `known`.
async fn read_phone_mutes(store: &SqliteStore, reg: &RegistrationData, known: u64) -> anyhow::Result<Option<PhoneMutes>> {
    let master_key = store.fetch_master_key().await?.context("no master key")?;
    let storage = StorageService::new(push_service(reg)?, StorageServiceKey::from_master_key(&master_key)).await?;
    let manifest = if known == 0 {
        storage.manifest().await?
    } else {
        match storage.manifest_if_changed(known).await? {
            Some(m) => m,
            None => return Ok(None),
        }
    };
    let ids: Vec<Vec<u8>> = manifest
        .identifiers
        .iter()
        .filter(|id| matches!(id.r#type(), IdType::Contact | IdType::Groupv2))
        .map(|id| id.raw.clone())
        .collect();
    let (mut mutes, mut no_mentions) = (HashMap::new(), HashSet::new());
    for chunk in ids.chunks(READ_CHUNK) {
        for rec in storage.read_items(chunk.to_vec(), Some(&manifest.record_ikm)).await? {
            let (key, until) = match rec.record {
                Some(Record::Contact(c)) => {
                    let uuid = Uuid::from_slice(&c.aci_binary).ok().or_else(|| c.aci.parse().ok());
                    (uuid.map(|u| format!("c:{u}")), c.muted_until_timestamp)
                }
                Some(Record::GroupV2(g)) if g.master_key.len() == 32 => {
                    let key = format!("g:{}", hex::encode(&g.master_key));
                    if g.dont_notify_for_mentions_if_muted {
                        no_mentions.insert(key.clone());
                    }
                    (Some(key), g.muted_until_timestamp)
                }
                _ => continue,
            };
            if let (Some(key), true) = (key, until > 0) {
                mutes.insert(key, until);
            }
        }
    }
    Ok(Some(PhoneMutes { version: manifest.version, mutes, no_mentions }))
}

/// Whether a message gets through to a thread with notifications off: @mentions of us and
/// replies to our messages do, if the thread's settings say so (Signal's defaults: both).
pub(super) fn breaks_mute(dm: &DataMessage, me: Uuid, mentions: bool, replies: bool) -> bool {
    let mentioned = || dm.body_ranges.iter().filter_map(view::mention_uuid).any(|u| u == me);
    let replied = || dm.quote.as_ref().and_then(view::quote_author) == Some(me);
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
