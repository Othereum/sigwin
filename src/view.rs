//! Turns stored `Content`s into display rows: folds reactions, edits and receipts into
//! the messages they target and drops protocol-only messages.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Local, TimeZone};
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::sync_message::{Content as SyncContent, Sent};
use presage::proto::body_range::AssociatedValue;
use presage::proto::data_message::pin_message::PinDuration;
use presage::proto::{receipt_message, AttachmentPointer, BodyRange, EditMessage, ReceiptMessage, SyncMessage};

use presage::store::ContentExt;

use crate::types::MsgDto;

pub const THUMB_MAX: u32 = 400;
/// DataMessage.Flags.EXPIRATION_TIMER_UPDATE
const EXPIRATION_TIMER_UPDATE: u32 = 2;
/// Signal accepts edits and deletes for everyone up to a day after sending.
pub const EDIT_WINDOW_MS: u64 = 24 * 3600 * 1000;
pub const STICKER_SIZE: u32 = 140;

pub enum Kind<'a> {
    Msg { outgoing: bool, dm: &'a DataMessage },
    System(String),
    Reaction { target: u64, emoji: String, remove: bool },
    Edit { target: u64, dm: &'a DataMessage },
    Receipt { read: bool, targets: &'a [u64] },
    Deleted,
    /// `secs` None = until unpinned
    Pin { target: u64, author: Uuid, secs: Option<u32> },
    Unpin { target: u64 },
    Skip,
}

fn classify_data<'a>(dm: &'a DataMessage, outgoing: bool) -> Kind<'a> {
    if let Some(r) = &dm.reaction {
        if let (Some(target), Some(emoji)) = (r.target_sent_timestamp, &r.emoji) {
            return Kind::Reaction { target, emoji: emoji.clone(), remove: r.remove.unwrap_or(false) };
        }
        return Kind::Skip;
    }
    if let Some(p) = &dm.pin_message {
        let (Some(target), Some(author)) = (p.target_sent_timestamp, p.parse_target_author_aci()) else { return Kind::Skip };
        let secs = match p.pin_duration {
            Some(PinDuration::PinDurationSeconds(s)) if s > 0 => Some(s),
            _ => None,
        };
        return Kind::Pin { target, author: author.into(), secs };
    }
    if let Some(target) = dm.unpin_message.as_ref().and_then(|u| u.target_sent_timestamp) {
        return Kind::Unpin { target };
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
    if dm.flags.unwrap_or(0) & EXPIRATION_TIMER_UPDATE != 0 {
        return Kind::System(match dm.expire_timer.unwrap_or(0) {
            0 => "사라지는 메시지를 껐습니다".into(),
            t => format!("사라지는 메시지: {}", timer_label(t)),
        });
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

/// The data message carried by a stored content (plain, synced from another device, or an edit).
pub fn data_message(c: &Content) -> Option<&DataMessage> {
    match &c.body {
        ContentBody::DataMessage(dm) => Some(dm),
        ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Sent(s)), .. }) => s
            .message
            .as_ref()
            .or_else(|| s.edit_message.as_ref().and_then(|e| e.data_message.as_ref())),
        ContentBody::EditMessage(e) => e.data_message.as_ref(),
        _ => None,
    }
}

