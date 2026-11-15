//! UI language. Strings are written in English (`tr!` here, `@tr` in ui/*.slint) and looked up
//! in `i18n/<lang>/LC_MESSAGES/sigwin-ui.po`: crates/ui bundles those for Slint, build.rs turns
//! them into static tables for Rust. Both use Slint's format syntax: `{}` takes the next
//! argument, `{0}` a given one, `{n}` the count of a plural, `{{`/`}}` a brace.
//!
//! The language is fixed for the run (`init`): Rust-made strings (rows, notifications) don't
//! follow a change, so picking another one applies on restart.

use std::fmt::{Display, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Catalog {
    pub lang: &'static str,
    /// `(msgctxt, msgid, msgstr forms)`, sorted by context then id
    pub entries: &'static [(&'static str, &'static str, &'static [&'static str])],
}

include!(concat!(env!("OUT_DIR"), "/i18n.rs"));

/// Index into `CATALOGS`; `usize::MAX` for the English source.
static CURRENT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Picks the language for this run: `SIGWIN_LANG` (any run, for checks), else English for
/// headless runs (shots and scenarios look the same on every PC), else `chosen` (the setting),
/// else the system's. Returns its code for `slint::select_bundled_translation`.
pub fn init(headless: bool, chosen: &str) -> &'static str {
    let wanted = std::env::var("SIGWIN_LANG").ok().or_else(|| match () {
        _ if headless => None,
        _ if !chosen.is_empty() => Some(chosen.to_string()),
        _ => sys_locale::get_locale(),
    });
    let idx = wanted.as_deref().and_then(catalog_for).unwrap_or(usize::MAX);
    CURRENT.store(idx, Ordering::Relaxed);
    CATALOGS.get(idx).map_or("en", |c| c.lang)
}

/// The catalog for a locale like "ko-KR": an exact match, else the language part.
fn catalog_for(locale: &str) -> Option<usize> {
    fn base(l: &str) -> &str {
        l.find(['-', '_', '@']).map_or(l, |i| &l[..i])
    }
    CATALOGS
        .iter()
        .position(|c| c.lang.eq_ignore_ascii_case(locale))
        .or_else(|| CATALOGS.iter().position(|c| c.lang.eq_ignore_ascii_case(base(locale))))
}

/// The current language's code ("en" for the source), for what code formats per language
/// (dates in `view::time`).
pub fn lang() -> &'static str {
    current().map_or("en", |c| c.lang)
}

fn current() -> Option<&'static Catalog> {
    CATALOGS.get(CURRENT.load(Ordering::Relaxed))
}

fn find(ctx: &str, id: &str) -> Option<&'static [&'static str]> {
    find_in(current()?, ctx, id)
}

