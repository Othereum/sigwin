//! Small read-only queries on presage's sqlite schema that the `Store` trait doesn't offer:
//! listing threads by recency and paging messages in display order without loading whole threads.

use std::collections::{HashMap, HashSet};

use presage::store::Thread;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::types::SearchKind;

/// Version of our `ls_*` tables (presage's own tables migrate themselves). Kept in our `ls_meta`
/// table rather than `PRAGMA user_version`, which the DB shares with presage.
///
/// `CREATE TABLE IF NOT EXISTS` does nothing on a DB that has the table, and installed apps
/// auto-update and keep their DB, so:
/// - a new table: just add its `CREATE TABLE IF NOT EXISTS` to `Index::open`;
/// - a change to an existing table: leave its `CREATE` as it is, add a `step` and bump
///   `SCHEMA`. Most of ours can be rebuilt (receipts and pins are backfilled from presage's
///   messages, packs/stickers are a cache): prefer drop, recreate and make the backfill run
///   again over moving columns;
/// - `ls_deleted_mine` has no source to rebuild from (which deleted rows were ours): its data
///   must be carried over.
const SCHEMA: i64 = 2;

async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS ls_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID")
        .execute(pool)
        .await?;
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT value FROM ls_meta WHERE key = 'schema'").fetch_optional(pool).await?;
    // no row: a new DB, or one from before versioning; both have exactly the version 1 tables
    let mut v = stored.unwrap_or(1);
    if v > SCHEMA {
        // a newer build ran on this DB (the installer refuses downgrades, so rare)
        tracing::warn!("db: schema {v} is newer than this build's {SCHEMA}");
        return Ok(());
    }
    while v < SCHEMA {
        v = step(pool, v).await?;
    }
    if stored != Some(v) {
        sqlx::query("INSERT INTO ls_meta (key, value) VALUES ('schema', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
            .bind(v)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// One migration step up from `v`; returns the new version. Run in order, each exactly once.
async fn step(pool: &SqlitePool, v: i64) -> anyhow::Result<i64> {
    match v {
        // packs get their author; the picker reads each pack from presage again once
        1 => {
            sqlx::query("DROP TABLE ls_packs").execute(pool).await?;
            sqlx::query(
                "CREATE TABLE ls_packs (
                    id BLOB PRIMARY KEY,
                    title TEXT NOT NULL,
                    author TEXT NOT NULL,
                    cover INTEGER
                ) WITHOUT ROWID",
            )
            .execute(pool)
            .await?;
            Ok(2)
        }
        _ => anyhow::bail!("db: no migration from schema {v}"),
    }
}

/// What a chat's media and files list an attachment under (`ls_media.kind`; stored, so the values stay).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// pictures and videos
    Media = 0,
    Files = 1,
}

/// What `Index::search` looks for.
pub struct Searched<'a> {
    /// in this thread only
    pub thread: Option<&'a Thread>,
    /// what a text must hold, then pass `keep`; None: any text, and attachments
    pub needle: Option<&'a str>,
    /// given a text and whom it @mentions (ACIs)
    pub keep: &'a dyn Fn(&str, &[uuid::Uuid]) -> bool,
    /// as stored: `ServiceId::service_id_string`
    pub sender: Option<&'a str>,
    /// sent before (ms)
    pub until: Option<u64>,
    pub kind: Option<SearchKind>,
    /// only those that @mention them (ACI)
    pub mentions: Option<uuid::Uuid>,
    /// whose mention shows the needle (ACIs): a text holds a mention as a placeholder, not
    /// the name, so what mentions them is looked through too
    pub named: &'a [uuid::Uuid],
}

impl Searched<'_> {
    /// Asks for what only a text holds, so a message without text can't pass.
    fn in_text(&self) -> bool {
        self.mentions.is_some() || self.kind == Some(SearchKind::Links)
    }
}

#[derive(Clone)]
pub struct Index {
    pool: SqlitePool,
}

/// A sticker pack made on this PC (`ls_made_packs`).
pub struct MadePack {
    pub id: Vec<u8>,
    pub key: Vec<u8>,
    pub title: String,
    pub author: String,
    /// a small PNG of its cover
    pub cover: Option<Vec<u8>>,
}

/// A pack of stickers others sent us (`ls_sticker_msgs`, `ls_shared_cache`).
pub struct SharedPack {
    pub id: Vec<u8>,
    /// title and author, once its manifest was fetched
    pub names: Option<(String, String)>,
    /// its cover sticker's id
    pub cover: Option<u32>,
}

pub struct ThreadRow {
    pub thread: Thread,
    /// newest sent timestamp (a wrong clock can put it in the future)
    pub last_ts: u64,
    /// sent timestamp of the last message stored, what the thread's bottom row is built from
    pub tail_ts: u64,
}

/// Display order: stored order, not sent timestamps, which come from the senders' clocks.
///
/// Triggers rather than app code, so every save path is covered and a crash can't lose a row.
/// presage's save is an `INSERT OR REPLACE`, which also replaces a message deleted for everyone.
/// That must keep its place, hence two details: the outer `OR REPLACE` overrides any conflict
/// clause inside a trigger (so `WHERE NOT EXISTS`, not `OR IGNORE`), and delete triggers don't
/// fire for replaced rows while `recursive_triggers` is off (the default).
async fn ensure_order(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ls_order (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_id INTEGER NOT NULL,
            ts INTEGER NOT NULL,
            UNIQUE (thread_id, ts)
        )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS ls_order_page ON ls_order (thread_id, seq)").execute(&mut *tx).await?;
    let triggers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'trigger' AND tbl_name = 'thread_messages' AND name IN ('ls_order_add', 'ls_order_del')",
    )
    .fetch_one(&mut *tx)
    .await?;
    if triggers == 2 {
        return Ok(tx.commit().await?);
    }
    // first run, or a presage migration rebuilt the table (which drops its triggers).
    // Existing history keeps the order it always had: by sent timestamp
    sqlx::query(
        "DELETE FROM ls_order WHERE NOT EXISTS
            (SELECT 1 FROM thread_messages m WHERE m.thread_id = ls_order.thread_id AND m.ts = ls_order.ts)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO ls_order (thread_id, ts)
         SELECT m.thread_id, m.ts FROM thread_messages m
         WHERE NOT EXISTS (SELECT 1 FROM ls_order o WHERE o.thread_id = m.thread_id AND o.ts = m.ts)
         ORDER BY m.ts",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS ls_order_add AFTER INSERT ON thread_messages BEGIN
            INSERT INTO ls_order (thread_id, ts)
            SELECT new.thread_id, new.ts
            WHERE NOT EXISTS (SELECT 1 FROM ls_order WHERE thread_id = new.thread_id AND ts = new.ts);
        END",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS ls_order_del AFTER DELETE ON thread_messages BEGIN
            DELETE FROM ls_order WHERE thread_id = old.thread_id AND ts = old.ts;
        END",
    )
    .execute(&mut *tx)
    .await?;
    Ok(tx.commit().await?)
}

/// A list of timestamps as one bind value, for `IN (SELECT value FROM json_each(?))`.
fn json_list(tss: &[u64]) -> String {
    format!("[{}]", tss.iter().map(u64::to_string).collect::<Vec<_>>().join(","))
}

/// The thread a `threads` row stands for; `pni`: its recipient is a PNI (`ls_pni`).
fn thread_of(gk: Option<Vec<u8>>, rid: Option<Vec<u8>>, pni: bool) -> Option<Thread> {
    match (gk, rid) {
        (Some(gk), _) => Some(Thread::Group(gk.try_into().ok()?)),
        (None, Some(rid)) => {
            let u = uuid::Uuid::from_slice(&rid).ok()?;
            crate::types::parse_thread_key(&format!("{}:{u}", if pni { "p" } else { "c" }))
        }
        _ => None,
    }
}

/// The message search's text (`Index::record_text`), and the trigger that drops it with its
/// message, so that no way of deleting one (expiry, a deleted chat) leaves text behind.
/// presage keys the table by (thread_id, ts) too; `ls_order.seq` is renumbered by
/// `order_by_sent`, so it can't be the key. A rowid table: a body can be far bigger than a page.
async fn ensure_text(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ls_text (
            thread_id INTEGER NOT NULL,
            ts INTEGER NOT NULL,
            text TEXT NOT NULL,
            UNIQUE (thread_id, ts)
        )",
    )
    .execute(&mut *tx)
    .await?;
    // the search reads newest first and stops once it has enough
    sqlx::query("CREATE INDEX IF NOT EXISTS ls_text_ts ON ls_text (ts)").execute(&mut *tx).await?;
    // a search by sender reads only theirs, newest first. On presage's table: here, on every
    // open, since a presage migration that rebuilds the table drops it
    sqlx::query("CREATE INDEX IF NOT EXISTS ls_by_sender ON thread_messages (sender_service_id, ts)").execute(&mut *tx).await?;
    // who each indexed text @mentions (ACI, 16 bytes), read by person, newest first: a text
    // holds a mention as a placeholder, so searching for a name can't find it there
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ls_mentions (
            aci BLOB NOT NULL,
            ts INTEGER NOT NULL,
            thread_id INTEGER NOT NULL,
            PRIMARY KEY (aci, ts, thread_id)
        ) WITHOUT ROWID",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS ls_mentions_msg ON ls_mentions (thread_id, ts)").execute(&mut *tx).await?;
    let triggers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'thread_messages'
            AND name IN ('ls_text_del', 'ls_mentions_del')",
    )
    .fetch_one(&mut *tx)
    .await?;
    if triggers == 2 {
        return Ok(tx.commit().await?);
    }
    // first run, or a presage migration rebuilt the table and dropped the triggers
    sqlx::query(
        "DELETE FROM ls_text WHERE NOT EXISTS
            (SELECT 1 FROM thread_messages m WHERE m.thread_id = ls_text.thread_id AND m.ts = ls_text.ts)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM ls_mentions WHERE NOT EXISTS
            (SELECT 1 FROM thread_messages m WHERE m.thread_id = ls_mentions.thread_id AND m.ts = ls_mentions.ts)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS ls_text_del AFTER DELETE ON thread_messages BEGIN
            DELETE FROM ls_text WHERE thread_id = old.thread_id AND ts = old.ts;
        END",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS ls_mentions_del AFTER DELETE ON thread_messages BEGIN
            DELETE FROM ls_mentions WHERE thread_id = old.thread_id AND ts = old.ts;
        END",
    )
    .execute(&mut *tx)
    .await?;
    Ok(tx.commit().await?)
}

