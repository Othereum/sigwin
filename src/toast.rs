//! Notification identity. Without an AppUserModelID notify-rust borrows PowerShell's, so
//! toasts said "Windows PowerShell". We register our own under HKCU (name + icon); no
//! installer or shortcut needed.

pub const APP_ID: &str = "Sigwin";

pub fn notification() -> notify_rust::Notification {
    let mut n = notify_rust::Notification::new();
    #[cfg(windows)]
    n.app_id(APP_ID);
    n
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
