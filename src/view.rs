//! Turns stored `Content`s into display rows: folds reactions, edits and receipts into
//! the messages they target and drops protocol-only messages.

pub mod media;
pub mod text;
pub mod time;

use std::collections::HashMap;

use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::sync_message::{Content as SyncContent, Sent};
use presage::proto::data_message::pin_message::PinDuration;
use presage::proto::data_message::Quote;
use presage::proto::{receipt_message, AttachmentPointer, EditMessage, ReceiptMessage, SyncMessage};

use presage::store::ContentExt;

use crate::types::MsgDto;
use media::{attachment_key, link_card, set_attachments, shown_attachments, WantedCell, STICKER_SIZE};
use text::{
    body_text_spans, cut_for_read_more, long_text, pin_label, push_with_mentions, quote_author_name, with_mentions, LongTexts,
    READ_MORE_AT, STYLE_SPOILER,
};
use time::{ampm, day_num, local, now_ms, timer_label};

/// DataMessage.Flags.EXPIRATION_TIMER_UPDATE
const EXPIRATION_TIMER_UPDATE: u32 = 2;
/// Signal accepts edits and deletes for everyone up to a day after sending.
pub const EDIT_WINDOW_MS: u64 = 24 * 3600 * 1000;

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

/// `attachment_key` index of the picture in a reply's quote, and its size (logical px).
const QUOTE_IMAGE_INDEX: usize = 97;
/// Logical size of the picture in a quote box; `ui/bubble.slint` `QuoteBox` draws it at the same size.
pub(crate) const QUOTE_IMAGE: (u32, u32) = (54, 54);

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
    /// stored edits of window rows: (stored ts, content)
    pub edits: Vec<(u64, Content)>,
    /// target ts -> how many messages quote it
    pub replies: HashMap<u64, u32>,
}

pub struct Built {
    pub items: Vec<MsgDto>,
    /// image attachments that need a thumbnail: (key, pointer, square box in logical px to fit into, 0 = as is)
    pub wanted_thumbs: Vec<(String, AttachmentPointer, u32)>,
    pub wanted_cells: Vec<WantedCell>,
    /// long-text attachments not in `LongTexts::full` yet: (key, pointer)
    pub wanted_texts: Vec<(String, AttachmentPointer)>,
    /// every long-text attachment in the window, downloaded or not
    pub long_keys: Vec<String>,
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
        // reactions and the reply count hang below the bubble
        && older.reactions.is_empty()
        && older.replies == 0
        && older.outgoing == newer.outgoing
        // deleted rows from others no longer say who sent them
        && (older.outgoing || !older.sender_key.is_empty())
        && older.sender_key == newer.sender_key
        && !newer.day_sep
        && !newer.unread_divider
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
    let Indexed { status, my_deletes, reactions, edits: outside, replies } = indexed;
    let mut edits: HashMap<u64, (u64, &DataMessage, Uuid)> = HashMap::new();
    let now = now_ms();
    let mut next_expiry: Option<u64> = None;

