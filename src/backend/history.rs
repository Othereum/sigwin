//! The message history the phone sends when linking ("link and sync").
//!
//! The QR code advertises [`LINK_CAPABILITY`], so the phone offers to transfer history. If the
//! user accepts, the provisioning message carries a one-time key and the phone uploads an
//! archive in Signal's backup format to the transit CDN. We wait for where it went, download it
//! and import contacts, groups and messages into presage's store. All of it happens before
//! receiving starts: display order is store order (`ls_order`), so nothing live may come in
//! between.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures::AsyncReadExt as _;
use libsignal_account_keys::BackupKey;
use libsignal_message_backup::frame::{FileReaderFactory, FramesReader, VerifyHmac as _};
use libsignal_message_backup::key::MessageBackupKey;
use libsignal_message_backup::parse::VarintDelimitedReader;
use libsignal_message_backup::proto::backup as proto;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage, GroupContextV2, Metadata};
use presage::libsignal_service::groups_v2::{Role, Timer};
use presage::libsignal_service::prelude::{phonenumber, ProfileKey, Uuid};
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::libsignal_service::push_service::{HttpAuthOverride, PushService, DEFAULT_DEVICE_ID};
use presage::manager::RegistrationData;
use presage::model::contacts::Contact;
use presage::model::groups::{Group, Member};
use presage::proto::attachment_pointer::{AttachmentIdentifier, Flags};
use presage::proto::data_message::{Quote, Reaction, Sticker};
use presage::proto::sync_message::{Content as SyncContent, Sent};
use presage::proto::{AttachmentPointer, SyncMessage};
use presage::store::{ContentsStore, Thread};
use presage_store_sqlite::SqliteStore;

use crate::db::Index;
use protobuf::Message as _;
use serde::Deserialize;
use tracing::{info, warn};

/// Makes the phone offer "transfer message history" (Signal Desktop's value).
pub(crate) const LINK_CAPABILITY: &str = "backup5";

/// How long the phone gets to upload, as in Signal Desktop.
const MAX_WAIT: Duration = Duration::from_secs(60 * 60);
/// Per request, below the HTTP client's own timeout.
const POLL: Duration = Duration::from_secs(55);
/// The archive holds text and attachment pointers, not the files. This only stops a misbehaving
/// server from filling the disk.
const MAX_ARCHIVE: u64 = 1 << 30;
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// How the transfer ended, for the backend.
pub(crate) enum History {
    Imported,
    /// by the user, here or on the phone
    Skipped,
    Relink,
    Failed(String),
}

/// [`receive`] with its progress on the loading screen, until done or skipped. Skipping or
/// failing keeps the link: the history is a bonus, and Signal Desktop's way (unlink and start
/// over) would make the user scan again for nothing.
pub(crate) async fn run(
    reg: &RegistrationData,
    key: [u8; 32],
    path: &Path,
    store: &mut SqliteStore,
    index: &Index,
    emit: &super::Emit,
    skip: futures::channel::oneshot::Receiver<()>,
) -> History {
    use crate::types::Event;
    let progress = |phase: Phase| match phase {
        Phase::Waiting => {
            emit(Event::Loading("휴대폰에서 대화 기록을 보내기를 기다리는 중…".into()));
            emit(Event::HistorySkippable(true));
        }
        Phase::Downloading(bytes) => {
            emit(Event::Loading(format!("대화 기록 받는 중… {}", crate::view::media::size_text(bytes))));
        }
        // from here it is quick and local; stopping halfway would leave a gap before what comes live
        Phase::Importing(n) => {
            emit(Event::HistorySkippable(false));
            emit(Event::Loading(format!("대화 기록 가져오는 중… 메시지 {n}개")));
        }
    };
    let importing = std::cell::Cell::new(false);
    let progress = |phase: Phase| {
        if matches!(phase, Phase::Importing(_)) {
            importing.set(true);
        }
        progress(phase)
    };
    let work = std::pin::pin!(receive(reg, key, path, store, index, &progress));
    // a dropped sender (the link was restarted) is no skip
    let skip = std::pin::pin!(async {
        if skip.await.is_err() {
            std::future::pending::<()>().await;
        }
    });
    let result = match futures::future::select(work, skip).await {
        futures::future::Either::Left((r, _)) => r,
        futures::future::Either::Right(((), work)) if importing.get() => work.await,
        futures::future::Either::Right(_) => {
            info!("history skipped");
            return History::Skipped;
        }
    };
    match result {
        Ok(Outcome::Imported) => History::Imported,
        Ok(Outcome::WithoutHistory) => History::Skipped,
        Ok(Outcome::Relink) => History::Relink,
        Err(e) => {
            warn!("history transfer: {e:#}");
            History::Failed(e.to_string())
        }
    }
}

