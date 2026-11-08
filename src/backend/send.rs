//! Sending messages and attachments; pending rows until presage saves them.

use super::*;
use sigwin_thumb::photo::{self, Quality};

impl App {
    pub(super) async fn send(&mut self, text: String, files: Vec<PathBuf>, quote_id: Option<String>, mentions: Vec<crate::mention::Member>, link: Option<String>, quality: Quality) {
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
                // a sticker or photo without text: tell the other side what is quoted
                // (the original's pointer doubles as the thumbnail: no new upload, like Desktop's reuse)
                attachments: dm
                    .sticker
                    .as_ref()
                    .and_then(|s| s.data.as_ref())
                    .or_else(|| view::media::shown_attachments(&dm).next().map(|(_, a)| a))
                    .map(|a| {
                        vec![presage::proto::data_message::quote::QuotedAttachment {
                            content_type: a.content_type.clone(),
                            file_name: a.file_name.clone(),
                            thumbnail: Some(a.clone()),
                        }]
                    })
                    .unwrap_or_default(),
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
                    .map(|t| view::text::with_mentions(t, &q.body_ranges, &name_of))
                    .unwrap_or_else(|| "(첨부파일)".into());
                (label, view::text::quote_author_name(q, self.me, &name_of))
            })
            .unwrap_or_default();
        let (body, body_ranges) = crate::mention::encode(&text, &mentions);
        if body.len() > view::text::MAX_LONG_TEXT_BYTES {
            self.notice("메시지가 너무 깁니다 (최대 64KB)");
            return;
        }
        // the message itself ends our typing indicator on the other side
        self.composing = None;
        let ts = self.next_send_ts();
        // the same line the stored row gets, so it doesn't change when the send ends
        // photos show as photos while they go; what the row will show once stored
        let (photo_files, other_files): (Vec<&PathBuf>, Vec<&PathBuf>) = files.iter().partition(|f| {
            mime_guess::from_path(f).first_or_octet_stream().type_() == "image"
        });
        let mut photo_paths = Vec::new();
        let mut photos = Vec::new();
        let mut other_files = other_files;
        for f in photo_files {
            match sigwin_thumb::photo_size(f) {
                Ok(size) => {
                    photo_paths.push(f.clone());
                    photos.push(size);
                }
                // one this PC can't read goes as a file
                Err(_) => other_files.push(f),
            }
        }
        let attachment = other_files.first().map_or(String::new(), |first| {
            let bytes = other_files.iter().map(|f| std::fs::metadata(f).ok().map(|m| m.len())).sum();
            view::media::file_line(&file_label(first), other_files.len() - 1, bytes)
        });
        self.spawn_pending_thumbs(ts, photo_paths, &photos);
        self.pending.push(Pending {
            photos,
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
            job: SendJob::Text { body, body_ranges, files, quote, link, quality },
        });
        self.emit_messages(Scroll::Bottom).await;
        self.start_send(ts).await;
    }

    /// Pictures for the pending row's photos, straight from the files being sent (no cache).
    /// One photo is fitted to its size, several fill the cells of a grid.
    fn spawn_pending_thumbs(&self, ts: u64, paths: Vec<PathBuf>, sizes: &[(u32, u32)]) {
        if paths.is_empty() {
            return;
        }
        let sizes = sizes.to_vec();
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            let single = paths.len() == 1;
            let shown = paths.len().min(view::media::GRID_SHOWN);
            for (n, path) in paths.iter().take(shown).enumerate() {
                let Ok(bytes) = std::fs::read(path) else { continue };
                let Ok(img) = sigwin_thumb::decode_thumb(&bytes, view::media::PHOTO_THUMB_MAX) else { continue };
                let rgba = sigwin_thumb::to_rgba(&img);
                let key = pending_photo_key(ts, n, single);
                // the cell it fills: one photo's own box, or its place in the grid
                let (w, h) = if single { view::media::fit(Some(sizes[0].0), Some(sizes[0].1)) } else { view::media::grid_cell_size(paths.len(), n) };
                let rgba = sigwin_thumb::cover(&rgba, crate::types::physical_px(w), crate::types::physical_px(h));
                emit(Event::Thumb(Arc::new(Thumb { key, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })));
            }
        });
    }

    /// Queues the send of the pending row `ts`.
    /// presage stored what was sent as `ts` (a message or an op).
    pub(super) async fn sent(&mut self, ts: u64) {
        // presage stored the message before returning: swap the stub for the real row now
        if let Some(p) = self.pending.iter().find(|p| p.ts == ts) {
            if let Ok(target) = p.quote_id.parse::<u64>() {
                if let Err(e) = self.index.record_reply(&p.thread_key, target, ts, self.me).await {
                    warn!("record reply: {e:#}");
                }
            }
        }
        self.pending.retain(|p| p.ts != ts);
        self.op_sent(ts).await;
        if let Err(e) = self.reload_window().await {
            warn!("reload window: {e:#}");
        }
        self.threads_dirty = true;
    }

    /// Sending `ts` failed: an op is taken back, a message stays as a failed row to retry.
    pub(super) async fn send_failed(&mut self, ts: u64, e: String) {
        warn!("send {ts} failed: {e}");
        if self.op_failed(ts, &e).await {
            self.emit_messages(Scroll::KeepIfNotAtBottom).await;
            return;
        }
        // the open thread shows it on the row
        let open = self.current.as_ref().map(|c| c.key.as_str());
        if self.pending.iter().any(|p| p.ts == ts && Some(p.thread_key.as_str()) != open) {
            self.notice("다른 대화로 보낸 메시지를 전송하지 못했습니다");
        }
        let thread = self.pending.iter().find(|p| p.ts == ts).and_then(|p| parse_thread_key(&p.thread_key));
        let after_seq = match thread {
            Some(t) => self.index.page(&t, i64::MAX, 1).await.ok().and_then(|r| r.first().map(|r| r.0)),
            None => None,
        };
        if let Some(p) = self.pending.iter_mut().find(|p| p.ts == ts) {
            p.error = Some(e);
            p.after_seq = Some(after_seq.unwrap_or(0));
        }
        self.emit_messages(Scroll::KeepIfNotAtBottom).await;
    }

    pub(super) async fn start_send(&mut self, ts: u64) {
        let Some(manager) = self.manager.clone() else { return };
        let Some(p) = self.pending.iter().find(|p| p.ts == ts) else { return };
        let Some(thread) = parse_thread_key(&p.thread_key) else { return };
        let (key, job) = (p.thread_key.clone(), p.job.clone());
        let group_revision = self.group_revision(&thread).await;
        let tx = self.tx.clone();
        let dir = self.paths.attachments.clone();
        let uploads = Uploads { index: self.index.clone(), thread: key.clone() };
        let (turn, done) = self.send_turn(&key);
        tokio::task::spawn_local(async move {
            let _done = done;
            if let Some(turn) = turn {
                let _ = turn.await;
            }
            let started = std::time::Instant::now();
            let res = send_job(manager, thread, job, group_revision, ts, dir, uploads).await;
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
    Text { body: String, body_ranges: Vec<BodyRange>, files: Vec<PathBuf>, quote: Option<Quote>, link: LinkSend, quality: Quality },
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

/// Uploads of attachments we sent to one thread: sending the same bytes there again reuses the
/// earlier upload instead of sending them to the server again (a retry, or the same photo twice).
#[derive(Clone)]
pub(super) struct Uploads {
    index: Index,
    thread: String,
}

/// How long an upload is reused. The server keeps attachments for longer (Signal Desktop
/// assumes weeks); this stays well inside that, since a recipient can't fetch an expired one.
const REUSE_UPLOAD_FOR: u64 = 7 * 24 * 3600 * 1000;

impl Uploads {
    /// Of the bytes as they would be uploaded; the type goes in too, as it's part of the pointer.
    fn hash(spec: &AttachmentSpec, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update((spec.content_type.len() as u64).to_le_bytes());
        h.update(spec.content_type.as_bytes());
        h.update(data);
        h.finalize().to_vec()
    }

    async fn find(&self, hash: &[u8]) -> Option<AttachmentPointer> {
        use presage::libsignal_service::prelude::ProtobufMessage;
        let since = now_ms().saturating_sub(REUSE_UPLOAD_FOR);
        let raw = self.index.upload_find(&self.thread, hash, since).await.ok()??;
        AttachmentPointer::decode(raw.as_slice()).ok().filter(|p| p.cdn_number.is_some() && p.key.is_some() && p.digest.is_some())
    }

    async fn put(&self, hash: &[u8], ptr: &AttachmentPointer) {
        use presage::libsignal_service::prelude::ProtobufMessage;
        let now = now_ms();
        let keep_since = now.saturating_sub(REUSE_UPLOAD_FOR);
        if let Err(e) = self.index.upload_put(&self.thread, hash, &ptr.encode_to_vec(), now, keep_since).await {
            warn!("remember upload: {e:#}");
        }
    }
}

/// Pointers for `specs`, in order: an earlier upload where there is one, the rest uploaded now.
/// Each upload is remembered right away, so a send that fails later doesn't upload it again.
async fn upload_all(m: &mut Mgr, specs: Vec<(AttachmentSpec, Vec<u8>)>, uploads: &Uploads) -> anyhow::Result<Vec<AttachmentPointer>> {
    let mut slots: Vec<Option<AttachmentPointer>> = Vec::with_capacity(specs.len());
    let mut todo = Vec::new();
    let mut todo_hashes = Vec::new();
    for (spec, data) in specs {
        let hash = Uploads::hash(&spec, &data);
        match uploads.find(&hash).await {
            Some(mut ptr) => {
                // same bytes, maybe another name
                ptr.file_name = spec.file_name;
                slots.push(Some(ptr));
            }
            None => {
                slots.push(None);
                todo_hashes.push(hash);
                todo.push((spec, data));
            }
        }
    }
    let (reused, uploaded) = (slots.len() - todo.len(), todo.len());
    let mut results = if todo.is_empty() { Vec::new() } else { m.upload_attachments(todo).await? };
    let mut failed = None;
    let mut fresh = Vec::with_capacity(results.len());
    for (res, hash) in results.drain(..).zip(&todo_hashes) {
        match res {
            Ok(ptr) => {
                uploads.put(hash, &ptr).await;
                fresh.push(ptr);
            }
            Err(e) => failed = failed.or(Some(e)),
        }
    }
    if let Some(e) = failed {
        return Err(e.into());
    }
    info!(reused, uploaded, "attachments ready");
    let mut fresh = fresh.into_iter();
    Ok(slots.into_iter().filter_map(|s| s.or_else(|| fresh.next())).collect())
}

async fn send_job(m: Mgr, thread: Thread, job: SendJob, group_revision: u32, ts: u64, dir: PathBuf, uploads: Uploads) -> anyhow::Result<()> {
    let mut tries = 0;
    loop {
        let res = match job.clone() {
            SendJob::Text { body, body_ranges, files, quote, link, quality } => {
                Box::pin(send_task(m.clone(), thread.clone(), body, body_ranges, files, quote, link, quality, group_revision, ts, dir.clone(), uploads.clone())).await
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
    quality: Quality,
    group_revision: u32,
    ts: u64,
    attachments_dir: PathBuf,
    uploads: Uploads,
) -> anyhow::Result<()> {
    let mut text = text;
    let long = long_body(&mut m, &mut text, &attachments_dir).await?;
    let mut specs = Vec::new();
    for path in &files {
        let mut ct = mime_guess::from_path(path).first_or_octet_stream().to_string();
        let mut data = std::fs::read(path).with_context(|| format!("{} 읽기 실패", path.display()))?;
        let mut file_name = path.file_name().map(|s| s.to_string_lossy().to_string());
        let mut facts = None;
        if ct.starts_with("image/") {
            // a photo is shrunk and loses its EXIF; one this can't handle goes as it is
            match prepare_photo(data, quality).await? {
                (orig, Ok(None)) => data = orig,
                (orig, Err(e)) => {
                    warn!("prepare photo: {e:#}");
                    data = orig;
                }
                (_, Ok(Some(p))) => {
                    if p.content_type != ct {
                        ct = p.content_type.to_string();
                        let ext = match p.content_type {
                            "image/png" => "png",
                            "image/webp" => "webp",
                            _ => "jpg",
                        };
                        file_name = file_name.map(|n| Path::new(&n).with_extension(ext).to_string_lossy().to_string());
                    }
                    if let Some(size) = p.size {
                        facts = Some((Some(size), p.blurhash));
                    }
                    data = p.bytes;
                }
            }
        }
        let (size, blur_hash) = match facts {
            Some(f) => f,
            None if ct.starts_with("image/") => read_facts(path, photo_facts).await,
            None if ct.starts_with("video/") => read_facts(path, video_facts).await,
            None => (None, None),
        };
        let (width, height) = size.unzip();
        specs.push((
            AttachmentSpec {
                content_type: ct,
                length: data.len(),
                file_name,
                preview: None,
                voice_note: None,
                borderless: None,
                width,
                height,
                caption: None,
                blur_hash,
            },
            data,
        ));
    }
    let mut attachments: Vec<AttachmentPointer> = long.into_iter().collect();
    if !specs.is_empty() {
        attachments.extend(upload_all(&mut m, specs, &uploads).await?);
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

/// Past this a photo goes as it is: decoding it would take too much memory and time.
const PREPARE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// `data` prepared for sending (off the backend thread), and handed back for when it stays as it is.
async fn prepare_photo(data: Vec<u8>, quality: Quality) -> anyhow::Result<(Vec<u8>, anyhow::Result<Option<photo::Prepared>>)> {
    if data.len() > PREPARE_MAX_BYTES {
        return Ok((data, Ok(None)));
    }
    let job = tokio::task::spawn_blocking(move || {
        let res = photo::prepare(&data, quality).map_err(anyhow::Error::from);
        (data, res)
    });
    Ok(job.await?)
}

/// A size and a blurhash, as the message has them.
type Facts = (Option<(u32, u32)>, Option<String>);

/// Past this a photo goes without a blurhash: decoding it isn't worth delaying the send.
const SEND_MAX_PIXELS: u64 = 100_000_000;

fn photo_facts(path: &Path) -> anyhow::Result<Facts> {
    let (size, hash) = sigwin_thumb::photo_facts(path, SEND_MAX_PIXELS)?;
    Ok((Some(size), hash))
}

fn video_facts(path: &Path) -> anyhow::Result<Facts> {
    let v = crate::videoframe::first_frame_plain(path)?;
    Ok(v.map_or((None, None), |v| (Some(v.size), v.blurhash())))
}

/// What a file being sent says about itself. One this PC can't decode goes without it.
async fn read_facts(path: &Path, read: fn(&Path) -> anyhow::Result<Facts>) -> Facts {
    let path = path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || read(&path));
    match read.await.map_err(anyhow::Error::from).and_then(|r| r) {
        Ok(facts) => facts,
        Err(e) => {
            warn!("read file to send: {e:#}");
            (None, None)
        }
    }
}

/// Signal's long messages: past 2KB the body keeps only its start and the full text goes as a
/// `text/x-signal-plain` attachment (first in the list, as Signal Desktop sends it). The text is
/// cached right away, so our own copy shows in full without downloading it back.
pub(super) async fn long_body(m: &mut Mgr, body: &mut String, attachments_dir: &Path) -> anyhow::Result<Option<AttachmentPointer>> {
    if body.len() <= view::text::MAX_BODY_BYTES {
        return Ok(None);
    }
    let data = body.as_bytes().to_vec();
    let spec = AttachmentSpec {
        content_type: view::text::LONG_TEXT.into(),
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
    let path = super::files::attachment_path(attachments_dir, &view::media::attachment_key(&ptr, 0, 0));
    if let Err(e) = sigwin_vault::write(&path, &data) {
        warn!("cache long text: {e}");
    }
    *body = view::text::trim_body(body, view::text::MAX_BODY_BYTES).to_string();
    Ok(Some(ptr))
}

pub(super) fn file_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Thumbnail key of photo `n` of a pending row.
fn pending_photo_key(ts: u64, n: usize, single: bool) -> String {
    if single { format!("pending-{ts}") } else { format!("pending-{ts}-{n}") }
}

pub(super) fn pending_row(p: &Pending, prev_ts: Option<u64>) -> MsgDto {
    let mut m = view::outgoing_stub(p.ts, prev_ts);
    (m.text, m.styles) = crate::format::parse(&p.text);
    m.quote = p.quote.clone();
    m.quote_author = p.quote_author.clone();
    m.quote_id = p.quote_id.clone();
    m.attachment = p.attachment.clone();
    m.link = p.link.clone();
    match p.photos[..] {
        [] => {}
        [(w, h)] => view::media::set_single_photo(&mut m, pending_photo_key(p.ts, 0, true), view::media::fit(Some(w), Some(h))),
        _ => view::media::set_photo_grid(&mut m, p.photos.len(), |n, _, _| pending_photo_key(p.ts, n, false)),
    }
    if !p.image_key.is_empty() {
        m.image_key = p.image_key.clone();
        m.img_w = view::media::STICKER_SIZE;
        m.img_h = view::media::STICKER_SIZE;
        m.sticker = true;
    }
    m.big_emoji = !m.sticker
        && m.attachment.is_empty()
        && p.photos.is_empty()
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
