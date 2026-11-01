//! Clickable links in message text.
//!
//! The visible text is a read-only TextInput (so selection works), which grabs every left
//! press and has no click callback. A click is detected from its side effects instead: the
//! press moves the cursor (`cursor-position-changed`) with anchor == cursor, a drag moves it
//! again with anchor != cursor, and the release comes from the window proc (`winhook.rs`)
//! or the headless driver. Press inside a link + release without a drag = open the link.

use std::cell::RefCell;

thread_local! {
    static ARMED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Byte ranges of the URLs in `text` (http(s)://… and www.…).
pub fn find(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let rest = &lower[i..];
        let scheme = ["https://", "http://", "www."].iter().find(|p| rest.starts_with(**p));
        let boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.' || b[i - 1] == b'/');
        let (Some(p), true) = (scheme, boundary) else {
            i += lower[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let end = i + text[i..].find(|c: char| c.is_whitespace() || "<>\"'`".contains(c)).unwrap_or(text.len() - i);
        let end = trim_end(&text[i..end]) + i;
        if end > i + p.len() && text[i + p.len()..end].contains(|c: char| c.is_alphanumeric()) {
            out.push((i, end));
            i = end;
        } else {
            i += p.len();
        }
    }
    out
}

/// Length of `url` without trailing punctuation that belongs to the sentence,
/// keeping closing brackets that close one inside the URL (wikipedia "Foo_(bar)").
fn trim_end(url: &str) -> usize {
    let mut end = url.len();
    loop {
        let Some(c) = url[..end].chars().last() else { return 0 };
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '…' | '。' | '、' => true,
            ')' | ']' | '}' => {
                let open = match c { ')' => '(', ']' => '[', _ => '{' };
                let s = &url[..end];
                s.matches(c).count() > s.matches(open).count()
            }
            _ => false,
        };
        if !drop {
            return end;
        }
        end -= c.len_utf8();
    }
}

/// What to hand to the shell: www.… gets a scheme.
pub fn target(url: &str) -> String {
    if url.to_ascii_lowercase().starts_with("www.") { format!("https://{url}") } else { url.to_string() }
}

/// The TextInput's cursor moved (press or drag).
pub fn cursor_moved(text: &str, cursor: usize, anchor: usize) {
    let hit = (cursor == anchor)
        .then(|| find(text).into_iter().find(|&(s, e)| s <= cursor && cursor < e))
        .flatten()
        .map(|(s, e)| target(&text[s..e]));
    ARMED.with_borrow_mut(|a| *a = hit);
}

/// Left button released: the link to open, if the press was a click on one.
pub fn released() -> Option<String> {
    ARMED.with_borrow_mut(Option::take)
}

pub fn open(url: &str) {
    // only web links ever reach here (see `find`); never pass anything else to the shell
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    if crate::headless::active() {
        tracing::info!(url, "link clicked (headless: not opened)");
        return;
    }
    tracing::info!(url, "open link");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (verb, file) = (wide("open"), wide(url));
        // off the UI thread: called from the window proc, and the shell may take a while
        std::thread::spawn(move || {
            // SAFETY: valid NUL-terminated UTF-16 strings that outlive the call
            unsafe {
                ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
            }
        });
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(t: &str) -> Vec<&str> {
        find(t).into_iter().map(|(s, e)| &t[s..e]).collect()
    }

    #[test]
    fn finds_urls() {
        assert_eq!(urls("see https://example.com/a?b=1."), ["https://example.com/a?b=1"]);
        assert_eq!(urls("여기 www.naver.com 봐"), ["www.naver.com"]);
        assert_eq!(urls("(https://en.wikipedia.org/wiki/Foo_(bar))"), ["https://en.wikipedia.org/wiki/Foo_(bar)"]);
        assert_eq!(urls("링크:https://a.kr/한글경로, 끝"), ["https://a.kr/한글경로"]);
        assert_eq!(urls("HTTP://X.COM"), ["HTTP://X.COM"]);
        assert_eq!(urls("two http://a.b and http://c.d!"), ["http://a.b", "http://c.d"]);
        assert!(urls("https:// nothing, www. and awww.x").is_empty());
        assert!(urls("plain text 한글").is_empty());
        assert_eq!(urls("@이서연 넵 🙂 자료는 https://signal.org/ko/ 그리고 www.naver.com."), ["https://signal.org/ko/", "www.naver.com"]);
    }

    #[test]
    fn click_vs_drag() {
        let t = "go https://x.io now";
        cursor_moved(t, 5, 5);
        assert_eq!(released().as_deref(), Some("https://x.io"));
        assert_eq!(released(), None);
        cursor_moved(t, 5, 5);
        cursor_moved(t, 8, 5); // dragged
        assert_eq!(released(), None);
        cursor_moved(t, 1, 1); // outside
        assert_eq!(released(), None);
        cursor_moved("www.a.kr", 0, 0);
        assert_eq!(released().as_deref(), Some("https://www.a.kr"));
    }
}
