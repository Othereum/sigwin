//! Small read-only queries on presage's sqlite schema that the `Store` trait doesn't offer:
//! listing threads by recency and paging message timestamps without loading whole threads.

use presage::store::Thread;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

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
        Ok(Self { pool })
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
