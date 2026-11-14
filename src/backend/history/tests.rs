use super::*;
use crate::db::tests::Db;
use presage::store::ContentExt as _;
use proto::chat_item::DirectionalDetails;
use proto::recipient::Destination;
use protobuf::MessageField;

const ME: [u8; 16] = [1; 16];
const ALICE: [u8; 16] = [2; 16];
const GROUP_KEY: [u8; 32] = [7; 32];
const ALICE_KEY: [u8; 32] = [9; 32];
const NOW: u64 = 1_000_000;

fn frame(item: proto::frame::Item) -> Vec<u8> {
    proto::Frame { item: Some(item), ..Default::default() }.write_to_bytes().unwrap()
}

fn recipient(id: u64, destination: Destination) -> Vec<u8> {
    frame(proto::frame::Item::Recipient(proto::Recipient { id, destination: Some(destination), ..Default::default() }))
}

fn chat(id: u64, recipient_id: u64) -> Vec<u8> {
    frame(proto::frame::Item::Chat(proto::Chat { id, recipientId: recipient_id, ..Default::default() }))
}

fn item(chat_id: u64, author_id: u64, sent: u64, outgoing: bool, message: proto::StandardMessage) -> proto::ChatItem {
    let direction = if outgoing {
        DirectionalDetails::Outgoing(Default::default())
    } else {
        DirectionalDetails::Incoming(proto::chat_item::IncomingMessageDetails { dateReceived: sent + 5, ..Default::default() })
    };
    proto::ChatItem {
        chatId: chat_id,
        authorId: author_id,
        dateSent: sent,
        directionalDetails: Some(direction),
        item: Some(proto::chat_item::Item::StandardMessage(message)),
        ..Default::default()
    }
}

fn standard(body: &str) -> proto::StandardMessage {
    proto::StandardMessage { text: MessageField::some(proto::Text { body: body.into(), ..Default::default() }), ..Default::default() }
}

fn text(chat_id: u64, author_id: u64, sent: u64, outgoing: bool, body: &str) -> Vec<u8> {
    frame(proto::frame::Item::ChatItem(item(chat_id, author_id, sent, outgoing, standard(body))))
}

fn alice_id() -> ServiceId {
    ServiceId::Aci(Aci::from_uuid_bytes(ALICE))
}

/// Recipients 1 (me), 2 (Alice) and 3 (a group, listed before Alice); chats 10 (Alice) and
/// 11 (the group).
fn archive() -> Vec<Vec<u8>> {
    let alice = proto::Contact {
        aci: Some(ALICE.to_vec()),
        e164: Some(4915112345678),
        profileKey: Some(ALICE_KEY.to_vec()),
        profileGivenName: Some("Ali".into()),
        systemGivenName: "Alice".into(),
        systemFamilyName: "Liddell".into(),
        ..Default::default()
    };
    let title = proto::group::GroupAttributeBlob {
        content: Some(proto::group::group_attribute_blob::Content::Title("Climbing".into())),
        ..Default::default()
    };
    let member = |id: [u8; 16]| proto::group::Member { userId: id.to_vec(), ..Default::default() };
    let group = proto::Group {
        masterKey: GROUP_KEY.to_vec(),
        snapshot: MessageField::some(proto::group::GroupSnapshot {
            title: MessageField::some(title),
            version: 5,
            members: vec![member(ME), member(ALICE)],
            ..Default::default()
        }),
        ..Default::default()
    };
    let update = frame(proto::frame::Item::ChatItem(proto::ChatItem {
        chatId: 11,
        authorId: 2,
        dateSent: 4000,
        directionalDetails: Some(DirectionalDetails::Directionless(Default::default())),
        item: Some(proto::chat_item::Item::UpdateMessage(Default::default())),
        ..Default::default()
    }));
    vec![
        recipient(1, Destination::Self_(Default::default())),
        recipient(3, Destination::Group(group)),
        recipient(2, Destination::Contact(alice)),
        chat(10, 2),
        chat(11, 3),
        text(10, 2, 1000, false, "Hi!"),
        text(10, 1, 2000, true, "Hello, Alice"),
        text(11, 2, 3000, false, "Who is in on Saturday?"),
        update,
    ]
}

fn convert(frames: &[Vec<u8>]) -> (Vec<Record>, Stats) {
    let mut importer = Importer::new(Aci::from_uuid_bytes(ME), NOW);
    let mut records = Vec::new();
    for f in frames {
        records.extend(importer.frame(f).unwrap());
    }
    records.extend(importer.finish());
    (records, importer.stats)
}

fn messages(records: &[Record]) -> Vec<(&Thread, &Content)> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Message { thread, content } => Some((thread, content)),
            _ => None,
        })
        .collect()
}

fn sent(c: &Content) -> &Sent {
    let ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Sent(sent)), .. }) = &c.body else {
        panic!("not a sent sync message")
    };
    sent
}

