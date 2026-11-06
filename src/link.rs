//! Clickable links in message text.
//!
//! `find` locates URLs; `main::rich_text` turns them into markdown links in the StyledText
//! drawn over the message's TextInput. The vendored i-slint-core patch makes that StyledText
//! take only the mouse events over a link (pointer cursor, `link-clicked`), so selection in
//! the TextInput underneath keeps working everywhere else.

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

/// Links in `text` a preview can be for, the way receivers will find them in the body.
pub fn preview_urls(text: &str) -> Vec<String> {
    let mut urls = presage::model::previews::extract_urls_from_text_block(text);
    urls.retain(|u| !is_sneaky(u));
    urls
}

/// Whether a preview claiming to be for `url` may be shown on a message with this `body`.
pub fn preview_allowed(url: &str, body: &str) -> bool {
    preview_urls(body).iter().any(|u| u == url)
}

/// Links dressed up as something else (Signal Desktop's `isLinkSneaky`): an image and a title on
/// them would make them look more real. Mixed scripts in a domain are judged by its punycode: a
/// label in another script beside plain ones, not a mix inside one label.
fn is_sneaky(href: &str) -> bool {
    const MAX_HREF: usize = 2048;
    if href.len() > MAX_HREF || href.chars().any(|c| ('\u{2500}'..='\u{25ff}').contains(&c)) {
        return true;
    }
    let Ok(url) = url::Url::parse(href) else { return true };
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    let Some(host) = url.host_str() else { return true };
    if host.contains('%') {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return true;
    }
    let puny = labels.iter().filter(|l| l.starts_with("xn--")).count();
    puny != 0 && puny != labels.len()
}

/// The site's name on a preview card. Punycode stays as it is: it can't pass for another site.
pub fn domain(url: &str) -> String {
    let Ok(url) = url::Url::parse(url) else { return String::new() };
    let host = url.host_str().unwrap_or_default();
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Text from a preview, which anyone can make say anything: one line, no control or direction
/// characters, at most `max` characters.
pub fn clean_preview_text(text: &str, max: usize) -> String {
    let hidden = |c: char| c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}');
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = line.chars().filter(|c| !hidden(*c)).take(max + 1).collect();
    if out.chars().count() > max {
        out = out.chars().take(max).collect::<String>().trim_end().to_string();
        out.push('…');
    }
    out
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
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (verb, file) = (wide("open"), wide(url));
        // off the UI thread: the shell may take a while
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
    fn sneaky_links_get_no_preview() {
        assert_eq!(preview_urls("봐 https://www.youtube.com/watch?v=abc"), ["https://www.youtube.com/watch?v=abc"]);
        for bad in [
            "http://youtube.com/a",
            "https://youtube.com@evil.com/a",
            "https://localhost/a",
            "https://single/a",
            "https://аррӏе.com/a",
            "https://192.168.0.1/a",
        ] {
            assert!(preview_urls(bad).is_empty(), "{bad}");
        }
        assert!(preview_allowed("https://a.kr/x", "여기 https://a.kr/x 봐"));
        assert!(!preview_allowed("https://b.kr/x", "여기 https://a.kr/x 봐"));
        assert!(!preview_allowed("", "여기 https://a.kr/x 봐"));
    }

    #[test]
    fn preview_text_is_one_short_line() {
        assert_eq!(clean_preview_text(" 제목\n\t둘째\u{202e}줄 ", 20), "제목 둘째줄");
        assert_eq!(clean_preview_text("가나다라", 3), "가나다…");
        assert_eq!(domain("https://www.youtube.com/watch"), "youtube.com");
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
}
