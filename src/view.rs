//! Turns stored `Content`s into display rows: folds reactions, edits and receipts into
//! the messages they target and drops protocol-only messages.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Local, TimeZone};
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::sync_message::{Content as SyncContent, Sent};
use presage::proto::{receipt_message, AttachmentPointer, EditMessage, ReceiptMessage, SyncMessage};

use crate::types::MsgDto;

pub const THUMB_MAX: u32 = 260;
pub const STICKER_SIZE: u32 = 140;

pub enum Kind<'a> {
    Msg { outgoing: bool, dm: &'a DataMessage },
    System(String),
    Reaction { target: u64, emoji: String, remove: bool },
    Edit { target: u64, dm: &'a DataMessage },
    Receipt { read: bool, targets: &'a [u64] },
    Deleted,
    Skip,
}

fn classify_data<'a>(dm: &'a DataMessage, outgoing: bool) -> Kind<'a> {
    if let Some(r) = &dm.reaction {
        if let (Some(target), Some(emoji)) = (r.target_sent_timestamp, &r.emoji) {
            return Kind::Reaction { target, emoji: emoji.clone(), remove: r.remove.unwrap_or(false) };
        }
        return Kind::Skip;
    }
    let has_content = dm.body.as_deref().is_some_and(|b| !b.is_empty())
        || !dm.attachments.is_empty()
        || dm.sticker.is_some()
        || !dm.contact.is_empty()
        || dm.payment.is_some()
        || dm.poll_create.is_some()
        || dm.gift_badge.is_some();
    if has_content {
        return Kind::Msg { outgoing, dm };
    }
    if dm.group_call_update.is_some() {
        return Kind::System("그룹 통화가 시작되었습니다".into());
    }
    if dm.group_v2.as_ref().is_some_and(|g| g.group_change.is_some()) {
        return Kind::System("그룹 정보가 변경되었습니다".into());
    }
    Kind::Skip
}

pub fn classify(c: &Content, me: Uuid) -> Kind<'_> {
    let from_me = c.metadata.sender.raw_uuid() == me;
    match &c.body {
        ContentBody::DataMessage(dm) => classify_data(dm, from_me),
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::Sent(Sent { message: Some(dm), .. })),
            ..
        }) => classify_data(dm, true),
        ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(Sent {
                    edit_message: Some(EditMessage { target_sent_timestamp: Some(t), data_message: Some(dm) }),
                    ..
                })),
            ..
        })
        | ContentBody::EditMessage(EditMessage { target_sent_timestamp: Some(t), data_message: Some(dm) }) => {
            Kind::Edit { target: *t, dm }
        }
        ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::CallEvent(_)), .. }) => {
            Kind::System("📞 통화".into())
        }
        ContentBody::NullMessage(_) => Kind::Deleted,
        ContentBody::ReceiptMessage(ReceiptMessage { r#type, timestamp }) if !from_me => {
            let t = r#type.and_then(|t| receipt_message::Type::try_from(t).ok());
            match t {
                Some(receipt_message::Type::Delivery) => Kind::Receipt { read: false, targets: timestamp },
                Some(_) => Kind::Receipt { read: true, targets: timestamp },
                None => Kind::Skip,
            }
        }
        _ => Kind::Skip,
    }
}

pub fn attachment_key(ptr: &AttachmentPointer, ts: u64, idx: usize) -> String {
    match &ptr.digest {
        Some(d) if d.len() >= 12 => hex::encode(&d[..12]),
        _ => format!("{ts}-{idx}"),
    }
}

pub fn is_image(ptr: &AttachmentPointer) -> bool {
    matches!(
        ptr.content_type.as_deref(),
        Some("image/jpeg" | "image/png" | "image/apng" | "image/gif" | "image/webp")
    ) && ptr.size.unwrap_or(0) < 15 * 1024 * 1024
}

fn attachment_label(ptr: &AttachmentPointer) -> String {
    let ct = ptr.content_type.as_deref().unwrap_or("");
    if let Some(name) = ptr.file_name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    if ct.starts_with("image/") {
        "사진".into()
    } else if ct.starts_with("video/") {
        "동영상".into()
    } else if ct.starts_with("audio/") {
        if ptr.flags.unwrap_or(0) & 1 == 1 { "음성 메시지".into() } else { "오디오".into() }
    } else {
        "파일".into()
    }
}

