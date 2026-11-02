//! Log file, so builds without a console leave a trace: `<data>\logs\sigwin.log`, rotated to
//! `sigwin.log.1` past 1MB (at most ~2MB on disk). Panics are logged too (`install_panic_hook`).
//! Message contents and link URLs stay at debug level, which the default filter leaves out.

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

/// One event's output (the fmt layer writes a formatted line at a time).
pub struct Writer<'a>(MutexGuard<'a, Inner>);

impl Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let inner = &mut *self.0;
        if inner.len + buf.len() as u64 > MAX {
            inner.rotate();
        }
        if let Some(f) = inner.file.as_mut() {
            f.write_all(buf)?;
            inner.len += buf.len() as u64;
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

/// Logs panics (message, location, backtrace) before the default hook runs.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        tracing::error!("panic on thread {thread}: {info}\n{}", std::backtrace::Backtrace::force_capture());
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

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