fn find_in(catalog: &'static Catalog, ctx: &str, id: &str) -> Option<&'static [&'static str]> {
    let entries = catalog.entries;
    let i = entries.binary_search_by(|(c, m, _)| (*c, *m).cmp(&(ctx, id))).ok()?;
    Some(entries[i].2)
}

/// Which plural form `n` takes. Keep in step with each catalog's `Plural-Forms` header (Slint
/// reads that one; a test checks they agree).
fn plural_form(lang: &str, n: i64) -> usize {
    match lang {
        "ko" => 0,
        _ => (n != 1) as usize,
    }
}

/// `id` in the current language. Use `tr!`.
pub fn text(ctx: &str, id: &'static str) -> &'static str {
    find(ctx, id).and_then(|f| f.first().copied()).filter(|s| !s.is_empty()).unwrap_or(id)
}

/// `one` / `other` for `n` in the current language, formatted. Use `tr!`.
pub fn plural(ctx: &str, one: &'static str, other: &'static str, n: i64, args: &[&dyn Display]) -> String {
    let translated = current().zip(find(ctx, one)).and_then(|(c, f)| f.get(plural_form(c.lang, n)).copied());
    let template = translated.filter(|s| !s.is_empty()).unwrap_or(if n == 1 { one } else { other });
    format(template, args, Some(n))
}

/// Slint's formatting (`i_slint_core::translations`, not public): an argument or brace that
/// doesn't parse is left as written.
pub fn format(template: &str, args: &[&dyn Display], n: Option<i64>) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(p) = rest.find(['{', '}']) {
        out.push_str(&rest[..p]);
        let tail = &rest[p..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        let Some(end) = tail.find('}').filter(|_| tail.starts_with('{')) else {
            out.push_str(&tail[..1]);
            rest = &tail[1..];
            continue;
        };
        let name = tail[1..end].trim();
        let arg: Option<&dyn Display> = if name.is_empty() {
            next += 1;
            args.get(next - 1).copied()
        } else if let Ok(i) = name.parse::<usize>() {
            args.get(i).copied()
        } else {
            None
        };
        match (arg, name) {
            (Some(a), _) => write!(out, "{a}").unwrap(),
            (None, "n") if n.is_some() => write!(out, "{}", n.unwrap()).unwrap(),
            _ => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A UI string in the current language: `tr!("Settings")` is a `&'static str`; with arguments
/// (`tr!("This device: {}", name)`) or a plural (`tr!("{n} message" | "{n} messages" % count)`)
/// it is a `String`. `tr!("context" => "…")` tells apart the same English for different uses.
/// Only literals: the catalog check (`tests::catalogs_match_the_code`) finds strings by reading
/// the source.
macro_rules! tr {
    (@ $ctx:literal; $one:literal | $other:literal % $n:expr $(, $a:expr)* $(,)?) => {
        $crate::i18n::plural($ctx, $one, $other, ($n) as i64, &[$(&$a as &dyn ::std::fmt::Display),*])
    };
    (@ $ctx:literal; $id:literal $(,)?) => {
        $crate::i18n::text($ctx, $id)
    };
    (@ $ctx:literal; $id:literal, $($a:expr),+ $(,)?) => {
        $crate::i18n::format($crate::i18n::text($ctx, $id), &[$(&$a as &dyn ::std::fmt::Display),+], None)
    };
    ($ctx:literal => $($rest:tt)+) => {
        tr!(@ $ctx; $($rest)+)
    };
    ($($rest:tt)+) => {
        tr!(@ ""; $($rest)+)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_slint() {
        assert_eq!(format("Hello {}!", &[&"world"], None), "Hello world!");
        assert_eq!(format("-{1}- -{0}-", &[&45, &"World"], None), "-World- -45-");
        assert_eq!(format("{{}} {n} left", &[], Some(3)), "{} 3 left");
        assert_eq!(format("{} and {x} {", &[], None), "{} and {x} {");
    }

    #[test]
    fn source_strings_without_a_language() {
        // CURRENT starts at the English source
        assert_eq!(tr!("Settings"), "Settings");
        assert_eq!(tr!("{n} message" | "{n} messages" % 1), "1 message");
        assert_eq!(tr!("{n} message" | "{n} messages" % 2), "2 messages");
        assert_eq!(tr!("{} of {}", 1, 2), "1 of 2");
    }

    /// Catalogs every used string must be translated in; others may lag (English shows).
    const COMPLETE: &[&str] = &["ko"];

    type Key = (String, String, Option<String>);

    /// `(context, id, plural)` of every `tr!(…)` in src/ and `@tr(…)` in ui/, with where each is
    /// used (`ui/app.slint:42`), so whoever translates can read the code around it.
    fn used() -> std::collections::BTreeMap<Key, Vec<String>> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs" || x == "slint") {
                    out.push(p);
                }
            }
        }
        /// A plain string literal at the start of `s` (after spaces): its value and the rest.
        fn literal(s: &str) -> Option<(String, &str)> {
            let s = s.trim_start().strip_prefix('"')?;
            let mut value = String::new();
            let mut chars = s.char_indices();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => return Some((value, &s[i + 1..])),
                    '\\' => match chars.next()?.1 {
                        'n' => value.push('\n'),
                        't' => value.push('\t'),
                        c => value.push(c),
                    },
                    c => value.push(c),
                }
            }
            None
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        walk(&root.join("ui"), &mut files);
        let mut keys = std::collections::BTreeMap::<Key, Vec<String>>::new();
        for f in files.iter().filter(|f| !f.ends_with("i18n.rs")) {
            let text = std::fs::read_to_string(f).unwrap();
            let marker = if f.extension().unwrap() == "rs" { "tr!(" } else { "@tr(" };
            for (at, _) in text.match_indices(marker) {
                if text[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let where_ = || format!("{}: `{}`", f.display(), &text[at..text.len().min(at + 60)]);
                let (first, rest) = literal(&text[at + marker.len()..]).unwrap_or_else(|| panic!("not a literal: {}", where_()));
                let (ctx, id, rest) = match rest.trim_start().strip_prefix("=>") {
                    Some(rest) => {
                        let (id, rest) = literal(rest).unwrap_or_else(|| panic!("not a literal: {}", where_()));
                        (first, id, rest)
                    }
                    None => (String::new(), first, rest),
                };
                let plural = rest.trim_start().strip_prefix('|').map(|r| literal(r).unwrap_or_else(|| panic!("not a literal: {}", where_())).0);
                let file = f.strip_prefix(root).unwrap_or(f).to_string_lossy().replace('\\', "/");
                let line = text[..at].matches('\n').count() + 1;
                keys.entry((ctx, id, plural)).or_default().push(format!("{file}:{line}"));
            }
        }
        keys
    }

    fn po_entry(k: &Key, at: &[String]) -> String {
        let refs = if at.is_empty() { String::new() } else { format!("#: {}\n", at.join(" ")) };
        let ctx = if k.0.is_empty() { refs } else { format!("{refs}msgctxt {:?}\n", k.0) };
        match &k.2 {
            None => format!("{ctx}msgid {:?}\nmsgstr \"\"\n", k.1),
            Some(p) => format!("{ctx}msgid {:?}\nmsgid_plural {p:?}\nmsgstr[0] \"\"\n", k.1),
        }
    }

    /// Every catalog's entries are strings the code uses, and complete catalogs translate all of
    /// them; the failure lists what to add or remove, in .po form.
    #[test]
    fn catalogs_match_the_code() {
        use rspolib::TranslatedEntry;
        let used = used();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n");
        let mut problems = String::new();
        for lang in std::fs::read_dir(&dir).unwrap() {
            let lang = lang.unwrap();
            let code = lang.file_name().to_string_lossy().into_owned();
            let path = lang.path().join("LC_MESSAGES").join("sigwin-ui.po");
            let po = rspolib::pofile(path.as_path()).unwrap();
            let header = po.metadata.get("Plural-Forms").map(|h| h.trim().to_string());
            let expected = if plural_form(&code, 1) == plural_form(&code, 2) { "nplurals=1; plural=0;" } else { "nplurals=2; plural=(n != 1);" };
            if header.as_deref() != Some(expected) {
                problems += &format!("{code}: Plural-Forms should be `{expected}` (`plural_form`)\n");
            }
            let entries: std::collections::BTreeMap<Key, bool> = po
                .entries
                .iter()
                .filter(|e| !e.obsolete)
                .map(|e| ((e.msgctxt.clone().unwrap_or_default(), e.msgid.clone(), e.msgid_plural.clone()), e.translated()))
                .collect();
            let stale: Vec<_> = entries.keys().filter(|k| !used.contains_key(*k)).collect();
            if !stale.is_empty() {
                problems += &format!("{code}: not used by the code, remove:\n");
                stale.iter().for_each(|k| problems += &po_entry(k, &[]));
            }
            if COMPLETE.contains(&code.as_str()) {
                let missing: Vec<_> = used.iter().filter(|(k, _)| entries.get(*k) != Some(&true)).collect();
                if !missing.is_empty() {
                    problems += &format!("{code}: missing or untranslated, add:\n");
                    missing.iter().for_each(|(k, at)| problems += &po_entry(k, at));
                }
            }
        }
        assert!(problems.is_empty(), "{problems}");
    }

    #[test]
    fn locales_pick_a_catalog() {
        let ko = CATALOGS.iter().position(|c| c.lang == "ko");
        // the generated table is sorted for the binary search
        assert_eq!(find_in(&CATALOGS[ko.unwrap()], "", "dev build"), Some(&["개발 빌드"][..]));
        assert_eq!(catalog_for("ko-KR"), ko);
        assert_eq!(catalog_for("ko"), ko);
        assert_eq!(catalog_for("fr-FR"), None);
    }
}