/// Body text of a data message, with placeholders for content we don't render.
fn body_text(dm: &DataMessage) -> String {
    let mut parts: Vec<String> = Vec::new();
    if dm.is_view_once.unwrap_or(false) {
        parts.push("[한 번 보기 미디어 — 휴대폰에서 확인하세요]".into());
    }
    if let Some(s) = &dm.sticker {
        parts.push(format!("[스티커 {}]", s.emoji.as_deref().unwrap_or("")));
    }
    if !dm.contact.is_empty() {
        parts.push("[연락처 공유]".into());
    }
    if dm.payment.is_some() {
        parts.push("[결제]".into());
    }
    if let Some(p) = &dm.poll_create {
        parts.push(format!("[투표] {}", p.question.as_deref().unwrap_or("")));
    }
    if dm.gift_badge.is_some() {
        parts.push("[선물 배지]".into());
    }
    if let Some(b) = dm.body.as_deref().filter(|b| !b.is_empty()) {
        parts.push(b.to_string());
    }
    parts.join("\n")
}

/// One-line preview used in the thread list and notifications.
pub fn snippet(c: &Content, me: Uuid) -> Option<String> {
    let text = match classify(c, me) {
        Kind::Msg { dm, outgoing } => {
            let mut t = body_text(dm);
            if t.is_empty() {
                t = dm
                    .attachments
                    .first()
                    .map(|a| {
                        let l = attachment_label(a);
                        if is_image(a) { format!("📷 {l}") } else { format!("📎 {l}") }
                    })
                    .unwrap_or_default();
            }
            let t = t.replace('\n', " ");
            if outgoing { format!("나: {t}") } else { t }
        }
        Kind::System(s) => s,
        Kind::Deleted => "삭제된 메시지".into(),
        _ => return None,
    };
    Some(text)
}

fn local(ts: u64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ts as i64).single().unwrap_or_else(Local::now)
}

fn ampm(dt: &DateTime<Local>) -> String {
    let h = dt.format("%H").to_string().parse::<u32>().unwrap_or(0);
    let (p, h12) = match h {
        0 => ("오전", 12),
        1..=11 => ("오전", h),
        12 => ("오후", 12),
        _ => ("오후", h - 12),
    };
    format!("{p} {h12}:{}", dt.format("%M"))
}

pub fn time_short(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let dt = local(ts);
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        ampm(&dt)
    } else if (now.date_naive() - dt.date_naive()).num_days() == 1 {
        "어제".into()
    } else if dt.year() == now.year() {
        format!("{}월 {}일", dt.month(), dt.day())
    } else {
        format!("{}. {}. {}.", dt.year(), dt.month(), dt.day())
    }
}

fn day_label(dt: &DateTime<Local>) -> String {
    const WD: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];
    format!(
        "{}년 {}월 {}일 {}요일",
        dt.year(),
        dt.month(),
        dt.day(),
        WD[dt.weekday().num_days_from_monday() as usize]
    )
}

fn fit(w: Option<u32>, h: Option<u32>) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let s = (THUMB_MAX as f32 / w.max(h) as f32).min(1.0);
            (((w as f32 * s) as u32).max(40), ((h as f32 * s) as u32).max(40))
        }
        _ => (THUMB_MAX, THUMB_MAX * 3 / 4),
    }
}

pub struct Built {
    pub items: Vec<MsgDto>,
    /// image attachments that need a thumbnail: (key, pointer)
    pub wanted_thumbs: Vec<(String, AttachmentPointer)>,
}