    for (ts, c) in outside {
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
    let mut long_keys = Vec::new();
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
        let mut m = MsgDto { id: ts.to_string(), ts: *ts, time: ampm(&dt), day: day_num(&dt), ..Default::default() };
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
                    long_keys.push(key.clone());
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
                    set_quote(&mut m, q, *ts, me, name_of, &mut wanted_cells);
                }
                set_attachments(&mut m, dm, *ts, &mut wanted_cells);
                // not next to attachments (Signal's clients don't send that), and it would give
                // away what a spoiler in the text hides
                if !m.sticker && m.attachment.is_empty() && m.cells.is_empty() && !m.styles.iter().any(|s| s.2 == STYLE_SPOILER) {
                    if let Some((card, image)) = link_card(dm, *ts) {
                        m.link = Some(card);
                        wanted_cells.extend(image);
                    }
                }
                if let Some(r) = reactions.get(ts).filter(|r| !r.is_empty()) {
                    m.reactions = reaction_counts(r, me, name_of);
                    m.my_reaction = r.iter().find(|(s, _)| *s == me).map(|(_, e)| e.clone()).unwrap_or_default();
                }
                m.replies = replies.get(ts).copied().unwrap_or(0);
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
                if !m.sticker && m.attachment.is_empty() && m.cells.is_empty() && m.quote.is_empty() {
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
            m.day_sep = true;
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
    Built { items, wanted_thumbs: wanted, wanted_cells, long_keys, wanted_texts, next_expiry }
}

/// The quote at the top of a reply: its text, author, target and picture.
fn set_quote(m: &mut MsgDto, q: &Quote, ts: u64, me: Uuid, name_of: &dyn Fn(&Uuid) -> String, wanted_cells: &mut Vec<WantedCell>) {
    m.quote = q
        .text
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| with_mentions(t, &q.body_ranges, name_of))
        .unwrap_or_else(|| "(첨부파일)".into());
    m.quote_author = quote_author_name(q, me, name_of);
    m.quote_id = q.id.map(|i| i.to_string()).unwrap_or_default();
    // the quoted message's picture, as the sender's app cut it
    if let Some(a) = q.attachments.iter().find_map(|a| a.thumbnail.as_ref()) {
        let file_key = attachment_key(a, ts, QUOTE_IMAGE_INDEX);
        m.quote_image_key = format!("{file_key}@{}x{}", QUOTE_IMAGE.0, QUOTE_IMAGE.1);
        wanted_cells.push(WantedCell { key: m.quote_image_key.clone(), file_key, ptr: a.clone(), size: QUOTE_IMAGE });
    }
}

/// Reactions as (emoji, count, who), in the order each emoji first came; we are "나".
fn reaction_counts(r: &[(Uuid, String)], me: Uuid, name_of: &dyn Fn(&Uuid) -> String) -> Vec<(String, u32, String)> {
    let mut counts: Vec<(String, u32, String)> = Vec::new();
    for (who, e) in r {
        let name = if *who == me { "나".to_string() } else { name_of(who) };
        match counts.iter_mut().find(|(x, ..)| x == e) {
            Some(c) => {
                c.1 += 1;
                add_name(&mut c.2, &name);
            }
            None => counts.push((e.clone(), 1, name)),
        }
    }
    counts
}

/// Optimistic update while our reaction is being sent: set (or with `None` take back) our reaction.
pub fn set_my_reaction(m: &mut MsgDto, emoji: Option<&str>) {
    if !m.my_reaction.is_empty() {
        if let Some(i) = m.reactions.iter().position(|(e, ..)| *e == m.my_reaction) {
            m.reactions[i].1 -= 1;
            if m.reactions[i].1 == 0 {
                m.reactions.remove(i);
            } else {
                let rest: Vec<&str> = m.reactions[i].2.split(", ").filter(|n| *n != "나").collect();
                m.reactions[i].2 = rest.join(", ");
            }
        }
    }
    m.my_reaction = emoji.unwrap_or_default().to_string();
    if let Some(e) = emoji {
        match m.reactions.iter_mut().find(|(x, ..)| x == e) {
            Some(r) => {
                r.1 += 1;
                add_name(&mut r.2, "나");
            }
            None => m.reactions.push((e.to_string(), 1, "나".to_string())),
        }
    }
}

fn add_name(names: &mut String, name: &str) {
    if name.is_empty() {
        return;
    }
    if !names.is_empty() {
        names.push_str(", ");
    }
    names.push_str(name);
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
        day_sep: m.day_sep,
        day: m.day,
        outgoing: true,
        deleted: true,
        text: DELETED_MINE.into(),
        ..Default::default()
    };
}

