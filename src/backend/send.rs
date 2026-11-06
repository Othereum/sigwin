//! Sending messages and attachments; pending rows until presage saves them.

use super::*;

impl App {
    pub(super) async fn send(&mut self, text: String, files: Vec<PathBuf>, quote_id: Option<String>, mentions: Vec<crate::mention::Member>, link: Option<String>) {
        let link = self.link_for_send(link);
        if self.manager.is_none() {
            return;
        }
        let Some(open) = self.current.as_ref() else { return };
        let text = text.trim_end().to_string();
        if text.trim().is_empty() && files.is_empty() {
            return;
        }
        let key = open.key.clone();
        let quote = quote_id.and_then(|id| id.parse::<u64>().ok()).and_then(|qts| {
            let (_, c) = open.window.iter().find(|(t, _)| *t == qts)?;
            let Kind::Msg { dm, .. } = view::classify(c, self.me) else { return None };
            Some(Quote {
                id: Some(qts),
                author_aci: Some(c.metadata.sender.raw_uuid().to_string()),
                text: dm.body.clone(),
                // keeps the quoted @mentions readable on the other side
                body_ranges: dm.body_ranges.clone(),
                ..Default::default()
            })
        });
        let names = &self.names;
        let name_of = |u: &Uuid| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into());
        // same labels as view::build gives the stored row
        let (quote_label, quote_author) = quote
            .as_ref()
            .map(|q| {
                let label = q
                    .text
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| view::with_mentions(t, &q.body_ranges, &name_of))
                    .unwrap_or_else(|| "(첨부파일)".into());
                (label, view::quote_author_name(q, self.me, &name_of))
            })
            .unwrap_or_default();
        let (body, body_ranges) = crate::mention::encode(&text, &mentions);
        if body.len() > view::MAX_LONG_TEXT_BYTES {
            self.notice("메시지가 너무 깁니다 (최대 64KB)");
            return;
        }
        // the message itself ends our typing indicator on the other side
        self.composing = None;
        let ts = self.next_send_ts();
        let attachment = match files.as_slice() {
            [] => String::new(),
            [one] => file_label(one),
            [first, rest @ ..] => format!("{} 외 {}개", file_label(first), rest.len()),
        };
        self.pending.push(Pending {
            ts,
            thread_key: key,
            text: text.clone(),
            quote: quote_label,
            quote_author,
            quote_id: quote.as_ref().and_then(|q| q.id).map(|i| i.to_string()).unwrap_or_default(),
            attachment,
            image_key: String::new(),
            link: link.card(),
            error: None,
            after_seq: None,
            job: SendJob::Text { body, body_ranges, files, quote, link },
        });
        self.emit_messages(Scroll::Bottom).await;
        self.start_send(ts).await;
    }

    /// Queues the send of the pending row `ts`.
    pub(super) async fn start_send(&mut self, ts: u64) {
        let Some(manager) = self.manager.clone() else { return };
        let Some(p) = self.pending.iter().find(|p| p.ts == ts) else { return };
        let Some(thread) = parse_thread_key(&p.thread_key) else { return };
        let (key, job) = (p.thread_key.clone(), p.job.clone());
        let group_revision = self.group_revision(&thread).await;
        let tx = self.tx.clone();
        let dir = self.paths.attachments.clone();
        let (turn, done) = self.send_turn(&key);
        tokio::task::spawn_local(async move {
            let _done = done;
            if let Some(turn) = turn {
                let _ = turn.await;
            }
            let started = std::time::Instant::now();
            let res = send_job(manager, thread, job, group_revision, ts, dir).await;
            info!(ms = started.elapsed().as_millis() as u64, ok = res.is_ok(), "message sent");
            let _ = tx.send(Internal::Sent(ts, res.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Sends a failed row again with its timestamp, the message's id: recipients who got it
    /// the first time drop the copy. It goes to the bottom again while it's on its way.
    pub(super) async fn retry_send(&mut self, id: &str) {
        let Ok(ts) = id.parse::<u64>() else { return };
        let Some(i) = self.pending.iter().position(|p| p.ts == ts && p.error.is_some()) else { return };
        let mut p = self.pending.remove(i);
        p.error = None;
        p.after_seq = None;
        self.pending.push(p);
        self.emit_messages(Scroll::Bottom).await;
        self.start_send(ts).await;
    }

    pub(super) async fn discard_failed(&mut self, id: &str) {
        let Ok(ts) = id.parse::<u64>() else { return };
        self.pending.retain(|p| p.ts != ts || p.error.is_none());
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
    }

    /// A thread's messages go out one at a time: they're displayed in the order presage stores
    /// them, after each send, and recipients get them in the order the server took them.
    /// Returns what to wait for before sending, and what to drop when done.
    pub(super) fn send_turn(&mut self, key: &str) -> (Option<oneshot::Receiver<()>>, oneshot::Sender<()>) {
        let (done, tail) = oneshot::channel();
        (self.send_tail.insert(key.to_string(), tail), done)
    }

    /// Unique, increasing send timestamps (they are the message ids).
    pub(super) fn next_send_ts(&self) -> u64 {
        let last = self.pending.iter().map(|p| p.ts).chain(self.ops.iter().map(|o| o.ts)).max().unwrap_or(0);
        now_ms().max(last + 1)
    }
}

/// What a pending row sends.
#[derive(Clone)]
pub(super) enum SendJob {
    Text { body: String, body_ranges: Vec<BodyRange>, files: Vec<PathBuf>, quote: Option<Quote>, link: LinkSend },
    Sticker(StickerSend),
}

/// Tries when the server limits our rate. A burst gets through at the rate the server allows:
/// it's a matter of seconds, a longer wait is reported as a failure.
const RATE_LIMIT_RETRIES: u32 = 5;
const RATE_LIMIT_MAX_WAIT: Duration = Duration::from_secs(30);

/// How long the server asks us to wait, if that's why the send failed.
fn rate_limit_wait(e: &anyhow::Error) -> Option<Duration> {
    use presage::libsignal_service::prelude::{MessageSenderError, ServiceError};
    let service = match e.downcast_ref::<presage::Error<presage_store_sqlite::SqliteStoreError>>()? {
        presage::Error::ServiceError(s) => s,
        presage::Error::MessageSenderError(m) => match m.as_ref() {
            MessageSenderError::ServiceError(s) => s,
            _ => return None,
        },
        _ => return None,
    };
    let ServiceError::RateLimitExceeded { retry_after } = service else { return None };
    let wait = retry_after.and_then(|d| d.to_std().ok()).unwrap_or(Duration::from_secs(1));
    (wait <= RATE_LIMIT_MAX_WAIT).then_some(wait)
}

async fn send_job(m: Mgr, thread: Thread, job: SendJob, group_revision: u32, ts: u64, dir: PathBuf) -> anyhow::Result<()> {
    let mut tries = 0;
    loop {
        let res = match job.clone() {
            SendJob::Text { body, body_ranges, files, quote, link } => {
                Box::pin(send_task(m.clone(), thread.clone(), body, body_ranges, files, quote, link, group_revision, ts, dir.clone())).await
            }
            SendJob::Sticker(s) => Box::pin(send_sticker_task(m.clone(), thread.clone(), s, group_revision, dir.clone(), ts)).await,
        };
        let Err(e) = &res else { return res };
        match rate_limit_wait(e) {
            Some(wait) if tries < RATE_LIMIT_RETRIES => {
                tries += 1;
                warn!(ts, tries, ms = wait.as_millis() as u64, "send rate limited, waiting");
                tokio::time::sleep(wait).await;
            }
            _ => return res,
        }
    }
}

/// Where a failed row stored right after `after_seq` goes among `items`: above the first row
/// stored later. `seqs` maps the window's timestamps to their display positions.
pub(super) fn failed_row_index(items: &[MsgDto], seqs: &HashMap<u64, i64>, after_seq: i64) -> usize {
    items.iter().position(|m| seqs.get(&m.ts).is_some_and(|s| *s > after_seq)).unwrap_or(items.len())
}

pub(super) async fn send_task(
    mut m: Mgr,
    thread: Thread,
    text: String,
    body_ranges: Vec<BodyRange>,
    files: Vec<PathBuf>,
    quote: Option<Quote>,
    link: LinkSend,
    group_revision: u32,
    ts: u64,
    attachments_dir: PathBuf,
) -> anyhow::Result<()> {
    let mut text = text;
    let long = long_body(&mut m, &mut text, &attachments_dir).await?;
    let mut specs = Vec::new();
    for path in &files {
        let data = std::fs::read(path).with_context(|| format!("{} 읽기 실패", path.display()))?;
        let ct = mime_guess::from_path(path).first_or_octet_stream().to_string();
        let (width, height) = if ct.starts_with("image/") {
            image::image_dimensions(path).map(|(w, h)| (Some(w), Some(h))).unwrap_or((None, None))
        } else {
            (None, None)
        };
        specs.push((
            AttachmentSpec {
                content_type: ct,
                length: data.len(),
                file_name: path.file_name().map(|s| s.to_string_lossy().to_string()),
                preview: None,
                voice_note: None,
                borderless: None,
                width,
                height,
                caption: None,
                blur_hash: None,
            },
            data,
        ));
    }
    let mut attachments: Vec<AttachmentPointer> = long.into_iter().collect();
    if !specs.is_empty() {
        attachments.extend(m.upload_attachments(specs).await?.into_iter().collect::<Result<Vec<_>, _>>()?);
    }
    let preview = Box::pin(for_message(&m, &text, !files.is_empty(), link, ts, &attachments_dir)).await;
    let dm = DataMessage {
        body: (!text.is_empty()).then_some(text),
        body_ranges,
        attachments,
        preview,
        quote,
        timestamp: Some(ts),
        ..Default::default()
    };
    deliver(&mut m, thread, dm.into(), ts, group_revision).await
}

/// Signal's long messages: past 2KB the body keeps only its start and the full text goes as a
/// `text/x-signal-plain` attachment (first in the list, as Signal Desktop sends it). The text is
/// cached right away, so our own copy shows in full without downloading it back.
pub(super) async fn long_body(m: &mut Mgr, body: &mut String, attachments_dir: &Path) -> anyhow::Result<Option<AttachmentPointer>> {
    if body.len() <= view::MAX_BODY_BYTES {
        return Ok(None);
    }
    let data = body.as_bytes().to_vec();
    let spec = AttachmentSpec {
        content_type: view::LONG_TEXT.into(),
        length: data.len(),
        file_name: None,
        preview: None,
        voice_note: None,
        borderless: None,
        width: None,
        height: None,
        caption: None,
        blur_hash: None,
    };
    let ptr = m.upload_attachments(vec![(spec, data.clone())]).await?.into_iter().next().context("no upload result")??;
    let path = super::files::attachment_path(attachments_dir, &view::attachment_key(&ptr, 0, 0));
    if let Err(e) = sigwin_vault::write(&path, &data) {
        warn!("cache long text: {e}");
    }
    *body = view::trim_body(body, view::MAX_BODY_BYTES).to_string();
    Ok(Some(ptr))
}

pub(super) fn file_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

pub(super) fn pending_row(p: &Pending, prev_ts: Option<u64>) -> MsgDto {
    let mut m = view::outgoing_stub(p.ts, prev_ts);
    (m.text, m.styles) = crate::format::parse(&p.text);
    m.quote = p.quote.clone();
    m.quote_author = p.quote_author.clone();
    m.quote_id = p.quote_id.clone();
    m.attachment = p.attachment.clone();
    m.link = p.link.clone();
    if !p.image_key.is_empty() {
        m.image_key = p.image_key.clone();
        m.img_w = view::STICKER_SIZE;
        m.img_h = view::STICKER_SIZE;
        m.sticker = true;
    }
    m.big_emoji = !m.sticker
        && m.attachment.is_empty()
        && m.quote.is_empty()
        && crate::emoji::only_emoji(&m.text, 5).is_some();
    match &p.error {
        None => {
            m.status = "전송 중".into();
            m.receipt_info = "서버로 보내는 중입니다.".into();
        }
        Some(e) => {
            m.status = "실패".into();
            m.failed = true;
            m.receipt_info = format!("보내지 못했습니다. 일부 사람에게는 전달되었을 수 있습니다.\n{e}");
        }
    }
    m
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) async fn deliver(m: &mut Mgr, thread: Thread, mut body: ContentBody, ts: u64, group_revision: u32) -> anyhow::Result<()> {
    match thread {
        Thread::Contact(id) => m.send_message(id, body, ts).await?,
        Thread::Group(key) => {
            let ctx = GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(group_revision), ..Default::default() };
            match &mut body {
                ContentBody::DataMessage(dm) => dm.group_v2 = Some(ctx),
                ContentBody::EditMessage(EditMessage { data_message: Some(dm), .. }) => dm.group_v2 = Some(ctx),
                _ => {}
            }
            let started = std::time::Instant::now();
            m.send_message_to_group(&key, body, ts).await?;
            debug!(ms = started.elapsed().as_millis() as u64, "group send done");
        }
    }
    Ok(())
}

/// Identifier of a group as typing messages carry it.
pub(super) fn group_identifier(master_key: &[u8; 32]) -> [u8; 32] {
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key)).get_group_identifier()
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::prelude::{MessageSenderError, ServiceError};
    use presage_store_sqlite::SqliteStoreError;

    fn sender_error(s: ServiceError) -> anyhow::Error {
        presage::Error::<SqliteStoreError>::MessageSenderError(Box::new(MessageSenderError::ServiceError(s))).into()
    }

    #[test]
    fn rate_limits_are_waited_out_unless_long() {
        let limited = |secs| sender_error(ServiceError::RateLimitExceeded { retry_after: Some(chrono::Duration::seconds(secs)) });
        assert_eq!(rate_limit_wait(&limited(1)), Some(Duration::from_secs(1)));
        assert_eq!(rate_limit_wait(&limited(3600)), None);
        let unspecified = sender_error(ServiceError::RateLimitExceeded { retry_after: None });
        assert_eq!(rate_limit_wait(&unspecified), Some(Duration::from_secs(1)));
        assert_eq!(rate_limit_wait(&sender_error(ServiceError::Unauthorized)), None);
        // wrapped on its way up, as the send path may do
        assert_eq!(rate_limit_wait(&limited(1).context("sending")), Some(Duration::from_secs(1)));
        assert_eq!(rate_limit_wait(&anyhow::anyhow!("other")), None);
    }

    #[test]
    fn failed_rows_stay_below_what_was_stored_before_them() {
        let row = |ts| MsgDto { ts, ..Default::default() };
        // display order isn't timestamp order; 25 is an earlier failed row, not stored
        let items = [row(30), row(10), row(25), row(20)];
        let seqs = HashMap::from([(30, 1), (10, 2), (20, 3)]);
        assert_eq!(failed_row_index(&items, &seqs, 0), 0);
        assert_eq!(failed_row_index(&items, &seqs, 1), 1);
        // after the earlier failed row anchored at the same place
        assert_eq!(failed_row_index(&items, &seqs, 2), 3);
        assert_eq!(failed_row_index(&items, &seqs, 3), 4);
    }
}