/// `window` must be sorted by timestamp ascending.
pub fn build(window: &[(u64, Content)], me: Uuid, is_group: bool, name_of: &dyn Fn(&Uuid) -> String) -> Built {
    let mut reactions: HashMap<u64, Vec<(Uuid, String)>> = HashMap::new();
    let mut edits: HashMap<u64, (u64, &DataMessage)> = HashMap::new();
    let mut status: HashMap<u64, u8> = HashMap::new();

    for (ts, c) in window {
        match classify(c, me) {
            Kind::Reaction { target, emoji, remove } => {
                let sender = c.metadata.sender.raw_uuid();
                let list = reactions.entry(target).or_default();
                list.retain(|(s, _)| *s != sender);
                if !remove {
                    list.push((sender, emoji));
                }
            }
            Kind::Edit { target, dm } => {
                let e = edits.entry(target).or_insert((*ts, dm));
                if *ts >= e.0 {
                    *e = (*ts, dm);
                }
            }
            Kind::Receipt { read, targets } => {
                for t in targets {
                    let s = status.entry(*t).or_insert(1);
                    *s = (*s).max(if read { 2 } else { 1 });
                }
            }
            _ => {}
        }
    }

    let mut items = Vec::new();
    let mut wanted = Vec::new();
    let mut prev_day = None;
    let mut prev_sender: Option<Uuid> = None;

    for (ts, c) in window {
        let kind = classify(c, me);
        let dt = local(*ts);
        let day = dt.date_naive();
        let mut m = MsgDto { id: ts.to_string(), time: ampm(&dt), ..Default::default() };
        match kind {
            Kind::Msg { outgoing, dm } => {
                let sender = c.metadata.sender.raw_uuid();
                let (dm, edited) = match edits.get(ts) {
                    Some((_, e)) => (*e, true),
                    None => (dm, false),
                };
                m.outgoing = outgoing;
                m.text = body_text(dm);
                // stickers with image data render as a borderless picture instead of text
                if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.as_ref()) {
                    let key = attachment_key(data, *ts, 99);
                    wanted.push((key.clone(), data.clone()));
                    m.image_key = key;
                    m.img_w = STICKER_SIZE;
                    m.img_h = STICKER_SIZE;
                    m.sticker = true;
                    m.text = dm.body.clone().unwrap_or_default();
                }
                if !outgoing {
                    m.sender = name_of(&sender);
                    m.sender_key = sender.to_string();
                }
                m.show_sender = is_group && !outgoing && (prev_sender != Some(sender) || prev_day != Some(day));
                if let Some(q) = &dm.quote {
                    m.quote = q
                        .text
                        .clone()
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| "(첨부파일)".into());
                }
                let mut others = 0;
                for (i, a) in dm.attachments.iter().enumerate() {
                    if m.image_key.is_empty() && is_image(a) {
                        let key = attachment_key(a, *ts, i);
                        let (w, h) = fit(a.width, a.height);
                        m.img_w = w;
                        m.img_h = h;
                        wanted.push((key.clone(), a.clone()));
                        m.image_key = key;
                    } else if m.attachment.is_empty() {
                        m.attachment = attachment_label(a);
                    } else {
                        others += 1;
                    }
                }
                if others > 0 {
                    m.attachment = format!("{} 외 {}개", m.attachment, others);
                }
                if let Some(r) = reactions.get(ts).filter(|r| !r.is_empty()) {
                    let mut counts: Vec<(String, usize)> = Vec::new();
                    for (_, e) in r {
                        match counts.iter_mut().find(|(x, _)| x == e) {
                            Some(c) => c.1 += 1,
                            None => counts.push((e.clone(), 1)),
                        }
                    }
                    m.reactions = counts
                        .into_iter()
                        .map(|(e, n)| if n > 1 { format!("{e}{n}") } else { e })
                        .collect::<Vec<_>>()
                        .join(" ");
                }
                if outgoing {
                    m.status = match status.get(ts) {
                        Some(2) => "읽음",
                        Some(_) => "✓✓",
                        None => "✓",
                    }
                    .into();
                }
                if edited {
                    m.time = format!("{} · 수정됨", m.time);
                }
                prev_sender = Some(sender);
            }
            Kind::System(s) => {
                m.system = true;
                m.text = s;
                prev_sender = None;
            }
            Kind::Deleted => {
                m.deleted = true;
                m.text = "삭제된 메시지입니다".into();
                prev_sender = None;
            }
            _ => continue,
        }
        if prev_day != Some(day) {
            m.day_sep = day_label(&dt);
            prev_day = Some(day);
        }
        items.push(m);
    }
    Built { items, wanted_thumbs: wanted }
}

