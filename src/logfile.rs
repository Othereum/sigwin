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

/// Drops known false errors from dependencies, so the log file shows only real ones.
pub struct Noise;

/// libsignal-service logs this at ERROR for every group typing indicator: those are never
/// synced to our own devices, which is expected.
const NOISE: &[(&str, &str)] = &[("libsignal_service::sender", "could not create sync message from a group message")];

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Noise {
    fn event_enabled(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) -> bool {
        let meta = event.metadata();
        if *meta.level() != tracing::Level::ERROR || !NOISE.iter().any(|(t, _)| *t == meta.target()) {
            return true;
        }
        struct Msg(String);
        impl tracing::field::Visit for Msg {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0 = format!("{v:?}");
                }
            }
        }
        let mut m = Msg(String::new());
        event.record(&mut m);
        !NOISE.iter().any(|(t, msg)| *t == meta.target() && m.0 == *msg)
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
    fn drops_only_known_noise() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::prelude::*;
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let w = buf.clone();
        let sub = tracing_subscriber::registry()
            .with(Noise)
            .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(move || w.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::error!(target: "libsignal_service::sender", "could not create sync message from a group message");
            tracing::error!(target: "libsignal_service::sender", "failed to send a synchronization message");
            tracing::error!("could not create sync message from a group message");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.contains("failed to send"), "{out}");
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
