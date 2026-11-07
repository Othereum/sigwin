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

/// Emoji per row of the picker grid (`PickerLayout.columns` in ui/reactions.slint).
pub const PICKER_COLUMNS: usize = 8;

/// Newest Emoji version the picker offers. Newer sequences (15.1: 🚶‍➡️) aren't combined by the font
/// and show as their base emoji, and 15.0 is what current Windows 11 draws.
const PICKER_MAX_VERSION: (u32, u32) = (15, 0);

/// The reaction picker's tabs, in the order of the icons in ui/reactions.slint. Flags are left
/// out (Windows draws them as letters).
const PICKER_TABS: [emojis::Group; 8] = [
    emojis::Group::SmileysAndEmotion,
    emojis::Group::PeopleAndBody,
    emojis::Group::AnimalsAndNature,
    emojis::Group::FoodAndDrink,
    emojis::Group::Activities,
    emojis::Group::TravelAndPlaces,
    emojis::Group::Objects,
    emojis::Group::Symbols,
];

/// The emoji of one picker tab (base skin tone only, in the order of Unicode's list).
pub fn picker_tab(index: usize) -> Vec<&'static str> {
    let Some(group) = PICKER_TABS.get(index) else { return Vec::new() };
    group
        .emojis()
        .filter(|e| (e.emoji_version().major(), e.emoji_version().minor()) <= PICKER_MAX_VERSION)
        .map(|e| e.as_str())
        .collect()
}

/// `recent` with `emoji` moved to the front (added if new), cut to `max`.
pub fn bump_recent(recent: &mut Vec<String>, emoji: &str, max: usize) {
    recent.retain(|e| e != emoji);
    recent.insert(0, emoji.to_string());
    recent.truncate(max);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_tabs_have_emoji_and_no_flags() {
        for i in 0..PICKER_TABS.len() {
            let tab = picker_tab(i);
            assert!(tab.len() > 20, "tab {i} is nearly empty");
            assert!(tab.iter().all(|e| !e.is_empty()));
        }
        assert!(picker_tab(PICKER_TABS.len()).is_empty());
        let all: Vec<&str> = (0..PICKER_TABS.len()).flat_map(picker_tab).collect();
        assert!(!all.contains(&"🇰🇷") && !all.contains(&"🏁"), "flags are left out");
        assert!(all.contains(&"❤️") && all.contains(&"👍") && all.contains(&"😂"), "the quick six are in it");
        assert!(!all.contains(&"👍🏽"), "no skin tone variants");
    }

    #[test]
    fn recent_moves_to_front_without_duplicates_and_is_capped() {
        let mut r: Vec<String> = Vec::new();
        for e in ["👍", "❤️", "😂", "👍"] {
            bump_recent(&mut r, e, 3);
        }
        assert_eq!(r, ["👍", "😂", "❤️"]);
        bump_recent(&mut r, "🎉", 3);
        assert_eq!(r, ["🎉", "👍", "😂"]);
    }

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
