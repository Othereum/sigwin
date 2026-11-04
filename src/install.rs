//! Self-install and uninstall, no admin rights and no installer.
//!
//! Run from anywhere else, the exe copies itself to `%LOCALAPPDATA%\Sigwin\sigwin.exe`, adds a
//! Start menu shortcut and an entry under "Installed apps" (HKCU Uninstall key), then starts
//! the installed copy and exits. One fixed path keeps the shortcut, the updater and startup
//! registration pointing at the same file. A newer installed version is never overwritten
//! (its DB may already be migrated). `--uninstall` (what "Installed apps" runs) removes all of
//! it; the account data only if the user says so.
//!
//! Skipped for dev builds (built without `SIGWIN_RELEASE`, see build.rs).

use std::path::Path;

/// Returns false when this process should exit (installed and handed over, or failed).
pub fn run(data_dir: &Path) -> bool {
    #[cfg(windows)]
    return win::run(data_dir);
    #[cfg(not(windows))]
    {
        let _ = data_dir;
        true
    }
}

/// The running exe when it is the installed copy (the only one that updates itself).
pub fn installed_self() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    return win::installed_self();
    #[cfg(not(windows))]
    None
}

/// Built without `SIGWIN_RELEASE` (`sigwin_dev`, see build.rs): any local build, wherever its
/// exe is run from. Dev builds never install or update themselves.
pub const fn is_dev_build() -> bool {
    cfg!(sigwin_dev)
}

/// Modal error box with no owner window (before the app window exists).
pub fn error(text: &str) {
    #[cfg(windows)]
    win::error(text);
    #[cfg(not(windows))]
    let _ = text;
}

/// `sigwin.exe --uninstall`.
pub fn uninstall(data_dir: &Path) {
    #[cfg(windows)]
    win::uninstall(data_dir);
    #[cfg(not(windows))]
    let _ = data_dir;
}

/// "1.2.3" -> [1, 2, 3]; a pre-release suffix is ignored.
pub fn version(v: &str) -> Vec<u64> {
    v.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
}

