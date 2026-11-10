//! What the phone keeps in the Storage Service, read only (never written back): notifications
//! off per thread (`mute.rs` resolves them), the people the phone lists as contacts, whom
//! the contact list shows and whose names the threads use, and whom it accepts or blocks
//! (`requests.rs`). The contact sync the phone sends after linking holds only a few of them.

use presage::libsignal_service::configuration::ServiceCredentials;
use presage::libsignal_service::master_key::StorageServiceKey;
use presage::libsignal_service::prelude::phonenumber;
use presage::libsignal_service::proto::{manifest_record::identifier::Type as IdType, storage_record::Record, ContactRecord};
use presage::libsignal_service::protocol::E164;
use presage::libsignal_service::push_service::PushService;
use presage::libsignal_service::StorageService;
use presage::manager::RegistrationData;
use presage::model::contacts::Contact;

use std::collections::BTreeMap;

use super::threads::combine_names;
use super::*;

/// items per Storage Service read (presage reads groups 100 at a time too)
const READ_CHUNK: usize = 100;

/// bumped when a read collects more (2: group mention settings, 3: contacts, 4: accepted and
/// blocked, which message requests go by)
pub(super) const STORAGE_FORMAT: u32 = 4;

/// What one Storage Service manifest says.
pub(super) struct PhoneStorage {
    version: u64,
    /// thread key -> muted until
    mutes: HashMap<String, u64>,
    /// groups whose mentions don't notify while muted
    no_mentions: HashSet<String>,
    people: Vec<Person>,
}

/// A contact record, as much as we use of it.
struct Person {
    uuid: Uuid,
    /// what the phone calls them; empty when it has nothing
    name: String,
    e164: String,
    /// the phone would list them as a contact
    listed: bool,
    /// why not, for the log
    unlisted: Option<&'static str>,
    /// their messages are no message request: in the address book or accepted, and not hidden
    accepted: bool,
    blocked: bool,
}

impl Person {
    fn from_record(uuid: Uuid, c: &ContactRecord) -> Self {
        let nickname = c.nickname.as_ref().map(|n| combine_names(&n.given, &n.family)).unwrap_or_default();
        let mut system = combine_names(&c.system_given_name, &c.system_family_name);
        if system.is_empty() {
            system = crate::link::one_line(&c.system_nickname);
        }
        let profile = combine_names(&c.given_name, &c.family_name);
        // the official apps' order: what we named them, the address book, their own profile
        let name = [nickname, system.clone(), profile].into_iter().find(|n| !n.is_empty()).unwrap_or_default();
        // in the address book or allowed to message us, and not hidden, blocked or gone
        let unlisted = if c.blocked {
            Some("blocked")
        } else if c.hidden {
            Some("hidden")
        } else if c.unregistered_at_timestamp != 0 {
            Some("unregistered")
        } else if system.is_empty() && !c.whitelisted {
            Some("not_a_contact")
        } else {
            None
        };
        // Signal Desktop's `isTrustedContact`: in the address book or sharing our profile
        // (`whitelisted`); a contact removed on the phone (`hidden`) writes as a request again
        let accepted = !c.hidden && (!system.is_empty() || c.whitelisted);
        Self { uuid, name, e164: c.e164.clone(), listed: unlisted.is_none(), unlisted, accepted, blocked: c.blocked }
    }
}