/// When a disappearing message expires (ms). The timer runs from the send time: we don't
/// track when it was read, so it may vanish here earlier than on the phone, never later.
pub fn expires_at(c: &Content) -> Option<u64> {
    let t = data_message(c)?.expire_timer.filter(|t| *t > 0)?;
    Some((c.metadata.client_timestamp.timestamp_millis() as u64).saturating_add(t as u64 * 1000))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// "1주", "8시간", "30초" for an expire timer in seconds.
pub fn timer_label(secs: u32) -> String {
    const UNITS: [(u32, &str); 4] = [(604_800, "주"), (86_400, "일"), (3_600, "시간"), (60, "분")];
    for (n, u) in UNITS {
        if secs >= n && secs % n == 0 {
            return format!("{}{u}", secs / n);
        }
    }
    format!("{secs}초")
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

fn mention_uuid(r: &BodyRange) -> Option<Uuid> {
    match r.associated_value.as_ref()? {
        AssociatedValue::MentionAci(s) => s.parse().ok(),
        AssociatedValue::MentionAciBinary(b) => Uuid::from_slice(b).ok(),
        AssociatedValue::Style(_) => None,
    }
}

/// Everyone @mentioned in a data message (body and quote), so callers can resolve names first.
pub fn mentioned(dm: &DataMessage) -> Vec<Uuid> {
    let quote = dm.quote.iter().flat_map(|q| q.body_ranges.iter());
    dm.body_ranges.iter().chain(quote).filter_map(mention_uuid).collect()
}

/// Replaces mention ranges (a U+FFFC placeholder in the text, offsets in UTF-16 units) with "@name".
pub fn with_mentions(text: &str, ranges: &[BodyRange], name_of: &dyn Fn(&Uuid) -> String) -> String {
    let mut out = String::new();
    push_with_mentions(&mut out, text, ranges, name_of, &mut Vec::new());
    out
}

/// Appends `text` to `out` with mentions replaced; records each "@name" as a byte range of `out`.
fn push_with_mentions(
    out: &mut String,
    text: &str,
    ranges: &[BodyRange],
    name_of: &dyn Fn(&Uuid) -> String,
    spans: &mut Vec<(usize, usize, String)>,
) {
    let mut ms: Vec<(usize, usize, Uuid)> = ranges
        .iter()
        .filter_map(|r| Some((r.start? as usize, r.length.unwrap_or(0) as usize, mention_uuid(r)?)))
        .collect();
    if ms.is_empty() {
        out.push_str(text);
        return;
    }
    ms.sort_by_key(|m| m.0);
    let mut mention = |out: &mut String, uuid: &Uuid| {
        let start = out.len();
        out.push('@');
        out.push_str(&name_of(uuid));
        spans.push((start, out.len(), uuid.to_string()));
    };
    let mut next = ms.iter().peekable();
    let mut pos = 0; // UTF-16 offset of the current char
    let mut skip_until = 0;
    for ch in text.chars() {
        while let Some(&&(start, len, uuid)) = next.peek() {
            if start > pos {
                break;
            }
            next.next();
            if start == pos && pos >= skip_until {
                mention(out, &uuid);
                skip_until = pos + len;
            }
        }
        if pos >= skip_until {
            out.push(ch);
        }
        pos += ch.len_utf16();
    }
    // a mention at the very end
    if let Some(&(start, _, uuid)) = next.next() {
        if start >= skip_until {
            mention(out, &uuid);
        }
    }
}

/// Body text of a data message, with placeholders for content we don't render.
fn body_text(dm: &DataMessage, name_of: &dyn Fn(&Uuid) -> String) -> String {
    body_text_spans(dm, name_of, &mut Vec::new())
}

/// `body_text`, recording where the mentions ended up (byte ranges).
fn body_text_spans(dm: &DataMessage, name_of: &dyn Fn(&Uuid) -> String, spans: &mut Vec<(usize, usize, String)>) -> String {
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
        let mut out = parts.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        push_with_mentions(&mut out, b, &dm.body_ranges, name_of, spans);
        return out;
    }
    parts.join("\n")
}

/// One-line preview used in the thread list and notifications.
pub fn snippet(c: &Content, me: Uuid, name_of: &dyn Fn(&Uuid) -> String) -> Option<String> {
    if expires_at(c).is_some_and(|t| t <= now_ms()) {
        return None;
    }
    let text = match classify(c, me) {
        Kind::Msg { dm, outgoing } => {
            let t = one_line(dm, name_of);
            if outgoing { format!("나: {t}") } else { t }
        }
        Kind::System(s) => s,
        Kind::Pin { .. } => pin_label(c, me, name_of),
        Kind::Deleted => "삭제된 메시지".into(),
        _ => return None,
    };
    Some(text)
}

/// A message's text on one line, or what it carries when it has no text.
pub fn one_line(dm: &DataMessage, name_of: &dyn Fn(&Uuid) -> String) -> String {
    let mut t = body_text(dm, name_of);
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
    t.replace('\n', " ")
}

/// Timeline line for a pin message.
fn pin_label(c: &Content, me: Uuid, name_of: &dyn Fn(&Uuid) -> String) -> String {
    let who = c.metadata.sender.raw_uuid();
    if who == me {
        "메시지를 고정했습니다".into()
    } else {
        format!("{}님이 메시지를 고정했습니다", name_of(&who))
    }
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
        match (now - dt).num_minutes() {
            m if m < 1 => "방금".into(),
            m if m < 60 => format!("{m}분"),
            _ => ampm(&dt),
        }
    } else if (now.date_naive() - dt.date_naive()).num_days() == 1 {
        "어제".into()
    } else if dt.year() == now.year() {
        format!("{}월 {}일", dt.month(), dt.day())
    } else {
        format!("{}. {}. {}.", dt.year(), dt.month(), dt.day())
    }
}