pub(crate) enum Phase {
    Waiting,
    Downloading(u64),
    Importing(usize),
}

pub(crate) enum Outcome {
    Imported,
    /// The phone's upload failed and the user chose to go on without history there.
    WithoutHistory,
    /// The user asked on the phone to link again.
    Relink,
}

/// Waits for the phone's archive, downloads it to `path` and imports it into `store`. On `Err`,
/// part of the history may have been stored. `path` is gone afterwards, however this ends
/// (dropping the future included).
pub(crate) async fn receive(
    reg: &RegistrationData,
    key: [u8; 32],
    path: &Path,
    store: &mut SqliteStore,
    index: &Index,
    progress: &dyn Fn(Phase),
) -> anyhow::Result<Outcome> {
    let _cleanup = RemoveOnDrop(path);
    progress(Phase::Waiting);
    let started = Instant::now();
    let (cdn, cdn_key) = match wait_for_archive(reg).await? {
        Archive::Uploaded { cdn, key } => (cdn, key),
        Archive::ContinueWithoutUpload => return Ok(Outcome::WithoutHistory),
        Archive::RelinkRequested => return Ok(Outcome::Relink),
    };
    info!(waited_s = started.elapsed().as_secs(), "history archive uploaded");
    let mut attempt = 1;
    let bytes = loop {
        match download(cdn, &cdn_key, path, progress).await {
            Ok(n) => break n,
            Err(e) if attempt < DOWNLOAD_ATTEMPTS && e.downcast_ref::<reqwest::Error>().is_some() => {
                warn!(attempt, "history download: {e:#}");
                tokio::time::sleep(Duration::from_secs(5) * attempt).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let started = Instant::now();
    let stats = import(key, &Aci::from(reg.service_ids.aci), path, store, index, progress).await?;
    info!(bytes, ms = started.elapsed().as_millis() as u64, ?stats, "history imported");
    Ok(Outcome::Imported)
}

struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("remove history archive: {e}");
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Archive {
    Uploaded { cdn: u32, key: String },
    RelinkRequested,
    ContinueWithoutUpload,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArchiveResponse {
    Uploaded { cdn: u32, key: String },
    Error { error: String },
}

impl TryFrom<ArchiveResponse> for Archive {
    type Error = anyhow::Error;
    fn try_from(r: ArchiveResponse) -> anyhow::Result<Self> {
        Ok(match r {
            ArchiveResponse::Uploaded { cdn, key } => Self::Uploaded { cdn, key },
            ArchiveResponse::Error { error } => match error.as_str() {
                "RELINK_REQUESTED" => Self::RelinkRequested,
                "CONTINUE_WITHOUT_UPLOAD" => Self::ContinueWithoutUpload,
                _ => bail!("휴대폰이 알 수 없는 응답을 보냈습니다"),
            },
        })
    }
}

/// presage #447 would add this call; until then it is made here, with the same authenticated
/// service the Storage Service reads use.
async fn wait_for_archive(reg: &RegistrationData) -> anyhow::Result<Archive> {
    let service = super::storage::push_service(reg)?;
    let started = Instant::now();
    let mut backoff = Duration::from_secs(1);
    while started.elapsed() < MAX_WAIT {
        match poll(&service).await {
            Ok(Some(archive)) => return Ok(archive),
            Ok(None) => backoff = Duration::from_secs(1),
            Err(Poll::Refused(status)) => bail!("서버가 거부했습니다 ({status})"),
            // a network hiccup shouldn't cost the history: the archive may still come
            Err(Poll::Failed(e)) => {
                warn!("history poll: {e:#}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
    bail!("휴대폰이 한 시간 안에 대화 기록을 보내지 않았습니다")
}

enum Poll {
    /// waiting longer won't help (the link is no good)
    Refused(reqwest::StatusCode),
    Failed(anyhow::Error),
}

async fn poll(service: &PushService) -> Result<Option<Archive>, Poll> {
    let path = format!("/v1/devices/transfer_archive?timeout={}", POLL.as_secs());
    let res = service
        .request(reqwest::Method::GET, Endpoint::service(path), HttpAuthOverride::NoOverride)
        .map_err(|e| Poll::Failed(e.into()))?
        .send()
        .await
        .map_err(|e| Poll::Failed(e.without_url().into()))?;
    match res.status() {
        reqwest::StatusCode::NO_CONTENT => Ok(None),
        reqwest::StatusCode::OK => {
            let body = res.bytes().await.map_err(|e| Poll::Failed(e.without_url().into()))?;
            let r: ArchiveResponse = serde_json::from_slice(&body).map_err(|e| Poll::Failed(e.into()))?;
            Archive::try_from(r).map(Some).map_err(Poll::Failed)
        }
        s @ (reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => Err(Poll::Refused(s)),
        s => Err(Poll::Failed(anyhow::anyhow!("HTTP {s}"))),
    }
}

/// Streams the archive to `path`, which the one-time key encrypts. Returns its size.
async fn download(cdn: u32, cdn_key: &str, path: &Path, progress: &dyn Fn(Phase)) -> anyhow::Result<u64> {
    let service = super::files::cdn_service();
    let mut res = service
        .request(reqwest::Method::GET, Endpoint::cdn(cdn, format!("/attachments/{cdn_key}")), HttpAuthOverride::Unidentified)?
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    if res.content_length().is_some_and(|n| n > MAX_ARCHIVE) {
        bail!("대화 기록이 너무 큽니다");
    }
    progress(Phase::Downloading(0));
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).context("create history archive")?);
    let (mut size, mut shown) = (0u64, 0u64);
    while let Some(chunk) = res.chunk().await.map_err(reqwest::Error::without_url)? {
        size += chunk.len() as u64;
        if size > MAX_ARCHIVE {
            bail!("대화 기록이 너무 큽니다");
        }
        file.write_all(&chunk).context("write history archive")?;
        if size - shown >= 1 << 20 {
            shown = size;
            progress(Phase::Downloading(size));
        }
    }
    file.into_inner().map_err(|e| e.into_error())?.sync_all().context("write history archive")?;
    Ok(size)
}

/// Decrypts the archive and imports it frame by frame. `FramesReader` checks the HMAC over the
/// whole file before handing out a frame, so nothing unauthenticated is stored.
async fn import(key: [u8; 32], aci: &Aci, path: &Path, store: &mut SqliteStore, index: &Index, progress: &dyn Fn(Phase)) -> anyhow::Result<Stats> {
    let key = message_backup_key(key, aci);
    let frames = FramesReader::new(&key, FileReaderFactory { path }).await.context("대화 기록을 풀지 못했습니다")?;
    let mut reader = VarintDelimitedReader::new(frames);
    let mut importer = Importer::new(*aci, super::now_ms());
    progress(Phase::Importing(0));
    // the first frame is a `BackupInfo` (version and time)
    let mut first = true;
    let mut shown = 0;
    let mut batch = Vec::new();
    while let Some(frame) = reader.read_next().await.context("read history")? {
        if std::mem::take(&mut first) {
            continue;
        }
        let records = importer.frame(&frame)?;
        store_records(store, index, records, &mut batch, &mut importer.stats).await?;
        if importer.stats.messages >= shown + 500 {
            shown = importer.stats.messages;
            progress(Phase::Importing(shown));
            // let commands (and the UI's progress) through on a long import
            tokio::task::yield_now().await;
        }
    }
    let records = importer.finish();
    store_records(store, index, records, &mut batch, &mut importer.stats).await?;
    flush(index, &mut batch, &mut importer.stats).await?;
    index.order_by_sent().await.context("order history")?;
    // the trailing bytes too, so the HMAC covers everything
    let mut rest = reader.into_inner();
    rest.read_to_end(&mut Vec::new()).await.context("read history")?;
    rest.verify_hmac().await.context("대화 기록이 손상되었습니다")?;
    Ok(importer.stats)
}

/// The one-time key is the backup key itself, with no forward secrecy token.
fn message_backup_key(key: [u8; 32], aci: &Aci) -> MessageBackupKey {
    let backup_key: BackupKey = BackupKey(key);
    MessageBackupKey::derive(&backup_key, &backup_key.derive_backup_id(aci), None)
}

/// What a frame turns into, ready to be stored.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // one at a time
enum Record {
    Contact { contact: Contact, profile_key: Option<ProfileKey> },
    Group { master_key: [u8; 32], group: Group },
    Message { thread: Thread, content: Content },
    /// a contact's disappearing-messages timer (seconds) at its version
    Timer { contact: Aci, seconds: u32, version: u32 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub contacts: usize,
    pub groups: usize,
    pub messages: usize,
    pub reactions: usize,
    /// frames and chat items not imported (calls, group updates, expired, colliding, …)
    pub skipped: usize,
}

/// Who a recipient id in the archive stands for.
#[derive(Clone, Copy)]
enum Party {
    Me,
    Contact(Aci),
    Group { master_key: [u8; 32], revision: u32 },
}

/// Turns frames into records. The archive refers only to frames already sent (libsignal's
/// format guarantees it), so one frame is converted at a time; groups wait for the first
/// non-recipient frame because their members' profile keys come with the contact frames.
struct Importer {
    me: Aci,
    /// ms; disappearing messages gone by then are dropped
    now: u64,
    recipients: HashMap<u64, Party>,
    /// chat id -> recipient id
    chats: HashMap<u64, u64>,
    profile_keys: HashMap<Aci, ProfileKey>,
    pending_groups: Vec<([u8; 32], proto::Group)>,
    stats: Stats,
}

impl Importer {
    fn new(me: Aci, now: u64) -> Self {
        Self {
            me,
            now,
            recipients: HashMap::new(),
            chats: HashMap::new(),
            profile_keys: HashMap::new(),
            pending_groups: Vec::new(),
            stats: Stats::default(),
        }
    }

    fn frame(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<Record>> {
        let frame = proto::Frame::parse_from_bytes(bytes).context("대화 기록의 형식이 잘못되었습니다")?;
        let mut records = Vec::new();
        match frame.item {
            Some(proto::frame::Item::Recipient(r)) => records.extend(self.recipient(r)),
            Some(proto::frame::Item::Chat(chat)) => {
                records.extend(self.flush_groups());
                // a group's timer comes with the group, a contact's with the chat
                if let Some(Party::Contact(contact)) = self.recipients.get(&chat.recipientId) {
                    records.push(Record::Timer {
                        contact: *contact,
                        seconds: (chat.expirationTimerMs.unwrap_or_default() / 1000).try_into().unwrap_or(u32::MAX),
                        version: chat.expireTimerVersion.max(1),
                    });
                }
                self.chats.insert(chat.id, chat.recipientId);
            }
            Some(proto::frame::Item::ChatItem(item)) => {
                records.extend(self.flush_groups());
                match self.message(&item) {
                    Some((thread, content, group_v2)) => {
                        self.stats.messages += 1;
                        let reactions = self.reactions(&item, &thread, group_v2);
                        self.stats.reactions += reactions.len();
                        records.push(Record::Message { thread, content });
                        records.extend(reactions);
                    }
                    None => self.stats.skipped += 1,
                }
            }
            // account settings, sticker packs, calls, notification profiles, chat folders
            Some(_) => self.stats.skipped += 1,
            None => {}
        }
        Ok(records)
    }

    fn finish(&mut self) -> Vec<Record> {
        self.flush_groups()
    }

    fn recipient(&mut self, recipient: proto::Recipient) -> Option<Record> {
        use proto::recipient::Destination;
        match recipient.destination? {
            Destination::Self_(_) => {
                self.recipients.insert(recipient.id, Party::Me);
                None
            }
            Destination::Contact(contact) => {
                let (contact, profile_key) = convert_contact(&contact)?;
                let aci = Aci::from_uuid_bytes(contact.uuid.into_bytes());
                self.recipients.insert(recipient.id, Party::Contact(aci));
                if let Some(key) = profile_key {
                    self.profile_keys.insert(aci, key);
                }
                self.stats.contacts += 1;
                Some(Record::Contact { contact, profile_key })
            }
            Destination::Group(group) => {
                let master_key: [u8; 32] = group.masterKey.as_slice().try_into().ok()?;
                let revision = group.snapshot.version;
                self.recipients.insert(recipient.id, Party::Group { master_key, revision });
                self.pending_groups.push((master_key, group));
                None
            }
            // distribution lists (stories), release notes, call links
            _ => {
                self.stats.skipped += 1;
                None
            }
        }
    }

    fn flush_groups(&mut self) -> Vec<Record> {
        let pending = std::mem::take(&mut self.pending_groups);
        self.stats.groups += pending.len();
        pending.into_iter().map(|(master_key, g)| Record::Group { master_key, group: self.convert_group(&g) }).collect()
    }

    fn convert_group(&self, group: &proto::Group) -> Group {
        use proto::group::group_attribute_blob::Content as Blob;
        let snapshot = &group.snapshot;
        let text = |blob: &protobuf::MessageField<proto::group::GroupAttributeBlob>| match &blob.content {
            Some(Blob::Title(t) | Blob::DescriptionText(t)) => t.clone(),
            _ => String::new(),
        };
        let members = snapshot
            .members
            .iter()
            .filter_map(|m| {
                let aci = Aci::from_uuid_bytes(m.userId.as_slice().try_into().ok()?);
                Some(Member {
                    aci,
                    role: match m.role.enum_value_or_default() {
                        proto::group::member::Role::ADMINISTRATOR => Role::Administrator,
                        _ => Role::Default,
                    },
                    // unknown keys are fixed when presage next fetches the group
                    profile_key: self.profile_keys.get(&aci).copied().unwrap_or(ProfileKey::create([0; 32])),
                    joined_at_revision: m.joinedAtVersion,
                    label: None,
                    label_emoji: None,
                })
            })
            .collect();
        let description = text(&snapshot.description);
        let timer = match &snapshot.disappearingMessagesTimer.content {
            Some(Blob::DisappearingMessagesDuration(s)) if *s > 0 => Some(Timer { duration: *s }),
            _ => None,
        };
        Group {
            title: text(&snapshot.title),
            avatar: snapshot.avatarUrl.clone(),
            disappearing_messages_timer: timer,
            access_control: None,
            revision: snapshot.version,
            members,
            pending_members: Vec::new(),
            requesting_members: Vec::new(),
            invite_link_password: snapshot.inviteLinkPassword.clone(),
            description: (!description.is_empty()).then_some(description),
        }
    }

    /// A message as presage would have stored it on receipt: a data message if incoming, a
    /// "sent" sync message if we wrote it. Other chat items (calls, group updates) are skipped.
    fn message(&self, item: &proto::ChatItem) -> Option<(Thread, Content, Option<GroupContextV2>)> {
        use proto::chat_item::Item;
        let empty = proto::StandardMessage::default();
        let (message, sticker, view_once) = match &item.item {
            Some(Item::StandardMessage(m)) => (m, None, None),
            Some(Item::StickerMessage(s)) => (&empty, Some(&s.sticker), None),
            Some(Item::ViewOnceMessage(v)) => (&empty, None, Some(v)),
            _ => return None,
        };
        // gone on the phone already (our sweep counts from the send time too)
        if item.expiresInMs.is_some_and(|ms| ms > 0 && item.dateSent.saturating_add(ms) <= self.now) {
            return None;
        }
        let body = message.text.body.clone();
        // attachments no longer on the transit CDN can't be fetched: left out
        let mut attachments: Vec<_> =
            message.attachments.iter().filter_map(|a| attachment_pointer(&a.pointer, a.flag.enum_value_or_default())).collect();
        // the body holds the beginning of a long message, this its full text
        attachments.extend(message.longText.as_ref().and_then(|f| attachment_pointer(f, Default::default())));
        // a viewed view-once message has no attachment left
        attachments.extend(
            view_once.and_then(|v| v.attachment.as_ref()).and_then(|a| attachment_pointer(&a.pointer, a.flag.enum_value_or_default())),
        );
        let sticker = sticker.map(|s| Sticker {
            pack_id: Some(s.packId.clone()),
            pack_key: Some(s.packKey.clone()),
            sticker_id: Some(s.stickerId),
            emoji: s.emoji.clone(),
            data: attachment_pointer(&s.data, Default::default()),
        });
        if body.is_empty() && attachments.is_empty() && sticker.as_ref().is_none_or(|s| s.data.is_none()) {
            return None;
        }
        let (thread, group_v2) = match *self.recipients.get(self.chats.get(&item.chatId)?)? {
            Party::Me => (Thread::Contact(ServiceId::Aci(self.me)), None),
            Party::Contact(aci) => (Thread::Contact(ServiceId::Aci(aci)), None),
            Party::Group { master_key, revision } => (
                Thread::Group(master_key),
                Some(GroupContextV2 { master_key: Some(master_key.to_vec()), revision: Some(revision), ..Default::default() }),
            ),
        };
        let author = self.aci_of(item.authorId)?;
        let quote = message.quote.as_ref().and_then(|q| {
            Some(Quote {
                id: q.targetSentTimestamp,
                author_aci: Some(ServiceId::Aci(self.aci_of(q.authorId)?).service_id_string()),
                text: q.text.as_ref().map(|t| t.body.clone()),
                body_ranges: q.text.as_ref().map(|t| body_ranges(&t.bodyRanges)).unwrap_or_default(),
                ..Default::default()
            })
        });
        let data = DataMessage {
            body: (!body.is_empty()).then_some(body),
            body_ranges: body_ranges(&message.text.bodyRanges),
            timestamp: Some(item.dateSent),
            group_v2: group_v2.clone(),
            quote,
            attachments,
            sticker,
            is_view_once: view_once.map(|_| true),
            expire_timer: item.expiresInMs.map(|ms| (ms / 1000) as u32),
            ..Default::default()
        };
        use proto::chat_item::DirectionalDetails;
        let (outgoing, server_ts, sealed) = match &item.directionalDetails {
            Some(DirectionalDetails::Outgoing(o)) => (true, o.dateReceived, false),
            Some(DirectionalDetails::Incoming(i)) => (false, i.dateServerSent.unwrap_or(i.dateReceived), i.sealedSender),
            _ => return None,
        };
        let content = self.content(&thread, author, data, item.dateSent, server_ts, sealed, outgoing);
        Some((thread, content, group_v2))
    }

    /// Each reaction on a chat item as the reaction message its author sent, the way presage
    /// stores live ones.
    fn reactions(&self, item: &proto::ChatItem, thread: &Thread, group_v2: Option<GroupContextV2>) -> Vec<Record> {
        use proto::chat_item::Item;
        let reactions = match &item.item {
            Some(Item::StandardMessage(m)) => &m.reactions,
            Some(Item::StickerMessage(s)) => &s.reactions,
            Some(Item::ViewOnceMessage(v)) => &v.reactions,
            _ => return Vec::new(),
        };
        let Some(target) = self.aci_of(item.authorId) else { return Vec::new() };
        reactions
            .iter()
            .filter_map(|r| {
                let author = self.aci_of(r.authorId)?;
                let data = DataMessage {
                    reaction: Some(Reaction {
                        emoji: Some(r.emoji.clone()),
                        remove: Some(false),
                        target_author_aci: Some(ServiceId::Aci(target).service_id_string()),
                        target_sent_timestamp: Some(item.dateSent),
                        ..Default::default()
                    }),
                    timestamp: Some(r.sentTimestamp),
                    group_v2: group_v2.clone(),
                    ..Default::default()
                };
                let sent = r.sentTimestamp;
                let content = self.content(thread, author, data, sent, sent, false, author == self.me);
                Some(Record::Message { thread: thread.clone(), content })
            })
            .collect()
    }

    fn aci_of(&self, recipient: u64) -> Option<Aci> {
        match self.recipients.get(&recipient)? {
            Party::Me => Some(self.me),
            Party::Contact(aci) => Some(*aci),
            Party::Group { .. } => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn content(&self, thread: &Thread, author: Aci, data: DataMessage, sent: u64, server_ts: u64, sealed: bool, outgoing: bool) -> Content {
        let body = if outgoing {
            let destination = match thread {
                Thread::Contact(id) => Some(id.service_id_string()),
                Thread::Group(_) => None,
            };
            let sent = Sent { destination_service_id: destination, timestamp: Some(sent), message: Some(data), ..Default::default() };
            ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Sent(sent)), ..Default::default() })
        } else {
            ContentBody::DataMessage(data)
        };
        let metadata = Metadata {
            sender: ServiceId::Aci(author),
            destination: ServiceId::Aci(self.me),
            sender_device: *DEFAULT_DEVICE_ID,
            pni_verified: None,
            client_timestamp: datetime(sent),
            server_timestamp: datetime(server_ts),
            needs_receipt: false,
            unidentified_sender: sealed,
            was_plaintext: false,
            server_guid: None,
        };
        Content { metadata, body }
    }
}

/// Mentions and styles (spoilers among them), numbered alike in both formats.
fn body_ranges(ranges: &[proto::BodyRange]) -> Vec<presage::proto::BodyRange> {
    use presage::proto::body_range::AssociatedValue as Value;
    use proto::body_range::AssociatedValue;
    ranges
        .iter()
        .filter_map(|r| {
            let value = match r.associatedValue.as_ref()? {
                AssociatedValue::MentionAci(aci) => {
                    Value::MentionAci(ServiceId::Aci(Aci::from_uuid_bytes(aci.as_slice().try_into().ok()?)).service_id_string())
                }
                AssociatedValue::Style(style) => Value::Style(style.value()),
                _ => return None,
            };
            Some(presage::proto::BodyRange { start: Some(r.start), length: Some(r.length), associated_value: Some(value) })
        })
        .collect()
}

/// Where to download an attachment from the transit CDN, as a live message carries it. `None`
/// if it isn't there (any more).
fn attachment_pointer(file: &proto::FilePointer, flag: proto::message_attachment::Flag) -> Option<AttachmentPointer> {
    use proto::file_pointer::locator_info::IntegrityCheck;
    use proto::message_attachment::Flag;
    let locator = file.locatorInfo.as_ref()?;
    let (Some(cdn_key), Some(cdn_number)) = (locator.transitCdnKey.clone(), locator.transitCdnNumber) else { return None };
    // Media the phone has downloaded carries a hash of the plaintext instead, which the pointer
    // has no field for; the attachment's own MAC still authenticates it.
    let digest = match &locator.integrityCheck {
        Some(IntegrityCheck::EncryptedDigest(d)) => Some(d.clone()),
        _ => None,
    };
    let flags = match flag {
        Flag::VOICE_MESSAGE => Some(Flags::VoiceMessage as u32),
        Flag::BORDERLESS => Some(Flags::Borderless as u32),
        Flag::GIF => Some(Flags::Gif as u32),
        Flag::NONE => None,
    };
    Some(AttachmentPointer {
        attachment_identifier: Some(AttachmentIdentifier::CdnKey(cdn_key)),
        cdn_number: Some(cdn_number),
        key: Some(locator.key.clone()),
        size: Some(locator.size),
        digest,
        content_type: file.contentType.clone(),
        file_name: file.fileName.clone(),
        width: file.width,
        height: file.height,
        caption: file.caption.clone(),
        blur_hash: file.blurHash.clone(),
        upload_timestamp: locator.transitTierUploadTimestamp,
        incremental_mac: file.incrementalMac.clone(),
        chunk_size: file.incrementalMacChunkSize,
        flags,
        ..Default::default()
    })
}

/// Contacts without an ACI can't be messaged or stored by presage: dropped.
fn convert_contact(contact: &proto::Contact) -> Option<(Contact, Option<ProfileKey>)> {
    let uuid = Uuid::from_slice(contact.aci.as_deref()?).ok()?;
    let profile_key: Option<[u8; 32]> = contact.profileKey.as_deref().and_then(|k| k.try_into().ok());
    let phone_number = contact.e164.and_then(|n| phonenumber::parse(None, format!("+{n}")).ok());
    let contact = Contact {
        uuid,
        phone_number,
        name: display_name(contact),
        verified: Default::default(),
        profile_key: profile_key.map(Vec::from).unwrap_or_default(),
        expire_timer: 0,
        expire_timer_version: 2,
        inbox_position: 0,
        avatar: None,
    };
    Some((contact, profile_key.map(ProfileKey::create)))
}

/// Signal's order: the nickname you gave them, their name in your address book, then their
/// profile name. Empty if none is set.
fn display_name(contact: &proto::Contact) -> String {
    let join = |given: &str, family: &str| format!("{given} {family}").trim().to_owned();
    let nickname = contact.nickname.as_ref().map(|n| join(&n.given, &n.family)).unwrap_or_default();
    let system = join(&contact.systemGivenName, &contact.systemFamilyName);
    let profile = join(contact.profileGivenName.as_deref().unwrap_or_default(), contact.profileFamilyName.as_deref().unwrap_or_default());
    [nickname, system, profile].into_iter().find(|n| !n.is_empty()).unwrap_or_default()
}

fn datetime(ms: u64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms as i64).unwrap_or_default()
}

/// Messages written per transaction (see `Index::save_messages`).
const BATCH: usize = 1000;

/// Stores records; messages wait in `batch` until it is full, then [`flush`] writes them.
/// Messages are keyed by thread and timestamp, so importing twice is harmless.
async fn store_records(
    store: &mut SqliteStore,
    index: &Index,
    records: Vec<Record>,
    batch: &mut Vec<(Thread, Content)>,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    for record in records {
        match record {
            Record::Message { thread, content } => batch.push((thread, content)),
            other => store_other(store, other).await?,
        }
    }
    if batch.len() >= BATCH {
        flush(index, batch, stats).await?;
    }
    Ok(())
}

async fn flush(index: &Index, batch: &mut Vec<(Thread, Content)>, stats: &mut Stats) -> anyhow::Result<()> {
    if !batch.is_empty() {
        stats.skipped += index.save_messages(std::mem::take(batch)).await.context("save messages")?;
    }
    Ok(())
}

async fn store_other(store: &mut SqliteStore, record: Record) -> anyhow::Result<()> {
    match record {
        Record::Contact { contact, profile_key } => {
            store.save_contact(&contact).await.context("save contact")?;
            if let Some(key) = profile_key {
                store.upsert_profile_key(&contact.uuid, key).await.context("save profile key")?;
            }
        }
        Record::Group { master_key, group } => store.save_group(master_key, group).await.context("save group")?,
        Record::Message { .. } => unreachable!("messages are batched"),
        Record::Timer { contact, seconds, version } => {
            if let Some(mut c) = store.contact_by_id(&ServiceId::Aci(contact)).await.context("read contact")? {
                c.expire_timer = seconds;
                c.expire_timer_version = version;
                store.save_contact(&c).await.context("save contact")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
