//! Hands a file or a web link to the app Windows has for it.

use std::ffi::OsStr;

/// Opens `target` (a path, or an http(s) URL) with its default app, off the calling thread: the
/// shell may take a while. Straight to `ShellExecuteW`, never through `cmd`: names come from
/// other people (an attachment's file name), and cmd would run what follows a `&` in one.
pub fn open(target: &OsStr) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
        let file: Vec<u16> = target.encode_wide().chain(Some(0)).collect();
        std::thread::spawn(move || {
            // SAFETY: valid NUL-terminated UTF-16 strings that outlive the call
            unsafe {
                ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
            }
        });
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
}
