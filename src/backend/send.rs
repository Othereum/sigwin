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
        let quote_label = quote
            .as_ref()
            .and_then(|q| {
                let t = q.text.as_deref().filter(|t| !t.is_empty())?;
                Some(view::with_mentions(t, &q.body_ranges, &|u| names.get(u).cloned().unwrap_or_else(|| "알 수 없음".into())))
            })
            .unwrap_or_default();
        let (body, body_ranges) = crate::mention::encode(&text, &mentions);
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
            attachment,
            image_key: String::new(),
            error: None,
        });
        self.emit_messages(Scroll::Bottom).await;
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let started = std::time::Instant::now();
            let res = Box::pin(send_task(manager, thread, body, body_ranges, files, quote, group_revision, ts)).await;
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
) -> anyhow::Result<()> {
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
    let attachments = if specs.is_empty() {
        Vec::new()
    } else {
        m.upload_attachments(specs).await?.into_iter().collect::<Result<Vec<_>, _>>()?
    };
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

pub(super) fn file_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

pub(super) fn pending_row(p: &Pending, prev_ts: Option<u64>) -> MsgDto {
    let mut m = view::outgoing_stub(p.ts, prev_ts);
    (m.text, m.styles) = crate::format::parse(&p.text);
    m.quote = p.quote.clone();
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
