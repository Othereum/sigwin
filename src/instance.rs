//! One process per data directory. Two instances would share one Signal device and DB.
//!
//! The first instance owns two named auto-reset events (`Local\Sigwin-<hash of data dir>` and
//! the same name with `-quit`) and waits on them. A second launch finds the first event,
//! signals it (the first comes to the front) and exits. The installer and uninstaller signal
//! the quit event to close a running app. The kernel objects die with their last handle, so a
//! crash leaves no stale lock.

use std::path::Path;

/// Held by the first instance for the whole process.
pub struct Instance {
    #[cfg(windows)]
    events: [isize; 2],
}

/// `Some` for the first instance; `None` when another one owns `data_dir` and was asked to
/// come to the front.
pub fn claim(data_dir: &Path) -> Option<Instance> {
    imp::claim(&name(data_dir))
}

/// Asks the app running on `data_dir` (if any) to quit and waits for it. False if it is
/// still running after the timeout.
pub fn quit_running(data_dir: &Path) -> bool {
    imp::quit_running(&name(data_dir))
}

impl Instance {
    /// Calls `activate` / `quit` on a background thread each time another process asks.
    pub fn serve(self, activate: impl Fn() + Send + 'static, quit: impl Fn() + Send + 'static) {
        imp::serve(self, activate, quit);
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
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, OpenEventW, SetEvent, WaitForMultipleObjects, EVENT_MODIFY_STATE, INFINITE,
        SYNCHRONIZATION_SYNCHRONIZE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    use super::Instance;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn quit_name(name: &str) -> String {
        format!("{name}-quit")
    }

    pub fn claim(name: &str) -> Option<Instance> {
        let (main, quit) = (w(name), w(&quit_name(name)));
        // SAFETY: NUL-terminated names; handles are closed below or kept for the process
        unsafe {
            let event = CreateEventW(std::ptr::null(), 0, 0, main.as_ptr());
            if event.is_null() {
                // can't tell: running without the guard beats not running
                tracing::warn!("instance: CreateEventW failed ({})", GetLastError());
                return Some(Instance { events: [0; 2] });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // we are the foreground launch: let the first instance take the foreground
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(event);
                CloseHandle(event);
                return None;
            }
            let quit = CreateEventW(std::ptr::null(), 0, 0, quit.as_ptr());
            Some(Instance { events: [event as isize, quit as isize] })
        }
    }

    pub fn serve(inst: Instance, activate: impl Fn() + Send + 'static, quit: impl Fn() + Send + 'static) {
        let events = inst.events;
        if events.contains(&0) {
            return;
        }
        std::thread::Builder::new()
            .name("instance".into())
            .spawn(move || loop {
                let handles = events.map(|h| h as HANDLE);
                // SAFETY: the handles stay open for the process lifetime
                match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
                    WAIT_OBJECT_0 => activate(),
                    r if r == WAIT_OBJECT_0 + 1 => quit(),
                    _ => return,
                }
            })
            .ok();
    }

    fn open(name: &str, access: u32) -> HANDLE {
        let n = w(name);
        // SAFETY: NUL-terminated name
        unsafe { OpenEventW(access, 0, n.as_ptr()) }
    }

    pub fn quit_running(name: &str) -> bool {
        let quit = open(&quit_name(name), EVENT_MODIFY_STATE);
        if quit.is_null() {
            return true;
        }
        // SAFETY: a handle we just opened
        unsafe {
            SetEvent(quit);
            CloseHandle(quit);
        }
        // the main event goes away when the process has exited
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let h = open(name, SYNCHRONIZATION_SYNCHRONIZE);
            if h.is_null() {
                return true;
            }
            // SAFETY: a handle we just opened
            unsafe { CloseHandle(h) };
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Instance;

    pub fn claim(_name: &str) -> Option<Instance> {
        Some(Instance {})
    }

    pub fn serve(_inst: Instance, _activate: impl Fn() + Send + 'static, _quit: impl Fn() + Send + 'static) {}

    pub fn quit_running(_name: &str) -> bool {
        true
    }
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
        let quit_tx = tx.clone();
        first.serve(move || tx.send("activate").unwrap(), move || quit_tx.send("quit").unwrap());
        assert!(claim(&dir).is_none());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok("activate"));
        assert!(claim(&dir).is_none());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok("activate"));
        // the "process" never exits in a test, so quit_running times out; only check the signal
        std::thread::spawn(move || quit_running(&dir));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok("quit"));
    }

    #[cfg(windows)]
    #[test]
    fn quit_running_without_app_returns_at_once() {
        let dir = std::env::temp_dir().join(format!("sigwin-instance-none-{}", std::process::id()));
        assert!(quit_running(&dir));
    }
}
