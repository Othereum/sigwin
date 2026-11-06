//! Tasks using one session at the same time must not lose each other's updates. Runs the vendored
//! `ServiceCipher` on the real store, one thread, as the backend does; the peer is libsignal's
//! in-memory store. Synthetic keys, no network. Fails if `cipher::SESSION_LOCK` is missed on an
//! upgrade (see vendor/PATCHES.md): without it, the first test fails every round and the second
//! in about 40% of rounds.

use base64::Engine;
use presage::libsignal_service::cipher::ServiceCipher;
use presage::libsignal_service::utils::BASE64_RELAXED;
use presage::libsignal_service::prelude::ProtobufMessage;
use presage::libsignal_service::proto::{self, envelope::Type, Envelope};
use presage::libsignal_service::protocol::*;
use presage::model::identity::OnNewIdentity;
use presage::store::{StateStore, Store};
use presage_store_sqlite::SqliteStore;
use rand::Rng;
use sqlx::sqlite::SqliteConnectOptions;
use std::path::PathBuf;
use std::time::SystemTime;
use uuid::Uuid;

type Proto = <SqliteStore as Store>::AciStore;

struct Db {
    store: SqliteStore,
    identity: IdentityKeyPair,
    dir: PathBuf,
}

impl Db {
    async fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sigwin-{name}-{}", std::process::id()));
        // an earlier run with the same process id may have left its DB here
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let options = SqliteConnectOptions::new().filename(dir.join("db.sqlite")).create_if_missing(true);
        let store = SqliteStore::open_with_options(options, OnNewIdentity::Trust).await.unwrap();
        let identity = IdentityKeyPair::generate(&mut rand::rng());
        store.set_aci_identity_key_pair(identity).await.unwrap();
        Self { store, identity, dir }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Us and one peer, with a session both have already used.
struct Chat {
    cipher: ServiceCipher<Proto>,
    me: ProtocolAddress,
    them: ProtocolAddress,
    peer: InMemSignalProtocolStore,
}

fn content(text: &str) -> Vec<u8> {
    let data = proto::DataMessage { body: Some(text.into()), ..Default::default() };
    proto::Content { content: Some(proto::content::Content::DataMessage(data)), ..Default::default() }.encode_to_vec()
}

/// What clients add before encrypting (`cipher::add_padding`, private).
fn padded(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.push(0x80);
    bytes.resize(bytes.len().div_ceil(160) * 160, 0);
    bytes
}

fn parse(sent: &presage::libsignal_service::sender::OutgoingPushMessage) -> CiphertextMessage {
    let bytes = BASE64_RELAXED.decode(&sent.content).unwrap();
    if sent.r#type == Type::PrekeyMessage as u32 {
        CiphertextMessage::PreKeySignalMessage(PreKeySignalMessage::try_from(&bytes[..]).unwrap())
    } else {
        CiphertextMessage::SignalMessage(SignalMessage::try_from(&bytes[..]).unwrap())
    }
}

/// A short random head start, so both orders of the overlapping tasks get exercised.
async fn stagger() {
    for _ in 0..rand::rng().random_range(0..12) {
        tokio::task::yield_now().await;
    }
}

impl Chat {
    async fn new(db: &Db, n: u128) -> Self {
        let mut rng = rand::rng();
        let device = DeviceId::new(1).unwrap();
        let me = ProtocolAddress::new(Uuid::from_u128(1).to_string(), device);
        let them = ProtocolAddress::new(Uuid::from_u128(n + 2).to_string(), device);

        let identity = IdentityKeyPair::generate(&mut rng);
        let mut peer = InMemSignalProtocolStore::new(identity, 2).unwrap();
        let signed = KeyPair::generate(&mut rng);
        let signed_sig = identity.private_key().calculate_signature(&signed.public_key.serialize(), &mut rng).unwrap();
        let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rng);
        let kyber_sig = identity.private_key().calculate_signature(&kyber.public_key.serialize(), &mut rng).unwrap();
        let bundle = PreKeyBundle::new(
            2,
            device,
            None,
            1u32.into(),
            signed.public_key,
            signed_sig.to_vec(),
            1u32.into(),
            kyber.public_key.clone(),
            kyber_sig.to_vec(),
            *identity.identity_key(),
        )
        .unwrap();
        let at = Timestamp::from_epoch_millis(42);
        peer.save_signed_pre_key(1u32.into(), &SignedPreKeyRecord::new(1u32.into(), at, &signed, &signed_sig)).await.unwrap();
        peer.save_kyber_pre_key(1u32.into(), &KyberPreKeyRecord::new(1u32.into(), at, &kyber, &kyber_sig)).await.unwrap();

