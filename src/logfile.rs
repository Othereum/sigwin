//! Log file, so builds without a console leave a trace: `<data>\logs\sigwin.log`, rotated to
//! `sigwin.log.1` past 1MB (at most ~2MB on disk). Panics are logged too (`install_panic_hook`).
//! Account identifiers are masked on the way to the file, whoever logged them (`scrub`).

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const MAX: u64 = 1 << 20;

pub struct LogFile {
    inner: Mutex<Inner>,
}

struct Inner {
    path: PathBuf,
    file: Option<File>,
    len: u64,
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

impl LogFile {
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let dir = dir(data_dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("sigwin.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata()?.len();
        Ok(LogFile { inner: Mutex::new(Inner { path, file: Some(file), len }) })
    }
}

impl Inner {
    fn rotate(&mut self) {
        self.file = None;
        let old = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&self.path, &old);
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.len = 0;
    }
}

/// Masks what identifies people or groups in a log line: UUIDs, phone numbers and long hex (a
/// group's master key in a thread key, digests), and the path of URLs. A safety net for what libraries log too: code
/// here shouldn't log these in the first place.
fn scrub(line: &[u8]) -> Cow<'_, [u8]> {
    let word = |b: u8| b.is_ascii_alphanumeric();
    let mut out: Option<Vec<u8>> = None;
    let (mut i, mut kept) = (0, 0);
    while i < line.len() {
        let rest = &line[i..];
        let at_start = i == 0 || !word(line[i - 1]);
        let ends_at = |n: usize| rest.len() == n || !word(rest[n]);
        let found: Option<(usize, Cow<'_, [u8]>)> = if !at_start {
            None
        } else if let Some(scheme) = [&b"https://"[..], b"http://"].into_iter().find(|s| rest.starts_with(s)) {
            // a URL keeps its host: its path and query can hold a token or an id (network errors
            // quote the URL of the request)
            let end = rest.iter().position(|b| b.is_ascii_whitespace() || matches!(b, b')' | b']' | b'>' | b'"' | b'\'')).unwrap_or(rest.len());
            let host = scheme.len() + rest[scheme.len()..end].iter().position(|b| matches!(b, b'/' | b'?' | b'#')).unwrap_or(end - scheme.len());
            (host < end).then(|| (end, Cow::Owned([&rest[..host], b"/<path>"].concat())))
        } else if rest[0] == b'+' {
            let digits = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
            ((9..=15).contains(&digits) && ends_at(digits + 1)).then_some((digits + 1, Cow::Borrowed(&b"<phone>"[..])))
        } else if rest[0].is_ascii_hexdigit() {
            let hex = rest.iter().take_while(|b| b.is_ascii_hexdigit()).count();
            let uuid = rest.len() >= 36
                && ends_at(36)
                && rest[..36].iter().enumerate().all(|(n, b)| if matches!(n, 8 | 13 | 18 | 23) { *b == b'-' } else { b.is_ascii_hexdigit() });
            if uuid {
                Some((36, Cow::Borrowed(&b"<uuid>"[..])))
            } else {
                (hex >= 32 && ends_at(hex)).then_some((hex, Cow::Borrowed(&b"<hex>"[..])))
            }
        } else {
            None
        };
        if let Some((len, mask)) = found {
            let out = out.get_or_insert_with(|| Vec::with_capacity(line.len()));
            out.extend_from_slice(&line[kept..i]);
            out.extend_from_slice(&mask);
            i += len;
            kept = i;
        } else {
            i += 1;
        }
    }
    match out {
        Some(mut out) => {
            out.extend_from_slice(&line[kept..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(line),
    }
}

/// One event's output (the fmt layer writes a formatted line at a time).
pub struct Writer<'a>(MutexGuard<'a, Inner>);

impl Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let inner = &mut *self.0;
        let line = scrub(buf);
        if inner.len + line.len() as u64 > MAX {
            inner.rotate();
        }
        if let Some(f) = inner.file.as_mut() {
            f.write_all(&line)?;
            inner.len += line.len() as u64;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.file.as_mut().map_or(Ok(()), |f| f.flush())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFile {
    type Writer = Writer<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Writer(self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Logs panics (location, backtrace) before the default hook runs. Not the message: it can quote the
/// string being handled, such as a message's text. The default hook still says it on the console.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let at = info.location().map(ToString::to_string).unwrap_or_default();
        tracing::error!("panic on thread {thread} at {at}\n{}", std::backtrace::Backtrace::force_capture());
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn scrub_masks_identifiers_only() {
        let s = |t: &str| String::from_utf8(scrub(t.as_bytes()).into_owned()).unwrap();
        let id = "1a2b3c4d-1a2b-4c3d-8e9f-001122334455";
        assert_eq!(s(&format!("receipt from {id}.1 for [5]")), "receipt from <uuid>.1 for [5]");
        assert_eq!(s(&format!("c:{id}")), "c:<uuid>");
        assert_eq!(s(&format!("key=g:{}!", "ab".repeat(32))), "key=g:<hex>!");
        assert_eq!(s("to +821012345678, ok"), "to <phone>, ok");
        assert_eq!(
            s("error sending request for url (https://cdn3.example.org/upload/attachments/aB3x?sig=Zz9), retry"),
            "error sending request for url (https://cdn3.example.org/<path>), retry"
        );
        assert_eq!(s("at http://example.org:8080/a/b c"), "at http://example.org:8080/<path> c");
        // what logs are made of stays
        for keep in [
            "2026-11-05T23:22:48.669398Z DEBUG sigwin::backend: mem: rust heap 4.8MB (peak 19.1MB)",
            "send 1762383768123 failed: HTTP 400 +09:00",
            "index.crates.io-1949cf8c6b5b557f\\euclid-0.22.13\\src\\point.rs:12:5",
            "한글은 그대로 abcdef",
            "connecting to https://chat.signal.org",
            &"a".repeat(31),
        ] {
            assert!(matches!(scrub(keep.as_bytes()), Cow::Borrowed(_)), "{keep}");
        }
    }

    #[test]
    fn rotates_past_the_limit() {
        let data = std::env::temp_dir().join(format!("sigwin-logfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let log = LogFile::open(&data).unwrap();
        let line = vec![b'x'; 1000];
        for _ in 0..(MAX / 1000 + 10) {
            log.make_writer().write_all(&line).unwrap();
        }
        let cur = std::fs::metadata(dir(&data).join("sigwin.log")).unwrap().len();
        let old = std::fs::metadata(dir(&data).join("sigwin.log.1")).unwrap().len();
        assert!(cur > 0 && cur <= MAX, "{cur}");
        assert!(old <= MAX && old > MAX - 1000, "{old}");
        drop(log);
        let _ = std::fs::remove_dir_all(&data);
    }
}
