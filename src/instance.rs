//! One process per data directory. Two instances would share one Signal device and DB.
//!
//! The first instance owns a named auto-reset event (`Local\Sigwin-<hash of data dir>`) and
//! waits on it; a second launch finds the event, signals it (the first then comes to the
//! front) and exits. The kernel object dies with its last handle, so a crash leaves no stale lock.

use std::path::Path;

/// Held by the first instance for the whole process.
pub struct Instance {
    #[cfg(windows)]
    event: isize,
}

/// `Some` for the first instance; `None` when another one owns `data_dir` and was asked to
/// come to the front.
pub fn claim(data_dir: &Path) -> Option<Instance> {
    imp::claim(&name(data_dir))
}

impl Instance {
    /// Calls `f` on a background thread each time another launch asks for the window.
    pub fn on_activate(self, f: impl Fn() + Send + 'static) {
        imp::wait(self, f);
    }
}

/// Stable across builds (an old and a new version must agree during an update), so no
/// `DefaultHasher`. Case-insensitive like Windows paths.
fn name(data_dir: &Path) -> String {
    let path = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let key = path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    // FNV-1a 64
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("Local\\Sigwin-{hash:016x}")
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    use super::Instance;

    pub fn claim(name: &str) -> Option<Instance> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name; the handle is closed below or kept for the process
        unsafe {
            let event = CreateEventW(std::ptr::null(), 0, 0, wide.as_ptr());
            if event.is_null() {
                // can't tell: running without the guard beats not running
                tracing::warn!("instance: CreateEventW failed ({})", GetLastError());
                return Some(Instance { event: 0 });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // we are the foreground launch: let the first instance take the foreground
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(event);
                CloseHandle(event);
                return None;
            }
            Some(Instance { event: event as isize })
        }
    }

    pub fn wait(inst: Instance, f: impl Fn() + Send + 'static) {
        if inst.event == 0 {
            return;
        }
        let event = inst.event;
        std::thread::Builder::new()
            .name("instance".into())
            .spawn(move || loop {
                // SAFETY: the handle stays open for the process lifetime
                if unsafe { WaitForSingleObject(event as _, INFINITE) } != WAIT_OBJECT_0 {
                    return;
                }
                f();
            })
            .ok();
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Instance;

    pub fn claim(_name: &str) -> Option<Instance> {
        Some(Instance {})
    }

    pub fn wait(_inst: Instance, _f: impl Fn() + Send + 'static) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_ignores_case_and_separators() {
        let a = name(Path::new(r"C:\Users\A\AppData\Local\Sigwin\data"));
        assert_eq!(a, name(Path::new(r"c:/users/a/appdata/local/sigwin/DATA/")));
        assert_ne!(a, name(Path::new(r"C:\scratch")));
        assert!(a.starts_with("Local\\Sigwin-") && !a["Local\\".len()..].contains('\\'));
    }

    #[cfg(windows)]
    #[test]
    fn second_claim_activates_first() {
        let dir = std::env::temp_dir().join(format!("sigwin-instance-test-{}", std::process::id()));
        let first = claim(&dir).expect("first claim");
        let (tx, rx) = std::sync::mpsc::channel();
        first.on_activate(move || tx.send(()).unwrap());
        assert!(claim(&dir).is_none());
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("activation");
        assert!(claim(&dir).is_none());
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("second activation");
    }
}