impl App {
    /// Reads the phone's storage in the background (skipped when the manifest is unchanged).
    /// Runs after each (re)connect and when the phone says storage changed.
    pub(super) fn sync_phone_storage(&mut self) {
        if self.storage_syncing {
            return;
        }
        let Some(m) = self.manager.as_ref() else { return };
        let reg = m.registration_data().clone();
        let (store, tx) = (self.store.clone(), self.tx.clone());
        let known = if self.persist.storage_format < STORAGE_FORMAT { 0 } else { self.persist.storage_version };
        self.storage_syncing = true;
        tokio::task::spawn_local(async move {
            let res = Box::pin(read_phone_storage(&store, &reg, known)).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::PhoneStorage(res));
        });
    }

    pub(super) async fn phone_storage(&mut self, res: Result<Option<PhoneStorage>, String>) {
        self.storage_syncing = false;
        match res {
            Ok(Some(PhoneStorage { version, mutes, no_mentions, people })) => {
                let listed = people.iter().filter(|p| p.listed).count();
                let mut unlisted: BTreeMap<&str, usize> = BTreeMap::new();
                for why in people.iter().filter_map(|p| p.unlisted) {
                    *unlisted.entry(why).or_default() += 1;
                }
                let accepted = people.iter().filter(|p| p.accepted).count();
                let blocked = people.iter().filter(|p| p.blocked).count();
                info!(version, muted = mutes.len(), no_mentions = no_mentions.len(), people = people.len(), listed, ?unlisted, accepted, blocked, "phone storage");
                self.persist.storage_version = version;
                self.persist.storage_format = STORAGE_FORMAT;
                self.persist.mute_phone = mutes;
                self.persist.phone_no_mentions = no_mentions;
                self.persist.listed = people.iter().filter(|p| p.listed).map(|p| format!("c:{}", p.uuid)).collect();
                self.phone_answers(
                    people.iter().filter(|p| p.accepted).map(|p| format!("c:{}", p.uuid)).collect(),
                    people.iter().filter(|p| p.blocked).map(|p| format!("c:{}", p.uuid)).collect(),
                );
                if let Err(e) = self.save_people(&people).await {
                    warn!("contacts from the phone: {e:#}");
                }
                // names may have changed
                self.names.clear();
                self.persist_dirty = true;
                self.threads_dirty = true;
            }
            Ok(None) => debug!("storage manifest unchanged"),
            // the threads keep the last known values (or Sigwin's own)
            Err(e) => warn!("phone storage: {e}"),
        }
    }

    /// Files the phone's names and numbers into the store's contacts, keeping what else they
    /// hold (timers, keys). People the phone wouldn't list are added only if already there.
    async fn save_people(&mut self, people: &[Person]) -> anyhow::Result<()> {
        for p in people {
            let id = ServiceId::Aci(p.uuid.into());
            let phone = (!p.e164.is_empty()).then(|| phonenumber::parse(None, &p.e164).ok()).flatten();
            let contact = match self.store.contact_by_id(&id).await? {
                Some(mut c) => {
                    let named = !p.name.is_empty() && c.name != p.name;
                    let numbered = c.phone_number.is_none() && phone.is_some();
                    if !named && !numbered {
                        continue;
                    }
                    if named {
                        c.name = p.name.clone();
                    }
                    if numbered {
                        c.phone_number = phone;
                    }
                    c
                }
                None if p.listed => Contact {
                    uuid: p.uuid,
                    phone_number: phone,
                    name: p.name.clone(),
                    verified: Default::default(),
                    profile_key: Vec::new(),
                    expire_timer: 0,
                    expire_timer_version: 2,
                    inbox_position: 0,
                    avatar: None,
                },
                None => continue,
            };
            self.store.save_contact(&contact).await?;
        }
        Ok(())
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

/// Contact and group records. None: the manifest is still `known`.
async fn read_phone_storage(store: &SqliteStore, reg: &RegistrationData, known: u64) -> anyhow::Result<Option<PhoneStorage>> {
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
    let (mut mutes, mut no_mentions, mut people) = (HashMap::new(), HashSet::new(), Vec::new());
    for chunk in ids.chunks(READ_CHUNK) {
        for rec in storage.read_items(chunk.to_vec(), Some(&manifest.record_ikm)).await? {
            let (key, until) = match rec.record {
                Some(Record::Contact(c)) => {
                    let uuid = Uuid::from_slice(&c.aci_binary).ok().or_else(|| c.aci.parse().ok());
                    if let Some(u) = uuid {
                        people.push(Person::from_record(u, &c));
                    }
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
    Ok(Some(PhoneStorage { version: manifest.version, mutes, no_mentions, people }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::contact_record::Name;

    #[test]
    fn names_and_listing_follow_the_phone() {
        let u = Uuid::from_u128(5);
        let record = |f: &dyn Fn(&mut ContactRecord)| {
            let mut c = ContactRecord { given_name: "길동".into(), family_name: "홍".into(), ..Default::default() };
            f(&mut c);
            Person::from_record(u, &c)
        };
        // only a profile: the name, but not a contact unless allowed to message us
        let p = record(&|_| {});
        assert_eq!((p.name.as_str(), p.listed), ("홍길동", false));
        assert!(record(&|c| c.whitelisted = true).listed);
        // the address book wins over the profile, a nickname over both
        let p = record(&|c| c.system_given_name = "\u{2068}길동이\u{2069}".into());
        assert_eq!((p.name.as_str(), p.listed), ("길동이", true));
        let p = record(&|c| {
            c.system_given_name = "길동이".into();
            c.nickname = Some(Name { given: "동생".into(), family: String::new() });
        });
        assert_eq!(p.name, "동생");
        for hide in [
            &(|c: &mut ContactRecord| c.blocked = true) as &dyn Fn(&mut ContactRecord),
            &|c| c.hidden = true,
            &|c| c.unregistered_at_timestamp = 1,
        ] {
            assert!(!record(&|c| {
                c.whitelisted = true;
                hide(c);
            })
            .listed);
        }
    }
}