        // the real store needs registration data to build a session: build it aside, then copy it
        let mut setup = InMemSignalProtocolStore::new(db.identity, 1).unwrap();
        process_prekey_bundle(&them, &me, &mut setup.session_store, &mut setup.identity_store, &bundle, SystemTime::now(), &mut rng)
            .await
            .unwrap();
        let record = setup.load_session(&them).await.unwrap().unwrap();
        let mut store = db.store.aci_protocol_store();
        store.store_session(&them, &record).await.unwrap();

        let cipher = ServiceCipher::new(store, vec![], me.clone());
        let mut chat = Self { cipher, me, them, peer };

        let first = chat.send("hello").await;
        chat.peer_reads(&first).await.unwrap();
        let reply = chat.peer_writes("hi").await;
        chat.cipher.clone().open_envelope(reply, &mut rand::rng()).await.unwrap().unwrap();
        let second = chat.send("how are you").await;
        chat.peer_reads(&second).await.unwrap();
        chat
    }

    async fn send(&self, text: &str) -> CiphertextMessage {
        let sent = self.cipher.clone().encrypt(&self.them, None, &content(text), &mut rand::rng()).await.unwrap();
        parse(&sent)
    }

    async fn peer_reads(&mut self, msg: &CiphertextMessage) -> Result<Vec<u8>, SignalProtocolError> {
        let p = &mut self.peer;
        message_decrypt(
            msg,
            &self.me,
            &self.them,
            &mut p.session_store,
            &mut p.identity_store,
            &mut p.pre_key_store,
            &p.signed_pre_key_store,
            &mut p.kyber_pre_key_store,
            &mut rand::rng(),
        )
        .await
    }

    async fn peer_writes(&mut self, text: &str) -> Envelope {
        let p = &mut self.peer;
        let plain = padded(content(text));
        let msg = message_encrypt(&plain, &self.me, &self.them, &mut p.session_store, &mut p.identity_store, SystemTime::now(), &mut rand::rng())
            .await
            .unwrap();
        Envelope {
            r#type: Some(Type::DoubleRatchet as i32),
            source_service_id: Some(self.them.name().to_string()),
            source_device_id: Some(1),
            destination_service_id: Some(self.me.name().to_string()),
            client_timestamp: Some(1_000),
            server_timestamp: Some(1_000),
            content: Some(msg.serialize().to_vec()),
            ..Default::default()
        }
    }
}

fn run(test: impl AsyncFnOnce()) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, test());
}

#[test]
fn two_sends_at_once_stay_readable() {
    run(async || {
        let db = Db::new("session-lock-send").await;
        for n in 0..20 {
            let mut chat = Chat::new(&db, n).await;
            let (a, b) = (chat.cipher.clone(), chat.cipher.clone());
            let (them_a, them_b) = (chat.them.clone(), chat.them.clone());
            let one = tokio::task::spawn_local(async move {
                let mut a = a;
                stagger().await;
                a.encrypt(&them_a, None, &content("one"), &mut rand::rng()).await.unwrap()
            });
            let two = tokio::task::spawn_local(async move {
                let mut b = b;
                stagger().await;
                b.encrypt(&them_b, None, &content("two"), &mut rand::rng()).await.unwrap()
            });
            let (one, two) = (parse(&one.await.unwrap()), parse(&two.await.unwrap()));
            chat.peer_reads(&one).await.unwrap();
            chat.peer_reads(&two).await.expect("second send reused the first one's message key");
        }
    });
}

#[test]
fn a_receive_during_a_send_keeps_the_next_send_readable() {
    run(async || {
        let db = Db::new("session-lock-recv").await;
        for n in 0..40 {
            let mut chat = Chat::new(&db, n).await;
            let incoming = chat.peer_writes("incoming").await;
            let (mut recv, mut send) = (chat.cipher.clone(), chat.cipher.clone());
            let them = chat.them.clone();
            let received = tokio::task::spawn_local(async move {
                stagger().await;
                recv.open_envelope(incoming, &mut rand::rng()).await.unwrap().unwrap();
            });
            let sent = tokio::task::spawn_local(async move {
                stagger().await;
                send.encrypt(&them, None, &content("during"), &mut rand::rng()).await.unwrap()
            });
            received.await.unwrap();
            let during = parse(&sent.await.unwrap());
            chat.peer_reads(&during).await.unwrap();

            let next = chat.send("next").await;
            chat.peer_reads(&next).await.expect("the overlap lost a session update");
        }
    });
}
