//! Matching names as Korean is typed: the query's last character may still be in IME
//! composition, and consonants alone stand for the name's initials.

/// Where in `name` the query starts (case-insensitive), in chars, or `None`.
/// The query's last char matches as keystrokes (`keys`): "ㅇ", "우", "윚" and "위지" all find
/// "위진수". Consonants alone match the name's initials: "ㅇㅈㅅ" finds it too.
pub fn find(name: &str, query: &str) -> Option<usize> {
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let q: Vec<char> = query.to_lowercase().chars().collect();
    composing(&n, &q).or_else(|| initials(&n, &q))
}

/// All but the query's last char match exactly, the last one's keystrokes are a prefix of the
/// keystrokes from there on. A final consonant may still move to the next syllable
/// ("윚" → "위진"), and a vowel may still grow ("우" → "위").
fn composing(name: &[char], q: &[char]) -> Option<usize> {
    let Some((&last, head)) = q.split_last() else { return Some(0) };
    let mut want = Vec::new();
    keys(last, &mut want);
    (0..name.len().checked_sub(head.len())?).find(|&i| {
        if name[i..i + head.len()] != *head {
            return false;
        }
        let mut got = Vec::new();
        for &c in &name[i + head.len()..] {
            keys(c, &mut got);
            if got.len() >= want.len() {
                break;
            }
        }
        got.starts_with(&want)
    })
}

/// A query of consonants only, against the name's initials: "ㅎㄱㄷ" finds "홍길동". Spaces in
/// the name are skipped, so it also finds "홍 길동".
fn initials(name: &[char], q: &[char]) -> Option<usize> {
    let mut want = Vec::new();
    for &c in q {
        keys(c, &mut want);
    }
    if want.is_empty() || !want.iter().all(|c| ('ㄱ'..='ㅎ').contains(c)) {
        return None;
    }
    let initials: Vec<char> = name
        .iter()
        .filter(|c| !c.is_whitespace())
        .map(|&c| syllable(c).map_or(c, |s| INITIAL.chars().nth(s / 588).unwrap()))
        .collect();
    initials.windows(want.len()).position(|w| w == want)
}

const INITIAL: &str = "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ";
const MEDIAL: &str = "ㅏㅐㅑㅒㅓㅔㅕㅖㅗㅘㅙㅚㅛㅜㅝㅞㅟㅠㅡㅢㅣ";
const FINAL: &str = "ㄱㄲㄳㄴㄵㄶㄷㄹㄺㄻㄼㄽㄾㄿㅀㅁㅂㅄㅅㅆㅇㅈㅊㅋㅌㅍㅎ";

/// The index of a precomposed Hangul syllable (가 = 0).
fn syllable(c: char) -> Option<usize> {
    (c as u32).checked_sub(0xAC00).filter(|&s| s < 11172).map(|s| s as usize)
}

/// The jamo a 2-beolsik keyboard types for `c`, as compatibility jamo with compound vowels and
/// finals split into their keys ("쵧" → ㅊㅗㅣㄹㄱ). Other characters stand for themselves.
fn keys(c: char, out: &mut Vec<char>) {
    let split = |j: char, out: &mut Vec<char>| {
        let keys = match j {
            'ㄳ' => "ㄱㅅ",
            'ㄵ' => "ㄴㅈ",
            'ㄶ' => "ㄴㅎ",
            'ㄺ' => "ㄹㄱ",
            'ㄻ' => "ㄹㅁ",
            'ㄼ' => "ㄹㅂ",
            'ㄽ' => "ㄹㅅ",
            'ㄾ' => "ㄹㅌ",
            'ㄿ' => "ㄹㅍ",
            'ㅀ' => "ㄹㅎ",
            'ㅄ' => "ㅂㅅ",
            'ㅘ' => "ㅗㅏ",
            'ㅙ' => "ㅗㅐ",
            'ㅚ' => "ㅗㅣ",
            'ㅝ' => "ㅜㅓ",
            'ㅞ' => "ㅜㅔ",
            'ㅟ' => "ㅜㅣ",
            'ㅢ' => "ㅡㅣ",
            _ => return out.push(j),
        };
        out.extend(keys.chars());
    };
    let Some(s) = syllable(c) else { return split(c, out) };
    out.extend(INITIAL.chars().nth(s / 588));
    split(MEDIAL.chars().nth(s % 588 / 28).unwrap(), out);
    if s % 28 > 0 {
        split(FINAL.chars().nth(s % 28 - 1).unwrap(), out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_while_composing() {
        for q in ["ㅇ", "우", "위", "윚", "위지", "위진", "위진수", "진수", "수", ""] {
            assert!(find("위진수", q).is_some(), "{q}");
        }
        assert_eq!(find("위진수", "윚"), Some(0));
        assert_eq!(find("위진수", "진"), Some(1));
        // only the last char is still composing: earlier ones match as typed
        assert_eq!(find("위진수", "워"), None);
        assert_eq!(find("위진수", "우지"), None);
        assert_eq!(find("가나나", "난"), Some(1));
        assert_eq!(find("가나나", "간나"), None);
        assert_eq!(find("Kim", "KI"), Some(0));
    }

    #[test]
    fn matches_initials() {
        assert_eq!(find("위진수", "ㅇㅈㅅ"), Some(0));
        assert_eq!(find("위진수", "ㅈㅅ"), Some(1));
        assert_eq!(find("한 지수", "ㅎㅈㅅ"), Some(0));
        // the IME may join two consonants into one: ㄱ + ㅅ = ㄳ
        assert_eq!(find("강수", "ㄳ"), Some(0));
        assert_eq!(find("위진수", "ㅇㅅ"), None);
    }
}
