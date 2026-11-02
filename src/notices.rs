//! Third-party license notices, embedded in the exe (`build-support/about.toml`).

use std::path::Path;

/// What the settings page's "보기" button asks `Links.open` for.
pub const URL: &str = "sigwin-licenses:";

static HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/licenses.html"));

/// Writes the notices next to the other plain files handed to outside apps and opens them.
pub fn open(data_dir: &Path) {
    let dir = data_dir.join("open");
    let path = dir.join("licenses.html");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, HTML)) {
        tracing::warn!("write licenses: {e}");
        return;
    }
    if crate::headless::active() {
        tracing::info!("licenses written (headless: not opened)");
        return;
    }
    if let Err(e) = std::process::Command::new("explorer").arg(&path).spawn() {
        tracing::warn!("open licenses: {e}");
    }
}