/// Bubble time: "방금" / "3분" for the last hour, else the absolute `abs` ("오후 3:12 · 수정됨").
pub fn bubble_time(ts: u64, abs: &str) -> String {
    let mins = (Local::now() - local(ts)).num_minutes();
    if !(0..60).contains(&mins) && mins >= 0 {
        return abs.to_string();
    }
    let rel = if mins < 1 { "방금".to_string() } else { format!("{mins}분") };
    match abs.split_once(" · ") {
        Some((_, tail)) => format!("{rel} · {tail}"),
        None => rel,
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
    /// image attachments that need a thumbnail: (key, pointer, square box in logical px to fit into, 0 = as is)
    pub wanted_thumbs: Vec<(String, AttachmentPointer, u32)>,
    /// earliest time a still-shown disappearing message expires
    pub next_expiry: Option<u64>,
}

/// `window` must be sorted by timestamp ascending.
/// Group rows from others get a photo column; the photo sits on the last row of each sender's run.
/// Rows followed by one of the same run and minute drop their time / status (`hide_time`).
pub fn mark_avatar_runs(items: &mut [MsgDto]) {
    // Whoever read a message has read everything before it: a reader's photo stays on the newest
    // message they read only.
    let mut seen = std::collections::HashSet::new();
    for m in items.iter_mut().rev().filter(|m| m.outgoing && !m.readers.is_empty()) {
        m.readers.retain(|(u, _)| seen.insert(u.clone()));
    }
    for i in 0..items.len() {
        let m = &items[i];
        let col = !m.outgoing && !m.system;
        let last = col
            && !m.sender_key.is_empty()
            && items.get(i + 1).is_none_or(|n| {
                n.outgoing || n.system || n.sender_key != m.sender_key || !n.day_sep.is_empty()
            });
        let hide_time = !m.pinned
            && !m.system
            && items.get(i + 1).is_some_and(|n| {
                !n.system
                    && n.outgoing == m.outgoing
                    && n.sender_key == m.sender_key
                    && n.day_sep.is_empty()
                    && n.time == m.time
                    && n.status == m.status
            });
        items[i].avatar_col = col;
        items[i].show_avatar = last;
        items[i].hide_time = hide_time;
    }
}

pub fn build(
    window: &[(u64, Content)],
    me: Uuid,
    is_group: bool,
    members: &[Uuid],
    // target ts -> recipient -> 1 delivered / 2 read (from the receipt index)
    status: &HashMap<u64, HashMap<Uuid, u8>>,
    // our messages deleted for everyone (the stored row no longer says who sent it)
    my_deletes: &std::collections::HashSet<u64>,
    name_of: &dyn Fn(&Uuid) -> String,
) -> Built {
    let mut reactions: HashMap<u64, Vec<(Uuid, String)>> = HashMap::new();
    let mut edits: HashMap<u64, (u64, &DataMessage, Uuid)> = HashMap::new();
    let now = now_ms();
    let mut next_expiry: Option<u64> = None;

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
                let sender = c.metadata.sender.raw_uuid();
                let e = edits.entry(target).or_insert((*ts, dm, sender));
                if *ts >= e.0 {
                    *e = (*ts, dm, sender);
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
        if let Some(exp) = expires_at(c) {
            if exp <= now {
                continue;
            }
            next_expiry = Some(next_expiry.map_or(exp, |n| n.min(exp)));
        }
        let kind = classify(c, me);
        let dt = local(*ts);
        let day = dt.date_naive();
        let mut m = MsgDto { id: ts.to_string(), time: ampm(&dt), ..Default::default() };
        match kind {
            Kind::Msg { outgoing, dm } => {
                let sender = c.metadata.sender.raw_uuid();
                // only the author may edit a message
                let (dm, edited) = match edits.get(ts) {
                    Some((_, e, by)) if *by == sender => (*e, true),
                    _ => (dm, false),
                };
                m.outgoing = outgoing;
                m.text = body_text_spans(dm, name_of, &mut m.mentions);
                // stickers with image data render as a borderless picture instead of text
                if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.as_ref()) {
                    let key = attachment_key(data, *ts, 99);
                    wanted.push((key.clone(), data.clone(), STICKER_SIZE));
                    m.image_key = key;
                    m.img_w = STICKER_SIZE;
                    m.img_h = STICKER_SIZE;
                    m.sticker = true;
                    m.text.clear();
                    m.mentions.clear();
                    push_with_mentions(
                        &mut m.text,
                        dm.body.as_deref().unwrap_or_default(),
                        &dm.body_ranges,
                        name_of,
                        &mut m.mentions,
                    );
                }
                if !outgoing {
                    m.sender = name_of(&sender);
                    m.sender_key = sender.to_string();
                }
                m.show_sender = is_group && !outgoing && (prev_sender != Some(sender) || prev_day != Some(day));
                if let Some(q) = &dm.quote {
                    m.quote = q
                        .text
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .map(|t| with_mentions(t, &q.body_ranges, name_of))
                        .unwrap_or_else(|| "(첨부파일)".into());
                }
                let mut others = 0;
                for (i, a) in dm.attachments.iter().enumerate() {
                    if m.image_key.is_empty() && is_image(a) {
                        let key = attachment_key(a, *ts, i);
                        let (w, h) = fit(a.width, a.height);
                        m.img_w = w;
                        m.img_h = h;
                        wanted.push((key.clone(), a.clone(), 0));
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
                    let mut counts: Vec<(String, u32)> = Vec::new();
                    for (_, e) in r {
                        match counts.iter_mut().find(|(x, _)| x == e) {
                            Some(c) => c.1 += 1,
                            None => counts.push((e.clone(), 1)),
                        }
                    }
                    m.reactions = counts;
                    m.my_reaction = r.iter().find(|(s, _)| *s == me).map(|(_, e)| e.clone()).unwrap_or_default();
                }
                let fresh = outgoing && now.saturating_sub(*ts) < EDIT_WINDOW_MS;
                m.can_delete = fresh;
                m.can_edit = fresh
                    && dm.attachments.is_empty()
                    && dm.sticker.is_none()
                    && dm.body.as_deref().is_some_and(|b| !b.is_empty());
                if !m.sticker && m.attachment.is_empty() && m.image_key.is_empty() && m.quote.is_empty() {
                    m.big_emoji = crate::emoji::only_emoji(&m.text, 5).is_some();
                }
                if outgoing {
                    let (status, info, readers) = receipt_status(status.get(&c.timestamp()).or_else(|| status.get(ts)), members, me, name_of);
                    m.status = status;
                    m.readers = readers;
                    m.receipt_info = info;
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
            Kind::Pin { .. } => {
                m.system = true;
                m.text = pin_label(c, me, name_of);
                prev_sender = None;
            }
            Kind::Deleted => {
                // rows presage stored for messages sent from this PC have no server guid
                let mine = my_deletes.contains(ts) || c.metadata.server_guid.is_none();
                m.deleted = true;
                m.outgoing = mine;
                m.text = if mine { DELETED_MINE } else { DELETED_OTHER }.into();
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
    if is_group {
        mark_avatar_runs(&mut items);
    }
    Built { items, wanted_thumbs: wanted, next_expiry }
}

/// Optimistic update while our reaction is being sent: set (or with `None` take back) our reaction.
pub fn set_my_reaction(m: &mut MsgDto, emoji: Option<&str>) {
    if !m.my_reaction.is_empty() {
        if let Some(i) = m.reactions.iter().position(|(e, _)| *e == m.my_reaction) {
            m.reactions[i].1 -= 1;
            if m.reactions[i].1 == 0 {
                m.reactions.remove(i);
            }
        }
    }
    m.my_reaction = emoji.unwrap_or_default().to_string();
    if let Some(e) = emoji {
        match m.reactions.iter_mut().find(|(x, _)| x == e) {
            Some(r) => r.1 += 1,
            None => m.reactions.push((e.to_string(), 1)),
        }
    }
}

/// Optimistic update while an edit is being sent.
pub fn set_edited_text(m: &mut MsgDto, text: &str) {
    m.text = text.to_string();
    m.mentions.clear();
    if !m.time.ends_with("수정됨") {
        m.time = format!("{} · 수정됨", m.time);
    }
    m.big_emoji = m.attachment.is_empty() && m.quote.is_empty() && crate::emoji::only_emoji(&m.text, 5).is_some();
}

const DELETED_MINE: &str = "내가 삭제한 메시지입니다";
const DELETED_OTHER: &str = "삭제된 메시지입니다";

/// Optimistic update while a delete for everyone is being sent.
pub fn set_deleted(m: &mut MsgDto) {
    *m = MsgDto {
        id: std::mem::take(&mut m.id),
        time: std::mem::take(&mut m.time),
        day_sep: std::mem::take(&mut m.day_sep),
        outgoing: true,
        deleted: true,
        text: DELETED_MINE.into(),
        ..Default::default()
    };
}


/// Skeleton row for a message we are still sending; `prev_ts` is the row above (for the day separator).
pub fn outgoing_stub(ts: u64, prev_ts: Option<u64>) -> MsgDto {
    let dt = local(ts);
    let mut m = MsgDto { id: ts.to_string(), time: ampm(&dt), outgoing: true, pending: true, ..Default::default() };
    if prev_ts.map(|p| local(p).date_naive()) != Some(dt.date_naive()) {
        m.day_sep = day_label(&dt);
    }
    m
}

/// Status label for an outgoing message, plus a "who has it" breakdown.
fn receipt_status(
    recv: Option<&HashMap<Uuid, u8>>,
    members: &[Uuid],
    me: Uuid,
    name_of: &dyn Fn(&Uuid) -> String,
) -> (String, String, Vec<(String, String)>) {
    let empty = HashMap::new();
    let recv = recv.unwrap_or(&empty);
    let mut readers: Vec<(String, String)> =
        recv.iter().filter(|(_, s)| **s >= 2).map(|(u, _)| (name_of(u), u.to_string())).collect();
    readers.sort();
    let mut read: Vec<String> = readers.iter().map(|(n, _)| n.clone()).collect();
    let mut delivered: Vec<String> = recv.iter().filter(|(_, s)| **s == 1).map(|(u, _)| name_of(u)).collect();
    let mut pending: Vec<String> =
        members.iter().filter(|u| **u != me && !recv.contains_key(u)).map(|u| name_of(u)).collect();
    read.sort();
    delivered.sort();
    pending.sort();

    // read shows as ✓✓ too: the readers' photos under the bubble tell them apart
    let status = if !read.is_empty() || !delivered.is_empty() { "✓✓" } else { "✓" }.to_string();

    let mut info = Vec::new();
    if !read.is_empty() {
        info.push(format!("읽음 ({})\n{}", read.len(), read.join(", ")));
    }
    if !delivered.is_empty() {
        info.push(format!("전달됨 ({})\n{}", delivered.len(), delivered.join(", ")));
    }
    if !pending.is_empty() {
        info.push(format!("아직 전달 안 됨 ({})\n{}", pending.len(), pending.join(", ")));
    }
    if info.is_empty() {
        info.push("서버로 전송됨. 아직 상대 기기에 전달되지 않았습니다.".into());
    }
    info.push("※ 읽음 확인을 끈 사람은 '읽음'으로 표시되지 않습니다.".into());
    (status, info.join("\n\n"), readers.into_iter().map(|(n, u)| (u, n)).collect())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_become_names() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let r = |start, uuid: Uuid| BodyRange {
            start: Some(start),
            length: Some(1),
            associated_value: Some(AssociatedValue::MentionAci(uuid.to_string())),
        };
        let name = |u: &Uuid| if *u == a { "철수".to_string() } else { "영희".to_string() };
        // "😀" is two UTF-16 units, so the second placeholder sits at offset 5
        let text = "\u{FFFC} 😀 \u{FFFC} 안녕";
        assert_eq!(with_mentions(text, &[r(5, b), r(0, a)], &name), "@철수 😀 @영희 안녕");
        // style ranges are left alone
        let style = BodyRange { start: Some(0), length: Some(2), associated_value: Some(AssociatedValue::Style(1)) };
        assert_eq!(with_mentions("hi", &[style], &name), "hi");
        // binary ACI
        let bin = BodyRange {
            start: Some(3),
            length: Some(1),
            associated_value: Some(AssociatedValue::MentionAciBinary(a.as_bytes().to_vec())),
        };
        assert_eq!(with_mentions("hi \u{FFFC}", &[bin], &name), "hi @철수");
        // spans point at the "@name"s, after placeholder parts like a sticker label
        let dm = DataMessage {
            body: Some(text.into()),
            body_ranges: vec![r(0, a), r(5, b)],
            is_view_once: Some(true),
            ..Default::default()
        };
        let mut spans = Vec::new();
        let t = body_text_spans(&dm, &name, &mut spans);
        let got: Vec<&str> = spans.iter().map(|&(s, e, _)| &t[s..e]).collect();
        assert_eq!(got, ["@철수", "@영희"]);
    }

    #[test]
    fn pins_are_classified() {
        use presage::proto::data_message::{PinMessage, UnpinMessage};
        let author = Uuid::from_u128(7);
        let pin = |d| DataMessage {
            pin_message: Some(PinMessage {
                target_author_aci_binary: Some(author.as_bytes().to_vec()),
                target_sent_timestamp: Some(42),
                pin_duration: d,
            }),
            ..Default::default()
        };
        assert!(matches!(
            classify_data(&pin(Some(PinDuration::PinDurationSeconds(86400))), false),
            Kind::Pin { target: 42, author: a, secs: Some(86400) } if a == author
        ));
        assert!(matches!(
            classify_data(&pin(Some(PinDuration::PinDurationForever(true))), true),
            Kind::Pin { secs: None, .. }
        ));
        // no target author: ignored
        let mut bad = pin(None);
        bad.pin_message.as_mut().unwrap().target_author_aci_binary = None;
        assert!(matches!(classify_data(&bad, false), Kind::Skip));
        let unpin = DataMessage {
            unpin_message: Some(UnpinMessage { target_author_aci_binary: None, target_sent_timestamp: Some(42) }),
            ..Default::default()
        };
        assert!(matches!(classify_data(&unpin, false), Kind::Unpin { target: 42 }));
    }

    #[test]
    fn timer_labels() {
        assert_eq!(timer_label(30), "30초");
        assert_eq!(timer_label(300), "5분");
        assert_eq!(timer_label(8 * 3600), "8시간");
        assert_eq!(timer_label(86_400), "1일");
        assert_eq!(timer_label(4 * 604_800), "4주");
        assert_eq!(timer_label(90), "90초");
    }

    #[test]
    fn my_reaction_is_replaced_and_taken_back() {
        let mut m = MsgDto { reactions: vec![("👍".into(), 2)], my_reaction: "👍".into(), ..Default::default() };
        set_my_reaction(&mut m, Some("❤️"));
        assert_eq!(m.reactions, vec![("👍".to_string(), 1), ("❤️".to_string(), 1)]);
        // idempotent: applying the same pending op twice changes nothing
        set_my_reaction(&mut m, Some("❤️"));
        assert_eq!(m.reactions, vec![("👍".to_string(), 1), ("❤️".to_string(), 1)]);
        set_my_reaction(&mut m, None);
        assert_eq!(m.reactions, vec![("👍".to_string(), 1)]);
        assert!(m.my_reaction.is_empty());
    }
}
