//! Small read-only queries on presage's sqlite schema that the `Store` trait doesn't offer:
//! listing threads by recency and paging message timestamps without loading whole threads.

use presage::store::Thread;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// Version of our `ls_*` tables (presage's own tables migrate themselves). The `CREATE TABLE`
/// statements in `Index::open` stay at version 1 forever; any later change is a step in
/// `migrate` with `SCHEMA` bumped, so a fresh DB and an auto-updated old one take the same path.
/// Kept in our `ls_meta` table rather than `PRAGMA user_version`, which the DB shares with presage.
const SCHEMA: i64 = 1;

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
async fn step(_pool: &SqlitePool, v: i64) -> anyhow::Result<i64> {
    match v {
        // 1 => { sqlx::query("ALTER TABLE ls_pins ADD COLUMN ...").execute(pool).await?; Ok(2) }
        _ => anyhow::bail!("db: no migration from schema {v}"),
    }
}

#[derive(Clone)]
pub struct Index {
    pool: SqlitePool,
}

pub struct ThreadRow {
    pub thread: Thread,
    pub last_ts: u64,
}

fn unzip(thread: &Thread) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    match thread {
        Thread::Group(key) => (Some(key.to_vec()), None),
        // presage stores the raw uuid as a 16-byte blob
        Thread::Contact(id) => (None, Some(id.raw_uuid().as_bytes().to_vec())),
    }
}

impl Index {
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
        // title and cover of installed sticker packs: presage keeps every sticker's bytes in one
        // JSON column, so listing packs from its table means reading all of them
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ls_packs (
                id BLOB PRIMARY KEY,
                title TEXT NOT NULL,
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
        migrate(&pool).await?;
        Ok(Self { pool })
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

    /// Ids of installed sticker packs. Reads the id column only, not the manifests.
    pub async fn pack_ids(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        let rows = sqlx::query("SELECT id FROM sticker_packs").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().filter_map(|r| r.try_get("id").ok()).collect())
    }

    /// Cached pack metadata: (id, title, cover sticker id).
    pub async fn cached_packs(&self) -> anyhow::Result<Vec<(Vec<u8>, String, Option<u32>)>> {
        let rows = sqlx::query("SELECT id, title, cover FROM ls_packs").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let cover: Option<i64> = r.try_get("cover").ok()?;
                Some((r.try_get("id").ok()?, r.try_get("title").ok()?, cover.map(|c| c as u32)))
            })
            .collect())
    }

    pub async fn cache_pack(&self, id: &[u8], title: &str, cover: Option<u32>) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO ls_packs (id, title, cover) VALUES (?, ?, ?)")
            .bind(id)
            .bind(title)
            .bind(cover.map(|c| c as i64))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn uncache_pack(&self, id: &[u8]) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ls_packs WHERE id = ?").bind(id).execute(&self.pool).await?;
        sqlx::query("DELETE FROM ls_stickers WHERE pack = ?").bind(id).execute(&self.pool).await?;
        Ok(())
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

    /// Our messages deleted for everyone, sent within [from, to].
    pub async fn my_deletes_between(&self, from: u64, to: u64) -> anyhow::Result<std::collections::HashSet<u64>> {
        let rows = sqlx::query("SELECT ts FROM ls_deleted_mine WHERE ts BETWEEN ? AND ?")
            .bind(from as i64)
            .bind(to as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().filter_map(|r| r.try_get::<i64, _>("ts").ok()).map(|t| t as u64).collect())
    }

    /// Receipts for messages sent within [from, to].
    pub async fn receipts_between(&self, from: u64, to: u64) -> anyhow::Result<Vec<(u64, uuid::Uuid, u8)>> {
        let rows = sqlx::query("SELECT target_ts, who, level FROM ls_receipts WHERE target_ts BETWEEN ? AND ?")
            .bind(from as i64)
            .bind(to as i64)
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
            "SELECT t.group_master_key AS gk, t.recipient_id AS rid, MAX(m.ts) AS last_ts
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
            let thread = match (gk, rid) {
                (Some(gk), _) => match gk.try_into() {
                    Ok(key) => Thread::Group(key),
                    Err(_) => continue,
                },
                (None, Some(rid)) => match uuid::Uuid::from_slice(&rid) {
                    Ok(u) => match crate::types::parse_thread_key(&format!("c:{u}")) {
                        Some(t) => t,
                        None => continue,
                    },
                    Err(_) => continue,
                },
                _ => continue,
            };
            out.push(ThreadRow { thread, last_ts: last_ts as u64 });
        }
        Ok(out)
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

    /// Timestamps of up to `limit` messages strictly older than `before`, newest first.
    pub async fn page(&self, thread: &Thread, before: u64, limit: u32) -> anyhow::Result<Vec<u64>> {
        let (gk, rid) = unzip(thread);
        let rows = sqlx::query(
            "SELECT ts FROM thread_messages
             WHERE thread_id = (SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?)
               AND ts < ?
             ORDER BY ts DESC LIMIT ?",
        )
        .bind(gk)
        .bind(rid)
        .bind(before.min(i64::MAX as u64) as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get::<i64, _>("ts").ok())
            .map(|t| t as u64)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[tokio::test(flavor = "current_thread")]
    async fn schema_version_is_recorded_and_kept() {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let index = Index::open(opts).await.unwrap();
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
}
