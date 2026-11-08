//! Composing text styles with markers: `**bold**`, `_italic_`, `~~strike~~`, `` `mono` ``,
//! `||spoiler||`. `parse` strips them into plain text + style ranges when sending, `to_markup`
//! turns a message back into markers for editing, `toggle` is what the composer shortcuts do.
//!
//! Rules (tuned for Korean chat): a span stays on one line, its content is non-empty and doesn't
//! start or end with whitespace; particles may follow directly (`**굵게**를`). `_` only counts
//! when the outer neighbors aren't ASCII letters/digits (`file_name_v2` stays as is). Nothing is
//! parsed inside `` ` ``. A backslash before a marker that would open a span keeps it literal.
//! Unmatched markers stay as typed.

use crate::view::text::{STYLE_BOLD, STYLE_ITALIC, STYLE_MONO, STYLE_SPOILER, STYLE_STRIKE};

/// Marker of each style, longest first where they share a prefix.
const MARKERS: [(&str, u8); 5] =
    [("**", STYLE_BOLD), ("~~", STYLE_STRIKE), ("||", STYLE_SPOILER), ("`", STYLE_MONO), ("_", STYLE_ITALIC)];

pub fn marker(style: u8) -> &'static str {
    MARKERS.iter().find(|m| m.1 == style).map_or("", |m| m.0)
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric())
}

/// A span opening at byte `i` of `s`: (style, marker length, content end = closing marker start).
fn span_at(s: &str, i: usize) -> Option<(u8, usize, usize)> {
    let rest = &s[i..];
    let &(m, style) = MARKERS.iter().find(|(m, _)| rest.starts_with(m))?;
    let start = i + m.len();
    if style == STYLE_ITALIC && is_word(s[..i].chars().next_back()) {
        return None;
    }
    let first = s[start..].chars().next()?;
    if first.is_whitespace() || rest[m.len()..].starts_with(m) {
        return None;
    }
    let line_end = s[start..].find('\n').map_or(s.len(), |n| start + n);
    let mut from = start + first.len_utf8();
    while let Some(off) = s[from..line_end].find(m) {
        let end = from + off;
        let ok_inner = !s[..end].chars().next_back().is_some_and(char::is_whitespace);
        let ok_outer = style != STYLE_ITALIC || !is_word(s[end + m.len()..].chars().next());
        if ok_inner && ok_outer {
            return Some((style, m.len(), end));
        }
        from = end + 1;
        while !s.is_char_boundary(from) {
            from += 1;
        }
    }
    None
}

/// Strips the markers: plain text and (start, end, style) byte ranges in it.
pub fn parse(s: &str) -> (String, Vec<(usize, usize, u8)>) {
    let mut out = String::with_capacity(s.len());
    let mut styles = Vec::new();
    parse_into(s, &mut out, &mut styles);
    (out, styles)
}

