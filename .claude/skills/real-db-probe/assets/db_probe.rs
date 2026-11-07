// Read-only probe of the real account DB. Temporary: lives in examples/ only while it runs, never
// commit it. Prints counts and ages only: no text, names, numbers, UUIDs, keys or thread keys.
// Numeric `thread_id`s are shown as list positions, so output says nothing that outlives the run.
use std::collections::BTreeMap;

use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{ConnectOptions, Row};

fn unprotect(data: &[u8]) -> Vec<u8> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    // SAFETY: input points to `data` for the call; DPAPI allocates the output, released below.
    unsafe {
        assert!(CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) != 0);
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        v
    }
}

/// The real DB, read-only. The key stays in this function: never print it or the options.
async fn connect() -> anyhow::Result<SqliteConnection> {
    let dir = std::path::PathBuf::from(std::env::var("LOCALAPPDATA")?).join("Sigwin").join("data");
    let pass = String::from_utf8(unprotect(&std::fs::read(dir.join("db.key"))?))?;
    Ok(SqliteConnectOptions::new()
        .filename(dir.join("signal.db3"))
        .read_only(true)
        .pragma("key", format!("\"x'{pass}'\""))
        .disable_statement_logging()
        .connect()
        .await?)
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let (mut v, mut s) = (0u64, 0);
    loop {
        let (x, r) = b.split_first()?;
        *b = r;
        v |= ((*x & 0x7f) as u64) << s;
        if *x < 0x80 {
            return Some(v);
        }
        s += 7;
        if s > 63 {
            return None;
        }
    }
}

/// Protobuf fields of a message: (number, payload). Payloads are for walking down, not printing.
fn fields(mut b: &[u8]) -> Vec<(u64, &[u8])> {
    let mut out = vec![];
    while !b.is_empty() {
        let Some(tag) = varint(&mut b) else { break };
        match tag & 7 {
            0 => {
                if varint(&mut b).is_none() {
                    break;
                }
                out.push((tag >> 3, &b[..0]));
            }
            1 if b.len() >= 8 => b = &b[8..],
            5 if b.len() >= 4 => b = &b[4..],
            2 => {
                let Some(n) = varint(&mut b) else { break };
                let n = n as usize;
                if b.len() < n {
                    break;
                }
                out.push((tag >> 3, &b[..n]));
                b = &b[n..];
            }
            _ => break,
        }
    }
    out
}

/// What a stored `content_body` is. Close to `view::classify`, from field numbers alone.
fn kind(body: &[u8]) -> &'static str {
    match fields(body).first() {
        Some((1, d)) if fields(d).iter().any(|f| f.0 == 16) => "reaction",
        Some((1, _)) => "message",
        Some((2, d)) => match fields(d).first() {
            Some((1, _)) => "sent-sync",
            _ => "other-sync",
        },
        Some((3, _)) => "call",
        Some((4, _)) => "deleted",
        Some((5, _)) => "receipt",
        Some((6, _)) => "typing",
        Some((11, _)) => "edit",
        Some(_) => "other",
        None => "empty",
    }
}

/// Rows that show in the message list, by `kind`.
fn shows(kind: &str) -> bool {
    matches!(kind, "message" | "sent-sync" | "deleted")
}

fn age_hours(ts: i64) -> f64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    ((now - ts) as f64 / 3.6e5).round() / 10.0
}

struct ThreadRef {
    /// the DB's id, for queries only: print `pos`
    id: i64,
    pos: usize,
    group: bool,
    rows: i64,
}

/// Threads by stored rows, most first.
async fn threads(c: &mut SqliteConnection) -> anyhow::Result<Vec<ThreadRef>> {
    let rows = sqlx::query(
        "SELECT o.thread_id, t.group_master_key IS NOT NULL, COUNT(*) FROM ls_order o JOIN threads t ON t.id = o.thread_id
         GROUP BY o.thread_id ORDER BY COUNT(*) DESC",
    )
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .iter()
        .enumerate()
        .map(|(pos, r)| ThreadRef { id: r.get(0), pos, group: r.get::<i64, _>(1) == 1, rows: r.get(2) })
        .collect())
}

/// Kinds of a thread's rows, newest first: what most questions about a thread come down to.
async fn kinds_newest_first(c: &mut SqliteConnection, thread: i64) -> anyhow::Result<Vec<&'static str>> {
    let rows = sqlx::query(
        "SELECT m.content_body FROM ls_order o JOIN thread_messages m ON m.ts = o.ts AND m.thread_id = o.thread_id
         WHERE o.thread_id = ? ORDER BY o.seq DESC",
    )
    .bind(thread)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows.iter().map(|r| kind(&r.get::<Vec<u8>, _>(0))).collect())
}

/// The question being asked goes here. This default is an overview of every thread.
async fn report(c: &mut SqliteConnection) -> anyhow::Result<()> {
    println!("pos kind  rows | shown in newest 600 | shown in all | kinds");
    for t in threads(c).await?.iter().take(14) {
        let kinds = kinds_newest_first(c, t.id).await?;
        let shown = |n: usize| kinds.iter().take(n).filter(|k| shows(k)).count();
        let mut by_kind = BTreeMap::<&str, u32>::new();
        for k in &kinds {
            *by_kind.entry(k).or_default() += 1;
        }
        let newest: Option<i64> = sqlx::query_scalar("SELECT MAX(ts) FROM ls_order WHERE thread_id = ?").bind(t.id).fetch_one(&mut *c).await?;
        println!(
            "#{:<2} {} {:>6} | {:>4} | {:>5} | newest {:?}h ago, {by_kind:?}",
            t.pos,
            if t.group { "group" } else { "1:1  " },
            t.rows,
            shown(600),
            shown(usize::MAX),
            newest.map(age_hours),
        );
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut c = connect().await?;
    report(&mut c).await?;
    // tearing the connection down crashes on exit (0xc0000005) after a good run: skip it
    std::process::exit(0);
}
