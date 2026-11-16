//! Composing text styles with markers, as in Discord: `**bold**`, `_italic_` or `*italic*`,
//! `~~strike~~`, `` `mono` ``, `||spoiler||`. `parse` strips them into plain text + style ranges
//! when sending, `to_markup` turns a message back into markers for editing, `toggle` is what the
//! composer shortcuts do.
//!
//! Rules (tuned for Korean chat): a span stays on one line, its content is non-empty and doesn't
//! start or end with whitespace; particles may follow directly (`**굵게**를`). `_` only counts
//! when the outer neighbors aren't ASCII letters/digits (`file_name_v2` stays as is); `*` works
//! inside a word too, and is what italics are written with. Nothing is parsed inside
//! `` ` ``. A backslash before a marker that would open a span keeps it literal. Unmatched
//! markers stay as typed.

use crate::view::text::{STYLE_BOLD, STYLE_ITALIC, STYLE_MONO, STYLE_SPOILER, STYLE_STRIKE};

/// Marker of each style, longest first where they share a prefix; a style's first one is what
/// it is written with.
const MARKERS: [(&str, u8); 6] = [
    ("**", STYLE_BOLD),
    ("~~", STYLE_STRIKE),
    ("||", STYLE_SPOILER),
    ("`", STYLE_MONO),
    ("*", STYLE_ITALIC),
    ("_", STYLE_ITALIC),
];

pub fn marker(style: u8) -> &'static str {
    MARKERS.iter().find(|m| m.1 == style).map_or("", |m| m.0)
}