fn parse_into(s: &str, out: &mut String, styles: &mut Vec<(usize, usize, u8)>) {
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        if c == '\\' && span_at(s, i + 1).is_some() {
            // escaped: the marker itself is text
            let (_, len, _) = span_at(s, i + 1).unwrap();
            out.push_str(&s[i + 1..i + 1 + len]);
            i += 1 + len;
            continue;
        }
        if let Some((style, len, end)) = span_at(s, i) {
            let start = out.len();
            let inner = &s[i + len..end];
            if style == STYLE_MONO {
                out.push_str(inner);
            } else {
                parse_into(inner, out, styles);
            }
            styles.push((start, out.len(), style));
            i = end + len;
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
}

/// Plain text + style ranges back to markers (inverse of `parse` for well-nested styles).
/// Styles are opened outermost first, monospace always innermost (nothing parses inside it).
pub fn to_markup(text: &str, styles: &[(usize, usize, u8)]) -> String {
    let styles: Vec<(usize, usize, u8)> = styles
        .iter()
        .copied()
        .filter(|&(s, e, k)| s < e && e <= text.len() && !marker(k).is_empty())
        .collect();
    if styles.is_empty() {
        return text.to_string();
    }
    let mut cuts: Vec<usize> = styles.iter().flat_map(|&(s, e, _)| [s, e]).chain([0, text.len()]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = String::with_capacity(text.len() + styles.len() * 4);
    // open styles: (style, end)
    let mut stack: Vec<(u8, usize)> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        let active = |k: u8| styles.iter().any(|&(s, e, kk)| kk == k && s <= a && b <= e);
        let wanted: Vec<u8> = MARKERS.iter().map(|m| m.1).filter(|&k| active(k)).collect();
        let new: Vec<u8> = wanted.iter().copied().filter(|k| !stack.iter().any(|s| s.0 == *k)).collect();
        // keep the longest prefix of the stack that stays open (mono can't have anything inside)
        let keep = stack
            .iter()
            .position(|&(k, _)| !wanted.contains(&k) || (k == STYLE_MONO && !new.is_empty()))
            .unwrap_or(stack.len());
        while stack.len() > keep {
            out.push_str(marker(stack.pop().unwrap().0));
        }
        let mut open: Vec<(u8, usize)> = wanted
            .iter()
            .copied()
            .filter(|k| !stack.iter().any(|s| s.0 == *k))
            .map(|k| {
                let end = styles.iter().filter(|&&(s, e, kk)| kk == k && s <= a && b <= e).map(|s| s.1).max().unwrap();
                (k, end)
            })
            .collect();
        open.sort_by_key(|&(k, end)| (k == STYLE_MONO, std::cmp::Reverse(end)));
        for o in open {
            out.push_str(marker(o.0));
            stack.push(o);
        }
        out.push_str(&text[a..b]);
    }
    while let Some((k, _)) = stack.pop() {
        out.push_str(marker(k));
    }
    out
}

/// Composer shortcut: wraps the selection `a..b` (byte offsets) in the style's markers, or
/// removes them when it is already wrapped. Whitespace at the selection's edges stays outside.
/// Returns the new text and the new selection.
pub fn toggle(text: &str, a: usize, b: usize, style: u8) -> (String, usize, usize) {
    let m = marker(style);
    let (mut a, mut b) = (a.min(b).min(text.len()), a.max(b).min(text.len()));
    if !text.is_char_boundary(a) || !text.is_char_boundary(b) {
        return (text.to_string(), a, b);
    }
    while a < b && text[a..b].starts_with(char::is_whitespace) {
        a += text[a..].chars().next().unwrap().len_utf8();
    }
    while a < b && text[a..b].ends_with(char::is_whitespace) {
        b -= text[..b].chars().next_back().unwrap().len_utf8();
    }
    // already wrapped (markers just outside, or selected along with the text): unwrap
    if text[..a].ends_with(m) && text[b..].starts_with(m) {
        let out = format!("{}{}{}", &text[..a - m.len()], &text[a..b], &text[b + m.len()..]);
        return (out, a - m.len(), b - m.len());
    }
    if b - a >= 2 * m.len() && text[a..b].starts_with(m) && text[a..b].ends_with(m) {
        let out = format!("{}{}{}", &text[..a], &text[a + m.len()..b - m.len()], &text[b..]);
        return (out, a, b - 2 * m.len());
    }
    let out = format!("{}{m}{}{m}{}", &text[..a], &text[a..b], &text[b..]);
    (out, a + m.len(), b + m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(s: &str) -> (String, Vec<(String, u8)>) {
        let (t, st) = parse(s);
        let spans = st.iter().map(|&(a, b, k)| (t[a..b].to_string(), k)).collect();
        (t, spans)
    }

    #[test]
    fn parses_markers() {
        let (t, st) = show("**굵게**를 _기울임_ ~~취소~~ `a*b*` ||비밀|| 끝");
        assert_eq!(t, "굵게를 기울임 취소 a*b* 비밀 끝");
        assert_eq!(
            st,
            [
                ("굵게".into(), STYLE_BOLD),
                ("기울임".into(), STYLE_ITALIC),
                ("취소".into(), STYLE_STRIKE),
                ("a*b*".into(), STYLE_MONO),
                ("비밀".into(), STYLE_SPOILER)
            ]
        );
        // nested
        let (t, st) = show("**굵은 _기울임_**");
        assert_eq!(t, "굵은 기울임");
        assert_eq!(st, [("기울임".into(), STYLE_ITALIC), ("굵은 기울임".into(), STYLE_BOLD)]);
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for s in [
            "10~20 좋아요~",
            "file_name_v2.txt",
            "a ** b ** c",
            "** 굵게**",
            "**굵게 **",
            "열린 **기호",
            "****",
            "줄을 **넘어\n가면** 안 됨",
            "C:\\_dir\\x",
            "|| ||",
        ] {
            assert_eq!(parse(s), (s.to_string(), vec![]), "{s}");
        }
        // escaped
        assert_eq!(parse("\\**그대로**"), ("**그대로**".to_string(), vec![]));
        // italic: Hangul neighbors are fine, ASCII ones aren't
        assert_eq!(show("한_글_자").1, [("글".into(), STYLE_ITALIC)]);
    }

    #[test]
    fn markup_round_trips() {
        for s in ["**굵게**를 _기울임_ 보통 `mono` ||스포 **일러**||", "**a _b `c`_ d**", "평문"] {
            let (t, st) = parse(s);
            assert_eq!(to_markup(&t, &st), s);
        }
        // overlapping (from another client): closed and reopened, still parses back the same
        let text = "가나다라마바";
        let st = [(0, 12, STYLE_BOLD), (6, 18, STYLE_ITALIC)];
        let m = to_markup(text, &st);
        assert_eq!(m, "**가나_다라_**_마바_");
        let (t2, mut st2) = parse(&m);
        assert_eq!(t2, text);
        st2.sort();
        assert_eq!(st2, [(0, 12, STYLE_BOLD), (6, 12, STYLE_ITALIC), (12, 18, STYLE_ITALIC)]);
        // bold inside mono: mono stays innermost
        assert_eq!(to_markup("abc", &[(0, 3, STYLE_MONO), (1, 2, STYLE_BOLD)]), "`a`**`b`**`c`");
    }

    #[test]
    fn toggles_markers() {
        // "안녕 하세요" with "하세요 " selected: the trailing space stays outside
        let t = "안녕 하세요 ";
        let (s, a, b) = toggle(t, 7, t.len(), STYLE_BOLD);
        assert_eq!(s, "안녕 **하세요** ");
        assert_eq!(&s[a..b], "하세요");
        // again: unwrap
        let (s2, a2, b2) = toggle(&s, a, b, STYLE_BOLD);
        assert_eq!(s2, t);
        assert_eq!(&s2[a2..b2], "하세요");
        // selected along with the markers
        assert_eq!(toggle("x **y** z", 2, 7, STYLE_BOLD).0, "x y z");
        // no selection: an empty pair with the cursor inside
        assert_eq!(toggle("ab", 1, 1, STYLE_SPOILER), ("a||||b".to_string(), 3, 3));
    }
}
