//! User settings (`settings.json` in the data folder). Owned by the UI thread; the backend
//! only reads the notification mode through an atomic. The toast sound is Windows' per-app
//! notification setting, not ours.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub notify: bool,
    /// 0 = sender and text, 1 = sender only, 2 = neither
    pub preview: u8,
    pub badge: bool,
    /// 0 = follow the system, 1 = light, 2 = dark
    pub theme: u8,
    /// UI size in percent, on top of the monitor's scaling
    pub zoom: u32,
    /// Enter sends (Shift+Enter: new line); otherwise Ctrl+Enter sends and Enter is a new line
    pub enter_sends: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { notify: true, preview: 0, badge: true, theme: 0, zoom: 100, enter_sends: true }
    }
}

pub const ZOOMS: [u32; 5] = [90, 100, 110, 125, 150];

impl Settings {
    pub fn load(path: &Path) -> Self {
        let mut s: Self = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        s.preview = s.preview.min(2);
        s.theme = s.theme.min(2);
        if !ZOOMS.contains(&s.zoom) {
            s.zoom = 100;
        }
        s
    }

    pub fn save(&self, path: &Path) {
        let tmp = path.with_extension("json.tmp");
        let res = serde_json::to_vec_pretty(self)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(std::fs::write(&tmp, b)?))
            .and_then(|_| Ok(std::fs::rename(&tmp, path)?));
        if let Err(e) = res {
            tracing::warn!("saving settings: {e:#}");
        }
    }

    /// Makes the notification choice visible to the backend thread.
    pub fn publish(&self) {
        NOTIFY.store(if self.notify { self.preview } else { OFF }, Ordering::Relaxed);    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

const OFF: u8 = 255;
static NOTIFY: AtomicU8 = AtomicU8::new(0);

pub fn notify_enabled() -> bool {
    NOTIFY.load(Ordering::Relaxed) != OFF
}

/// What toasts show (0 = sender and text, 1 = sender only, 2 = neither); None when they're off.
pub fn preview() -> Option<u8> {
    match NOTIFY.load(Ordering::Relaxed) {
        OFF => None,
        p => Some(p),
    }
}

/// Whether Windows apps are set to dark mode right now (for switching back to "system"
/// at runtime; at startup Slint follows the system by itself).
pub fn system_dark() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let key = w(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        let name = w("AppsUseLightTheme");
        let mut v: u32 = 1;
        let mut len = 4u32;
        // SAFETY: NUL-terminated strings; the output buffer is a u32 of `len` bytes
        let rc = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut v as *mut u32).cast(), &mut len)
        };
        rc == 0 && v == 0
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_follows_settings() {
        Settings { preview: 1, ..Default::default() }.publish();
        assert_eq!(preview(), Some(1));
        Settings { notify: false, ..Default::default() }.publish();
        assert_eq!(preview(), None);
        assert!(!notify_enabled());
        Settings::default().publish();
        assert_eq!(preview(), Some(0));
    }

    #[test]
    fn load_clamps_bad_values() {
        let dir = std::env::temp_dir().join(format!("sigwin-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = path(&dir);
        std::fs::write(&p, r#"{"zoom": 333, "theme": 9, "notify": false}"#).unwrap();
        let s = Settings::load(&p);
        assert_eq!((s.zoom, s.theme, s.notify, s.badge), (100, 2, false, true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
