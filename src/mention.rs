//! Composing @mentions: finding the "@query" being typed, filtering group members, and
//! turning picked "@name"s into Signal's wire form (U+FFFC + a BodyRange per mention).

use presage::proto::body_range::AssociatedValue;
use presage::proto::BodyRange;

/// A member that can be mentioned: (ACI uuid, display name).
pub type Member = (String, String);

/// Max rows in the mention popup.
pub const MAX_ROWS: usize = 6;

/// The "@query" ending at `cursor` (byte offset): the byte offset of the '@' and the query.
/// The '@' must start the text or follow whitespace, and the query has no whitespace.
pub fn query_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    if !before[..at].chars().next_back().is_none_or(char::is_whitespace) {
        return None;
    }
    Some((at, query))
}

/// Members whose name contains `query` (case-insensitive), names starting with it first.
pub fn filter<'a>(members: &'a [Member], query: &str) -> Vec<&'a Member> {
    let q = query.to_lowercase();
    let mut hits: Vec<(bool, &Member)> = members
        .iter()
        .filter_map(|m| {
            let n = m.1.to_lowercase();
            n.contains(&q).then(|| (!n.starts_with(&q), m))
        })
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1 .1.cmp(&b.1 .1)));
    hits.into_iter().map(|(_, m)| m).take(MAX_ROWS).collect()
}

/// Strips the style markers (`format::parse`) into style body ranges, and replaces each "@name"
/// of a picked member with the mention placeholder and a body range. Text the user changed after
/// picking no longer matches and is sent as typed.
pub fn encode(text: &str, picked: &[Member]) -> (String, Vec<BodyRange>) {
    let (plain, styles) = crate::format::parse(text);
    let text = plain.as_str();
    let mut picked: Vec<&Member> = picked.iter().filter(|m| !m.1.is_empty()).collect();
    // longest name first, so "@Kim Lee" wins over "@Kim"
    picked.sort_by_key(|m| std::cmp::Reverse(m.1.len()));
    let mut out = String::with_capacity(text.len());
    let mut ranges = Vec::new();
    let mut utf16 = 0u32;
    let mut rest = text;
    // (byte offset in `text`, UTF-16 offset in `out`) where each char or mention starts
    let mut map: Vec<(usize, u32)> = Vec::new();
    while let Some(c) = rest.chars().next() {
        map.push((text.len() - rest.len(), utf16));
        if c == '@' {
            if let Some(m) = picked.iter().find(|m| rest[1..].starts_with(m.1.as_str())) {
                ranges.push(BodyRange {
                    start: Some(utf16),
                    length: Some(1),
                    associated_value: Some(AssociatedValue::MentionAci(m.0.clone())),
                });
                out.push('\u{FFFC}');
                utf16 += 1;
                rest = &rest[1 + m.1.len()..];
                continue;
            }
        }
        out.push(c);
        utf16 += c.len_utf16() as u32;
        rest = &rest[c.len_utf8()..];
    }
    map.push((text.len(), utf16));
    // a byte inside a mention maps to the mention's end
    let at = |b: usize| match map.binary_search_by_key(&b, |m| m.0) {
        Ok(i) => map[i].1,
        Err(i) => map.get(i).map_or(utf16, |m| m.1),
    };
    for (s, e, style) in styles {
        let (s, e) = (at(s), at(e));
        if e > s {
            ranges.push(BodyRange {
                start: Some(s),
                length: Some(e - s),
                associated_value: Some(AssociatedValue::Style(style as i32)),
            });
        }
    }
    (out, ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_query_being_typed() {
        assert_eq!(query_at("@", 1), Some((0, "")));
        assert_eq!(query_at("안녕 @철", 11), Some((7, "철")));
        assert_eq!(query_at("a@b", 3), None); // e-mail like
        assert_eq!(query_at("@철수 hi", 10), None); // past a space
        assert_eq!(query_at("@철수 hi", 7), Some((0, "철수")));
    }

    #[test]
    fn filters_prefix_first() {
        let m: Vec<Member> = vec![("1".into(), "김철수".into()), ("2".into(), "철수".into()), ("3".into(), "영희".into())];
        let names: Vec<&str> = filter(&m, "철").iter().map(|m| m.1.as_str()).collect();
        assert_eq!(names, ["철수", "김철수"]);
        assert_eq!(filter(&m, "").len(), 3);
    }

    #[test]
    fn encodes_picked_names_only() {
        let picked: Vec<Member> = vec![("u1".into(), "Kim".into()), ("u2".into(), "Kim Lee".into())];
        let (body, ranges) = encode("😀 @Kim Lee hi @Kim and @Park", &picked);
        assert_eq!(body, "😀 \u{FFFC} hi \u{FFFC} and @Park");
        let got: Vec<(u32, String)> = ranges
            .iter()
            .map(|r| match &r.associated_value {
                Some(AssociatedValue::MentionAci(u)) => (r.start.unwrap(), u.clone()),
                _ => panic!(),
            })
            .collect();
        // "😀" is two UTF-16 units
        assert_eq!(got, [(3, "u2".to_string()), (8, "u1".to_string())]);
    }

    #[test]
    fn encodes_styles_around_mentions() {
        let picked: Vec<Member> = vec![("u1".into(), "철수".into())];
        let (body, ranges) = encode("😀 **@철수 안녕** ||비밀||", &picked);
        assert_eq!(body, "😀 \u{FFFC} 안녕 비밀");
        let got: Vec<(u32, u32, i32)> = ranges
            .iter()
            .filter_map(|r| match r.associated_value {
                Some(AssociatedValue::Style(s)) => Some((r.start.unwrap(), r.length.unwrap(), s)),
                _ => None,
            })
            .collect();
        assert_eq!(got, [(3, 4, 1), (8, 2, 3)]);
    }
}
