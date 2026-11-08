//! Message text: long messages, mentions and styles, previews.

use std::collections::HashMap;

use presage::libsignal_service::content::{Content, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::body_range::AssociatedValue;
use presage::proto::data_message::Quote;
use presage::proto::{AttachmentPointer, BodyRange};

use super::media::{attachment_key, attachment_label, is_image, shown_attachments};
use super::time::now_ms;
use super::{classify, expires_at, Kind};
use crate::types::MsgDto;

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
pub(super) fn push_with_mentions(
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
pub(super) fn body_text_spans(
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

/// The preview of a message, or of its edited text.
pub fn message_snippet(dm: &DataMessage, outgoing: bool, name_of: &dyn Fn(&Uuid) -> String) -> String {
    let t = one_line(dm, name_of);
    if outgoing { format!("나: {t}") } else { t }
}

/// One-line preview used in the thread list and notifications.
pub fn snippet(c: &Content, me: Uuid, name_of: &dyn Fn(&Uuid) -> String) -> Option<String> {
    if expires_at(c).is_some_and(|t| t <= now_ms()) {
        return None;
    }
    let text = match classify(c, me) {
        Kind::Msg { dm, outgoing } => message_snippet(dm, outgoing, name_of),
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
                if is_image(a) { format!("📷 {l}") } else { format!("📄 {l}") }
            })
            .unwrap_or_default();
    }
    t.replace('\n', " ")
}

/// Timeline line for a pin message.
pub(super) fn pin_label(c: &Content, me: Uuid, name_of: &dyn Fn(&Uuid) -> String) -> String {
    let who = c.metadata.sender.raw_uuid();
    if who == me {
        "메시지를 고정했습니다".into()
    } else {
        format!("{}님이 메시지를 고정했습니다", name_of(&who))
    }
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
pub(super) fn cut_for_read_more(m: &mut MsgDto, limit: usize) -> bool {
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
}
