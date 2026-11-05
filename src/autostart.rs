//! Start with Windows: a value under HKCU `...\CurrentVersion\Run` pointing at the installed
//! exe. Only the installed copy registers itself (a dev build or a downloaded exe would leave
//! a path that moves or disappears). With "start hidden" the command carries `--background`:
//! the window isn't shown until the tray icon is used, so there is no flash.
//!
//! Task Manager's Startup tab keeps its own on/off flag under `StartupApproved\Run`; turning
//! the setting on clears that flag so the choice made here wins.

pub const BACKGROUND_FLAG: &str = "--background";

/// Registers (`on`) or removes the Run value. `reapprove` also clears a "disabled" mark set in
/// Task Manager (only when the user just switched the setting on).
pub fn apply(on: bool, hidden: bool, reapprove: bool) {
    #[cfg(windows)]
    win::apply(on, hidden, reapprove);
    #[cfg(not(windows))]
    let _ = (on, hidden, reapprove);
}

/// The Run command for `exe`.
fn command(exe: &std::path::Path, hidden: bool) -> String {
    let mut c = format!("\"{}\"", exe.display());
    if hidden {
        c.push(' ');
        c.push_str(BACKGROUND_FLAG);
    }
    c
}

#[cfg(windows)]
mod win {
    use crate::reg;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    pub fn apply(on: bool, hidden: bool, reapprove: bool) {
        let name = crate::toast::APP_ID;
        let res = if on {
            match crate::install::installed_self() {
                Some(exe) => reg::set_str(RUN_KEY, name, &super::command(&exe, hidden)).and_then(|_| {
                    if reapprove { reg::delete_value(APPROVED_KEY, name) } else { Ok(()) }
                }),
                None => {
                    tracing::info!("autostart: not the installed copy, nothing registered");
                    Ok(())
                }
            }
        } else {
            reg::delete_value(RUN_KEY, name)
        };
        if let Err(e) = res {
            tracing::warn!("autostart: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn command_quotes_path_and_adds_flag() {
        let exe = Path::new(r"C:\Users\a b\Sigwin\sigwin.exe");
        assert_eq!(super::command(exe, false), r#""C:\Users\a b\Sigwin\sigwin.exe""#);
        assert_eq!(super::command(exe, true), r#""C:\Users\a b\Sigwin\sigwin.exe" --background"#);
    }
}