/// The marker that writes `style` between `before` and `after`: italics next to a star take `_`,
/// since `**a***b*` can't tell where the bold ends.
fn marker_between(style: u8, before: Option<char>, after: Option<char>) -> &'static str {
    if style == STYLE_ITALIC && (before == Some('*') || after == Some('*')) && !is_word(before) && !is_word(after) {
        "_"
    } else {
        marker(style)
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Per marker (index in `MARKERS`), a stretch of one line of the string being parsed known to
/// hold no closing marker: (from, line end). Whether a marker can close doesn't depend on where
/// its span opened, so a later opening on the line needn't search there again. Without it, a long
/// line of openings that never close (`a **b a **b …`) took time squared in its length.
type Unclosed = [(usize, usize); MARKERS.len()];

const NO_UNCLOSED: Unclosed = [(usize::MAX, 0); MARKERS.len()];

/// A span opening at byte `i` of `s`: (style, marker length, content end = closing marker start).
fn span_at(s: &str, i: usize, unclosed: &mut Unclosed) -> Option<(u8, usize, usize)> {
    let rest = &s[i..];
    let (k, &(m, style)) = MARKERS.iter().enumerate().find(|(_, (m, _))| rest.starts_with(m))?;
    let start = i + m.len();
    if m == "_" && is_word(s[..i].chars().next_back()) {
        return None;
    }
    let first = s[start..].chars().next()?;
    if first.is_whitespace() || rest[m.len()..].starts_with(m) {
        return None;
    }
    let line_end = s[start..].find('\n').map_or(s.len(), |n| start + n);
    let mut from = start + first.len_utf8();
    if unclosed[k].1 == line_end && from >= unclosed[k].0 {
        return None;
    }
    let searched = from;
    while let Some(off) = s[from..line_end].find(m) {
        let mut end = from + off;
        let mut next = end + 1;
        if m.starts_with('*') {
            // a run of stars: `*` passes over a `**` pair inside it (`*a **b** c*`). A longer run
            // closes with its first stars when that leaves no stray star inside, else its last
            // (which always can): `**a***b*` is bold `a` then italic `b`, `***x***` bold around `*x*`
            let run = s[end..line_end].bytes().take_while(|&b| b == b'*').count();
            next = end + run;
            if run < m.len() || (m.len() == 1 && run == 2) {
                from = next;
                continue;
            }
            if run > m.len()
                && (s[..end].ends_with(char::is_whitespace) || parse(&s[start..end]).0.contains('*'))
            {
                end += run - m.len();
            }
        }
        let ok_inner = !s[..end].chars().next_back().is_some_and(char::is_whitespace);
        let ok_outer = m != "_" || !is_word(s[end + m.len()..].chars().next());
        if ok_inner && ok_outer {
            return Some((style, m.len(), end));
        }
        from = next;
        while !s.is_char_boundary(from) {
            from += 1;
        }
    }
    unclosed[k] = (searched, line_end);
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
    let mut unclosed = NO_UNCLOSED;
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        if c == '\\' {
            if let Some((_, len, _)) = span_at(s, i + 1, &mut unclosed) {
                // escaped: the marker itself is text
                out.push_str(&s[i + 1..i + 1 + len]);
                i += 1 + len;
                continue;
            }
        }
        if let Some((style, len, end)) = span_at(s, i, &mut unclosed) {
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

/// What `parse` would make of `s`, in `s`'s own byte offsets, for showing a draft with its styles:
/// (start, end, style) of each span's content, and the ranges of markup that won't be sent
/// (markers, escaping backslashes).
pub fn preview(s: &str) -> (Vec<(usize, usize, u8)>, Vec<(usize, usize)>) {
    let (mut styles, mut markup) = (Vec::new(), Vec::new());
    preview_into(s, 0, &mut styles, &mut markup);
    (styles, markup)
}

// mirrors `parse_into`, which judges a span's inner text on its own
fn preview_into(s: &str, base: usize, styles: &mut Vec<(usize, usize, u8)>, markup: &mut Vec<(usize, usize)>) {
    let mut unclosed = NO_UNCLOSED;
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with('\\') {
            if let Some((_, len, _)) = span_at(s, i + 1, &mut unclosed) {
                markup.push((base + i, base + i + 1));
                i += 1 + len;
                continue;
            }
        }
        if let Some((style, len, end)) = span_at(s, i, &mut unclosed) {
            markup.push((base + i, base + i + len));
            markup.push((base + end, base + end + len));
            if style != STYLE_MONO {
                preview_into(&s[i + len..end], base + i + len, styles, markup);
            }
            styles.push((base + i + len, base + end, style));
            i = end + len;
            continue;
        }
        i += s[i..].chars().next().unwrap().len_utf8();
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
    // open styles: (style, end, marker)
    let mut stack: Vec<(u8, usize, &str)> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        let active = |k: u8| styles.iter().any(|&(s, e, kk)| kk == k && s <= a && b <= e);
        let mut wanted: Vec<u8> = MARKERS.iter().map(|m| m.1).filter(|&k| active(k)).collect();
        // italics have two markers
        wanted.dedup();
        let new: Vec<u8> = wanted.iter().copied().filter(|k| !stack.iter().any(|s| s.0 == *k)).collect();
        // keep the longest prefix of the stack that stays open (mono can't have anything inside)
        let keep = stack
            .iter()
            .position(|&(k, _, _)| !wanted.contains(&k) || (k == STYLE_MONO && !new.is_empty()))
            .unwrap_or(stack.len());
        while stack.len() > keep {
            out.push_str(stack.pop().unwrap().2);
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
        for (k, end) in open {
            let m = marker_between(k, out.chars().next_back(), text[end..].chars().next());
            out.push_str(m);
            stack.push((k, end, m));
        }
        out.push_str(&text[a..b]);
    }
    while let Some((_, _, m)) = stack.pop() {
        out.push_str(m);
    }
    out
}

/// The selection `a..b` (byte offsets, either order) without whitespace or `markup` (markers) at
/// its edges, so selecting `**_x_**` whole is selecting `x`; `None` when nothing else is selected.
fn trimmed(text: &str, a: usize, b: usize, markup: &[(usize, usize)]) -> Option<(usize, usize)> {
    let (mut a, mut b) = (a.min(b).min(text.len()), a.max(b).min(text.len()));
    if !text.is_char_boundary(a) || !text.is_char_boundary(b) {
        return None;
    }
    loop {
        if a < b && text[a..b].starts_with(char::is_whitespace) {
            a += text[a..].chars().next().unwrap().len_utf8();
        } else if a < b && text[a..b].ends_with(char::is_whitespace) {
            b -= text[..b].chars().next_back().unwrap().len_utf8();
        } else if let Some(&(_, me)) = markup.iter().find(|&&(ms, me)| ms == a && me <= b) {
            a = me;
        } else if let Some(&(ms, _)) = markup.iter().find(|&&(ms, me)| me == b && a <= ms) {
            b = ms;
        } else {
            break;
        }
    }
    (a < b).then_some((a, b))
}

/// The content range of the `style` span the selection `a..b` lies in (or is, with its markers).
fn span_around(spans: &[(usize, usize, u8)], a: usize, b: usize, style: u8) -> Option<(usize, usize)> {
    let m = marker(style).len();
    spans
        .iter()
        .find(|&&(s, e, k)| k == style && ((s <= a && b <= e) || (s - m, e + m) == (a, b)))
        .map(|&(s, e, _)| (s, e))
}

/// `text[r]`, found between `before` and `after`, in the markers of `style`, whitespace at its
/// edges kept outside. A part with nothing but whitespace and `markup` (other spans' markers, as
/// in `**_` when `_x_` is taken out of the bold) has nothing to wrap and stays as is.
fn wrap(
    text: &str,
    r: std::ops::Range<usize>,
    markup: &[(usize, usize)],
    style: u8,
    before: Option<char>,
    after: Option<char>,
) -> String {
    let seg = &text[r.clone()];
    let shown = seg
        .char_indices()
        .map(|(o, c)| (r.start + o, c))
        .any(|(at, c)| !c.is_whitespace() && !markup.iter().any(|&(ms, me)| ms <= at && at < me));
    if !shown {
        return seg.to_string();
    }
    let core = seg.trim();
    let lead = &seg[..seg.len() - seg.trim_start().len()];
    let trail = &seg[seg.trim_end().len()..];
    let m = marker_between(style, lead.chars().next_back().or(before), trail.chars().next().or(after));
    format!("{lead}{m}{core}{m}{trail}")
}

/// The styles the composer's selection `a..b` already has, as `1 << style` bits: what a style
/// shortcut would remove rather than add.
pub fn selected_styles(text: &str, a: usize, b: usize) -> u32 {
    let (spans, markup) = preview(text);
    let Some((a, b)) = trimmed(text, a, b, &strippable(&spans, &markup)) else { return 0 };
    MARKERS.iter().filter(|m| span_around(&spans, a, b, m.1).is_some()).fold(0, |bits, m| bits | 1 << m.1)
}

/// Markup without the backticks of monospace spans: a selection keeps them, since what styles
/// monospace text has to go outside them.
fn strippable(spans: &[(usize, usize, u8)], markup: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let ticks: Vec<(usize, usize)> =
        spans.iter().filter(|s| s.2 == STYLE_MONO).flat_map(|&(s, e, _)| [(s - 1, s), (e, e + 1)]).collect();
    markup.iter().copied().filter(|m| !ticks.contains(m)).collect()
}

/// Nothing is parsed inside `` ` ``, so a style for monospace text goes around its backticks: a
/// monospace span the selection `a..b` reaches into is split where the selection starts or ends,
/// and the selection grows to the backticks of its part. `None` when there is no such span.
fn split_mono(text: &str, a: usize, b: usize, spans: &[(usize, usize, u8)]) -> Option<(String, usize, usize)> {
    let &(s, e, _) = spans
        .iter()
        .find(|&&(s, e, k)| k == STYLE_MONO && a < e && s < b && !(a < s && e < b))?;
    let (lo, hi) = (a.max(s), b.min(e));
    let mut out = text[..s - 1].to_string();
    out += &wrap(text, s..lo, &[], STYLE_MONO, out.chars().next_back(), Some('`'));
    let na = if a < s { a } else { out.len() };
    out += "`";
    out += &text[lo..hi];
    out += "`";
    let mid_end = out.len();
    out += &wrap(text, hi..e, &[], STYLE_MONO, Some('`'), text[e + 1..].chars().next());
    let nb = if b > e { b + out.len() - (e + 1) } else { mid_end };
    out += &text[e + 1..];
    Some((out, na, nb))
}

/// Composer shortcut: wraps the selection `a..b` (byte offsets) in the style's markers, or, when
/// it lies in a span of that style, takes it out of the span (what's left of the span on either
/// side keeps the style). Whitespace at the selection's edges stays outside. Returns the new text
/// and the new selection; `None` when nothing (but whitespace) is selected.
pub fn toggle(text: &str, a: usize, b: usize, style: u8) -> Option<(String, usize, usize)> {
    let (out, a, b) = toggle_split(text, a, b, style)?;
    Some(join_mono(out, a, b))
}

/// Monospace pieces right next to each other (`` `a``b` ``, left by `split_mono` once the style
/// that set them apart is gone) become one again.
fn join_mono(text: String, a: usize, b: usize) -> (String, usize, usize) {
    let mut mono: Vec<(usize, usize)> =
        preview(&text).0.into_iter().filter(|s| s.2 == STYLE_MONO).map(|(s, e, _)| (s, e)).collect();
    mono.sort_unstable();
    // where each "``" between two pieces starts
    let joins: Vec<usize> = mono.windows(2).filter(|w| w[0].1 + 2 == w[1].0).map(|w| w[0].1).collect();
    if joins.is_empty() {
        return (text, a, b);
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for &j in &joins {
        out += &text[last..j];
        last = j + 2;
    }
    out += &text[last..];
    let at = |p: usize| p - joins.iter().map(|&j| p.saturating_sub(j).min(2)).sum::<usize>();
    (out, at(a), at(b))
}

fn toggle_split(text: &str, a: usize, b: usize, style: u8) -> Option<(String, usize, usize)> {
    let (spans, markup) = preview(text);
    let (a, b) = trimmed(text, a, b, &strippable(&spans, &markup))?;
    if style != STYLE_MONO {
        if let Some((t, a, b)) = split_mono(text, a, b, &spans) {
            return toggle_split(&t, a, b, style);
        }
    }
    if let Some((s, e)) = span_around(&spans, a, b, style) {
        let ml = marker(style).len();
        let (a, b) = (a.clamp(s, e), b.clamp(s, e));
        let mut out = text[..s - ml].to_string();
        let left = wrap(text, s..a, &markup, style, out.chars().next_back(), text[a..].chars().next());
        out += &left;
        let na = out.len();
        out += &text[a..b];
        let nb = out.len();
        out += &wrap(text, b..e, &markup, style, text[..b].chars().next_back(), text[e + ml..].chars().next());
        out += &text[e + ml..];
        return Some((out, na, nb));
    }
    let m = marker_between(style, text[..a].chars().next_back(), text[b..].chars().next());
    let out = format!("{}{m}{}{m}{}", &text[..a], &text[a..b], &text[b..]);
    Some((out, a + m.len(), b + m.len()))
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
    fn long_lines_of_unclosed_markers_stay_fast() {
        // each took seconds when every opening searched the rest of the line again
        for unit in ["a **b ", "_a _b ", "*a **b ", "a ~~b ", "a ||b "] {
            let s = unit.repeat(65536 / unit.len());
            let t = std::time::Instant::now();
            let _ = parse(&s);
            let _ = preview(&s);
            assert!(t.elapsed() < std::time::Duration::from_secs(1), "{unit}: {:?}", t.elapsed());
        }
    }

    #[test]
    fn parses_stars() {
        // inside a word, where `_` doesn't count
        assert_eq!(show("asdf*qwer*zxcv"), ("asdfqwerzxcv".into(), vec![("qwer".into(), STYLE_ITALIC)]));
        assert_eq!(show("asdf_qwer_zxcv").1, []);
        let both = vec![("x".into(), STYLE_ITALIC), ("x".into(), STYLE_BOLD)];
        assert_eq!(show("***x***"), ("x".into(), both));
        assert_eq!(show("*a **b** c*"), ("a b c".into(), vec![("b".into(), STYLE_BOLD), ("a b c".into(), STYLE_ITALIC)]));
        assert_eq!(show("**a *b* c**"), ("a b c".into(), vec![("b".into(), STYLE_ITALIC), ("a b c".into(), STYLE_BOLD)]));
        // runs of three between two spans
        assert_eq!(show("**a***b*c"), ("abc".into(), vec![("a".into(), STYLE_BOLD), ("b".into(), STYLE_ITALIC)]));
        assert_eq!(show("*a***b**c"), ("abc".into(), vec![("a".into(), STYLE_ITALIC), ("b".into(), STYLE_BOLD)]));
        assert_eq!(show("*a **b***").1, [("b".into(), STYLE_BOLD), ("a b".into(), STYLE_ITALIC)]);
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
        assert_eq!(parse("\\*그대로*"), ("*그대로*".to_string(), vec![]));
        for s in ["a * b * c", "*a", "* a*"] {
            assert_eq!(parse(s), (s.to_string(), vec![]), "{s}");
        }
        // italic: Hangul neighbors are fine, ASCII ones aren't
        assert_eq!(show("한_글_자").1, [("글".into(), STYLE_ITALIC)]);
    }

    #[test]
    fn markup_round_trips() {
        for s in ["**굵게**를 *기울임* 보통 `mono` ||스포 **일러**||", "**a *b `c`* d**", "평문"] {
            let (t, st) = parse(s);
            assert_eq!(to_markup(&t, &st), s);
        }
        // overlapping (from another client): closed and reopened, still parses back the same
        let text = "가나다라마바";
        let st = [(0, 12, STYLE_BOLD), (6, 18, STYLE_ITALIC)];
        let m = to_markup(text, &st);
        assert_eq!(m, "**가나*다라***_마바_");
        let (t2, mut st2) = parse(&m);
        assert_eq!(t2, text);
        st2.sort();
        assert_eq!(st2, [(0, 12, STYLE_BOLD), (6, 12, STYLE_ITALIC), (12, 18, STYLE_ITALIC)]);
        // bold inside mono: mono stays innermost
        assert_eq!(to_markup("abc", &[(0, 3, STYLE_MONO), (1, 2, STYLE_BOLD)]), "`a`**`b`**`c`");
        // italics take `*`, inside a word too; next to bold markers `_`
        let st = [(4, 8, STYLE_ITALIC), (13, 17, STYLE_ITALIC)];
        let m = to_markup("asdfqwerzxcv rest", &st);
        assert_eq!(m, "asdf*qwer*zxcv *rest*");
        assert_eq!(parse(&m), ("asdfqwerzxcv rest".to_string(), st.to_vec()));
        let st = [(0, 1, STYLE_BOLD), (1, 2, STYLE_ITALIC)];
        let m = to_markup("ab", &st);
        assert_eq!(m, "**a**_b_");
        assert_eq!(parse(&m), ("ab".to_string(), st.to_vec()));
    }

    #[test]
    fn toggles_markers() {
        // "안녕 하세요" with "하세요 " selected: the trailing space stays outside
        let t = "안녕 하세요 ";
        let (s, a, b) = toggle(t, 7, t.len(), STYLE_BOLD).unwrap();
        assert_eq!(s, "안녕 **하세요** ");
        assert_eq!(&s[a..b], "하세요");
        // again: unwrap
        let (s2, a2, b2) = toggle(&s, a, b, STYLE_BOLD).unwrap();
        assert_eq!(s2, t);
        assert_eq!(&s2[a2..b2], "하세요");
        // selected along with the markers
        assert_eq!(toggle("x **y** z", 2, 7, STYLE_BOLD).unwrap().0, "x y z");
        // nothing selected, or only spaces: no empty pair of markers
        assert_eq!(toggle("ab", 1, 1, STYLE_SPOILER), None);
        assert_eq!(toggle("a  b", 1, 3, STYLE_BOLD), None);
        // part of a bold span: only that part loses it
        let (s, a, b) = toggle("x **ab cd ef**", 7, 9, STYLE_BOLD).unwrap();
        assert_eq!(s, "x **ab** cd **ef**");
        assert_eq!(&s[a..b], "cd");
        assert_eq!(toggle("**ab cd**", 2, 4, STYLE_BOLD).unwrap().0, "ab **cd**");
        // italics inside a word, and taking part of one out
        let (s, a, b) = toggle("asdfqwerzxcv", 4, 8, STYLE_ITALIC).unwrap();
        assert_eq!(s, "asdf*qwer*zxcv");
        assert_eq!(&s[a..b], "qwer");
        assert_eq!(toggle(&s, a, b, STYLE_ITALIC).unwrap().0, "asdfqwerzxcv");
        assert_eq!(toggle("_abcd_", 2, 4, STYLE_ITALIC).unwrap().0, "*a*bc*d*");
        assert_eq!(toggle("한글 abc", 0, 6, STYLE_ITALIC).unwrap().0, "*한글* abc");
        assert_eq!(toggle("**a**b", 5, 6, STYLE_ITALIC).unwrap().0, "**a**_b_");
        // bold and italic toggled in turns on the same text come back to it, whichever goes first
        for order in [[STYLE_BOLD, STYLE_ITALIC], [STYLE_ITALIC, STYLE_BOLD]] {
            let (mut s, mut a, mut b) = ("abc".to_string(), 0, 3);
            for _ in 0..3 {
                for style in order.into_iter().chain(order) {
                    (s, a, b) = toggle(&s, a, b, style).unwrap();
                    assert_eq!(&s[a..b], "abc");
                }
                assert_eq!(s, "abc", "{order:?}");
            }
        }
        // monospace first: other styles go around its backticks, and come back off
        for order in [[STYLE_MONO, STYLE_BOLD, STYLE_ITALIC], [STYLE_BOLD, STYLE_MONO, STYLE_SPOILER]] {
            let (mut s, mut a, mut b) = ("abc".to_string(), 0, 3);
            for _ in 0..3 {
                for style in order.into_iter().chain(order) {
                    (s, a, b) = toggle(&s, a, b, style).unwrap();
                    assert!(s.len() <= 3 + 2 * (1 + 2 + 2), "{order:?}: {s}");
                }
                assert_eq!(s, "abc", "{order:?}");
            }
        }
        let (s, ..) = toggle("`abc`", 1, 4, STYLE_BOLD).unwrap();
        assert_eq!(s, "**`abc`**");
        assert_eq!(selected_styles(&s, 3, 6), 1 << STYLE_BOLD | 1 << STYLE_MONO);
        let (s, a, b) = toggle("x `abcd` y", 4, 6, STYLE_BOLD).unwrap();
        assert_eq!(s, "x `a`**`bc`**`d` y");
        assert_eq!(parse(&s).1, [(2, 3, STYLE_MONO), (3, 5, STYLE_MONO), (3, 5, STYLE_BOLD), (5, 6, STYLE_MONO)]);
        // and back: bold off, the monospace pieces one again
        let (s2, a2, b2) = toggle(&s, a, b, STYLE_BOLD).unwrap();
        assert_eq!(s2, "x `abcd` y");
        assert_eq!(&s2[a2..b2], "bc");
        // selected along with the markers: the styles inside are what's toggled
        assert_eq!(toggle("**_abc_**", 0, 9, STYLE_ITALIC).unwrap().0, "**abc**");
        assert_eq!(toggle("**_abc_**", 0, 9, STYLE_BOLD).unwrap().0, "_abc_");
        assert_eq!(toggle("**abc**", 0, 7, STYLE_ITALIC).unwrap().0, "**_abc_**");
        assert_eq!(selected_styles("**_abc_**", 0, 9), 1 << STYLE_ITALIC | 1 << STYLE_BOLD);
        let (s, ..) = toggle("**a**bc", 5, 6, STYLE_ITALIC).unwrap();
        assert_eq!(s, "**a***b*c");
        assert_eq!(parse(&s).1, [(0, 1, STYLE_BOLD), (1, 2, STYLE_ITALIC)]);
    }

    #[test]
    fn knows_selected_styles() {
        let t = "**_굵은 기울임_** 보통";
        let inner = t.find('굵').unwrap();
        let end = t.find("_**").unwrap();
        let both = 1 << STYLE_ITALIC | 1 << STYLE_BOLD;
        assert_eq!(selected_styles(t, inner, end), both);
        // with the italic markers: the bold span's content
        assert_eq!(selected_styles(t, inner - 1, end + 1), both);
        assert_eq!(selected_styles(t, inner, inner + 3), both);
        assert_eq!(selected_styles(t, end + 4, t.len()), 0);
        assert_eq!(selected_styles(t, 0, t.len()), 0);
        assert_eq!(selected_styles(t, 3, 3), 0);
    }

    #[test]
    fn previews_in_place() {
        let s = "**굵은 _기울임_**를 \\**그대로** `a_b_`";
        let (styles, markup) = preview(s);
        let at = |(a, b): (usize, usize)| &s[a..b];
        let spans: Vec<(&str, u8)> = styles.iter().map(|&(a, b, k)| (at((a, b)), k)).collect();
        assert_eq!(spans, [("기울임", STYLE_ITALIC), ("굵은 _기울임_", STYLE_BOLD), ("a_b_", STYLE_MONO)]);
        let marks: Vec<&str> = markup.iter().map(|&r| at(r)).collect();
        assert_eq!(marks, ["**", "**", "_", "_", "\\", "`", "`"]);
        // same spans as what gets sent
        let (plain, sent) = parse(s);
        assert_eq!(sent.len(), styles.len());
        assert_eq!(plain, "굵은 기울임를 **그대로** a_b_");
    }
}