#[test]
fn converts_contacts_groups_and_messages() {
    let (records, stats) = convert(&archive());
    assert_eq!(stats, Stats { contacts: 1, groups: 1, messages: 3, reactions: 0, skipped: 1 });
    let Some(Record::Contact { contact, profile_key }) = records.iter().find(|r| matches!(r, Record::Contact { .. })) else {
        panic!("no contact")
    };
    // the address book name wins over the profile name
    assert_eq!(contact.name, "Alice Liddell");
    assert_eq!(contact.phone_number.as_ref().unwrap().to_string(), "+4915112345678");
    assert!(profile_key.is_some());
    // held back until Alice's profile key was known
    let Some(Record::Group { master_key, group }) = records.iter().find(|r| matches!(r, Record::Group { .. })) else {
        panic!("no group")
    };
    assert_eq!(*master_key, GROUP_KEY);
    assert_eq!(group.title, "Climbing");
    assert_eq!(group.revision, 5);
    let alice = group.members.iter().find(|m| m.aci == Aci::from_uuid_bytes(ALICE)).unwrap();
    assert_eq!(alice.profile_key.get_bytes(), ALICE_KEY);
}

#[test]
fn messages_look_like_received_ones() {
    let (records, _) = convert(&archive());
    let messages = messages(&records);
    let alice = Thread::Contact(alice_id());

    let (thread, incoming) = messages[0];
    assert_eq!(*thread, alice);
    assert_eq!(incoming.metadata.sender, alice_id());
    assert_eq!(incoming.timestamp(), 1000);
    assert!(matches!(&incoming.body, ContentBody::DataMessage(DataMessage { body: Some(b), .. }) if b == "Hi!"));

    // what we wrote comes back as a "sent" sync message to Alice
    let (thread, outgoing) = messages[1];
    assert_eq!(*thread, alice);
    assert_eq!(outgoing.timestamp(), 2000);
    let sent = sent(outgoing);
    assert_eq!(sent.destination_service_id, Some(alice_id().service_id_string()));
    assert_eq!(sent.message.as_ref().unwrap().body.as_deref(), Some("Hello, Alice"));

    let (thread, in_group) = messages[2];
    assert_eq!(*thread, Thread::Group(GROUP_KEY));
    let ContentBody::DataMessage(data) = &in_group.body else { panic!("not a data message") };
    assert_eq!(data.group_v2.as_ref().unwrap().master_key.as_deref(), Some(&GROUP_KEY[..]));
}