#[cfg(windows)]
mod win {
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Context};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDCANCEL, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK,
        MB_YESNO, MB_YESNOCANCEL,
    };

    use crate::reg;

    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    /// Where everything goes. Tests use their own.
    pub struct Layout {
        pub home: PathBuf,
        pub exe: PathBuf,
        pub shortcut: PathBuf,
        /// registry names: Uninstall key, AppUserModelID, Run value
        pub id: String,
    }

    impl Layout {
        fn user() -> Option<Layout> {
            let home = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Sigwin");
            let programs = PathBuf::from(std::env::var_os("APPDATA")?).join(r"Microsoft\Windows\Start Menu\Programs");
            Some(Layout {
                exe: home.join("sigwin.exe"),
                home,
                shortcut: programs.join("Sigwin.lnk"),
                id: crate::toast::APP_ID.into(),
            })
        }

        fn old_exe(&self) -> PathBuf {
            self.home.join("sigwin.exe.old")
        }

        fn uninstall_key(&self) -> String {
            format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{}", self.id)
        }
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
            _ => false,
        }
    }

    pub fn installed_self() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let lay = Layout::user()?;
        (!super::is_dev_build() && same_path(&exe, &lay.exe)).then_some(lay.exe)
    }

    pub fn run(data_dir: &Path) -> bool {
        if super::is_dev_build() {
            return true;
        }
        let Ok(exe) = std::env::current_exe() else { return true };
        let Some(lay) = Layout::user() else { return true };
        if same_path(&exe, &lay.exe) {
            // left by an upgrade that replaced us while we ran
            let _ = fs::remove_file(lay.old_exe());
            // DisplayVersion follows updates that swap the exe in place
            if let Err(e) = register(&lay) {
                tracing::warn!("install: registering: {e:#}");
            }
            return true;
        }
        match install_from(&lay, &exe, data_dir) {
            Ok(Step::Cancel) => {}
            Ok(Step::Start) => {
                if let Err(e) = std::process::Command::new(&lay.exe).current_dir(&lay.home).spawn() {
                    message(&format!("설치한 Sigwin을 실행하지 못했습니다.\n\n{e}"), MB_OK | MB_ICONERROR);
                }
            }
            Err(e) => {
                message(&format!("Sigwin을 설치하지 못했습니다.\n\n{e:#}"), MB_OK | MB_ICONERROR);
            }
        }
        false
    }

    /// What `run` does next.
    enum Step {
        /// start the installed copy, then exit
        Start,
        /// exit
        Cancel,
    }

    fn install_from(lay: &Layout, src: &Path, data_dir: &Path) -> anyhow::Result<Step> {
        let installed = reg::get_str(&lay.uninstall_key(), "DisplayVersion");
        if lay.exe.exists() && installed.as_deref().is_some_and(|v| super::version(v) >= super::version(VERSION)) {
            tracing::info!("install: {} already installed ({installed:?}), starting it", lay.exe.display());
            return Ok(Step::Start);
        }
        let what = if installed.is_some() { "업데이트" } else { "설치" };
        let answer = message(
            &format!(
                "Sigwin {VERSION}을(를) {what}할까요?\n\n\
                 {what}하고 시작 메뉴에 등록합니다."
            ),
            MB_YESNO | MB_ICONQUESTION,
        );
        if answer != IDYES {
            return Ok(Step::Cancel);
        }
        if !crate::instance::quit_running(data_dir) {
            bail!("실행 중인 Sigwin이 종료되지 않았습니다. Sigwin을 닫은 뒤 다시 시도하세요.");
        }
        place(lay, src)?;
        register(lay)?;
        if let Err(e) = shortcut(lay) {
            tracing::warn!("install: shortcut: {e:#}");
        }
        tracing::info!("install: installed {VERSION} to {}", lay.exe.display());
        Ok(Step::Start)
    }

    /// Copies `src` to `lay.exe`. A running exe can't be overwritten but can be renamed.
    pub fn place(lay: &Layout, src: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(&lay.home).with_context(|| format!("{}", lay.home.display()))?;
        let old = lay.old_exe();
        let moved = lay.exe.exists() && {
            let _ = fs::remove_file(&old);
            fs::rename(&lay.exe, &old).context("moving the old version aside")?;
            true
        };
        if let Err(e) = fs::copy(src, &lay.exe) {
            if moved {
                let _ = fs::rename(&old, &lay.exe);
            }
            return Err(e).context("copying");
        }
        if moved {
            // fails while the old one still runs; cleaned up on the next start
            let _ = fs::remove_file(&old);
        }
        Ok(())
    }

    /// "Installed apps" entry.
    pub fn register(lay: &Layout) -> anyhow::Result<()> {
        let key = lay.uninstall_key();
        let exe = lay.exe.to_string_lossy();
        reg::set_str(&key, "DisplayName", "Sigwin")?;
        reg::set_str(&key, "DisplayVersion", VERSION)?;
        reg::set_str(&key, "Publisher", "Othereum")?;
        reg::set_str(&key, "DisplayIcon", &format!("{exe},0"))?;
        reg::set_str(&key, "UninstallString", &format!("\"{exe}\" --uninstall"))?;
        reg::set_str(&key, "InstallLocation", &lay.home.to_string_lossy())?;
        reg::set_str(&key, "URLInfoAbout", "https://github.com/Othereum/sigwin")?;
        reg::set_dword(&key, "NoModify", 1)?;
        reg::set_dword(&key, "NoRepair", 1)?;
        let kb = fs::metadata(&lay.exe).map(|m| m.len() / 1024).unwrap_or(0);
        reg::set_dword(&key, "EstimatedSize", kb as u32)?;
        Ok(())
    }

    pub fn shortcut(lay: &Layout) -> anyhow::Result<()> {
        use windows::core::{Interface, HSTRING};
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        if let Some(dir) = lay.shortcut.parent() {
            fs::create_dir_all(dir)?;
        }
        // SAFETY: plain COM calls on this thread; S_FALSE (already initialized) is fine
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(lay.exe.as_os_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(lay.home.as_os_str()))?;
            link.SetDescription(&HSTRING::from("Sigwin"))?;
            link.cast::<IPersistFile>()?.Save(&HSTRING::from(lay.shortcut.as_os_str()), true)?;
        }
        Ok(())
    }

    /// Everything the app put outside its data, plus the data when `delete_data`. `self_exe` is
    /// the running exe: it can only schedule its own deletion.
    pub fn remove(lay: &Layout, data_dir: &Path, delete_data: bool, self_exe: &Path) -> anyhow::Result<()> {
        reg::delete_tree(&lay.uninstall_key())?;
        reg::delete_tree(&crate::toast::aumid_key(&lay.id))?;
        reg::delete_value(RUN_KEY, &lay.id)?;
        reg::delete_value(APPROVED_KEY, &lay.id)?;
        if lay.shortcut.exists() {
            fs::remove_file(&lay.shortcut).context("removing the shortcut")?;
        }
        if delete_data && data_dir.exists() {
            fs::remove_dir_all(data_dir).context("removing the data")?;
        }
        let running_installed = same_path(self_exe, &lay.exe);
        if running_installed {
            // relocated out of `home` so the folder itself can go
            self_replace::self_delete_outside_path(&lay.home).context("scheduling self-deletion")?;
        }
        // the rest of `home`: the exe, its .old, the toast icon; the data unless kept
        if let Ok(entries) = fs::read_dir(&lay.home) {
            for e in entries.flatten() {
                let p = e.path();
                if same_path(&p, data_dir) {
                    continue;
                }
                let r = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
                if let Err(err) = r {
                    tracing::warn!("uninstall: {}: {err}", p.display());
                }
            }
        }
        let _ = fs::remove_dir(&lay.home); // only if empty
        Ok(())
    }

    pub fn uninstall(data_dir: &Path) {
        let Some(lay) = Layout::user() else { return };
        let Some(delete_data) = ask_uninstall() else { return };
        if !crate::instance::quit_running(data_dir) {
            message("실행 중인 Sigwin이 종료되지 않아 제거하지 못했습니다.\nSigwin을 닫은 뒤 다시 시도하세요.", MB_OK | MB_ICONERROR);
            return;
        }
        let self_exe = std::env::current_exe().unwrap_or_default();
        if let Err(e) = remove(&lay, data_dir, delete_data, &self_exe) {
            message(&format!("제거하는 중 문제가 생겼습니다.\n\n{e:#}"), MB_OK | MB_ICONERROR);
            return;
        }
        if delete_data {
            message(
                "Sigwin을 제거했습니다.\n\n휴대폰 Signal 앱의 설정 > 연결된 기기에서도 Sigwin 연결을 해제하세요.",
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    /// Some(delete the data too), or None for cancel.
    fn ask_uninstall() -> Option<bool> {
        const KEEP: i32 = 101;
        const WIPE: i32 = 102;
        let links = [
            (KEEP, "앱만 제거\n대화 기록과 계정은 남겨 둡니다. 다시 설치하면 이어서 쓸 수 있습니다."),
            (WIPE, "앱과 데이터 모두 삭제\n이 PC의 대화 기록과 계정 정보도 지웁니다."),
        ];
        match task_dialog("Sigwin을 제거합니다", &links) {
            Some(choice) => choice.map(|id| id == WIPE),
            None => {
                // no common-controls v6: plain yes/no/cancel
                let answer = message(
                    "Sigwin을 제거합니다.\n\n대화 기록과 계정 정보도 이 PC에서 삭제할까요?\n\n\
                     예: 모두 삭제\n아니요: 앱만 제거하고 데이터는 남겨 둡니다.",
                    MB_YESNOCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2,
                );
                (answer != IDCANCEL && answer != 0).then_some(answer == IDYES)
            }
        }
    }

    /// Command-link task dialog with a Cancel button; the first link is the default. Returns None
    /// when `TaskDialogIndirect` is unavailable, Some(None) when cancelled.
    fn task_dialog(instruction: &str, links: &[(i32, &str)]) -> Option<Option<i32>> {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        use windows_sys::Win32::UI::Controls::{
            TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDCBF_CANCEL_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
            TDF_USE_COMMAND_LINKS,
        };
        type Fn = unsafe extern "system" fn(*const TASKDIALOGCONFIG, *mut i32, *mut i32, *mut i32) -> i32;
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let lib = w("comctl32.dll");
        // SAFETY: the symbol has the documented TaskDialogIndirect signature; all pointers outlive the call
        unsafe {
            let h = LoadLibraryW(lib.as_ptr());
            if h.is_null() {
                return None;
            }
            let f: Fn = std::mem::transmute(GetProcAddress(h, c"TaskDialogIndirect".as_ptr().cast())?);
            let texts: Vec<Vec<u16>> = links.iter().map(|l| w(l.1)).collect();
            let buttons: Vec<TASKDIALOG_BUTTON> = links
                .iter()
                .zip(&texts)
                .map(|(l, t)| TASKDIALOG_BUTTON { nButtonID: l.0, pszButtonText: t.as_ptr() })
                .collect();
            let (title, head) = (w("Sigwin"), w(instruction));
            let mut cfg: TASKDIALOGCONFIG = std::mem::zeroed();
            cfg.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
            cfg.dwFlags = TDF_USE_COMMAND_LINKS | TDF_ALLOW_DIALOG_CANCELLATION;
            cfg.dwCommonButtons = TDCBF_CANCEL_BUTTON;
            cfg.pszWindowTitle = title.as_ptr();
            cfg.pszMainInstruction = head.as_ptr();
            cfg.cButtons = buttons.len() as u32;
            cfg.pButtons = buttons.as_ptr();
            cfg.nDefaultButton = links[0].0;
            let mut picked = 0;
            if f(&cfg, &mut picked, std::ptr::null_mut(), std::ptr::null_mut()) < 0 {
                return None;
            }
            Some(links.iter().any(|l| l.0 == picked).then_some(picked))
        }
    }

    pub fn error(text: &str) {
        message(text, MB_OK | MB_ICONERROR);
    }

    fn message(text: &str, style: u32) -> i32 {
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (t, c) = (w(text), w("Sigwin"));
        // SAFETY: NUL-terminated strings; no owner window
        unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), style) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn layout(tag: &str) -> Layout {
            let root = std::env::temp_dir().join(format!("sigwin-install-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let home = root.join("home");
            Layout {
                exe: home.join("sigwin.exe"),
                shortcut: root.join("Programs").join("Sigwin test.lnk"),
                home,
                id: format!("SigwinTest-{tag}-{}", std::process::id()),
            }
        }

        #[test]
        fn install_upgrade_uninstall_keeping_data() {
            let lay = layout("keep");
            let src = lay.home.parent().unwrap().join("download.exe");
            fs::create_dir_all(src.parent().unwrap()).unwrap();
            fs::write(&src, b"v1").unwrap();
            place(&lay, &src).unwrap();
            register(&lay).unwrap();
            shortcut(&lay).unwrap();
            assert!(lay.shortcut.exists());
            assert_eq!(reg::get_str(&lay.uninstall_key(), "DisplayVersion").as_deref(), Some(VERSION));
            let uninst = reg::get_str(&lay.uninstall_key(), "UninstallString").unwrap();
            assert!(uninst.ends_with("sigwin.exe\" --uninstall"), "{uninst}");

            // upgrade while the old file is open (stands in for a running exe)
            let held = fs::File::open(&lay.exe).unwrap();
            fs::write(&src, b"v2").unwrap();
            place(&lay, &src).unwrap();
            drop(held);
            assert_eq!(fs::read(&lay.exe).unwrap(), b"v2");

            let data = lay.home.join("data");
            fs::create_dir_all(&data).unwrap();
            fs::write(data.join("db"), b"x").unwrap();
            fs::write(lay.home.join("icon.png"), b"png").unwrap();
            reg::set_str(&crate::toast::aumid_key(&lay.id), "DisplayName", "t").unwrap();
            remove(&lay, &data, false, &std::env::current_exe().unwrap()).unwrap();
            assert!(reg::get_str(&lay.uninstall_key(), "DisplayVersion").is_none());
            assert!(reg::get_str(&crate::toast::aumid_key(&lay.id), "DisplayName").is_none());
            assert!(!lay.shortcut.exists() && !lay.exe.exists() && !lay.old_exe().exists());
            assert!(!lay.home.join("icon.png").exists());
            assert!(data.join("db").exists());
            let _ = fs::remove_dir_all(lay.home.parent().unwrap());
        }

        #[test]
        fn uninstall_deleting_data_removes_home() {
            let lay = layout("all");
            let src = lay.home.parent().unwrap().join("download.exe");
            fs::create_dir_all(src.parent().unwrap()).unwrap();
            fs::write(&src, b"v1").unwrap();
            place(&lay, &src).unwrap();
            register(&lay).unwrap();
            let data = lay.home.join("data");
            fs::create_dir_all(data.join("thumbs")).unwrap();
            remove(&lay, &data, true, &std::env::current_exe().unwrap()).unwrap();
            assert!(!lay.home.exists());
            let _ = fs::remove_dir_all(lay.home.parent().unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn versions_compare_numerically() {
        assert!(version("0.10.0") > version("0.9.3"));
        assert!(version("1.0.0") >= version("1.0.0-beta"));
        assert!(version("0.1.0") < version("0.1.1"));
        assert_eq!(version("garbage"), vec![0]);
    }
}
