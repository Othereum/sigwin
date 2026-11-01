//! Small read-only queries on presage's sqlite schema that the `Store` trait doesn't offer:
//! listing threads by recency and paging message timestamps without loading whole threads.

use presage::store::Thread;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

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
        Ok(Self { pool })
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