#[test]
fn drops_expired_and_keeps_reactions_quotes_and_transit_attachments() {
    let mut frames = archive();
    let mut gone = item(10, 2, 5000, false, standard("gone"));
    gone.expiresInMs = Some(NOW - 5000);
    frames.push(frame(proto::frame::Item::ChatItem(gone)));
    let mut reply = standard("Hi yourself");
    reply.quote = MessageField::some(proto::Quote {
        targetSentTimestamp: Some(1000),
        authorId: 2,
        text: MessageField::some(proto::Text { body: "Hi!".into(), ..Default::default() }),
        ..Default::default()
    });
    reply.reactions = vec![proto::Reaction { emoji: "+1".into(), authorId: 2, sentTimestamp: 6100, ..Default::default() }];
    let file = |transit: bool| proto::MessageAttachment {
        pointer: MessageField::some(proto::FilePointer {
            contentType: Some("image/jpeg".into()),
            locatorInfo: MessageField::some(proto::file_pointer::LocatorInfo {
                key: vec![3; 64],
                size: 10,
                transitCdnKey: transit.then(|| "abc".into()),
                transitCdnNumber: transit.then_some(3),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    reply.attachments = vec![file(true), file(false)];
    frames.push(frame(proto::frame::Item::ChatItem(item(10, 1, 6000, true, reply))));

    let (records, stats) = convert(&frames);
    assert_eq!((stats.messages, stats.reactions, stats.skipped), (4, 1, 2));
    let messages = messages(&records);
    let data = sent(messages[3].1).message.as_ref().unwrap();
    assert_eq!(data.quote.as_ref().unwrap().id, Some(1000));
    // the one no longer on the transit CDN is left out
    assert_eq!(data.attachments.len(), 1);
    assert!(data.attachments[0].digest.is_none());
    let (_, reaction) = messages[4];
    assert_eq!(reaction.metadata.sender, alice_id());
    let ContentBody::DataMessage(DataMessage { reaction: Some(r), .. }) = &reaction.body else { panic!("not a reaction") };
    assert_eq!(r.target_sent_timestamp, Some(6000));
}

#[tokio::test(flavor = "current_thread")]
async fn stores_what_it_converts_and_keeps_the_first_of_a_collision() {
    let mut db = Db::new().await;
    let mut frames = archive();
    // Alice wrote in the same millisecond as I did, in the same thread
    frames.push(text(10, 2, 2000, false, "same time"));
    let index = db.index().await;
    let (records, mut stats) = convert(&frames);
    let mut batch = Vec::new();
    store_records(&mut db.store, &index, records, &mut batch, &mut stats).await.unwrap();
    flush(&index, &mut batch, &mut stats).await.unwrap();
    assert_eq!(stats.skipped, 2);
    let alice = Thread::Contact(alice_id());
    let mut stored: Vec<Content> = db.store.messages(&alice, ..).await.unwrap().flatten().collect();
    stored.sort_by_key(|c| c.timestamp());
    assert_eq!(stored.iter().map(|c| c.timestamp()).collect::<Vec<_>>(), [1000, 2000]);
    assert_eq!(stored[1].metadata.sender, ServiceId::Aci(Aci::from_uuid_bytes(ME)));
    assert_eq!(db.store.group(GROUP_KEY).await.unwrap().unwrap().title, "Climbing");
    assert_eq!(db.store.contact_by_id(&alice_id()).await.unwrap().unwrap().name, "Alice Liddell");
}

#[tokio::test(flavor = "current_thread")]
async fn shows_the_history_in_sent_order_whatever_the_archive_order() {
    let mut db = Db::new().await;
    let index = db.index().await;
    let mut frames = archive();
    frames.push(text(10, 2, 500, false, "older, but last in the archive"));
    let (records, mut stats) = convert(&frames);
    let mut batch = Vec::new();
    store_records(&mut db.store, &index, records, &mut batch, &mut stats).await.unwrap();
    flush(&index, &mut batch, &mut stats).await.unwrap();
    index.order_by_sent().await.unwrap();
    let shown: Vec<u64> = index.page(&Thread::Contact(alice_id()), i64::MAX, 10).await.unwrap().into_iter().map(|(_, ts)| ts).collect();
    // newest first
    assert_eq!(shown, [2000, 1000, 500]);
}

/// Import speed: `cargo test --profile fast history::tests::import_speed -- --ignored --nocapture`
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn import_speed() {
    let mut db = Db::new().await;
    let index = db.index().await; // its `ls_order` triggers run on every save, as in the app
    let mut frames = archive();
    const N: u64 = 3000;
    for i in 0..N {
        frames.push(text(10 + i % 2, if i % 3 == 0 { 1 } else { 2 }, 10_000 + i, i % 3 == 0, "a message of ordinary length, say"));
    }
    let started = Instant::now();
    let (records, mut stats) = convert(&frames);
    let converted = started.elapsed();
    let mut batch = Vec::new();
    store_records(&mut db.store, &index, records, &mut batch, &mut stats).await.unwrap();
    flush(&index, &mut batch, &mut stats).await.unwrap();
    let total = started.elapsed();
    eprintln!("{N} messages: convert {converted:?}, total {total:?}, {:?} per message", total / N as u32);
}

/// libsignal's `legacy-account` test case: the same format as a link-time archive (no forward
/// secrecy token), with a known key. It holds account data and our own recipient.
const LEGACY: &[u8] = include_bytes!("../testdata/history-legacy-account.bin");

async fn import_legacy(aci: [u8; 16]) -> anyhow::Result<Stats> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("sigwin-history-{}-{n}", std::process::id()));
    std::fs::write(&path, LEGACY).unwrap();
    let _cleanup = RemoveOnDrop(&path);
    let mut db = Db::new().await;
    let index = db.index().await;
    let pool = "m".repeat(64).parse().unwrap();
    let key = BackupKey::derive_from_account_entropy_pool(&pool).0;
    import(key, &Aci::from_uuid_bytes(aci), &path, &mut db.store, &index, &|_| {}).await
}

#[tokio::test(flavor = "current_thread")]
async fn decrypts_an_archive_for_its_account_only() {
    // only the account frame, which isn't imported
    assert_eq!(import_legacy([0x11; 16]).await.unwrap(), Stats { skipped: 1, ..Stats::default() });
    assert!(import_legacy([0x22; 16]).await.is_err());
}

#[test]
fn reads_every_server_answer() {
    let parse = |json: &str| Archive::try_from(serde_json::from_str::<ArchiveResponse>(json).unwrap());
    assert_eq!(parse(r#"{"cdn":3,"key":"abc-_123"}"#).unwrap(), Archive::Uploaded { cdn: 3, key: "abc-_123".into() });
    assert_eq!(parse(r#"{"error":"RELINK_REQUESTED"}"#).unwrap(), Archive::RelinkRequested);
    assert_eq!(parse(r#"{"error":"CONTINUE_WITHOUT_UPLOAD"}"#).unwrap(), Archive::ContinueWithoutUpload);
    assert!(parse(r#"{"error":"SOMETHING_NEW"}"#).is_err());
}