/// Message `thread_id`/`ts` @mentions `who`, instead of whoever it did.
async fn put_mentions(tx: &mut sqlx::SqliteConnection, thread_id: i64, ts: i64, who: &[uuid::Uuid]) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM ls_mentions WHERE thread_id = ? AND ts = ?").bind(thread_id).bind(ts).execute(&mut *tx).await?;
    for u in who {
        sqlx::query("INSERT OR IGNORE INTO ls_mentions (aci, ts, thread_id) VALUES (?, ?, ?)")
            .bind(u.as_bytes().as_slice())
            .bind(ts)
            .bind(thread_id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// Whom a search row's text @mentions: its `who` column, `group_concat(hex(aci))`.
fn who(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Vec<uuid::Uuid>> {
    let who: Option<&str> = r.try_get("who")?;
    Ok(who.map_or_else(Vec::new, |w| w.split(',').filter_map(|h| uuid::Uuid::parse_str(h).ok()).collect()))
}

fn unzip(thread: &Thread) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    match thread {
        Thread::Group(key) => (Some(key.to_vec()), None),
        // presage stores the raw uuid as a 16-byte blob
        Thread::Contact(id) => (None, Some(id.raw_uuid().as_bytes().to_vec())),
    }
}

impl Index {
    /// presage's `save_message` for many messages, in one transaction: presage commits, and so
    /// flushes the disk, per message (about 1.7 ms each), which made a link-time history import
    /// take minutes. Same statements as presage's; `history::tests` reads the result back
    /// through presage, so a schema change there fails a test. Like the history import, a
    /// message at a thread and millisecond taken by someone else's is skipped; returns how many.
    pub async fn save_messages(&self, messages: Vec<(Thread, presage::libsignal_service::content::Content)>) -> anyhow::Result<usize> {
        use presage::libsignal_service::prelude::ProtobufMessage as _;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut skipped = 0;
        for (thread, presage::libsignal_service::content::Content { metadata: m, body }) in messages {
            let thread_id: i64 = match &thread {
                Thread::Contact(id) => {
                    sqlx::query_scalar(
                        "INSERT INTO threads(recipient_id, group_master_key) VALUES (?1, NULL)
                         ON CONFLICT DO UPDATE SET recipient_id = ?1 RETURNING id",
                    )
                    .bind(id.raw_uuid())
                    .fetch_one(&mut *tx)
                    .await?
                }
                Thread::Group(key) => {
                    sqlx::query_scalar(
                        "INSERT INTO threads(recipient_id, group_master_key) VALUES (NULL, ?1)
                         ON CONFLICT DO UPDATE SET group_master_key = ?1 RETURNING id",
                    )
                    .bind(key.as_slice())
                    .fetch_one(&mut *tx)
                    .await?
                }
            };
            let ts = m.client_timestamp.timestamp_millis();
            let sender = m.sender.service_id_string();
            let holder: Option<String> = sqlx::query_scalar("SELECT sender_service_id FROM thread_messages WHERE thread_id = ? AND ts = ?")
                .bind(thread_id)
                .bind(ts)
                .fetch_optional(&mut *tx)
                .await?;
            if holder.is_some_and(|h| h != sender) {
                skipped += 1;
                continue;
            }
            let device: u8 = m.sender_device.into();
            sqlx::query(
                "INSERT OR REPLACE INTO thread_messages (ts, thread_id, sender_service_id, sender_device_id,
                    destination_service_id, needs_receipt, unidentified_sender, content_body, was_plaintext, pni_verified)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(ts)
            .bind(thread_id)
            .bind(sender)
            .bind(device)
            .bind(m.destination.service_id_string())
            .bind(m.needs_receipt)
            .bind(m.unidentified_sender)
            .bind(body.into_proto().encode_to_vec())
            .bind(m.was_plaintext)
            .bind(m.pni_verified.as_ref().map(|p| p.service_id_string()))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(skipped)
    }

    /// Renumbers the display order by sent time. For the link-time history only: its archive
    /// isn't in time order, and it is imported into an empty store before anything is received,
    /// so nothing else has an order to keep.
    pub async fn order_by_sent(&self) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("CREATE TEMP TABLE ls_order_new AS SELECT thread_id, ts FROM ls_order ORDER BY ts, seq").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM ls_order").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO ls_order (thread_id, ts) SELECT thread_id, ts FROM ls_order_new ORDER BY rowid").execute(&mut *tx).await?;
        sqlx::query("DROP TABLE ls_order_new").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn open(options: SqliteConnectOptions) -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .connect_with(options)
            .await?;
        // keep this connection's page cache small, the store has its own
        sqlx::query("PRAGMA cache_size = -512").execute(&pool).await?;
        // presage files receipts under the 1:1 thread of whoever sent them, so receipts for
        // group messages never show up in the group's messages: index them by target instead
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_receipts (
                target_ts INTEGER NOT NULL,
                who BLOB NOT NULL,
                level INTEGER NOT NULL,
                PRIMARY KEY (target_ts, who)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // presage blanks the sender of a message deleted for everyone: remember which were ours
        sqlx::query("CREATE TABLE IF NOT EXISTS ls_deleted_mine (ts INTEGER PRIMARY KEY) WITHOUT ROWID")
            .execute(&pool)
            .await?;
        // pinned messages per thread: presage stores pin/unpin messages but doesn't track the state.
        // `expires` 0 = until unpinned
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_pins (
                thread TEXT NOT NULL,
                target_ts INTEGER NOT NULL,
                author BLOB NOT NULL,
                pinned_at INTEGER NOT NULL,
                expires INTEGER NOT NULL,
                PRIMARY KEY (thread, target_ts)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // where the newest edit of a message is stored: edits are rows of their own, so finding
        // one for a message outside the window would otherwise mean reading the thread
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_edits (
                thread TEXT NOT NULL,
                target_ts INTEGER NOT NULL,
                sender BLOB NOT NULL,
                edit_ts INTEGER NOT NULL,
                PRIMARY KEY (thread, target_ts, sender)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // who reacted how, so a message shows its reactions whichever rows are loaded (they are
        // stored as rows of their own, later in the thread). `emoji` "" = taken back: kept so
        // that an older reaction arriving late can't bring it back
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_reactions (
                thread TEXT NOT NULL,
                target_ts INTEGER NOT NULL,
                sender BLOB NOT NULL,
                emoji TEXT NOT NULL,
                at INTEGER NOT NULL,
                PRIMARY KEY (thread, target_ts, sender)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // which messages quote which: a reply stores its target, the target knows nothing of its
        // replies. Ids only, no text: a reply's text is read from the store when listed
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_replies (
                thread TEXT NOT NULL,
                target_ts INTEGER NOT NULL,
                reply_ts INTEGER NOT NULL,
                sender BLOB NOT NULL,
                PRIMARY KEY (thread, target_ts, reply_ts)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // finding a reply by itself, to forget it when it is deleted
        sqlx::query("CREATE INDEX IF NOT EXISTS ls_replies_by_reply ON ls_replies (thread, reply_ts)")
            .execute(&pool)
            .await?;
        // which messages carry pictures, videos or files, for a chat's media and files: finding
        // them in presage's table means reading every message. Ids only; `kind` is `MediaKind`
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_media (
                thread TEXT NOT NULL,
                ts INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                kind INTEGER NOT NULL,
                PRIMARY KEY (thread, ts, idx)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS ls_media_by_kind ON ls_media (thread, kind, ts)").execute(&pool).await?;
        // title and cover of installed sticker packs: presage keeps every sticker's bytes in one
        // JSON column, so listing packs from its table means reading all of them. Schema 1's
        // shape: `step` gives it its author
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_packs (
                id BLOB PRIMARY KEY,
                title TEXT NOT NULL,
                cover INTEGER
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // the picker's pack order as the user set it (0 first). Not a cache: nothing rebuilds it.
        // Packs without a row (installed since, or from another device) come after, in install order
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_pack_order (
                id BLOB PRIMARY KEY,
                pos INTEGER NOT NULL
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // sticker packs made on this PC, so their links can be found again: the server keeps no
        // list of them, and a pack without its key can't be opened. `cover` is a small PNG (the
        // thumbnail folder can be cleared), `made` ms since the epoch. Not a cache: nothing rebuilds it
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_made_packs (
                id BLOB PRIMARY KEY,
                key BLOB NOT NULL,
                title TEXT NOT NULL,
                author TEXT NOT NULL,
                cover BLOB,
                made INTEGER NOT NULL
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // stickers others sent us, one row a message, for the packs shared with us: finding them
        // in presage's table means reading every message. Rows go with their message (deleted,
        // expired), so a pack seen only in a disappearing message leaves no trace
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_sticker_msgs (
                thread TEXT NOT NULL,
                ts INTEGER NOT NULL,
                pack BLOB NOT NULL,
                key BLOB NOT NULL,
                PRIMARY KEY (thread, ts)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS ls_sticker_msgs_by_pack ON ls_sticker_msgs (pack, ts)").execute(&pool).await?;
        // title and cover of packs shared with us, from their manifests (`ls_packs`' shape), and
        // the key that opened them: messages may carry different keys for one pack. A cache: rows
        // of packs no message holds any more are dropped. It replaced `ls_shared_packs`, which
        // lacked the key: being a cache, it goes instead of a schema step, and an older build
        // that makes it again only fetches its titles again
        sqlx::query("DROP TABLE IF EXISTS ls_shared_packs").execute(&pool).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_shared_cache (
                id BLOB PRIMARY KEY,
                key BLOB NOT NULL,
                title TEXT NOT NULL,
                author TEXT NOT NULL,
                cover INTEGER
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // sticker ids and emoji per pack, in manifest order (same reason as `ls_packs`)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_stickers (
                pack BLOB NOT NULL,
                ord INTEGER NOT NULL,
                id INTEGER NOT NULL,
                emoji TEXT NOT NULL,
                PRIMARY KEY (pack, ord)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // stickers sent from this PC, for the picker's recently used tab (the official apps
        // don't sync it either). `used` is ms since the epoch. Not a cache: nothing rebuilds it
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_sticker_recent (
                pack BLOB NOT NULL,
                id INTEGER NOT NULL,
                used INTEGER NOT NULL,
                PRIMARY KEY (pack, id)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // attachments we uploaded, so sending the same bytes to the same thread again can reuse
        // the upload. `hash` is of the bytes as uploaded; `pointer` is the protobuf, `at` its
        // upload time. A cache: rows past their useful life are dropped on every `put`
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_uploads (
                thread TEXT NOT NULL,
                hash BLOB NOT NULL,
                pointer BLOB NOT NULL,
                at INTEGER NOT NULL,
                PRIMARY KEY (thread, hash)
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        // people we write to by PNI (a number never talked with): presage keeps their thread
        // under the bare uuid like any other, so this is what tells it apart. `e164` names them
        // (no contact or profile yet); `aci` is set once a reply proved whose PNI it is, until
        // the thread has moved over (`pni_merging`)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_pni (
                pni BLOB PRIMARY KEY,
                e164 TEXT,
                aci BLOB
            ) WITHOUT ROWID",
        )
        .execute(&pool)
        .await?;
        ensure_order(&pool).await?;
        ensure_text(&pool).await?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Message `ts` reads `text` (lowercase body, captions and file names) and @mentions
    /// `mentions` (ACIs), for the message search.
    pub async fn record_text(&self, thread: &Thread, ts: u64, text: String, mentions: Vec<uuid::Uuid>) -> anyhow::Result<()> {
        self.record_texts(thread, &[(ts, text, mentions)]).await?;
        Ok(())
    }

    /// How many rows the thread holds (through `ls_order`'s thread index, without reading them).
    pub async fn row_count(&self, thread: &Thread) -> anyhow::Result<u64> {
        let (gk, rid) = unzip(thread);
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ls_order WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)",
        )
        .bind(gk)
        .bind(rid)
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }

    /// `record_text` for many messages of one thread, in one transaction; how many.
    pub async fn record_texts(&self, thread: &Thread, texts: &[(u64, String, Vec<uuid::Uuid>)]) -> anyhow::Result<usize> {
        if texts.is_empty() {
            return Ok(0);
        }
        let (gk, rid) = unzip(thread);
        let mut tx = self.pool.begin().await?;
        let Some(id): Option<i64> = sqlx::query_scalar("SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?")
            .bind(gk)
            .bind(rid)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(0);
        };
        for (ts, text, mentions) in texts {
            sqlx::query("INSERT OR REPLACE INTO ls_text (thread_id, ts, text) VALUES (?, ?, ?)")
                .bind(id)
                .bind(*ts as i64)
                .bind(text)
                .execute(&mut *tx)
                .await?;
            put_mentions(&mut tx, id, *ts as i64, mentions).await?;
        }
        tx.commit().await?;
        Ok(texts.len())
    }

    /// An edit by `sender` (`service_id_string`) gives message `ts` new text. Only that
    /// message's own author can, and only a message already indexed (not one that disappears).
    pub async fn edit_text(&self, thread: &Thread, ts: u64, sender: &str, text: &str, mentions: &[uuid::Uuid]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(id) = self.thread_id_in(&mut tx, thread).await? else { return Ok(()) };
        let edited = sqlx::query(
            "UPDATE ls_text SET text = ?1
             WHERE thread_id = ?2 AND ts = ?3
               AND EXISTS (SELECT 1 FROM thread_messages m
                           WHERE m.thread_id = ls_text.thread_id AND m.ts = ls_text.ts AND m.sender_service_id = ?4)",
        )
        .bind(text)
        .bind(id)
        .bind(ts as i64)
        .bind(sender)
        .execute(&mut *tx)
        .await?;
        if edited.rows_affected() > 0 {
            put_mentions(&mut tx, id, ts as i64, mentions).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Message `ts` was deleted for everyone: presage replaces it in place, which the delete
    /// trigger doesn't see.
    pub async fn forget_text(&self, thread: &Thread, ts: u64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(id) = self.thread_id_in(&mut tx, thread).await? else { return Ok(()) };
        sqlx::query("DELETE FROM ls_text WHERE thread_id = ? AND ts = ?").bind(id).bind(ts as i64).execute(&mut *tx).await?;
        put_mentions(&mut tx, id, ts as i64, &[]).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Everyone some indexed text @mentions (ACIs).
    pub async fn mentioned(&self) -> anyhow::Result<Vec<uuid::Uuid>> {
        let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT DISTINCT aci FROM ls_mentions").fetch_all(&self.pool).await?;
        Ok(rows.iter().filter_map(|b| uuid::Uuid::from_slice(b).ok()).collect())
    }

    /// `thread_id`, inside a transaction.
    async fn thread_id_in(&self, tx: &mut sqlx::SqliteConnection, thread: &Thread) -> anyhow::Result<Option<i64>> {
        let (gk, rid) = unzip(thread);
        Ok(sqlx::query_scalar("SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?")
            .bind(gk)
            .bind(rid)
            .fetch_optional(&mut *tx)
            .await?)
    }

    /// The DB id of `thread`, if it has one.
    async fn thread_id(&self, thread: &Thread) -> anyhow::Result<Option<i64>> {
        let (gk, rid) = unzip(thread);
        Ok(sqlx::query_scalar("SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?")
            .bind(gk)
            .bind(rid)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The messages `s` asks for, newest first, at most `cap`: (thread, sent ts). Texts are
    /// read in chunks, so other queries get the connection in between and `cancelled` can stop
    /// a search the user has moved on from.
    ///
    /// Without `needle`, messages with attachments are found too, though they have no text.
    pub async fn search(&self, s: &Searched<'_>, cap: usize, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Vec<(Thread, u64)>> {
        let only = match s.thread {
            Some(t) => match self.thread_id(t).await? {
                Some(id) => Some(id),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let threads = self.threads_by_id().await?;
        let mut hits: Vec<(i64, i64)> = Vec::new();
        let media = match s.kind {
            Some(SearchKind::Media) => Some(MediaKind::Media),
            Some(SearchKind::Files) => Some(MediaKind::Files),
            _ => None,
        };
        if let Some(who) = s.mentions {
            // what mentions them is fewer still than what anyone sent
            hits = self.search_mentioning(s, who, only, media, &threads, cap, cancelled).await?;
        } else if let Some(sender) = s.sender {
            // theirs alone are fewer than the texts or attachments to go through
            hits = self.search_sender(s, sender, only, media, &threads, cap, cancelled).await?;
        } else if s.needle.is_some() || media.is_none() {
            // a needle, links or every text: the texts. Attachments alone have none, so without
            // a needle they come from `ls_media`. What `with_media` drops is unknown here, so
            // it needs them all
            let enough = if media.is_some() { usize::MAX } else { cap };
            hits = self.search_texts(s, only, enough, cancelled).await?;
            if let Some(kind) = media {
                hits = self.with_media(hits, &threads, kind, cap, cancelled).await?;
            }
        }
        if s.sender.is_none() && s.needle.is_none() && !s.in_text() {
            let ids: HashMap<String, i64> = threads.iter().map(|(id, t)| (crate::types::thread_key(t), *id)).collect();
            hits.extend(self.search_media(s, media, &ids, cap, cancelled).await?);
        }
        // the scans above take the needle from the text; it may show in a mention instead.
        // What mentions us was read whole already
        if s.needle.is_some() && s.mentions.is_none() {
            for &who in s.named {
                hits.extend(self.search_mentioning(s, who, only, media, &threads, cap, cancelled).await?);
            }
        }
        if cancelled() {
            return Ok(Vec::new());
        }
        hits.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hits.dedup();
        hits.truncate(cap);
        Ok(hits.into_iter().filter_map(|(id, ts)| Some((threads.get(&id)?.clone(), ts as u64))).collect())
    }

    /// The texts holding `needle` (all, without one) that pass `keep` and the filters, read
    /// newest first in spans of `CHUNK` texts; done once `enough` are found, since all the rest
    /// are older.
    async fn search_texts(
        &self,
        s: &Searched<'_>,
        only: Option<i64>,
        enough: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        // tiny in tests, so that they cross spans
        const CHUNK: i64 = if cfg!(test) { 2 } else { 20_000 };
        let links = s.kind == Some(SearchKind::Links);
        let mut hits = Vec::new();
        let mut before = s.until.map_or(i64::MAX, |t| t as i64);
        while hits.len() < enough {
            if cancelled() {
                return Ok(Vec::new());
            }
            // the span's oldest ts, found on the index alone; None: the rest fit in one span
            let from: Option<i64> = sqlx::query_scalar("SELECT ts FROM ls_text WHERE ts < ?1 ORDER BY ts DESC LIMIT 1 OFFSET ?2")
                .bind(before)
                .bind(CHUNK - 1)
                .fetch_optional(&self.pool)
                .await?;
            let rows = sqlx::query(
                "SELECT t.thread_id, t.ts, t.text, CASE WHEN ?4 IS NOT NULL THEN (SELECT group_concat(hex(y.aci)) FROM ls_mentions y
                    WHERE y.thread_id = t.thread_id AND y.ts = t.ts) END AS who
                 FROM ls_text t
                 WHERE t.ts < ?1 AND t.ts >= ?2 AND (?3 IS NULL OR t.thread_id = ?3)
                    AND (?4 IS NULL OR instr(t.text, ?4) > 0)
                    AND (?5 = 0 OR instr(t.text, 'http') > 0 OR instr(t.text, 'www.') > 0)",
            )
            .bind(before)
            .bind(from.unwrap_or(i64::MIN))
            .bind(only)
            .bind(s.needle)
            .bind(links)
            .fetch_all(&self.pool)
            .await?;
            for r in rows {
                let text: &str = r.try_get("text")?;
                if (s.keep)(text, &who(&r)?) && (!links || !crate::link::find(text).is_empty()) {
                    hits.push((r.try_get("thread_id")?, r.try_get("ts")?));
                }
            }
            match from {
                Some(ts) => before = ts,
                None => break,
            }
            tokio::task::yield_now().await;
        }
        Ok(hits)
    }

    /// Whether the message `thread`/`ts` carries an attachment (of `kind`, if given).
    async fn has_media(&self, thread: &Thread, ts: i64, kind: Option<MediaKind>) -> anyhow::Result<bool> {
        let has: Option<i64> = sqlx::query_scalar("SELECT 1 FROM ls_media WHERE thread = ?1 AND ts = ?2 AND (?3 IS NULL OR kind = ?3) LIMIT 1")
            .bind(crate::types::thread_key(thread))
            .bind(ts)
            .bind(kind.map(|k| k as i64))
            .fetch_optional(&self.pool)
            .await?;
        Ok(has.is_some())
    }

    /// The messages (thread id, ts) with an attachment (of `kind`, if given), in `s.thread` if it
    /// names one: ids only, so that a search checks its candidates without a query each.
    async fn attached(&self, s: &Searched<'_>, kind: Option<MediaKind>, threads: &HashMap<i64, Thread>) -> anyhow::Result<HashSet<(i64, i64)>> {
        use futures::TryStreamExt;
        let ids: HashMap<String, i64> = threads.iter().map(|(id, t)| (crate::types::thread_key(t), *id)).collect();
        let scope = s.thread.map(crate::types::thread_key);
        let mut set = HashSet::new();
        let mut rows = sqlx::query("SELECT thread, ts FROM ls_media WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR thread = ?2)")
            .bind(kind.map(|k| k as i64))
            .bind(scope.as_deref())
            .fetch(&self.pool);
        while let Some(r) = rows.try_next().await? {
            let key: &str = r.try_get("thread")?;
            if let Some(&id) = ids.get(key) {
                set.insert((id, r.try_get::<i64, _>("ts")?));
            }
        }
        Ok(set)
    }

    /// The newest `cap` messages `sender` sent that pass the rest of `s`, read newest first
    /// through `ls_by_sender`. Their rows that aren't messages (reactions, edits) have neither
    /// text nor attachment, so they drop out.
    #[allow(clippy::too_many_arguments)]
    async fn search_sender(
        &self,
        s: &Searched<'_>,
        sender: &str,
        only: Option<i64>,
        media: Option<MediaKind>,
        threads: &HashMap<i64, Thread>,
        cap: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        const PAGE: i64 = 500;
        let links = s.kind == Some(SearchKind::Links);
        // a kind asks for its attachment; with nothing asked, a message without text needs one
        let attached = if media.is_some() || (s.needle.is_none() && !s.in_text()) {
            Some(self.attached(s, media, threads).await?)
        } else {
            None
        };
        let mut hits = Vec::new();
        let mut before = s.until.map_or(i64::MAX, |t| t as i64);
        while hits.len() < cap {
            if cancelled() {
                return Ok(Vec::new());
            }
            // the needle and links narrow in SQL, as in `search_texts`
            let rows = sqlx::query(
                "SELECT m.thread_id, m.ts, t.text, CASE WHEN ?4 IS NOT NULL THEN (SELECT group_concat(hex(y.aci)) FROM ls_mentions y
                    WHERE y.thread_id = m.thread_id AND y.ts = m.ts) END AS who
                 FROM thread_messages m
                 LEFT JOIN ls_text t ON t.thread_id = m.thread_id AND t.ts = m.ts
                 WHERE m.sender_service_id = ?1 AND m.ts < ?2 AND (?3 IS NULL OR m.thread_id = ?3)
                    AND (?4 IS NULL OR instr(t.text, ?4) > 0)
                    AND (?5 = 0 OR instr(t.text, 'http') > 0 OR instr(t.text, 'www.') > 0)
                 ORDER BY m.ts DESC LIMIT ?6",
            )
            .bind(sender)
            .bind(before)
            .bind(only)
            .bind(s.needle)
            .bind(links)
            .bind(PAGE)
            .fetch_all(&self.pool)
            .await?;
            let last = rows.len() < PAGE as usize;
            for r in &rows {
                let (id, ts): (i64, i64) = (r.try_get("thread_id")?, r.try_get("ts")?);
                before = ts;
                let text: Option<&str> = r.try_get("text")?;
                if !threads.contains_key(&id) {
                    continue;
                }
                let mentioned = who(r)?;
                if text.is_some_and(|x| !(s.keep)(x, &mentioned)) || (links && !text.is_some_and(|x| !crate::link::find(x).is_empty())) {
                    continue;
                }
                if let Some(set) = &attached {
                    if (media.is_some() || text.is_none()) && !set.contains(&(id, ts)) {
                        continue;
                    }
                }
                hits.push((id, ts));
                if hits.len() >= cap {
                    break;
                }
            }
            if last {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(hits)
    }

    /// The newest `cap` texts that @mention `whom` and pass the rest of `s`, read through
    /// `ls_mentions`: they're few. The needle isn't asked of the text, since it may show in a
    /// mention instead (`Searched::named`).
    #[allow(clippy::too_many_arguments)]
    async fn search_mentioning(
        &self,
        s: &Searched<'_>,
        whom: uuid::Uuid,
        only: Option<i64>,
        media: Option<MediaKind>,
        threads: &HashMap<i64, Thread>,
        cap: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        const PAGE: i64 = 500;
        let links = s.kind == Some(SearchKind::Links);
        let mut hits = Vec::new();
        let mut before = s.until.map_or(i64::MAX, |t| t as i64);
        while hits.len() < cap {
            if cancelled() {
                return Ok(Vec::new());
            }
            let rows = sqlx::query(
                "SELECT x.thread_id, x.ts, t.text, (SELECT group_concat(hex(y.aci)) FROM ls_mentions y
                    WHERE y.thread_id = x.thread_id AND y.ts = x.ts) AS who
                 FROM ls_mentions x
                 JOIN ls_text t ON t.thread_id = x.thread_id AND t.ts = x.ts
                 WHERE x.aci = ?1 AND x.ts < ?2 AND (?3 IS NULL OR x.thread_id = ?3)
                    AND (?4 = 0 OR instr(t.text, 'http') > 0 OR instr(t.text, 'www.') > 0)
                    AND (?5 IS NULL OR EXISTS (SELECT 1 FROM thread_messages m
                        WHERE m.thread_id = x.thread_id AND m.ts = x.ts AND m.sender_service_id = ?5))
                    AND (?6 IS NULL OR EXISTS (SELECT 1 FROM ls_mentions y
                        WHERE y.aci = ?6 AND y.ts = x.ts AND y.thread_id = x.thread_id))
                 ORDER BY x.ts DESC LIMIT ?7",
            )
            .bind(whom.as_bytes().as_slice())
            .bind(before)
            .bind(only)
            .bind(links)
            .bind(s.sender)
            .bind(s.mentions.map(|u| u.as_bytes().to_vec()))
            .bind(PAGE)
            .fetch_all(&self.pool)
            .await?;
            let last = rows.len() < PAGE as usize;
            for r in &rows {
                let (id, ts): (i64, i64) = (r.try_get("thread_id")?, r.try_get("ts")?);
                before = ts;
                let text: &str = r.try_get("text")?;
                let Some(t) = threads.get(&id) else { continue };
                if !(s.keep)(text, &who(r)?) || links && crate::link::find(text).is_empty() {
                    continue;
                }
                if media.is_some() && !self.has_media(t, ts, media).await? {
                    continue;
                }
                hits.push((id, ts));
                if hits.len() >= cap {
                    break;
                }
            }
            if last {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(hits)
    }

    /// The newest `cap` of `hits` that carry an attachment of `kind`.
    async fn with_media(
        &self,
        mut hits: Vec<(i64, i64)>,
        threads: &HashMap<i64, Thread>,
        kind: MediaKind,
        cap: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        hits.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        let mut kept = Vec::new();
        for (i, (id, ts)) in hits.into_iter().enumerate() {
            if kept.len() >= cap || cancelled() {
                break;
            }
            let Some(t) = threads.get(&id) else { continue };
            if self.has_media(t, ts, Some(kind)).await? {
                kept.push((id, ts));
            }
            if i % 200 == 199 {
                tokio::task::yield_now().await;
            }
        }
        Ok(kept)
    }

    /// The newest `cap` messages with an attachment (of `kind`, if given) that pass the
    /// filters. `ids`: thread ids by thread key.
    async fn search_media(
        &self,
        s: &Searched<'_>,
        kind: Option<MediaKind>,
        ids: &HashMap<String, i64>,
        cap: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        const PAGE: i64 = 500;
        let scope = s.thread.map(crate::types::thread_key);
        let mut hits = Vec::new();
        let mut before = s.until.map_or(i64::MAX, |t| t as i64);
        while hits.len() < cap {
            if cancelled() {
                return Ok(Vec::new());
            }
            let rows = sqlx::query(
                "SELECT thread, ts FROM ls_media
                 WHERE ts < ?1 AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR thread = ?3)
                 GROUP BY thread, ts ORDER BY ts DESC LIMIT ?4",
            )
            .bind(before)
            .bind(kind.map(|k| k as i64))
            .bind(scope.as_deref())
            .bind(PAGE)
            .fetch_all(&self.pool)
            .await?;
            let last = rows.len() < PAGE as usize;
            for r in &rows {
                let (key, ts): (String, i64) = (r.try_get("thread")?, r.try_get("ts")?);
                before = ts;
                let Some(&id) = ids.get(&key) else { continue };
                hits.push((id, ts));
            }
            if last {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(hits)
    }

    /// Who sent rows to `thread` (to any, without one), as stored (`ServiceId::service_id_string`).
    pub async fn senders(&self, thread: Option<&Thread>) -> anyhow::Result<Vec<String>> {
        let only = match thread {
            Some(t) => match self.thread_id(t).await? {
                Some(id) => Some(id),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        Ok(sqlx::query_scalar("SELECT DISTINCT sender_service_id FROM thread_messages WHERE ?1 IS NULL OR thread_id = ?1")
            .bind(only)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Every thread by its DB id.
    async fn threads_by_id(&self) -> anyhow::Result<HashMap<i64, Thread>> {
        let rows = sqlx::query(
            "SELECT t.id, t.group_master_key AS gk, t.recipient_id AS rid,
                EXISTS (SELECT 1 FROM ls_pni p WHERE p.pni = t.recipient_id) AS pni
             FROM threads t",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = HashMap::new();
        for row in rows {
            if let Some(t) = thread_of(row.try_get("gk")?, row.try_get("rid")?, row.try_get("pni")?) {
                out.insert(row.try_get("id")?, t);
            }
        }
        Ok(out)
    }

    /// The pointer of an earlier upload of `hash` to `thread` made at or after `since` (ms).
    pub async fn upload_find(&self, thread: &str, hash: &[u8], since: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT pointer FROM ls_uploads WHERE thread = ? AND hash = ? AND at >= ?")
            .bind(thread)
            .bind(hash)
            .bind(since as i64)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Remembers an upload made at `at`, and forgets those made before `keep_since`.
    pub async fn upload_put(&self, thread: &str, hash: &[u8], pointer: &[u8], at: u64, keep_since: u64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_uploads WHERE at < ?").bind(keep_since as i64).execute(&self.pool).await?;
        sqlx::query(
            "INSERT INTO ls_uploads (thread, hash, pointer, at) VALUES (?, ?, ?, ?)
             ON CONFLICT (thread, hash) DO UPDATE SET pointer = excluded.pointer, at = excluded.at",
        )
        .bind(thread)
        .bind(hash)
        .bind(pointer)
        .bind(at as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Files `pni` as someone we write to by PNI, with the number we looked them up by, if any.
    pub async fn pni_put(&self, pni: uuid::Uuid, e164: Option<&str>) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO ls_pni (pni, e164) VALUES (?, ?) ON CONFLICT (pni) DO UPDATE SET e164 = COALESCE(excluded.e164, e164)")
            .bind(pni.as_bytes().as_slice())
            .bind(e164)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether `pni` is filed (`pni_put`), and its number if known.
    pub async fn pni_number(&self, pni: uuid::Uuid) -> anyhow::Result<Option<Option<String>>> {
        Ok(sqlx::query_scalar("SELECT e164 FROM ls_pni WHERE pni = ?")
            .bind(pni.as_bytes().as_slice())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// `pni` turned out to be `aci`'s: its thread is to move over (`pni_merging` until done).
    pub async fn pni_proved(&self, pni: uuid::Uuid, aci: uuid::Uuid) -> anyhow::Result<()> {
        sqlx::query("UPDATE ls_pni SET aci = ? WHERE pni = ?")
            .bind(aci.as_bytes().as_slice())
            .bind(pni.as_bytes().as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// PNI threads proved to be someone's whose move didn't finish: `(pni, aci)`.
    pub async fn pni_merging(&self) -> anyhow::Result<Vec<(uuid::Uuid, uuid::Uuid)>> {
        let rows = sqlx::query("SELECT pni, aci FROM ls_pni WHERE aci IS NOT NULL").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let pni = uuid::Uuid::from_slice(&r.try_get::<Vec<u8>, _>("pni").ok()?).ok()?;
                let aci = uuid::Uuid::from_slice(&r.try_get::<Vec<u8>, _>("aci").ok()?).ok()?;
                Some((pni, aci))
            })
            .collect())
    }

    pub async fn pni_forget(&self, pni: uuid::Uuid) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_pni WHERE pni = ?").bind(pni.as_bytes().as_slice()).execute(&self.pool).await?;
        Ok(())
    }

    /// Before `from`'s messages are stored again under `to`: their display order moves along,
    /// so they stay before what `to` already holds (the reply that proved who it is).
    /// `to` must exist in presage's `threads`; without it nothing moves and the copies are
    /// ordered as they're stored.
    pub async fn move_order(&self, from: &Thread, to: &Thread) -> anyhow::Result<()> {
        let ((fg, fr), (tg, tr)) = (unzip(from), unzip(to));
        sqlx::query(
            "UPDATE OR IGNORE ls_order SET thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND EXISTS (SELECT 1 FROM threads WHERE group_master_key = ? OR recipient_id = ?)",
        )
        .bind(&tg)
        .bind(&tr)
        .bind(&fg)
        .bind(&fr)
        .bind(&tg)
        .bind(&tr)
        .execute(&self.pool)
        .await?;
        // the moved copies are new rows of `to`: the text and its mentions go with them
        for q in [
            "UPDATE OR IGNORE ls_text SET thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND EXISTS (SELECT 1 FROM threads WHERE group_master_key = ? OR recipient_id = ?)",
            "UPDATE OR IGNORE ls_mentions SET thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND EXISTS (SELECT 1 FROM threads WHERE group_master_key = ? OR recipient_id = ?)",
        ] {
            sqlx::query(q).bind(&tg).bind(&tr).bind(&fg).bind(&fr).bind(&tg).bind(&tr).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Our per-thread rows of `from` become `to`'s; any `to` already has wins.
    pub async fn rekey_thread(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        // one pair per table keyed by thread (sqlx wants the strings static)
        const TABLES: [(&str, &str); 7] = [
            ("UPDATE OR IGNORE ls_media SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_media WHERE thread = ?1"),
            ("UPDATE OR IGNORE ls_sticker_msgs SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_sticker_msgs WHERE thread = ?1"),
            ("UPDATE OR IGNORE ls_pins SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_pins WHERE thread = ?1"),
            ("UPDATE OR IGNORE ls_edits SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_edits WHERE thread = ?1"),
            ("UPDATE OR IGNORE ls_reactions SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_reactions WHERE thread = ?1"),
            ("UPDATE OR IGNORE ls_replies SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_replies WHERE thread = ?1"),
            ("UPDATE OR IGNORE ls_uploads SET thread = ?1 WHERE thread = ?2", "DELETE FROM ls_uploads WHERE thread = ?1"),
        ];
        for (update, delete) in TABLES {
            sqlx::query(update).bind(to).bind(from).execute(&mut *tx).await?;
            // what `to` already had at the same place
            sqlx::query(delete).bind(from).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// A pin sent at `at`; an older pin of the same message is replaced, a newer one kept.
    pub async fn pin(&self, thread: &str, target: u64, author: uuid::Uuid, at: u64, expires: u64) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO ls_pins (thread, target_ts, author, pinned_at, expires) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (thread, target_ts) DO UPDATE SET
                author = excluded.author, pinned_at = excluded.pinned_at, expires = excluded.expires
             WHERE excluded.pinned_at >= ls_pins.pinned_at",
        )
        .bind(thread)
        .bind(target as i64)
        .bind(author.as_bytes().as_slice())
        .bind(at as i64)
        .bind(expires as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// An unpin sent at `at` (`u64::MAX >> 1`: unconditionally) removes pins made before it.
    pub async fn unpin(&self, thread: &str, target: u64, at: u64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_pins WHERE thread = ? AND target_ts = ? AND pinned_at <= ?")
            .bind(thread)
            .bind(target as i64)
            .bind(at.min(i64::MAX as u64) as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Pins of a thread, newest first: (target ts, target author, pinned at, expires or 0).
    pub async fn pins(&self, thread: &str) -> anyhow::Result<Vec<(u64, uuid::Uuid, u64, u64)>> {
        let rows = sqlx::query(
            "SELECT target_ts, author, pinned_at, expires FROM ls_pins WHERE thread = ? ORDER BY pinned_at DESC",
        )
        .bind(thread)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let ts: i64 = r.try_get("target_ts").ok()?;
                let author: Vec<u8> = r.try_get("author").ok()?;
                let at: i64 = r.try_get("pinned_at").ok()?;
                let exp: i64 = r.try_get("expires").ok()?;
                Some((ts as u64, uuid::Uuid::from_slice(&author).ok()?, at as u64, exp as u64))
            })
            .collect())
    }

    /// An edit of `target` by `sender`, stored at `edit_ts`; only the newest is kept.
    pub async fn record_edit(&self, thread: &str, target: u64, sender: uuid::Uuid, edit_ts: u64) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO ls_edits (thread, target_ts, sender, edit_ts) VALUES (?, ?, ?, ?)
             ON CONFLICT (thread, target_ts, sender) DO UPDATE SET edit_ts = MAX(edit_ts, excluded.edit_ts)",
        )
        .bind(thread)
        .bind(target as i64)
        .bind(sender.as_bytes().as_slice())
        .bind(edit_ts as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stored timestamp of the newest edit of `target` by `sender`. Per sender because only the
    /// author's edits count, and a stranger's must not hide them.
    pub async fn latest_edit(&self, thread: &str, target: u64, sender: uuid::Uuid) -> anyhow::Result<Option<u64>> {
        let ts: Option<i64> =
            sqlx::query_scalar("SELECT edit_ts FROM ls_edits WHERE thread = ? AND target_ts = ? AND sender = ?")
                .bind(thread)
                .bind(target as i64)
                .bind(sender.as_bytes().as_slice())
                .fetch_optional(&self.pool)
                .await?;
        Ok(ts.map(|t| t as u64))
    }

    /// Newest edits of `targets`: (target, sender, stored timestamp of the edit).
    pub async fn edits_for(&self, thread: &str, targets: &[u64]) -> anyhow::Result<Vec<(u64, uuid::Uuid, u64)>> {
        let rows = sqlx::query(
            "SELECT target_ts, sender, edit_ts FROM ls_edits
             WHERE thread = ? AND target_ts IN (SELECT value FROM json_each(?))",
        )
        .bind(thread)
        .bind(json_list(targets))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let ts: i64 = r.try_get("target_ts").ok()?;
                let who: Vec<u8> = r.try_get("sender").ok()?;
                let at: i64 = r.try_get("edit_ts").ok()?;
                Some((ts as u64, uuid::Uuid::from_slice(&who).ok()?, at as u64))
            })
            .collect())
    }

    /// `sender`'s reaction to `target`, sent at `at` (`emoji` "": taken back). One per sender;
    /// the newest counts, in whatever order they arrive.
    pub async fn record_reaction(&self, thread: &str, target: u64, sender: uuid::Uuid, emoji: &str, at: u64) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO ls_reactions (thread, target_ts, sender, emoji, at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (thread, target_ts, sender) DO UPDATE SET emoji = excluded.emoji, at = excluded.at
             WHERE excluded.at >= ls_reactions.at",
        )
        .bind(thread)
        .bind(target as i64)
        .bind(sender.as_bytes().as_slice())
        .bind(emoji)
        .bind(at as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reactions standing on `targets`, oldest first: target -> (sender, emoji).
    pub async fn reactions_for(&self, thread: &str, targets: &[u64]) -> anyhow::Result<HashMap<u64, Vec<(uuid::Uuid, String)>>> {
        let rows = sqlx::query(
            "SELECT target_ts, sender, emoji FROM ls_reactions
             WHERE thread = ? AND emoji <> '' AND target_ts IN (SELECT value FROM json_each(?))
             ORDER BY at",
        )
        .bind(thread)
        .bind(json_list(targets))
        .fetch_all(&self.pool)
        .await?;
        let mut out: HashMap<u64, Vec<(uuid::Uuid, String)>> = HashMap::new();
        for r in rows {
            let (Ok(ts), Ok(who), Ok(emoji)) =
                (r.try_get::<i64, _>("target_ts"), r.try_get::<Vec<u8>, _>("sender"), r.try_get::<String, _>("emoji"))
            else {
                continue;
            };
            let Ok(who) = uuid::Uuid::from_slice(&who) else { continue };
            out.entry(ts as u64).or_default().push((who, emoji));
        }
        Ok(out)
    }

    /// The message `target` is gone for good (disappearing): so is what we noted about it.
    pub async fn forget_reactions(&self, thread: &str, target: u64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_reactions WHERE thread = ? AND target_ts = ?")
            .bind(thread)
            .bind(target as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `reply` (sent at `reply_ts` by `sender`) quotes the message sent at `target`.
    pub async fn record_reply(&self, thread: &str, target: u64, reply_ts: u64, sender: uuid::Uuid) -> anyhow::Result<()> {
        sqlx::query("INSERT OR IGNORE INTO ls_replies (thread, target_ts, reply_ts, sender) VALUES (?, ?, ?, ?)")
            .bind(thread)
            .bind(target as i64)
            .bind(reply_ts as i64)
            .bind(sender.as_bytes().as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// How many replies each of `targets` has.
    pub async fn reply_counts(&self, thread: &str, targets: &[u64]) -> anyhow::Result<HashMap<u64, u32>> {
        let rows = sqlx::query(
            "SELECT target_ts, COUNT(*) AS n FROM ls_replies
             WHERE thread = ? AND target_ts IN (SELECT value FROM json_each(?)) GROUP BY target_ts",
        )
        .bind(thread)
        .bind(json_list(targets))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.try_get::<i64, _>("target_ts").ok()? as u64, r.try_get::<i64, _>("n").ok()? as u32)))
            .collect())
    }

    /// The newest `limit` replies to `target`, newest first: (reply sent ts, sender).
    pub async fn replies_to(&self, thread: &str, target: u64, limit: u32) -> anyhow::Result<Vec<(u64, uuid::Uuid)>> {
        let rows = sqlx::query("SELECT reply_ts, sender FROM ls_replies WHERE thread = ? AND target_ts = ? ORDER BY reply_ts DESC LIMIT ?")
            .bind(thread)
            .bind(target as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let who: Vec<u8> = r.try_get("sender").ok()?;
                Some((r.try_get::<i64, _>("reply_ts").ok()? as u64, uuid::Uuid::from_slice(&who).ok()?))
            })
            .collect())
    }

    /// The message sent at `ts` is gone (deleted, expired): it is no reply any more, and its
    /// replies have nothing to hang on.
    pub async fn forget_replies(&self, thread: &str, ts: u64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_replies WHERE thread = ? AND (reply_ts = ? OR target_ts = ?)")
            .bind(thread)
            .bind(ts as i64)
            .bind(ts as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The message sent at `ts` carries these attachments: (index in its `attachments`, kind).
    pub async fn record_media(&self, thread: &str, ts: u64, items: &[(usize, MediaKind)]) -> anyhow::Result<()> {
        for &(idx, kind) in items {
            sqlx::query("INSERT OR IGNORE INTO ls_media (thread, ts, idx, kind) VALUES (?, ?, ?, ?)")
                .bind(thread)
                .bind(ts as i64)
                .bind(idx as i64)
                .bind(kind as i64)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Up to `limit` attachments of `kind`, newest message first and in message order within
    /// one, after `after` (the last one of the previous page): (sent ts, index).
    pub async fn media_page(&self, thread: &str, kind: MediaKind, after: Option<(u64, usize)>, limit: u32) -> anyhow::Result<Vec<(u64, usize)>> {
        let (ts, idx) = after.map_or((i64::MAX, -1), |(t, i)| (t as i64, i as i64));
        let rows = sqlx::query(
            "SELECT ts, idx FROM ls_media WHERE thread = ?1 AND kind = ?2 AND (ts < ?3 OR (ts = ?3 AND idx > ?4))
             ORDER BY ts DESC, idx ASC LIMIT ?5",
        )
        .bind(thread)
        .bind(kind as i64)
        .bind(ts)
        .bind(idx)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.try_get::<i64, _>("ts").ok()? as u64, r.try_get::<i64, _>("idx").ok()? as usize)))
            .collect())
    }

    /// The message sent at `ts` is gone (deleted, expired): its attachments and its sticker.
    pub async fn forget_media(&self, thread: &str, ts: u64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_media WHERE thread = ? AND ts = ?").bind(thread).bind(ts as i64).execute(&self.pool).await?;
        sqlx::query("DELETE FROM ls_sticker_msgs WHERE thread = ? AND ts = ?").bind(thread).bind(ts as i64).execute(&self.pool).await?;
        Ok(())
    }

    /// Someone else's message sent at `ts` is a sticker of this pack.
    pub async fn record_sticker_msg(&self, thread: &str, ts: u64, pack: &[u8], key: &[u8]) -> anyhow::Result<()> {
        sqlx::query("INSERT OR IGNORE INTO ls_sticker_msgs (thread, ts, pack, key) VALUES (?, ?, ?, ?)")
            .bind(thread)
            .bind(ts as i64)
            .bind(pack)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Packs of the stickers others sent us, most recently sent first, with their cached title,
    /// author and cover sticker id (None: not fetched yet). Drops the cache of packs no message
    /// holds any more, and says which those were.
    pub async fn shared_packs(&self) -> anyhow::Result<(Vec<SharedPack>, Vec<Vec<u8>>)> {
        let gone: Vec<Vec<u8>> = sqlx::query_scalar("DELETE FROM ls_shared_cache WHERE id NOT IN (SELECT pack FROM ls_sticker_msgs) RETURNING id")
            .fetch_all(&self.pool)
            .await?;
        let rows = sqlx::query(
            "SELECT m.pack, MAX(m.ts) AS last, s.title, s.author, s.cover FROM ls_sticker_msgs m
             LEFT JOIN ls_shared_cache s ON s.id = m.pack
             GROUP BY m.pack ORDER BY last DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let packs: Vec<SharedPack> = rows
            .into_iter()
            .filter_map(|r| {
                let title: Option<String> = r.try_get("title").ok()?;
                let author: Option<String> = r.try_get("author").ok()?;
                let cover: Option<i64> = r.try_get("cover").ok()?;
                Some(SharedPack {
                    id: r.try_get("pack").ok()?,
                    names: title.zip(author),
                    cover: cover.map(|c| c as u32),
                })
            })
            .collect();
        Ok((packs, gone))
    }

    /// Keys to try for a pack shared with us: the one that opened it, once its manifest came;
    /// before that each different key its stickers carried, the most recently sent first. Anyone
    /// can send a sticker with a real pack's id and a wrong key: don't take one of these as the
    /// pack's key before it opened the manifest.
    pub async fn shared_pack_keys(&self, id: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
        if let Some(key) = sqlx::query_scalar("SELECT key FROM ls_shared_cache WHERE id = ?").bind(id).fetch_optional(&self.pool).await? {
            return Ok(vec![key]);
        }
        Ok(sqlx::query_scalar("SELECT key FROM ls_sticker_msgs WHERE pack = ? GROUP BY key ORDER BY MAX(ts) DESC")
            .bind(id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn cache_shared_pack(&self, id: &[u8], key: &[u8], title: &str, author: &str, cover: Option<u32>) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO ls_shared_cache (id, key, title, author, cover) VALUES (?, ?, ?, ?, ?)")
            .bind(id)
            .bind(key)
            .bind(title)
            .bind(author)
            .bind(cover.map(|c| c as i64))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Ids of installed sticker packs in the picker's order. Reads the id column only, not the
    /// manifests.
    pub async fn pack_ids(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        let rows = sqlx::query(
            "SELECT s.id FROM sticker_packs s LEFT JOIN ls_pack_order o ON o.id = s.id
             ORDER BY o.pos IS NULL, o.pos, s.rowid",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(|r| r.try_get("id").ok()).collect())
    }

    pub async fn set_pack_order(&self, ids: &[Vec<u8>]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ls_pack_order").execute(&mut *tx).await?;
        for (pos, id) in ids.iter().enumerate() {
            sqlx::query("INSERT INTO ls_pack_order (id, pos) VALUES (?, ?)").bind(id).bind(pos as i64).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// An installed pack's key, without reading its manifest.
    pub async fn pack_key(&self, id: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let row = sqlx::query("SELECT key FROM sticker_packs WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        Ok(row.and_then(|r| r.try_get("key").ok()))
    }

    pub async fn add_made_pack(&self, p: &MadePack, at: u64) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO ls_made_packs (id, key, title, author, cover, made) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&p.id)
            .bind(&p.key)
            .bind(&p.title)
            .bind(&p.author)
            .bind(&p.cover)
            .bind(at as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Packs made on this PC, newest first.
    pub async fn made_packs(&self) -> anyhow::Result<Vec<MadePack>> {
        let rows = sqlx::query("SELECT id, key, title, author, cover FROM ls_made_packs ORDER BY made DESC").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(MadePack {
                    id: r.try_get("id").ok()?,
                    key: r.try_get("key").ok()?,
                    title: r.try_get("title").ok()?,
                    author: r.try_get("author").ok()?,
                    cover: r.try_get("cover").ok()?,
                })
            })
            .collect())
    }

    pub async fn made_pack_key(&self, id: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let row = sqlx::query("SELECT key FROM ls_made_packs WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        Ok(row.and_then(|r| r.try_get("key").ok()))
    }

    pub async fn forget_made_pack(&self, id: &[u8]) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_made_packs WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// Cached pack metadata: (id, (title, author), cover sticker id).
    pub async fn cached_packs(&self) -> anyhow::Result<Vec<(Vec<u8>, (String, String), Option<u32>)>> {
        let rows = sqlx::query("SELECT id, title, author, cover FROM ls_packs").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let cover: Option<i64> = r.try_get("cover").ok()?;
                let names = (r.try_get("title").ok()?, r.try_get("author").ok()?);
                Some((r.try_get("id").ok()?, names, cover.map(|c| c as u32)))
            })
            .collect())
    }

    pub async fn cache_pack(&self, id: &[u8], title: &str, author: &str, cover: Option<u32>) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO ls_packs (id, title, author, cover) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(title)
            .bind(author)
            .bind(cover.map(|c| c as i64))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn uncache_pack(&self, id: &[u8]) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_packs WHERE id = ?").bind(id).execute(&self.pool).await?;
        sqlx::query("DELETE FROM ls_stickers WHERE pack = ?").bind(id).execute(&self.pool).await?;
        sqlx::query("DELETE FROM ls_pack_order WHERE id = ?").bind(id).execute(&self.pool).await?;
        sqlx::query("DELETE FROM ls_sticker_recent WHERE pack = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// A sticker was sent: it leads the recently used ones, of which `keep` stay.
    pub async fn use_sticker(&self, pack: &[u8], id: u32, at: u64, keep: usize) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR REPLACE INTO ls_sticker_recent (pack, id, used) VALUES (?, ?, ?)")
            .bind(pack)
            .bind(id as i64)
            .bind(at as i64)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM ls_sticker_recent WHERE (pack, id) NOT IN
             (SELECT pack, id FROM ls_sticker_recent ORDER BY used DESC LIMIT ?)",
        )
        .bind(keep as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn forget_recent_sticker(&self, pack: &[u8], id: u32) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_sticker_recent WHERE pack = ? AND id = ?").bind(pack).bind(id as i64).execute(&self.pool).await?;
        Ok(())
    }

    /// Recently used stickers of installed packs, newest first: (pack, sticker id, emoji).
    pub async fn recent_stickers(&self) -> anyhow::Result<Vec<(Vec<u8>, u32, String)>> {
        let rows = sqlx::query(
            "SELECT r.pack, r.id, COALESCE(s.emoji, '') AS emoji FROM ls_sticker_recent r
             JOIN sticker_packs p ON p.id = r.pack
             LEFT JOIN ls_stickers s ON s.pack = r.pack AND s.id = r.id
             ORDER BY r.used DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.try_get("pack").ok()?, r.try_get::<i64, _>("id").ok()? as u32, r.try_get("emoji").ok()?)))
            .collect())
    }

    /// Cached stickers of a pack in manifest order: (id, emoji). Empty if not cached yet.
    pub async fn cached_stickers(&self, pack: &[u8]) -> anyhow::Result<Vec<(u32, String)>> {
        let rows = sqlx::query("SELECT id, emoji FROM ls_stickers WHERE pack = ? ORDER BY ord")
            .bind(pack)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.try_get::<i64, _>("id").ok()? as u32, r.try_get("emoji").ok()?)))
            .collect())
    }

    pub async fn cache_stickers(&self, pack: &[u8], items: &[(u32, String)]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ls_stickers WHERE pack = ?").bind(pack).execute(&mut *tx).await?;
        for (ord, (id, emoji)) in items.iter().enumerate() {
            sqlx::query("INSERT INTO ls_stickers (pack, ord, id, emoji) VALUES (?, ?, ?, ?)")
                .bind(pack)
                .bind(ord as i64)
                .bind(*id as i64)
                .bind(emoji)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Drops presage's cached profile, so the next `retrieve_profile_by_uuid` fetches it again.
    pub async fn forget_profile(&self, uuid: uuid::Uuid) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM profiles WHERE uuid = ?").bind(uuid.as_bytes().to_vec()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn forget_profile_avatar(&self, uuid: uuid::Uuid) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM profile_avatars WHERE uuid = ?")
            .bind(uuid.as_bytes().to_vec())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn forget_group_avatar(&self, master_key: &[u8; 32]) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM group_avatars WHERE group_master_key = ?")
            .bind(master_key.to_vec())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// level: 1 delivered, 2 read/viewed. Never downgrades.
    pub async fn record_receipts(&self, who: uuid::Uuid, targets: &[u64], level: u8) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            sqlx::query(
                "INSERT INTO ls_receipts (target_ts, who, level) VALUES (?, ?, ?)
                 ON CONFLICT (target_ts, who) DO UPDATE SET level = MAX(level, excluded.level)",
            )
            .bind(*t as i64)
            .bind(who.as_bytes().as_slice())
            .bind(level as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_my_delete(&self, ts: u64) -> anyhow::Result<()> {
        sqlx::query("INSERT OR IGNORE INTO ls_deleted_mine (ts) VALUES (?)").bind(ts as i64).execute(&self.pool).await?;
        Ok(())
    }

    /// Which of these sent timestamps are our messages deleted for everyone.
    pub async fn my_deletes_in(&self, tss: &[u64]) -> anyhow::Result<std::collections::HashSet<u64>> {
        let rows = sqlx::query("SELECT ts FROM ls_deleted_mine WHERE ts IN (SELECT value FROM json_each(?))")
            .bind(json_list(tss))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().filter_map(|r| r.try_get::<i64, _>("ts").ok()).map(|t| t as u64).collect())
    }

    /// Receipts for messages sent within [from, to].
    pub async fn receipts_for(&self, tss: &[u64]) -> anyhow::Result<Vec<(u64, uuid::Uuid, u8)>> {
        let rows = sqlx::query("SELECT target_ts, who, level FROM ls_receipts WHERE target_ts IN (SELECT value FROM json_each(?))")
            .bind(json_list(tss))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let ts: i64 = r.try_get("target_ts").ok()?;
                let who: Vec<u8> = r.try_get("who").ok()?;
                let level: i64 = r.try_get("level").ok()?;
                Some((ts as u64, uuid::Uuid::from_slice(&who).ok()?, level as u8))
            })
            .collect())
    }

    pub async fn threads(&self) -> anyhow::Result<Vec<ThreadRow>> {
        let rows = sqlx::query(
            "SELECT t.group_master_key AS gk, t.recipient_id AS rid, MAX(m.ts) AS last_ts,
                (SELECT o.ts FROM ls_order o WHERE o.thread_id = t.id ORDER BY o.seq DESC LIMIT 1) AS tail_ts,
                EXISTS (SELECT 1 FROM ls_pni p WHERE p.pni = t.recipient_id) AS pni
             FROM threads t JOIN thread_messages m ON m.thread_id = t.id
             GROUP BY t.id ORDER BY last_ts DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let gk: Option<Vec<u8>> = row.try_get("gk")?;
            let rid: Option<Vec<u8>> = row.try_get("rid")?;
            let last_ts: i64 = row.try_get("last_ts")?;
            let tail_ts: Option<i64> = row.try_get("tail_ts")?;
            let pni: bool = row.try_get("pni")?;
            let Some(thread) = thread_of(gk, rid, pni) else { continue };
            out.push(ThreadRow { thread, last_ts: last_ts as u64, tail_ts: tail_ts.unwrap_or(last_ts) as u64 });
        }
        Ok(out)
    }

    /// Whether the thread holds anything `me` sent (from here or another device); None: it holds
    /// nothing at all. Goes through `ls_order`'s thread index: `thread_messages` has none by thread.
    pub async fn wrote_in(&self, thread: &Thread, me: uuid::Uuid) -> anyhow::Result<Option<bool>> {
        let (gk, rid) = unzip(thread);
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(m.sender_service_id = ?) FROM ls_order o
             JOIN thread_messages m ON m.thread_id = o.thread_id AND m.ts = o.ts
             WHERE o.thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)",
        )
        .bind(me.to_string())
        .bind(gk)
        .bind(rid)
        .fetch_one(&self.pool)
        .await?;
        Ok(found.map(|n| n != 0))
    }

    /// Forgets what the indexes hold about thread `key` (its messages are being deleted).
    pub async fn forget_thread(&self, key: &str) -> anyhow::Result<()> {
        for sql in [
            "DELETE FROM ls_pins WHERE thread = ?",
            "DELETE FROM ls_edits WHERE thread = ?",
            "DELETE FROM ls_reactions WHERE thread = ?",
            "DELETE FROM ls_replies WHERE thread = ?",
            "DELETE FROM ls_media WHERE thread = ?",
            "DELETE FROM ls_sticker_msgs WHERE thread = ?",
        ] {
            sqlx::query(sql).bind(key).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Timestamps of up to `limit` messages strictly newer than `after`, oldest first.
    pub async fn page_after(&self, thread: &Thread, after: u64, limit: u32) -> anyhow::Result<Vec<u64>> {
        let (gk, rid) = unzip(thread);
        let rows = sqlx::query(
            "SELECT ts FROM thread_messages
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts > ?
             ORDER BY ts ASC LIMIT ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(after.min(i64::MAX as u64) as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get::<i64, _>("ts").ok())
            .map(|t| t as u64)
            .collect())
    }

    /// `(seq, ts)` of up to `limit` messages displayed before `before`, last displayed first.
    pub async fn page(&self, thread: &Thread, before: i64, limit: u32) -> anyhow::Result<Vec<(i64, u64)>> {
        let (gk, rid) = unzip(thread);
        let rows = sqlx::query(
            "SELECT seq, ts FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND seq < ?
             ORDER BY seq DESC LIMIT ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(before)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(order_rows(rows))
    }

    /// `(seq, ts)` of the messages displayed from `from` on, in display order.
    pub async fn rows_from(&self, thread: &Thread, from: i64) -> anyhow::Result<Vec<(i64, u64)>> {
        let (gk, rid) = unzip(thread);
        let rows = sqlx::query(
            "SELECT seq, ts FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND seq >= ?
             ORDER BY seq ASC",
        )
        .bind(gk)
        .bind(rid)
        .bind(from)
        .fetch_all(&self.pool)
        .await?;
        Ok(order_rows(rows))
    }

    /// `(seq, ts)` of the stored messages sent at `tss`.
    pub async fn rows_at(&self, thread: &Thread, tss: &[u64]) -> anyhow::Result<Vec<(i64, u64)>> {
        let (gk, rid) = unzip(thread);
        let rows = sqlx::query(
            "SELECT seq, ts FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts IN (SELECT value FROM json_each(?))",
        )
        .bind(gk)
        .bind(rid)
        .bind(json_list(tss))
        .fetch_all(&self.pool)
        .await?;
        Ok(order_rows(rows))
    }

    /// `(seq, ts)` of up to `limit` messages displayed right after `after`, in display order.
    pub async fn page_newer(&self, thread: &Thread, after: i64, limit: u32) -> anyhow::Result<Vec<(i64, u64)>> {
        let (gk, rid) = unzip(thread);
        let rows = sqlx::query(
            "SELECT seq, ts FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND seq > ?
             ORDER BY seq ASC LIMIT ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(order_rows(rows))
    }

    /// How many messages are displayed within `from..to`.
    pub async fn count_rows(&self, thread: &Thread, from: i64, to: i64) -> anyhow::Result<u64> {
        let (gk, rid) = unzip(thread);
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND seq >= ? AND seq < ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }

    /// Display position of the first stored row sent at `ts` or later: where a message whose
    /// own row has another timestamp (synced) must be.
    pub async fn seq_from_ts(&self, thread: &Thread, ts: u64) -> anyhow::Result<Option<i64>> {
        let (gk, rid) = unzip(thread);
        Ok(sqlx::query_scalar(
            "SELECT seq FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts >= ?
             ORDER BY ts ASC LIMIT 1",
        )
        .bind(gk)
        .bind(rid)
        .bind(ts as i64)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Sent timestamp of the thread's first stored row sent at `ts` or later.
    pub async fn ts_from(&self, thread: &Thread, ts: u64) -> anyhow::Result<Option<u64>> {
        let (gk, rid) = unzip(thread);
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT ts FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts >= ?
             ORDER BY ts ASC LIMIT 1",
        )
        .bind(gk)
        .bind(rid)
        .bind(ts.min(i64::MAX as u64) as i64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.map(|t| t as u64))
    }

    /// Display position of a stored message.
    pub async fn seq_of(&self, thread: &Thread, ts: u64) -> anyhow::Result<Option<i64>> {
        let (gk, rid) = unzip(thread);
        Ok(sqlx::query_scalar(
            "SELECT seq FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts = ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(ts as i64)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// How many messages of the thread have a sent timestamp within [min, max].
    pub async fn count_between(&self, thread: &Thread, min: u64, max: u64) -> anyhow::Result<u64> {
        let (gk, rid) = unzip(thread);
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ls_order
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts BETWEEN ? AND ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(min as i64)
        .bind(max as i64)
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }
}

fn order_rows(rows: Vec<sqlx::sqlite::SqliteRow>) -> Vec<(i64, u64)> {
    rows.into_iter()
        .filter_map(|r| Some((r.try_get::<i64, _>("seq").ok()?, r.try_get::<i64, _>("ts").ok()? as u64)))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use presage::libsignal_service::content::{Content, DataMessage, Metadata};
    use presage::libsignal_service::protocol::ServiceId;
    use presage::store::ContentsStore;
    use presage_store_sqlite::{OnNewIdentity, SqliteStore};
    use uuid::Uuid;

    /// A real presage store on a scratch file, so the triggers meet its actual table and saves.
    pub(crate) struct Db {
        pub(crate) store: SqliteStore,
        options: SqliteConnectOptions,
        dir: std::path::PathBuf,
    }

    impl Db {
        pub(crate) async fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("sigwin-db-test-{}-{n}", std::process::id()));
            // an earlier run with the same process id may have left its DB here
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let options = SqliteConnectOptions::new().filename(dir.join("db.sqlite")).create_if_missing(true);
            let store = SqliteStore::open_with_options(options.clone(), OnNewIdentity::Trust).await.unwrap();
            Self { store, options, dir }
        }
        pub(crate) async fn index(&self) -> Index {
            Index::open(self.options.clone()).await.unwrap()
        }
        /// A row that shows nothing: what most of a 1:1 thread is.
        pub(crate) async fn save_receipt(&self, ts: u64) {
            use presage::proto::ReceiptMessage;
            let id = ServiceId::Aci(Uuid::from_u128(2).into());
            let at = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap();
            let c = Content::from_body(ReceiptMessage { r#type: Some(1), timestamp: vec![1] }, Metadata {
                sender: id,
                destination: id,
                sender_device: 1u32.try_into().unwrap(),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            });
            self.store.save_message(&thread(), c).await.unwrap();
        }
        pub(crate) async fn save(&self, ts: u64, body: &str) {
            let id = ServiceId::Aci(Uuid::from_u128(2).into());
            let dm = DataMessage { body: Some(body.into()), ..Default::default() };
            let at = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap();
            let c = Content::from_body(dm, Metadata {
                sender: id,
                destination: id,
                sender_device: 1u32.try_into().unwrap(),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            });
            self.store.save_message(&thread(), c).await.unwrap();
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn thread() -> Thread {
        Thread::Contact(ServiceId::Aci(Uuid::from_u128(2).into()))
    }

    /// Sent timestamps in display order.
    async fn order(index: &Index) -> Vec<u64> {
        index.rows_from(&thread(), 0).await.unwrap().into_iter().map(|(_, ts)| ts).collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uploads_are_reused_per_thread_while_fresh() {
        let db = Db::new().await;
        let index = db.index().await;
        index.upload_put("c:1", b"h", b"ptr", 1_000, 0).await.unwrap();
        assert_eq!(index.upload_find("c:1", b"h", 1_000).await.unwrap().as_deref(), Some(&b"ptr"[..]));
        // another thread, other bytes, or too old: no reuse
        assert!(index.upload_find("c:2", b"h", 0).await.unwrap().is_none());
        assert!(index.upload_find("c:1", b"x", 0).await.unwrap().is_none());
        assert!(index.upload_find("c:1", b"h", 1_001).await.unwrap().is_none());
        // a newer upload replaces it; putting also drops what is past keeping
        index.upload_put("c:1", b"h", b"ptr2", 2_000, 0).await.unwrap();
        index.upload_put("c:1", b"old", b"o", 10, 0).await.unwrap();
        index.upload_put("c:1", b"new", b"n", 3_000, 1_500).await.unwrap();
        assert_eq!(index.upload_find("c:1", b"h", 0).await.unwrap().as_deref(), Some(&b"ptr2"[..]));
        assert!(index.upload_find("c:1", b"old", 0).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn display_order_is_stored_order() {
        let mut db = Db::new().await;
        let index = db.index().await;
        db.save(100, "a").await;
        db.save(50, "wrong clock").await;
        db.save(200, "b").await;
        assert_eq!(order(&index).await, [100, 50, 200]);
        // saved again (a delete for everyone replaces the row): keeps its place
        db.save(100, "tombstone").await;
        assert_eq!(order(&index).await, [100, 50, 200]);
        // the thread's bottom row, not the newest timestamp
        db.save(150, "late").await;
        let row = index.threads().await.unwrap().remove(0);
        assert_eq!((row.last_ts, row.tail_ts), (200, 150));
        // removed for real: gone from the order too
        assert!(db.store.delete_message(&thread(), 200).await.unwrap());
        assert_eq!(order(&index).await, [100, 50, 150]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pages_walk_back_in_display_order() {
        let db = Db::new().await;
        let index = db.index().await;
        for ts in [10, 40, 20, 30] {
            db.save(ts, "m").await;
        }
        let first = index.page(&thread(), i64::MAX, 2).await.unwrap();
        assert_eq!(first.iter().map(|r| r.1).collect::<Vec<_>>(), [30, 20]);
        let second = index.page(&thread(), first[1].0, 2).await.unwrap();
        assert_eq!(second.iter().map(|r| r.1).collect::<Vec<_>>(), [40, 10]);
        assert!(index.page(&thread(), second[1].0, 2).await.unwrap().is_empty());
        assert_eq!(index.seq_of(&thread(), 40).await.unwrap(), Some(second[0].0));
        assert_eq!(index.seq_of(&thread(), 41).await.unwrap(), None);
        assert_eq!(index.count_between(&thread(), 20, 40).await.unwrap(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn existing_history_keeps_its_order_and_lost_triggers_come_back() {
        let mut db = Db::new().await;
        for ts in [30, 10, 20] {
            db.save(ts, "old").await;
        }
        let index = db.index().await;
        assert_eq!(order(&index).await, [10, 20, 30]);
        db.save(5, "new").await;
        // opening again changes nothing
        let index = db.index().await;
        assert_eq!(order(&index).await, [10, 20, 30, 5]);
        // what a presage migration rebuilding the table does; a save meanwhile isn't tracked
        sqlx::query("DROP TRIGGER ls_order_add").execute(&index.pool).await.unwrap();
        sqlx::query("DROP TRIGGER ls_order_del").execute(&index.pool).await.unwrap();
        db.save(7, "untracked").await;
        assert!(db.store.delete_message(&thread(), 20).await.unwrap());
        let index = db.index().await;
        assert_eq!(order(&index).await, [10, 30, 5, 7]);
        db.save(6, "tracked again").await;
        assert_eq!(order(&index).await, [10, 30, 5, 7, 6]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lookups_by_timestamp_list() {
        let db = Db::new().await;
        let index = db.index().await;
        let who = Uuid::from_u128(3);
        index.record_receipts(who, &[1, 2, 3], 1).await.unwrap();
        index.record_my_delete(2).await.unwrap();
        index.record_my_delete(4).await.unwrap();
        let tss: Vec<u64> = index.receipts_for(&[1, 3, 9]).await.unwrap().into_iter().map(|r| r.0).collect();
        assert_eq!(tss.len(), 2);
        assert!(tss.contains(&1) && tss.contains(&3));
        assert_eq!(index.my_deletes_in(&[2, 3]).await.unwrap().into_iter().collect::<Vec<_>>(), [2]);
        assert!(index.receipts_for(&[]).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn latest_edit_is_the_newest_per_sender() {
        let db = Db::new().await;
        let index = db.index().await;
        let (author, other) = (Uuid::from_u128(3), Uuid::from_u128(4));
        assert_eq!(index.latest_edit("t", 1, author).await.unwrap(), None);
        index.record_edit("t", 1, author, 20).await.unwrap();
        // arriving out of order doesn't bring an older text back
        index.record_edit("t", 1, author, 10).await.unwrap();
        index.record_edit("t", 1, other, 30).await.unwrap();
        index.record_edit("u", 1, author, 40).await.unwrap();
        assert_eq!(index.latest_edit("t", 1, author).await.unwrap(), Some(20));
        assert_eq!(index.latest_edit("t", 2, author).await.unwrap(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_newest_reaction_stands_whatever_the_arrival_order() {
        let db = Db::new().await;
        let index = db.index().await;
        let (a, b) = (Uuid::from_u128(3), Uuid::from_u128(4));
        index.record_reaction("t", 1, a, "up", 10).await.unwrap();
        index.record_reaction("t", 1, b, "heart", 5).await.unwrap();
        // replaced by a newer one; an older one arriving late changes nothing
        index.record_reaction("t", 1, a, "laugh", 20).await.unwrap();
        index.record_reaction("t", 1, a, "wow", 15).await.unwrap();
        // another thread and another message stay apart
        index.record_reaction("u", 1, a, "fire", 30).await.unwrap();
        index.record_reaction("t", 2, a, "fire", 30).await.unwrap();
        let got = index.reactions_for("t", &[1]).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[&1], [(b, "heart".to_string()), (a, "laugh".to_string())]);
        // taken back: stays gone even if the reaction it took back arrives afterwards
        index.record_reaction("t", 1, a, "", 25).await.unwrap();
        index.record_reaction("t", 1, a, "laugh", 20).await.unwrap();
        assert_eq!(index.reactions_for("t", &[1]).await.unwrap()[&1], [(b, "heart".to_string())]);
        index.forget_reactions("t", 1).await.unwrap();
        assert!(index.reactions_for("t", &[1]).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replies_are_counted_listed_and_forgotten() {
        let db = Db::new().await;
        let index = db.index().await;
        let (a, b) = (Uuid::from_u128(3), Uuid::from_u128(4));
        index.record_reply("t", 1, 10, a).await.unwrap();
        index.record_reply("t", 1, 20, b).await.unwrap();
        // twice (live, then the backfill) counts once
        index.record_reply("t", 1, 20, b).await.unwrap();
        index.record_reply("t", 2, 30, a).await.unwrap();
        index.record_reply("u", 1, 40, a).await.unwrap();
        let counts = index.reply_counts("t", &[1, 2, 3]).await.unwrap();
        assert_eq!((counts[&1], counts[&2], counts.get(&3)), (2, 1, None));
        assert_eq!(index.replies_to("t", 1, 10).await.unwrap(), [(20, b), (10, a)]);
        assert_eq!(index.replies_to("t", 1, 1).await.unwrap(), [(20, b)]);
        // a deleted reply leaves its target's count
        index.forget_replies("t", 20).await.unwrap();
        assert_eq!(index.reply_counts("t", &[1]).await.unwrap()[&1], 1);
        // a gone target takes its replies' entries with it
        index.forget_replies("t", 1).await.unwrap();
        assert!(index.reply_counts("t", &[1]).await.unwrap().is_empty());
        assert_eq!(index.reply_counts("u", &[1]).await.unwrap()[&1], 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn media_pages_go_back_in_time_by_kind() {
        let db = Db::new().await;
        let index = db.index().await;
        let (m, f) = (MediaKind::Media, MediaKind::Files);
        index.record_media("t", 10, &[(0, m), (1, m), (2, f)]).await.unwrap();
        index.record_media("t", 20, &[(0, m)]).await.unwrap();
        // twice (live, then the backfill) counts once
        index.record_media("t", 20, &[(0, m)]).await.unwrap();
        index.record_media("t", 30, &[(1, m)]).await.unwrap();
        index.record_media("u", 40, &[(0, m)]).await.unwrap();
        let first = index.media_page("t", m, None, 2).await.unwrap();
        assert_eq!(first, [(30, 1), (20, 0)]);
        // a page can end inside a message
        let next = index.media_page("t", m, first.last().copied(), 1).await.unwrap();
        assert_eq!(next, [(10, 0)]);
        assert_eq!(index.media_page("t", m, Some((10, 0)), 10).await.unwrap(), [(10, 1)]);
        assert_eq!(index.media_page("t", f, None, 10).await.unwrap(), [(10, 2)]);
        index.forget_media("t", 10).await.unwrap();
        assert_eq!(index.media_page("t", m, None, 10).await.unwrap(), [(30, 1), (20, 0)]);
        assert!(index.media_page("t", f, None, 10).await.unwrap().is_empty());
        index.rekey_thread("t", "v").await.unwrap();
        assert_eq!(index.media_page("v", m, None, 10).await.unwrap().len(), 2);
        index.forget_thread("v").await.unwrap();
        assert!(index.media_page("v", m, None, 10).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_packs_last_as_long_as_their_messages() {
        let db = Db::new().await;
        let index = db.index().await;
        let ids = |(packs, _): (Vec<SharedPack>, Vec<Vec<u8>>)| packs.into_iter().map(|p| p.id[0]).collect::<Vec<_>>();
        index.record_sticker_msg("t", 10, &[1], &[11]).await.unwrap();
        index.record_sticker_msg("t", 20, &[2], &[12]).await.unwrap();
        index.record_sticker_msg("u", 30, &[1], &[11]).await.unwrap();
        // most recently sent first, each pack once
        assert_eq!(ids(index.shared_packs().await.unwrap()), [1, 2]);
        // a later sticker with another key comes first, until a key opened the pack
        index.record_sticker_msg("u", 40, &[2], &[99]).await.unwrap();
        assert_eq!(index.shared_pack_keys(&[2]).await.unwrap(), [vec![99], vec![12]]);
        index.cache_shared_pack(&[2], &[12], "Two", "Someone", Some(5)).await.unwrap();
        assert_eq!(index.shared_pack_keys(&[2]).await.unwrap(), [vec![12]]);
        index.forget_media("u", 40).await.unwrap();
        let (packs, _) = index.shared_packs().await.unwrap();
        assert_eq!(packs[1].names, Some(("Two".into(), "Someone".into())));
        assert_eq!((packs[0].names.is_none(), packs[1].cover), (true, Some(5)));
        // its only message gone, the pack and its cache go too
        index.forget_media("t", 20).await.unwrap();
        let (packs, gone) = index.shared_packs().await.unwrap();
        assert_eq!((packs.len(), gone), (1, vec![vec![2]]));
        assert!(index.shared_pack_keys(&[2]).await.unwrap().is_empty());
        index.rekey_thread("u", "v").await.unwrap();
        index.forget_thread("t").await.unwrap();
        assert_eq!(ids(index.shared_packs().await.unwrap()), [1]);
        index.forget_thread("v").await.unwrap();
        assert!(ids(index.shared_packs().await.unwrap()).is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_window_can_be_read_anywhere_in_the_thread() {
        let db = Db::new().await;
        let index = db.index().await;
        for ts in [10, 20, 30, 40, 50] {
            db.save(ts, "x").await;
        }
        let seq = |ts| {
            let (index, thread) = (index.clone(), thread());
            async move { index.seq_of(&thread, ts).await.unwrap().unwrap() }
        };
        let (s20, s40) = (seq(20).await, seq(40).await);
        let tss = |rows: Vec<(i64, u64)>| rows.into_iter().map(|r| r.1).collect::<Vec<_>>();
        let mut at = tss(index.rows_at(&thread(), &[40, 20, 99]).await.unwrap());
        at.sort();
        assert_eq!(at, [20, 40]);
        assert_eq!(tss(index.page_newer(&thread(), s20, 2).await.unwrap()), [30, 40]);
        assert_eq!(tss(index.page_newer(&thread(), s40, 2).await.unwrap()), [50]);
        assert_eq!(index.count_rows(&thread(), s20, s40).await.unwrap(), 2);
        assert_eq!(index.seq_from_ts(&thread(), 15).await.unwrap(), Some(s20));
        assert_eq!(index.seq_from_ts(&thread(), 40).await.unwrap(), Some(s40));
        assert_eq!(index.seq_from_ts(&thread(), 51).await.unwrap(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edits_for_lists_the_targets_edits_only() {
        let db = Db::new().await;
        let index = db.index().await;
        let (a, b) = (Uuid::from_u128(3), Uuid::from_u128(4));
        index.record_edit("t", 1, a, 20).await.unwrap();
        index.record_edit("t", 1, b, 21).await.unwrap();
        index.record_edit("t", 2, a, 22).await.unwrap();
        index.record_edit("u", 1, a, 23).await.unwrap();
        let mut got = index.edits_for("t", &[1]).await.unwrap();
        got.sort_by_key(|e| e.2);
        assert_eq!(got, [(1, a, 20), (1, b, 21)]);
    }

    /// `ContentExt::timestamp` of an edit is its target's, but the row is stored under the
    /// metadata's: `ls_edits` must hold the latter to read the edit back.
    #[tokio::test(flavor = "current_thread")]
    async fn an_edit_is_stored_under_its_own_timestamp() {
        use presage::libsignal_service::content::ContentBody;
        use presage::proto::EditMessage;
        use presage::store::ContentExt;
        let db = Db::new().await;
        db.save(100, "original").await;
        let id = ServiceId::Aci(Uuid::from_u128(2).into());
        let at = chrono::DateTime::from_timestamp_millis(300).unwrap();
        let edit = EditMessage {
            target_sent_timestamp: Some(100),
            data_message: Some(DataMessage { body: Some("edited".into()), ..Default::default() }),
        };
        let c = Content::from_body(edit, Metadata {
            sender: id,
            destination: id,
            sender_device: 1u32.try_into().unwrap(),
            pni_verified: None,
            client_timestamp: at,
            server_timestamp: at,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        });
        assert_eq!(c.timestamp(), 100);
        db.store.save_message(&thread(), c).await.unwrap();
        let stored = db.store.message(&thread(), 300).await.unwrap().unwrap();
        assert!(matches!(stored.body, ContentBody::EditMessage(_)));
        let original = db.store.message(&thread(), 100).await.unwrap().unwrap();
        assert!(matches!(original.body, ContentBody::DataMessage(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schema_version_is_recorded_and_kept() {
        let db = Db::new().await;
        let index = db.index().await;
        let v: i64 = sqlx::query_scalar("SELECT value FROM ls_meta WHERE key = 'schema'")
            .fetch_one(&index.pool)
            .await
            .unwrap();
        assert_eq!(v, SCHEMA);
        // running again (every start) changes nothing
        migrate(&index.pool).await.unwrap();
        // a DB from a newer build is left alone
        sqlx::query("UPDATE ls_meta SET value = 99 WHERE key = 'schema'").execute(&index.pool).await.unwrap();
        migrate(&index.pool).await.unwrap();
        let v: i64 = sqlx::query_scalar("SELECT value FROM ls_meta WHERE key = 'schema'")
            .fetch_one(&index.pool)
            .await
            .unwrap();
        assert_eq!(v, 99);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn packs_from_schema_1_get_their_author() {
        let db = Db::new().await;
        let index = db.index().await;
        let pool = &index.pool;
        sqlx::query("DROP TABLE ls_packs").execute(pool).await.unwrap();
        sqlx::query("CREATE TABLE ls_packs (id BLOB PRIMARY KEY, title TEXT NOT NULL, cover INTEGER) WITHOUT ROWID")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO ls_packs (id, title, cover) VALUES (x'01', 'old', 1)").execute(pool).await.unwrap();
        sqlx::query("UPDATE ls_meta SET value = 1 WHERE key = 'schema'").execute(pool).await.unwrap();
        migrate(pool).await.unwrap();
        // a cache: emptied, the picker fills it again from presage
        assert!(index.cached_packs().await.unwrap().is_empty());
        index.cache_pack(&[2], "new", "someone", Some(3)).await.unwrap();
        let packs = index.cached_packs().await.unwrap();
        assert_eq!(packs, vec![(vec![2], ("new".to_string(), "someone".to_string()), Some(3))]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recent_stickers_keep_the_newest_of_installed_packs() {
        let db = Db::new().await;
        let index = db.index().await;
        for id in [1u8, 2] {
            sqlx::query("INSERT INTO sticker_packs (id, key, manifest) VALUES (?, x'00', x'00')").bind(vec![id]).execute(&index.pool).await.unwrap();
        }
        index.cache_stickers(&[1], &[(7, "😀".into())]).await.unwrap();
        index.use_sticker(&[1], 7, 100, 3).await.unwrap();
        index.use_sticker(&[2], 1, 200, 3).await.unwrap();
        index.use_sticker(&[2], 2, 300, 3).await.unwrap();
        // sent again: first, not twice
        index.use_sticker(&[1], 7, 400, 3).await.unwrap();
        // past `keep`, the oldest goes
        index.use_sticker(&[2], 3, 500, 3).await.unwrap();
        let ids = |r: Vec<(Vec<u8>, u32, String)>| r.into_iter().map(|(p, id, _)| (p[0], id)).collect::<Vec<_>>();
        let recent = index.recent_stickers().await.unwrap();
        assert_eq!(recent[1].2, "😀");
        assert_eq!(ids(recent), vec![(2, 3), (1, 7), (2, 2)]);
        // forgotten: only that one goes
        index.forget_recent_sticker(&[2], 3).await.unwrap();
        assert_eq!(ids(index.recent_stickers().await.unwrap()), vec![(1, 7), (2, 2)]);
        // a pack removed elsewhere shows none, and dropping its cache forgets them
        sqlx::query("DELETE FROM sticker_packs WHERE id = x'02'").execute(&index.pool).await.unwrap();
        assert_eq!(ids(index.recent_stickers().await.unwrap()), vec![(1, 7)]);
        index.uncache_pack(&[2]).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ls_sticker_recent").fetch_one(&index.pool).await.unwrap();
        assert_eq!(left, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn made_packs_outlive_their_install() {
        let db = Db::new().await;
        let index = db.index().await;
        let made = |id: u8, cover: Option<Vec<u8>>| MadePack { id: vec![id], key: vec![id; 32], title: format!("t{id}"), author: "me".into(), cover };
        index.add_made_pack(&made(1, Some(vec![9])), 100).await.unwrap();
        index.add_made_pack(&made(2, None), 200).await.unwrap();
        let ids = |r: Vec<MadePack>| r.into_iter().map(|p| (p.id[0], p.cover)).collect::<Vec<_>>();
        assert_eq!(ids(index.made_packs().await.unwrap()), vec![(2, None), (1, Some(vec![9]))]);
        // installed, then removed: the link stays
        index.uncache_pack(&[1]).await.unwrap();
        assert_eq!(index.made_pack_key(&[1]).await.unwrap(), Some(vec![1; 32]));
        index.forget_made_pack(&[1]).await.unwrap();
        assert_eq!(index.made_pack_key(&[1]).await.unwrap(), None);
        assert_eq!(ids(index.made_packs().await.unwrap()), vec![(2, None)]);
    }
}
