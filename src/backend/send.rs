//! Sending messages and attachments; pending rows until presage saves them.

use super::*;

impl App {
    pub(super) async fn send(&mut self, text: String, files: Vec<PathBuf>, quote_id: Option<String>, mentions: Vec<crate::mention::Member>) {
        let Some(manager) = self.manager.clone() else { return };
        let Some(open) = self.current.as_ref() else { return };
        let text = text.trim_end().to_string();
        if text.trim().is_empty() && files.is_empty() {
            return;
        }
        let thread = open.thread.clone();
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
            self.status("메시지가 너무 깁니다 (최대 64KB)");
            return;
        }
        let group_revision = self.group_revision(&thread).await;
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
            thread_key: open.key.clone(),
            text: text.clone(),
            quote: quote_label,
            quote_author,
            quote_id: quote.as_ref().and_then(|q| q.id).map(|i| i.to_string()).unwrap_or_default(),
            attachment,
            image_key: String::new(),
            error: None,
        });
        self.emit_messages(Scroll::Bottom).await;
        let tx = self.tx.clone();
        let dir = self.paths.attachments.clone();
        tokio::task::spawn_local(async move {
            let started = std::time::Instant::now();
            let res = Box::pin(send_task(manager, thread, body, body_ranges, files, quote, group_revision, ts, dir)).await;
            info!(ms = started.elapsed().as_millis() as u64, ok = res.is_ok(), "message sent");
            let _ = tx.send(Internal::Sent(ts, res.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Unique, increasing send timestamps (they are the message ids).
    pub(super) fn next_send_ts(&self) -> u64 {
        let last = self.pending.iter().map(|p| p.ts).chain(self.ops.iter().map(|o| o.ts)).max().unwrap_or(0);
        now_ms().max(last + 1)
    }
}

pub(super) async fn send_task(
    mut m: Mgr,
    thread: Thread,
    text: String,
    body_ranges: Vec<BodyRange>,
    files: Vec<PathBuf>,
    quote: Option<Quote>,
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
    let dm = DataMessage {
        body: (!text.is_empty()).then_some(text),
        body_ranges,
        attachments,
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
            m.receipt_info = format!("보내지 못했습니다.\n{e}");
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
