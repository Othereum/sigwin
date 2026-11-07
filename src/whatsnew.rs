//! "What's new" after an update: this version's release notes, embedded in the exe
//! (`SIGWIN_NOTES`, set by scripts/release.ps1; see docs/release.md "Release notes").
//!
//! The notes and this parser are built together, so no older app reads a newer notes format.
//! The parser is lenient on purpose: any heading is a section, anything it doesn't know is
//! plain text, so notes can use a new heading without an app change. Nothing is rendered but
//! text (no links, images or HTML).

static NOTES: &str = include_str!(concat!(env!("OUT_DIR"), "/release-notes.md"));

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Heading,
    Bullet,
    Text,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
}

/// The embedded notes. A dev build can read other notes from `SIGWIN_NOTES_FILE` instead.
pub fn notes() -> Vec<Line> {
    #[cfg(sigwin_dev)]
    if let Some(text) = std::env::var_os("SIGWIN_NOTES_FILE").and_then(|p| std::fs::read_to_string(p).ok()) {
        return parse(&text);
    }
    parse(NOTES)
}

pub fn parse(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (kind, body) = if line.starts_with('#') {
            (Kind::Heading, line.trim_start_matches('#'))
        } else if let Some(b) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            (Kind::Bullet, b)
        } else {
            (Kind::Text, line)
        };
        let body = body.trim();
        if !body.is_empty() {
            out.push(Line { kind, text: body.to_string() });
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    /// offer the notes, and remember the version once they were seen or dismissed
    Show,
    /// nothing to offer, but this version counts as seen
    Record,
    Nothing,
}

/// What to do at startup. `last` is the version whose notes were last seen ("" = never
/// recorded). An app from before this feature recorded nothing, so an account that already
/// exists means it was updated, while no account means a fresh install.
pub fn decide(last: &str, current: &str, has_account: bool) -> Action {
    if last.is_empty() {
        return if has_account { Action::Show } else { Action::Record };
    }
    // a downgrade or a rerun of the same version
    if crate::install::version(current) > crate::install::version(last) {
        Action::Show
    } else {
        Action::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_bullets_and_text() {
        let l = parse("## 새 기능\n\n- 하나\n* 둘\n\n설명\n##\n## 수정\n- 셋\n");
        let kinds: Vec<_> = l.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, [Kind::Heading, Kind::Bullet, Kind::Bullet, Kind::Text, Kind::Heading, Kind::Bullet]);
        assert_eq!(l[0].text, "새 기능");
        assert_eq!(l[2].text, "둘");
    }

    #[test]
    fn unknown_markup_stays_text() {
        let l = parse("<b>x</b> [a](http://b)\n1. 번호");
        assert!(l.iter().all(|x| x.kind == Kind::Text));
        assert_eq!(l[0].text, "<b>x</b> [a](http://b)");
    }

    #[test]
    fn decides_by_last_seen_version() {
        // updated from an app that didn't record versions
        assert_eq!(decide("", "0.5.0", true), Action::Show);
        // fresh install
        assert_eq!(decide("", "0.5.0", false), Action::Record);
        assert_eq!(decide("0.4.9", "0.5.0", true), Action::Show);
        assert_eq!(decide("0.4.9", "0.5.0", false), Action::Show);
        assert_eq!(decide("0.5.0", "0.5.0", true), Action::Nothing);
        assert_eq!(decide("0.5.0", "0.4.9", true), Action::Nothing);
        assert_eq!(decide("0.9.0", "0.10.0", true), Action::Show);
    }
}
