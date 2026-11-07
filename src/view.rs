//! Turns stored `Content`s into display rows: folds reactions, edits and receipts into
//! the messages they target and drops protocol-only messages.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Local, TimeZone};
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::sync_message::{Content as SyncContent, Sent};
use presage::proto::body_range::AssociatedValue;
use presage::proto::data_message::pin_message::PinDuration;
use presage::proto::data_message::Quote;
use presage::proto::{receipt_message, AttachmentPointer, BodyRange, EditMessage, ReceiptMessage, SyncMessage};

use presage::store::ContentExt;

use crate::types::MsgDto;

/// Decode size of stickers (also part of their cache file names).
pub const THUMB_MAX: u32 = 400;
/// Photo boxes in the chat, logical px (w, h): landscape and square vs portrait. A square comes out 300x300.
const PHOTO_BOX_WIDE: (u32, u32) = (400, 300);
const PHOTO_BOX_TALL: (u32, u32) = (300, 450);
/// Decode size of photo thumbnails: the longest side a photo can show.
pub const PHOTO_THUMB_MAX: u32 = PHOTO_BOX_TALL.1;
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

/// `attachment_key` index of a link preview's image (not in `attachments`, like a sticker's).
pub const LINK_IMAGE_INDEX: usize = 98;
/// Size of that image on the card (logical px): a wide one on top, others beside the text.
const LINK_IMAGE_WIDE: (u32, u32) = (300, 158);
const LINK_IMAGE_SMALL: (u32, u32) = (64, 64);
/// Cut-offs for what a preview can say (characters).
pub const LINK_TITLE_MAX: usize = 200;
pub const LINK_DESC_MAX: usize = 300;

/// An image too small or of unknown size stays small (site logos).
pub fn link_image_size(w: Option<u32>, h: Option<u32>) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w >= 200 && h >= 100 && w >= h => LINK_IMAGE_WIDE,
        _ => LINK_IMAGE_SMALL,
    }
}

/// The preview to show on a message, and its image to fetch. Like Signal's clients, it counts
/// only if its url really is a link in the text.
pub fn link_card(dm: &DataMessage, ts: u64) -> Option<(crate::types::LinkCard, Option<WantedCell>)> {
    let p = dm.preview.first()?;
    let url = p.url.as_deref()?;
    if !crate::link::preview_allowed(url, dm.body.as_deref()?) {
        return None;
    }
    let title = crate::link::clean_preview_text(p.title.as_deref().unwrap_or_default(), LINK_TITLE_MAX);
    let description = crate::link::clean_preview_text(p.description.as_deref().unwrap_or_default(), LINK_DESC_MAX);
    if title.is_empty() && p.image.is_none() {
        return None;
    }
    let mut card = crate::types::LinkCard { url: url.to_string(), title, description, domain: crate::link::domain(url), ..Default::default() };
    let wanted = p.image.as_ref().map(|a| {
        let size = link_image_size(a.width, a.height);
        let file_key = attachment_key(a, ts, LINK_IMAGE_INDEX);
        card.image_key = format!("{file_key}@{}x{}", size.0, size.1);
        (card.img_w, card.img_h) = size;
        WantedCell { key: card.image_key.clone(), file_key, ptr: a.clone(), size }
    });
    Some((card, wanted))
}

/// Content type of a long message's full text: the body carries only its first 2KB.
pub const LONG_TEXT: &str = "text/x-signal-plain";
/// Signal's limits: inline body, and the long-text attachment.
pub const MAX_BODY_BYTES: usize = 2 * 1024;
pub const MAX_LONG_TEXT_BYTES: usize = 64 * 1024;

pub fn is_long_text(ptr: &AttachmentPointer) -> bool {
    ptr.content_type.as_deref() == Some(LONG_TEXT)
}

/// The long-text attachment of a message, with its cache key.
pub fn long_text(dm: &DataMessage, ts: u64) -> Option<(String, &AttachmentPointer)> {
    let (i, a) = dm.attachments.iter().enumerate().find(|(_, a)| is_long_text(a))?;
    Some((attachment_key(a, ts, i), a))
}

/// Attachments shown as such (the long-text one is part of the body).
pub fn shown_attachments(dm: &DataMessage) -> impl Iterator<Item = (usize, &AttachmentPointer)> {
    dm.attachments.iter().enumerate().filter(|(_, a)| !is_long_text(a))
}

