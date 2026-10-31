//! Emoji helpers. Color emoji themselves are drawn by the patched software renderer
//! (vendor/i-slint-renderer-software, COLR v0), so emoji are just text everywhere.

/// Whether a grapheme cluster is an emoji.
pub fn is_emoji(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else { return false };
    let cp = first as u32;
    let pictographic = matches!(cp, 0x1f000..=0x1faff | 0x2600..=0x27bf | 0x2300..=0x23ff | 0x2b00..=0x2bff);
    // text-default symbols (e.g. ❤ ☺) count when followed by the emoji presentation selector
    pictographic && (cp >= 0x1f000 || cluster.contains('\u{fe0f}') || cluster.chars().count() == 1)
}

/// Splits `text` into emoji clusters if it consists only of 1..=max emoji (whitespace allowed).
pub fn only_emoji(text: &str, max: usize) -> Option<Vec<String>> {
    use unicode_segmentation::UnicodeSegmentation;
    let clusters: Vec<String> =
        text.graphemes(true).filter(|g| !g.trim().is_empty()).map(str::to_string).collect();
    (!clusters.is_empty() && clusters.len() <= max && clusters.iter().all(|c| is_emoji(c))).then_some(clusters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_emoji_only_messages() {
        assert_eq!(only_emoji("😀", 5).unwrap().len(), 1);
        assert_eq!(only_emoji("🎉🥳👍🏽", 5).unwrap(), vec!["🎉", "🥳", "👍🏽"]);
        assert_eq!(only_emoji(" ❤️ ", 5).unwrap(), vec!["❤️"]);
        assert_eq!(only_emoji("👨‍👩‍👧", 5).unwrap().len(), 1, "ZWJ family is one cluster");
        assert!(only_emoji("hi 😀", 5).is_none());
        assert!(only_emoji("ㅋㅋ", 5).is_none());
        assert!(only_emoji("123", 5).is_none());
        assert!(only_emoji("", 5).is_none());
        assert!(only_emoji("😀😀😀😀😀😀", 5).is_none(), "more than max");
    }
}
