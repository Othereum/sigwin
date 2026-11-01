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

/// Replaces each "@name" of a picked member with the mention placeholder and a body range.
/// Text the user changed after picking no longer matches and is sent as typed.
pub fn encode(text: &str, picked: &[Member]) -> (String, Vec<BodyRange>) {
    let mut picked: Vec<&Member> = picked.iter().filter(|m| !m.1.is_empty()).collect();
    // longest name first, so "@Kim Lee" wins over "@Kim"
    picked.sort_by_key(|m| std::cmp::Reverse(m.1.len()));
    let mut out = String::with_capacity(text.len());
    let mut ranges = Vec::new();
    let mut utf16 = 0u32;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
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
}