/// Cuts `text` to at most `max` bytes on a char boundary (the inline body of a long message).
pub fn trim_body(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// An edit that took `prev` past `max` bytes: keeps what was there and as much of the inserted
/// part as fits. Returns the text and the cursor (byte offset, after the kept insert), or None
/// if `new` fits.
pub fn fit_edit(prev: &str, new: &str, max: usize) -> Option<(String, usize)> {
    if new.len() <= max {
        return None;
    }
    // the inserted part lies between the common prefix and the common suffix
    let pre = prev.char_indices().zip(new.chars()).find(|((_, a), b)| a != b).map_or(prev.len().min(new.len()), |((i, _), _)| i);
    let suf_max = prev.len().min(new.len()) - pre;
    let suf = prev[pre..]
        .chars()
        .rev()
        .zip(new[pre..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .scan(0, |n, l| {
            *n += l;
            Some(*n)
        })
        .take_while(|n| *n <= suf_max)
        .last()
        .unwrap_or(0);
    let (head, tail) = (&new[..pre], &new[new.len() - suf..]);
    let room = max.saturating_sub(head.len() + tail.len());
    let inserted = trim_body(&new[pre..new.len() - suf], room);
    Some((format!("{head}{inserted}{tail}"), pre + inserted.len()))
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

pub fn mention_uuid(r: &BodyRange) -> Option<Uuid> {
    match r.associated_value.as_ref()? {
        AssociatedValue::MentionAci(s) => s.parse().ok(),
        AssociatedValue::MentionAciBinary(b) => Uuid::from_slice(b).ok(),
        AssociatedValue::Style(_) => None,
    }
}

/// Everyone named in a data message (@mentions in body and quote, the quoted author),
/// so callers can resolve names first.
pub fn mentioned(dm: &DataMessage) -> Vec<Uuid> {
    let quote = dm.quote.iter().flat_map(|q| q.body_ranges.iter());
    let mut v: Vec<Uuid> = dm.body_ranges.iter().chain(quote).filter_map(mention_uuid).collect();
    v.extend(dm.quote.as_ref().and_then(quote_author));
    v
}

/// Author of a quoted message (newer clients send only the binary form).
pub fn quote_author(q: &Quote) -> Option<Uuid> {
    q.author_aci_binary
        .as_deref()
        .and_then(|b| Uuid::from_slice(b).ok())
        .or_else(|| q.author_aci.as_deref()?.parse().ok())
}

/// The name shown above a quote: "나" for our own messages.
pub fn quote_author_name(q: &Quote, me: Uuid, name_of: &dyn Fn(&Uuid) -> String) -> String {
    match quote_author(q) {
        Some(u) if u == me => "나".into(),
        Some(u) => name_of(&u),
        None => String::new(),
    }
}

/// Replaces mention ranges (a U+FFFC placeholder in the text, offsets in UTF-16 units) with "@name",
/// and spoilers with blocks (previews never show them).
pub fn with_mentions(text: &str, ranges: &[BodyRange], name_of: &dyn Fn(&Uuid) -> String) -> String {
    let (mut out, mut styles) = (String::new(), Vec::new());
    push_with_mentions(&mut out, text, ranges, name_of, &mut Vec::new(), &mut styles);
    mask_spoilers(out, &styles)
}

/// Covers spoiler ranges with a fixed run of blocks (which hides their length too).
pub fn mask_spoilers(mut text: String, styles: &[(usize, usize, u8)]) -> String {
    let mut spoilers: Vec<(usize, usize)> =
        styles.iter().filter(|s| s.2 == STYLE_SPOILER).map(|&(s, e, _)| (s, e)).collect();
    spoilers.sort_unstable();
    // merge overlapping ones
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spoilers {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    for &(s, e) in merged.iter().rev() {
        text.replace_range(s..e, "■■■■");
    }
    text
}

/// Text styles of `BodyRange` (the proto's `Style` values).
pub const STYLE_BOLD: u8 = 1;
pub const STYLE_ITALIC: u8 = 2;
pub const STYLE_SPOILER: u8 = 3;
pub const STYLE_STRIKE: u8 = 4;
pub const STYLE_MONO: u8 = 5;

/// Appends `text` to `out` with mentions replaced; records each "@name" as a byte range of `out`,
/// and each style range as (start, end, style) bytes of `out`.
fn push_with_mentions(
    out: &mut String,
    text: &str,
    ranges: &[BodyRange],
    name_of: &dyn Fn(&Uuid) -> String,
    spans: &mut Vec<(usize, usize, String)>,
    styles: &mut Vec<(usize, usize, u8)>,
) {
    let base = out.len();
    // (UTF-16 offset in `text`, byte offset in `out`) at every char start, plus the end
    let mut map: Vec<(usize, usize)> = Vec::new();
    push_mentions(out, text, ranges, name_of, spans, &mut map);
    map.push((text.encode_utf16().count(), out.len()));
    let at = |u16: usize| match map.binary_search_by_key(&u16, |&(u, _)| u) {
        Ok(i) => map[i].1,
        // inside a surrogate pair: the char's end
        Err(i) => map.get(i).map_or(out.len(), |&(_, b)| b),
    };
    for r in ranges {
        let (Some(start), Some(AssociatedValue::Style(s))) = (r.start, &r.associated_value) else { continue };
        let Ok(s @ STYLE_BOLD..=STYLE_MONO) = u8::try_from(*s) else { continue };
        let start = start.max(0) as usize;
        let (a, b) = (at(start), at(start + r.length.unwrap_or(0).max(0) as usize));
        if b > a && a >= base {
            styles.push((a, b, s));
        }
    }
}

fn push_mentions(
    out: &mut String,
    text: &str,
    ranges: &[BodyRange],
    name_of: &dyn Fn(&Uuid) -> String,
    spans: &mut Vec<(usize, usize, String)>,
    map: &mut Vec<(usize, usize)>,
) {
    let mut ms: Vec<(usize, usize, Uuid)> = ranges
        .iter()
        .filter_map(|r| Some((r.start? as usize, r.length.unwrap_or(0) as usize, mention_uuid(r)?)))
        .collect();
    if ms.is_empty() {
        let mut pos = 0;
        for (i, ch) in text.char_indices() {
            map.push((pos, out.len() + i));
            pos += ch.len_utf16();
        }
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
                map.push((pos, out.len()));
                mention(out, &uuid);
                skip_until = pos + len;
            }
        }
        if pos >= skip_until {
            if map.last().is_none_or(|&(p, _)| p != pos) {
                map.push((pos, out.len()));
            }
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
/// Spoilers are masked: this is for previews.
fn body_text(dm: &DataMessage, name_of: &dyn Fn(&Uuid) -> String) -> String {
    let mut styles = Vec::new();
    let t = body_text_spans(dm, name_of, &mut Vec::new(), &mut styles);
    mask_spoilers(t, &styles)
}

/// `body_text`, recording where the mentions and styles ended up (byte ranges).
fn body_text_spans(
    dm: &DataMessage,
    name_of: &dyn Fn(&Uuid) -> String,
    spans: &mut Vec<(usize, usize, String)>,
    styles: &mut Vec<(usize, usize, u8)>,
) -> String {
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
        push_with_mentions(&mut out, b, &dm.body_ranges, name_of, spans, styles);
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
        t = shown_attachments(dm)
            .next()
            .map(|(_, a)| {
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

/// A future moment: "오후 3:00" today, "내일 오후 3:00", "11월 6일 오후 3:00", with the year
/// when it differs.
pub fn time_until(ts: u64) -> String {
    let dt = local(ts);
    let now = Local::now();
    let days = (dt.date_naive() - now.date_naive()).num_days();
    match days {
        0 => ampm(&dt),
        1 => format!("내일 {}", ampm(&dt)),
        _ if dt.year() == now.year() => format!("{}월 {}일 {}", dt.month(), dt.day(), ampm(&dt)),
        _ => format!("{}년 {}월 {}일 {}", dt.year(), dt.month(), dt.day(), ampm(&dt)),
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

/// An image wanted for a cell of a message's grid.
pub struct WantedCell {
    /// what the UI knows it by
    pub key: String,
    /// its files on disk
    pub file_key: String,
    pub ptr: AttachmentPointer,
    /// logical px
    pub size: (u32, u32),
}

/// Images a grid shows; the rest are a "+N" on its last cell and wait for the viewer.
const GRID_SHOWN: usize = 5;

/// Where a grid of `count` images (2 or more) puts them: x, y, width and height in logical px,
/// with a 1px gap. As in Signal's apps, the first ones are the largest.
fn grid_cells(count: usize) -> &'static [(u32, u32, u32, u32)] {
    match count {
        0 | 1 => &[],
        2 => &[(0, 0, 150, 150), (151, 0, 150, 150)],
        3 => &[(0, 0, 200, 201), (201, 0, 100, 100), (201, 101, 100, 100)],
        4 => &[(0, 0, 150, 150), (151, 0, 150, 150), (0, 151, 150, 150), (151, 151, 150, 150)],
        _ => &[(0, 0, 150, 150), (151, 0, 150, 150), (0, 151, 100, 100), (101, 151, 99, 100), (201, 151, 100, 100)],
    }
}

/// Makes `m` show `count` images as a grid, if that is 2 or more. `key` gets the index of each
/// image shown and the size of its cell, and returns its key in the UI thumbnail cache.
pub fn set_photo_grid(m: &mut MsgDto, count: usize, mut key: impl FnMut(usize, u32, u32) -> String) {
    let cells = grid_cells(count);
    let more = count.saturating_sub(GRID_SHOWN) as u32;
    m.cells = cells
        .iter()
        .enumerate()
        .map(|(n, &(x, y, w, h))| crate::types::CellDto {
            key: key(n, w, h),
            x,
            y,
            w,
            h,
            more: if n + 1 == GRID_SHOWN { more } else { 0 },
        })
        .collect();
    let Some(first) = m.cells.first() else { return };
    m.image_key = first.key.clone();
    m.img_w = cells.iter().map(|c| c.0 + c.2).max().unwrap_or(0);
    m.img_h = cells.iter().map(|c| c.1 + c.3).max().unwrap_or(0);
}

fn fit(w: Option<u32>, h: Option<u32>) -> (u32, u32) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            // scale into the box for its orientation, never up
            let (bw, bh) = if h > w { PHOTO_BOX_TALL } else { PHOTO_BOX_WIDE };
            let s = (bw as f32 / w as f32).min(bh as f32 / h as f32).min(1.0);
            (((w as f32 * s) as u32).max(40), ((h as f32 * s) as u32).max(40))
        }
        _ => PHOTO_BOX_WIDE,
    }
}

/// What a row shows but the window's rows may not say: kept apart from them, because what
/// changes a message is stored later than the message and may be outside the window.
#[derive(Default)]
pub struct Indexed {
    /// target ts -> recipient -> 1 delivered / 2 read
    pub status: HashMap<u64, HashMap<Uuid, u8>>,
    /// our messages deleted for everyone (the stored row no longer says who sent it)
    pub my_deletes: std::collections::HashSet<u64>,
    /// target ts -> (who, emoji), oldest first
    pub reactions: HashMap<u64, Vec<(Uuid, String)>>,
    /// stored edits of window rows that aren't in the window themselves: (stored ts, content)
    pub edits: Vec<(u64, Content)>,
}

pub struct Built {
    pub items: Vec<MsgDto>,
    /// image attachments that need a thumbnail: (key, pointer, square box in logical px to fit into, 0 = as is)
    pub wanted_thumbs: Vec<(String, AttachmentPointer, u32)>,
    pub wanted_cells: Vec<WantedCell>,
    /// long-text attachments not in `LongTexts::full` yet: (key, pointer)
    pub wanted_texts: Vec<(String, AttachmentPointer)>,
    /// earliest time a still-shown disappearing message expires
    pub next_expiry: Option<u64>,
}

/// Messages closer than this from one sender form a run (Signal Desktop: `COLLAPSE_WITHIN`).
const COLLAPSE_WITHIN_MS: u64 = 3 * 60 * 1000;

/// Whether `newer` continues the run of `older` (Signal Desktop's `areMessagesInSameGroup`).
fn same_run(older: &MsgDto, newer: &MsgDto) -> bool {
    !older.system
        && !newer.system
        // the retry / discard links hang below the bubble
        && !older.failed
        && !newer.failed
        // reactions hang below the bubble
        && older.reactions.is_empty()
        && older.outgoing == newer.outgoing
        // deleted rows from others no longer say who sent them
        && (older.outgoing || !older.sender_key.is_empty())
        && older.sender_key == newer.sender_key
        && newer.day_sep.is_empty()
        && newer.ts.abs_diff(older.ts) < COLLAPSE_WITHIN_MS
}

/// Marks runs of messages from one sender. `items` must be in display order (oldest first),
/// pending rows included. Within a run only the last row shows the time / status (unless it
/// is still being sent, edited or pinned), group rows show the name on the first row and the
/// photo on the last; corners and spacing between collapsed rows shrink in the UI.
pub fn mark_runs(items: &mut [MsgDto], is_group: bool) {
    for i in 0..items.len() {
        let above = i > 0 && same_run(&items[i - 1], &items[i]);
        let below = items.get(i + 1).is_some_and(|n| same_run(&items[i], n));
        let next_deleted = items.get(i + 1).is_some_and(|n| n.deleted);
        let m = &mut items[i];
        m.collapse_above = above;
        m.collapse_below = below;
        m.hide_time = below && !m.pending && !m.edited && !m.pinned && !(next_deleted && !m.deleted);
        let col = is_group && !m.outgoing && !m.system;
        m.avatar_col = col;
        m.show_avatar = col && !below && !m.sender_key.is_empty();
        m.show_sender = col && !above && !m.sender.is_empty() && !m.deleted;
    }
}

pub fn build(
    window: &[(u64, Content)],
    me: Uuid,
    members: &[Uuid],
    indexed: &Indexed,
    long: &LongTexts,
    name_of: &dyn Fn(&Uuid) -> String,
) -> Built {
    let Indexed { status, my_deletes, reactions, edits: outside } = indexed;
    let mut edits: HashMap<u64, (u64, &DataMessage, Uuid)> = HashMap::new();
    let now = now_ms();
    let mut next_expiry: Option<u64> = None;

    for (ts, c) in outside.iter().chain(window) {
        if let Kind::Edit { target, dm } = classify(c, me) {
            let sender = c.metadata.sender.raw_uuid();
            let e = edits.entry(target).or_insert((*ts, dm, sender));
            if *ts >= e.0 {
                *e = (*ts, dm, sender);
            }
        }
    }

    let mut items = Vec::new();
    let mut wanted = Vec::new();
    let mut wanted_cells = Vec::new();
    let mut wanted_texts = Vec::new();
    let mut prev_day = None;

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
        let mut m = MsgDto { id: ts.to_string(), ts: *ts, time: ampm(&dt), ..Default::default() };
        match kind {
            Kind::Msg { outgoing, dm } => {
                let sender = c.metadata.sender.raw_uuid();
                // only the author may edit a message
                let (dm, edited) = match edits.get(ts) {
                    Some((_, e, by)) if *by == sender => (*e, true),
                    _ => (dm, false),
                };
                m.outgoing = outgoing;
                // a long message: its full text replaces the 2KB body once downloaded
                let full;
                let mut text_dm = dm;
                let mut text_missing = false;
                if let Some((key, ptr)) = long_text(dm, *ts) {
                    match long.full.get(&key) {
                        Some(t) => {
                            full = DataMessage { body: Some(t.clone()), ..dm.clone() };
                            text_dm = &full;
                        }
                        None => {
                            text_missing = true;
                            wanted_texts.push((key, ptr.clone()));
                        }
                    }
                }
                m.text = body_text_spans(text_dm, name_of, &mut m.mentions, &mut m.styles);
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
                    m.styles.clear();
                    push_with_mentions(
                        &mut m.text,
                        dm.body.as_deref().unwrap_or_default(),
                        &dm.body_ranges,
                        name_of,
                        &mut m.mentions,
                        &mut m.styles,
                    );
                }
                if !outgoing {
                    m.sender = name_of(&sender);
                    m.sender_key = sender.to_string();
                }
                if let Some(q) = &dm.quote {
                    m.quote = q
                        .text
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .map(|t| with_mentions(t, &q.body_ranges, name_of))
                        .unwrap_or_else(|| "(첨부파일)".into());
                    m.quote_author = quote_author_name(q, me, name_of);
                    m.quote_id = q.id.map(|i| i.to_string()).unwrap_or_default();
                }
                let mut others = 0;
                let mut photos = Vec::new();
                for (i, a) in shown_attachments(dm) {
                    if !m.sticker && is_image(a) {
                        photos.push((i, a));
                    } else if m.attachment.is_empty() {
                        m.attachment = attachment_label(a);
                    } else {
                        others += 1;
                    }
                }
                if let [(i, a)] = photos[..] {
                    let key = attachment_key(a, *ts, i);
                    let (w, h) = fit(a.width, a.height);
                    m.img_w = w;
                    m.img_h = h;
                    // pre-resample to the on-screen size (nearest-neighbor scaling is jagged)
                    wanted.push((key.clone(), a.clone(), w.max(h)));
                    m.image_key = key;
                } else {
                    set_photo_grid(&mut m, photos.len(), |n, w, h| {
                        let (i, a) = photos[n];
                        let file_key = attachment_key(a, *ts, i);
                        // the same image may also be shown alone, or in a cell of another size
                        let key = format!("{file_key}@{w}x{h}");
                        wanted_cells.push(WantedCell { key: key.clone(), file_key, ptr: a.clone(), size: (w, h) });
                        key
                    });
                }
                if others > 0 {
                    m.attachment = format!("{} 외 {}개", m.attachment, others);
                }
                // not next to attachments (Signal's clients don't send that), and it would give
                // away what a spoiler in the text hides
                if !m.sticker && m.attachment.is_empty() && m.image_key.is_empty() && !m.styles.iter().any(|s| s.2 == STYLE_SPOILER) {
                    if let Some((card, image)) = link_card(dm, *ts) {
                        m.link = Some(card);
                        wanted_cells.extend(image);
                    }
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
                m.more = cut_for_read_more(&mut m, long.shown.get(ts).copied().unwrap_or(READ_MORE_AT));
                // the composer is filled from the shown text: only when all of it is shown
                m.can_edit = fresh
                    && !m.more
                    && !text_missing
                    && shown_attachments(dm).next().is_none()
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
                    m.edited = true;
                }
            }
            Kind::System(s) => {
                m.system = true;
                m.text = s;
            }
            Kind::Pin { .. } => {
                m.system = true;
                m.text = pin_label(c, me, name_of);
            }
            Kind::Deleted => {
                // rows presage stored for messages sent from this PC have no server guid
                let mine = my_deletes.contains(ts) || c.metadata.server_guid.is_none();
                m.deleted = true;
                m.outgoing = mine;
                m.text = if mine { DELETED_MINE } else { DELETED_OTHER }.into();
            }
            _ => continue,
        }
        if prev_day != Some(day) {
            m.day_sep = day_label(&dt);
            prev_day = Some(day);
        }
        items.push(m);
    }
    // Whoever read a message has read everything before it: a reader's photo stays on the newest
    // message they read only (1:1 and groups).
    let mut seen = std::collections::HashSet::new();
    for m in items.iter_mut().rev().filter(|m| m.outgoing && !m.readers.is_empty()) {
        m.readers.retain(|(u, _)| seen.insert(u.clone()));
    }
    // runs (`mark_runs`) are marked by the caller once pins, pending ops and stub rows are in
    Built { items, wanted_thumbs: wanted, wanted_cells, wanted_texts, next_expiry }
}

/// Texts of long messages in the open window, and how much of each is shown.
#[derive(Default)]
pub struct LongTexts {
    /// long-text attachment key -> full text
    pub full: HashMap<String, String>,
    /// row -> characters shown, for rows expanded with "더 읽기"
    pub shown: HashMap<u64, usize>,
}

/// Signal Desktop's `MessageBodyReadMore`: 800 characters at first, 3000 more per click,
/// and no cut that would hide fewer than 100.
pub const READ_MORE_AT: usize = 800;
pub const READ_MORE_STEP: usize = 3000;
const READ_MORE_SLACK: usize = 100;

/// Shortens a row's text to about `limit` characters (whole words, never inside a mention);
/// true if something was cut.
fn cut_for_read_more(m: &mut MsgDto, limit: usize) -> bool {
    let Some((mut cut, _)) = m.text.char_indices().nth(limit) else { return false };
    if m.text[cut..].chars().nth(READ_MORE_SLACK).is_none() {
        return false;
    }
    // back up to a space so words and links stay whole, unless that loses too much
    if let Some(sp) = m.text[..cut].rfind(char::is_whitespace) {
        if m.text[sp..cut].chars().count() <= READ_MORE_SLACK {
            cut = sp;
        }
    }
    if let Some(&(s, _, _)) = m.mentions.iter().find(|(s, e, _)| *s < cut && cut < *e) {
        cut = s;
    }
    m.text.truncate(cut);
    let kept = m.text.trim_end().len();
    m.text.truncate(kept);
    m.text.push('…');
    m.mentions.retain(|(_, e, _)| *e <= kept);
    m.styles.retain_mut(|st| {
        st.1 = st.1.min(kept);
        st.0 < st.1
    });
    true
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
    (m.text, m.styles) = crate::format::parse(text);
    m.mentions.clear();
    m.edited = true;
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
        ts: m.ts,
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
    let mut m = MsgDto { id: ts.to_string(), ts, time: ampm(&dt), outgoing: true, pending: true, ..Default::default() };
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
    fn photo_size_caps() {
        let f = |w, h| fit(Some(w), Some(h));
        assert_eq!(f(3000, 3000), (300, 300)); // square
        assert_eq!(f(4000, 3000), (400, 300)); // 4:3 unchanged
        assert_eq!(f(3000, 4000), (300, 400)); // 3:4 is width-limited
        assert_eq!(f(2000, 3000), (300, 450)); // 2:3
        assert_eq!(f(900, 1600), (253, 450)); // 9:16
        assert_eq!(f(1600, 900), (400, 225)); // 16:9 unchanged
        assert_eq!(f(100, 100), (100, 100)); // never upscaled
        assert_eq!(fit(None, None), (400, 300));
    }

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
        let t = body_text_spans(&dm, &name, &mut spans, &mut Vec::new());
        let got: Vec<&str> = spans.iter().map(|&(s, e, _)| &t[s..e]).collect();
        assert_eq!(got, ["@철수", "@영희"]);
    }

    #[test]
    fn styles_follow_mentions() {
        let a = Uuid::from_u128(1);
        let name = |_: &Uuid| "철수".to_string();
        let st = |start, length, s| BodyRange { start: Some(start), length: Some(length), associated_value: Some(AssociatedValue::Style(s)) };
        let mention = BodyRange {
            start: Some(3),
            length: Some(1),
            associated_value: Some(AssociatedValue::MentionAci(a.to_string())),
        };
        // "😀" is two UTF-16 units; the mention grows from 1 unit to "@철수"
        let text = "😀 \u{FFFC} 굵게 끝";
        let ranges = [mention, st(5, 2, 1), st(0, 2, 3), st(3, 1, 2), st(8, 1, 9)];
        let (mut out, mut spans, mut styles) = (String::from("x\n"), Vec::new(), Vec::new());
        push_with_mentions(&mut out, text, &ranges, &name, &mut spans, &mut styles);
        assert_eq!(out, "x\n😀 @철수 굵게 끝");
        let got: Vec<(&str, u8)> = styles.iter().map(|&(s, e, k)| (&out[s..e], k)).collect();
        assert_eq!(got, [("굵게", 1), ("😀", 3), ("@철수", 2)]);
        // no mentions
        let mut out = String::new();
        let mut styles = Vec::new();
        push_with_mentions(&mut out, "ab한글", &[st(2, 2, 4)], &name, &mut Vec::new(), &mut styles);
        assert_eq!(&out[styles[0].0..styles[0].1], "한글");
        // previews mask spoilers, overlapping ones too
        assert_eq!(with_mentions("ab 비밀 c 둘", &[st(3, 2, 3), st(8, 1, 3), st(7, 2, 3)], &name), "ab ■■■■ c■■■■");
    }

    #[test]
    fn over_long_edits_keep_what_fits() {
        assert_eq!(fit_edit("abc", "abcd", 4), None);
        // typed at the end
        assert_eq!(fit_edit("abcd", "abcde", 4), Some(("abcd".into(), 4)));
        // pasted in the middle: the paste is cut, the text around it stays
        assert_eq!(fit_edit("ab|cd", "ab|XYZW|cd", 7), Some(("ab|XYcd".into(), 5)));
        // never half a char
        assert_eq!(fit_edit("ab", "a가나b", 5), Some(("a가b".into(), 4)));
        // repeated chars around the insert
        assert_eq!(fit_edit("aa", "aaaa", 3), Some(("aaa".into(), 3)));
        // a stale `prev` (the draft was set from Rust) still ends within the limit
        let (t, _) = fit_edit("zzz", "abcdef", 4).unwrap();
        assert!(t.len() <= 4);
    }

    #[test]
    fn read_more_cuts_at_a_space() {
        let words = "단어 ".repeat(400); // 1200 chars
        let mut m = MsgDto { text: words.clone(), styles: vec![(0, words.len(), STYLE_BOLD)], ..Default::default() };
        assert!(cut_for_read_more(&mut m, READ_MORE_AT));
        assert!(m.text.ends_with("단어…"));
        assert!(m.text.chars().count() <= READ_MORE_AT + 1);
        let kept = m.text.len() - '…'.len_utf8();
        assert_eq!(m.styles, [(0, kept, STYLE_BOLD)]);
        // a little over the limit is shown whole
        let mut m = MsgDto { text: "가".repeat(READ_MORE_AT + 50), ..Default::default() };
        assert!(!cut_for_read_more(&mut m, READ_MORE_AT));
        // no space nearby: cut mid-word, but never inside a mention
        let text = format!("{}@철수 끝{}", "가".repeat(799), "나".repeat(300));
        let at = text.find('@').unwrap();
        let mut m = MsgDto { text, mentions: vec![(at, at + 7, "u".into())], ..Default::default() };
        assert!(cut_for_read_more(&mut m, READ_MORE_AT));
        assert!(m.mentions.is_empty());
        assert_eq!(m.text, format!("{}…", "가".repeat(799)));
    }

    fn incoming(dm: DataMessage) -> Content {
        Content::from_body(dm, presage::libsignal_service::content::Metadata {
            sender: presage::libsignal_service::protocol::ServiceId::Aci(Uuid::from_u128(2).into()),
            destination: presage::libsignal_service::protocol::ServiceId::Aci(Uuid::from_u128(1).into()),
            sender_device: 1u32.try_into().unwrap(),
            pni_verified: None,
            client_timestamp: chrono::DateTime::from_timestamp_millis(5).unwrap(),
            server_timestamp: chrono::DateTime::from_timestamp_millis(5).unwrap(),
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        })
    }

    #[test]
    fn grid_cells_fill_their_box_without_touching() {
        for count in 2..8 {
            let cells = grid_cells(count);
            assert_eq!(cells.len(), count.min(GRID_SHOWN));
            let width = cells.iter().map(|c| c.0 + c.2).max().unwrap();
            let height = cells.iter().map(|c| c.1 + c.3).max().unwrap();
            assert_eq!(width, 301);
            let area: u32 = cells.iter().map(|c| c.2 * c.3).sum();
            // what's left of the box is the 1px gaps
            assert!(area < width * height && width * height - area <= 2 * width + height, "{count}");
            for (i, a) in cells.iter().enumerate() {
                for b in &cells[i + 1..] {
                    let apart = a.0 + a.2 < b.0 + 1 || b.0 + b.2 < a.0 + 1 || a.1 + a.3 < b.1 + 1 || b.1 + b.3 < a.1 + 1;
                    assert!(apart, "{count}: {a:?} {b:?}");
                }
            }
        }
    }

    #[test]
    fn link_preview_shows_only_for_a_link_in_the_text() {
        let name = |_: &Uuid| String::new();
        let long = LongTexts::default();
        let image = AttachmentPointer { digest: Some(vec![7; 32]), width: Some(1280), height: Some(720), ..Default::default() };
        let preview = |url: &str| presage::proto::Preview {
            url: Some(url.into()),
            title: Some("제목".into()),
            image: Some(image.clone()),
            ..Default::default()
        };
        let run = |dm: DataMessage| {
            let window = [(5u64, incoming(dm))];
            build(&window, Uuid::from_u128(1), &[], &Default::default(), &long, &name)
        };
        let url = "https://www.youtube.com/watch?v=x";
        let body = format!("봐 {url}");

        let b = run(DataMessage { body: Some(body.clone()), preview: vec![preview(url)], ..Default::default() });
        let card = b.items[0].link.as_ref().unwrap();
        assert_eq!((card.domain.as_str(), card.title.as_str(), card.img_w), ("youtube.com", "제목", 300));
        assert_eq!(b.wanted_cells[0].key, card.image_key);

        // claims to be another site than the link in the text
        let b = run(DataMessage { body: Some("https://evil.example.kr/a".into()), preview: vec![preview(url)], ..Default::default() });
        assert!(b.items[0].link.is_none());
        assert!(b.wanted_cells.is_empty());

        // beside a file: the file is what shows
        let pdf = AttachmentPointer { content_type: Some("application/pdf".into()), ..Default::default() };
        let b = run(DataMessage { body: Some(body.clone()), preview: vec![preview(url)], attachments: vec![pdf], ..Default::default() });
        assert!(b.items[0].link.is_none());

        // it would give away what the spoiler hides
        let spoiler = BodyRange {
            start: Some(0),
            length: Some(body.encode_utf16().count() as u32),
            associated_value: Some(AssociatedValue::Style(presage::proto::body_range::Style::Spoiler as i32)),
        };
        let b = run(DataMessage { body: Some(body.clone()), body_ranges: vec![spoiler], preview: vec![preview(url)], ..Default::default() });
        assert!(b.items[0].link.is_none());
    }

    #[test]
    fn several_photos_make_a_grid_and_files_stay_files() {
        let photo = |d: u8| AttachmentPointer { content_type: Some("image/png".into()), digest: Some(vec![d; 32]), ..Default::default() };
        let pdf = |d: u8| AttachmentPointer {
            content_type: Some("application/pdf".into()),
            file_name: Some(format!("{d}.pdf")),
            digest: Some(vec![d; 32]),
            ..Default::default()
        };
        let name = |_: &Uuid| String::new();
        let long = LongTexts::default();
        let run = |attachments: Vec<AttachmentPointer>| {
            let window = [(5u64, incoming(DataMessage { attachments, ..Default::default() }))];
            build(&window, Uuid::from_u128(1), &[], &Default::default(), &long, &name)
        };

        let b = run(vec![photo(1), pdf(2), photo(3), photo(4), pdf(5)]);
        let m = &b.items[0];
        assert_eq!(m.attachment, "2.pdf 외 1개");
        assert_eq!((m.cells.len(), m.img_w, m.img_h), (3, 301, 201));
        assert_eq!(m.image_key, m.cells[0].key);
        assert_eq!(b.wanted_cells.len(), 3);
        assert!(b.wanted_thumbs.is_empty());
        assert_eq!(b.wanted_cells[1].file_key, attachment_key(&photo(3), 5, 2));
        // the key tells a cell from the same image shown alone
        assert_ne!(b.wanted_cells[1].key, b.wanted_cells[1].file_key);

        // past the last cell: counted on it, and not downloaded
        let b = run((1..=8).map(photo).collect());
        assert_eq!(b.wanted_cells.len(), GRID_SHOWN);
        let more: Vec<u32> = b.items[0].cells.iter().map(|c| c.more).collect();
        assert_eq!(more, [0, 0, 0, 0, 3]);

        // one photo stays as it was
        let b = run(vec![photo(1)]);
        assert!(b.items[0].cells.is_empty());
        assert_eq!(b.wanted_thumbs.len(), 1);
    }

    /// What changed a message may be stored past the window's end.
    #[test]
    fn rows_show_reactions_and_edits_stored_outside_the_window() {
        let original = incoming(DataMessage { body: Some("before".into()), ..Default::default() });
        let (row, author) = (original.metadata.client_timestamp.timestamp_millis() as u64, original.metadata.sender.raw_uuid());
        let edit = EditMessage {
            target_sent_timestamp: Some(row),
            data_message: Some(DataMessage { body: Some("after".into()), ..Default::default() }),
        };
        let mut indexed = Indexed {
            edits: vec![(row + 1, Content::from_body(edit, original.metadata.clone()))],
            ..Default::default()
        };
        indexed.reactions.insert(row, vec![(author, "up".into()), (Uuid::from_u128(1), "up".into())]);
        let window = [(row, original)];
        let b = build(&window, Uuid::from_u128(1), &[], &indexed, &LongTexts::default(), &|_: &Uuid| String::new());
        assert_eq!(b.items[0].text, "after");
        assert_eq!(b.items[0].reactions, [("up".to_string(), 2)]);
        assert_eq!(b.items[0].my_reaction, "up");
    }

    #[test]
    fn long_text_attachment_is_part_of_the_body() {
        let long = AttachmentPointer { content_type: Some(LONG_TEXT.into()), digest: Some(vec![7; 32]), ..Default::default() };
        let photo = AttachmentPointer { content_type: Some("image/png".into()), digest: Some(vec![8; 32]), ..Default::default() };
        let dm = DataMessage { body: Some("앞부분".into()), attachments: vec![long, photo], ..Default::default() };
        let window = [(5u64, incoming(dm))];
        let name = |_: &Uuid| String::new();
        let run = |long: &LongTexts| build(&window, Uuid::from_u128(1), &[], &Default::default(), long, &name);
        let b = run(&LongTexts::default());
        assert_eq!(b.items[0].text, "앞부분");
        assert_eq!(b.wanted_texts.len(), 1);
        // the long text isn't listed as a file
        assert!(b.items[0].attachment.is_empty());
        let mut long = LongTexts::default();
        long.full.insert(b.wanted_texts[0].0.clone(), "앞부분 그리고 나머지".into());
        let b = run(&long);
        assert_eq!(b.items[0].text, "앞부분 그리고 나머지");
        assert!(b.wanted_texts.is_empty());
        assert_eq!(trim_body("가나", 4), "가");
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
