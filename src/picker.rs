//! File picker run in a short-lived child process.
//!
//! The Windows common file dialog loads the shell, thumbnail providers and cloud-sync
//! extensions into the process (+28MB while open, ~10MB+ retained after closing).
//! Running it in a child keeps that out of the long-running app and keeps the UI responsive.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const ARG: &str = "--pick-files";
const ARG_SAVE: &str = "--save-file";
const ARG_FOLDER: &str = "--pick-folder";
/// Call first thing in `main`: returns true if this process was the picker child.
pub fn run_if_child() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let mode = match args.get(1).map(String::as_str) {
        Some(ARG) => ARG,
        Some(ARG_SAVE) => ARG_SAVE,
        Some(ARG_FOLDER) => ARG_FOLDER,
        _ => return false,
    };
    let owner = args.get(2).and_then(|s| s.parse::<isize>().ok()).unwrap_or(0);
    let mut dialog = rfd::FileDialog::new();
    if owner != 0 {
        dialog = dialog.set_parent(&Owner(owner));
    }
    let files: Vec<PathBuf> = match mode {
        ARG_SAVE => {
            let name = args.get(3).map(String::as_str).unwrap_or_default();
            dialog.set_title("저장").set_file_name(name).save_file().into_iter().collect()
        }
        ARG_FOLDER => dialog.set_title("저장할 폴더 선택").pick_folder().into_iter().collect(),
        _ => dialog.set_title("첨부할 파일 선택").pick_files().unwrap_or_default(),
    };
    let mut out = std::io::stdout().lock();
    for f in files {
        let _ = writeln!(out, "{}", f.to_string_lossy());
    }
    let _ = out.flush();
    true
}

/// Opens a save dialog owned by `owner` (HWND), suggesting `name`, and calls `done` with the
/// choice from a background thread.
pub fn spawn_save(owner: isize, name: String, done: impl FnOnce(Option<PathBuf>) + Send + 'static) {
    std::thread::spawn(move || {
        let file = (|| {
            let exe = std::env::current_exe().ok()?;
            let out = Command::new(exe)
                .arg(ARG_SAVE)
                .arg(owner.to_string())
                .arg(name)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            out.stdout.lines().map_while(Result::ok).find(|l| !l.is_empty()).map(PathBuf::from)
        })();
        reenable(owner);
        done(file);
    });
}

/// Opens a folder dialog owned by `owner` (HWND) and calls `done` with the choice from a
/// background thread.
pub fn spawn_folder(owner: isize, done: impl FnOnce(Option<PathBuf>) + Send + 'static) {
    std::thread::spawn(move || {
        let dir = (|| {
            let exe = std::env::current_exe().ok()?;
            let out = Command::new(exe)
                .arg(ARG_FOLDER)
                .arg(owner.to_string())
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            out.stdout.lines().map_while(Result::ok).find(|l| !l.is_empty()).map(PathBuf::from)
        })();
        reenable(owner);
        done(dir);
    });
}

/// Opens the picker owned by `owner` (HWND) and calls `done` with the selection from a
/// background thread.
pub fn spawn(owner: isize, done: impl FnOnce(Vec<PathBuf>) + Send + 'static) {
    std::thread::spawn(move || {
        let files = (|| {
            let exe = std::env::current_exe().ok()?;
            let out = Command::new(exe)
                .arg(ARG)
                .arg(owner.to_string())
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            Some(out.stdout.lines().map_while(Result::ok).filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        })()
        .unwrap_or_default();
        reenable(owner);
        done(files);
    });
}

/// The child disables its owner while modal; make sure we are usable again even if it crashed.
fn reenable(owner: isize) {
    #[cfg(windows)]
    if owner != 0 {
        // SAFETY: EnableWindow on our own top-level window handle
        unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow(owner as _, 1);
        }
    }
}

struct Owner(isize);

impl raw_window_handle::HasWindowHandle for Owner {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let h = std::num::NonZeroIsize::new(self.0).ok_or(raw_window_handle::HandleError::Unavailable)?;
        let raw = raw_window_handle::RawWindowHandle::Win32(raw_window_handle::Win32WindowHandle::new(h));
        // SAFETY: the owner window outlives the dialog (the parent waits for this process)
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

impl raw_window_handle::HasDisplayHandle for Owner {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::windows())
    }
}