/// Skeleton row for a message we are still sending; `prev_ts` is the row above (for the day separator).
pub fn outgoing_stub(ts: u64, prev_ts: Option<u64>) -> MsgDto {
    let dt = local(ts);
    let mut m = MsgDto { id: ts.to_string(), ts, time: ampm(&dt), day: day_num(&dt), outgoing: true, pending: true, ..Default::default() };
    if prev_ts.map(|p| local(p).date_naive()) != Some(dt.date_naive()) {
        m.day_sep = true;
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
    use media::GRID_SHOWN;
    use presage::proto::body_range::AssociatedValue;
    use presage::proto::BodyRange;
    use text::{trim_body, LONG_TEXT};

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
        indexed.replies.insert(row, 2);
        let window = [(row, original)];
        let b = build(&window, Uuid::from_u128(1), &[], &indexed, &LongTexts::default(), &|_: &Uuid| String::new());
        assert_eq!(b.items[0].text, "after");
        assert_eq!(b.items[0].reactions, [("up".to_string(), 2, "나".to_string())]);
        assert_eq!(b.items[0].my_reaction, "up");
        assert_eq!(b.items[0].replies, 2);
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
    fn stickers_keep_their_size() {
        let data = AttachmentPointer { content_type: Some("image/webp".into()), digest: Some(vec![7; 32]), ..Default::default() };
        let sticker = presage::proto::data_message::Sticker { data: Some(data), ..Default::default() };
        let window = [(5u64, incoming(DataMessage { sticker: Some(sticker), ..Default::default() }))];
        let b = build(&window, Uuid::from_u128(1), &[], &Default::default(), &LongTexts::default(), &|_: &Uuid| String::new());
        let m = &b.items[0];
        assert!(m.sticker && m.cells.is_empty());
        assert_eq!((m.img_w, m.img_h), (STICKER_SIZE, STICKER_SIZE));
        assert_eq!(b.wanted_thumbs[0].0, m.image_key);
    }

    #[test]
    fn videos_show_as_pictures() {
        let video = |d: u8| AttachmentPointer {
            content_type: Some("video/mp4".into()),
            size: Some(1024),
            width: Some(1920),
            height: Some(1080),
            digest: Some(vec![d; 32]),
            ..Default::default()
        };
        let photo = |d: u8| AttachmentPointer { content_type: Some("image/png".into()), digest: Some(vec![d; 32]), ..Default::default() };
        let name = |_: &Uuid| String::new();
        let long = LongTexts::default();
        let run = |attachments: Vec<AttachmentPointer>| {
            let window = [(5u64, incoming(DataMessage { attachments, ..Default::default() }))];
            build(&window, Uuid::from_u128(1), &[], &Default::default(), &long, &name)
        };

        let b = run(vec![video(1)]);
        let m = &b.items[0];
        assert!(m.cells.len() == 1 && m.cells[0].video && m.attachment.is_empty());
        assert_eq!((m.img_w, m.img_h), (400, 225));
        assert_eq!(b.wanted_cells[0].key, m.cells[0].key);
        assert_eq!(b.wanted_cells[0].file_key, attachment_key(&video(1), 5, 0));
        assert!(b.wanted_thumbs.is_empty());

        // a GIF plays in place, alone or in a grid
        let gif = AttachmentPointer { content_type: Some("image/gif".into()), ..photo(4) };
        assert!(run(vec![gif.clone()]).items[0].cells[0].gif);
        assert!(!run(vec![photo(1)]).items[0].cells[0].gif);
        let b = run(vec![gif, photo(3)]);
        assert_eq!(b.items[0].cells.iter().map(|c| c.gif).collect::<Vec<_>>(), [true, false]);

        // a grid knows which cells are videos, and a click finds the right one
        let b = run(vec![photo(1), video(2), photo(3)]);
        let m = &b.items[0];
        assert_eq!(m.cells.iter().map(|c| c.video).collect::<Vec<_>>(), [false, true, false]);
        assert_eq!(m.clicked_media(1), crate::types::ClickedMedia::Video(0));
        assert_eq!(m.clicked_media(2), crate::types::ClickedMedia::Image(1));
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

        // one photo is a grid of one cell
        let b = run(vec![photo(1)]);
        assert_eq!(b.items[0].cells.len(), 1);
        assert_eq!(b.wanted_cells.len(), 1);
        assert!(b.wanted_thumbs.is_empty());
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
    fn my_reaction_is_replaced_and_taken_back() {
        let mut m = MsgDto { reactions: vec![("👍".into(), 2, "가, 나".into())], my_reaction: "👍".into(), ..Default::default() };
        let (up, heart) = (("👍".to_string(), 1, "가".to_string()), ("❤️".to_string(), 1, "나".to_string()));
        set_my_reaction(&mut m, Some("❤️"));
        assert_eq!(m.reactions, vec![up.clone(), heart.clone()]);
        // idempotent: applying the same pending op twice changes nothing
        set_my_reaction(&mut m, Some("❤️"));
        assert_eq!(m.reactions, vec![up.clone(), heart]);
        set_my_reaction(&mut m, None);
        assert_eq!(m.reactions, vec![up]);
        assert!(m.my_reaction.is_empty());
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
}
