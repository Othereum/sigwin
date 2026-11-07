//! "What's new" after an update: the release notes of every version so far, embedded in the exe
//! (`SIGWIN_NOTES`, set by scripts/release.ps1; see docs/release.md "Release notes").
//!
//! The embedded text is one block per version, newest first, each starting with a `# <version>`
//! line. `since` picks the blocks an update brought.
//!
//! The notes and this parser are built together, so no older app reads a newer notes format.
//! The parser is lenient on purpose: any other heading is a section, anything it doesn't know is
//! plain text, so notes can use a new heading without an app change. Nothing is rendered but
//! text (no links, images or HTML).

static NOTES: &str = include_str!(concat!(env!("OUT_DIR"), "/release-notes.md"));

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// `# 0.2.3`: starts one version's block
    Version,
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
        let (kind, body) = if line.starts_with("##") {
            (Kind::Heading, line.trim_start_matches('#'))
        } else if line.starts_with('#') {
            (Kind::Version, line.trim_start_matches('#').trim().trim_start_matches('v'))
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

/// The blocks newer than `last` (all of them if it is empty: nothing says what was seen) up to
/// `current`. Text before the first version line (a dev build's notes) always stays.
pub fn since(lines: Vec<Line>, last: &str, current: &str) -> Vec<Line> {
    let (last, current) = (crate::install::version(last), crate::install::version(current));
    let mut keep = true;
    lines
        .into_iter()
        .filter(|l| {
            if l.kind == Kind::Version {
                let v = crate::install::version(&l.text);
                keep = v <= current && (last.iter().all(|&n| n == 0) || v > last);
            }
            keep
        })
        .collect()
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
        assert_eq!(parse("# v1.2")[0], Line { kind: Kind::Version, text: "1.2".into() });
        assert_eq!(l[0].text, "새 기능");
        assert_eq!(l[2].text, "둘");
    }

    #[test]
    fn picks_the_versions_an_update_brought() {
        let all = || parse("# 0.2.5\n## 수정\n- c\n# v0.2.4\n- b\n# 0.2.3\n- a\n# 0.10.0\n- future\n");
        let texts = |v: Vec<Line>| v.into_iter().filter(|l| l.kind == Kind::Version).map(|l| l.text).collect::<Vec<_>>();
        assert_eq!(texts(since(all(), "0.2.3", "0.2.5")), ["0.2.5", "0.2.4"]);
        assert_eq!(texts(since(all(), "0.2.5", "0.2.5")), Vec::<String>::new());
        // never recorded: everything up to this version
        assert_eq!(texts(since(all(), "", "0.2.5")), ["0.2.5", "0.2.4", "0.2.3"]);
        // notes without version lines (a dev build) stay whole
        assert_eq!(since(parse("- x"), "0.2.3", "0.2.5").len(), 1);
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
