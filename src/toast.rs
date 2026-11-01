//! Notifications. On Windows we drive the WinRT toast directly (the crate notify-rust uses)
//! to get the click: it opens the toast's thread while we're running. Without our own
//! AppUserModelID toasts said "Windows PowerShell"; we register one under HKCU (name + icon),
//! no installer or shortcut needed. No COM activator: clicks after exit do nothing.

use std::sync::OnceLock;

pub const APP_ID: &str = "Sigwin";

type OnClick = Box<dyn Fn(String) + Send + Sync>;
static ON_CLICK: OnceLock<OnClick> = OnceLock::new();

/// What a click on a toast does with its thread key (called on a WinRT thread).
pub fn on_click(f: impl Fn(String) + Send + Sync + 'static) {
    let _ = ON_CLICK.set(Box::new(f));
}

/// Shows a toast (blocking: run it off the UI and backend threads). `key` is the thread a
/// click opens.
#[cfg(windows)]
pub fn show(title: &str, body: &str, key: Option<String>) -> anyhow::Result<()> {
    let mut t = tauri_winrt_notification::Toast::new(APP_ID).title(title).text1(body);
    if let Some(key) = key {
        t = t.on_activated(move |_| {
            if let Some(f) = ON_CLICK.get() {
                f(key.clone());
            }
            Ok(())
        });
    }
    t.show()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn show(title: &str, body: &str, _key: Option<String>) -> anyhow::Result<()> {
    notify_rust::Notification::new().summary(title).body(body).show()?;
    Ok(())
}

/// Writes `HKCU\Software\Classes\AppUserModelId\Sigwin` (DisplayName, IconUri) and the icon
/// it points to. Cheap; done on every start so a moved exe or data folder can't go stale.
pub fn register() {
    #[cfg(windows)]
    if let Err(e) = win::register() {
        tracing::warn!("toast app id: {e:#}");
    }
}

#[cfg(windows)]
mod win {
    use std::path::PathBuf;

    use anyhow::{bail, Context};
    use windows_sys::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

    const ICON: &[u8] = include_bytes!("../assets/icon.png");

    pub fn register() -> anyhow::Result<()> {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA")?).join("Sigwin");
        std::fs::create_dir_all(&dir)?;
        let icon = dir.join("icon.png");
        if std::fs::metadata(&icon).map(|m| m.len() != ICON.len() as u64).unwrap_or(true) {
            std::fs::write(&icon, ICON).context("writing icon")?;
        }
        let key = format!(r"Software\Classes\AppUserModelId\{}", super::APP_ID);
        set(&key, "DisplayName", "Sigwin")?;
        set(&key, "IconUri", &icon.to_string_lossy())?;
        Ok(())
    }

    fn set(key: &str, name: &str, value: &str) -> anyhow::Result<()> {
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (k, n, v) = (w(key), w(name), w(value));
        // SAFETY: all pointers are NUL-terminated UTF-16 buffers alive for the call.
        let rc = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32)
        };
        if rc != 0 {
            bail!("RegSetKeyValueW {name}: {rc}");
        }
        Ok(())
    }
}
