//! Toasts like Signal Desktop's (`app/WindowsNotifications.main.ts`, `renderWindowsToast`):
//! one at a time (each show clears the previous one), sender photo cropped round, IM sound.
//! A click raises the window and opens the toast's chat while we're running; there is no COM
//! activator, so toasts are cleared on exit (and at startup, after a crash).
//!
//! Without our own AppUserModelID toasts said "Windows PowerShell": we register one under HKCU
//! (name + icon), no installer or shortcut needed.
//!
//! WinRT calls run on one worker thread, in order (a clear never overtakes a show).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};

pub const APP_ID: &str = "Sigwin";

/// Toast picture size in pixels (Windows scales it to the app logo slot).
const PICTURE_PX: u32 = 128;

pub enum Picture {
    /// photo bytes as stored (any format the thumb decoder reads)
    Photo(Vec<u8>),
    /// no photo: the initial on the chat's color, like the thread list
    Initial { text: String, rgb: u32 },
}

pub struct Toast {
    pub title: String,
    pub body: String,
    pub picture: Option<Picture>,
    /// chat a click opens
    pub key: String,
    /// sent timestamp of the message a click shows
    pub msg: Option<u64>,
}

/// Where a click leads (None: the toast was already replaced, just raise the window).
pub struct Click {
    pub key: String,
    pub msg: Option<u64>,
}

type OnClick = Box<dyn Fn(Option<Click>) + Send + Sync>;
static ON_CLICK: OnceLock<OnClick> = OnceLock::new();
/// folder for the toast pictures (plain PNG: Windows only loads files); emptied with `open/`
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// the shown toast's launch token and target, like Signal's `#tokenData`
static LAST: Mutex<Option<(String, Click)>> = Mutex::new(None);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

enum Op {
    Show(Toast, String),
    Clear,
}

/// Folder for toast pictures and the click handler (called on a WinRT thread).
pub fn init(dir: PathBuf, on_click: impl Fn(Option<Click>) + Send + Sync + 'static) {
    let _ = DIR.set(dir);
    let _ = ON_CLICK.set(Box::new(on_click));
}

/// Replaces whatever toast is up with this one.
pub fn show(t: Toast) {
    let token = format!("{}-{}", std::process::id(), NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), Click { key: t.key.clone(), msg: t.msg }));
    send(Op::Show(t, token));
}

/// Removes our toasts from the screen and the notification center.
pub fn clear() {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).take();
    send(Op::Clear);
}

/// `clear` right now, on this thread: for exit, when the worker may not get to run.
pub fn clear_now() {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).take();
    #[cfg(windows)]
    if let Err(e) = win::clear() {
        tracing::debug!("clear toasts: {e}");
    }
}

fn send(op: Op) {
    static TX: OnceLock<mpsc::Sender<Op>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Op>();
        let spawned = std::thread::Builder::new().name("toast".into()).stack_size(256 * 1024).spawn(move || {
            for op in rx {
                run(op);
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("toast thread: {e}");
        }
        tx
    });
    let _ = tx.send(op);
}

fn run(op: Op) {
    match op {
        Op::Show(t, token) => {
            let picture = t.picture.as_ref().and_then(|p| DIR.get().and_then(|d| write_picture(d, p)));
            let xml = toast_xml(&t.title, &t.body, picture.as_deref(), &token);
            #[cfg(windows)]
            if let Err(e) = win::show(&xml) {
                tracing::debug!("toast: {e}");
            }
            #[cfg(not(windows))]
            {
                let _ = xml;
                let _ = notify_rust::Notification::new().summary(&t.title).body(&t.body).show();
            }
        }
        Op::Clear => {
            #[cfg(windows)]
            if let Err(e) = win::clear() {
                tracing::debug!("clear toasts: {e}");
            }
        }
    }
}

/// A WinRT activation: map the launch token back to the chat (only the current toast's).
#[cfg_attr(not(windows), allow(dead_code))]
fn clicked(token: &str) {
    let hit = {
        let last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        last.as_ref().filter(|(t, _)| t == token).map(|(_, c)| Click { key: c.key.clone(), msg: c.msg })
    };
    tracing::debug!(token, found = hit.is_some(), "toast clicked");
    if let Some(f) = ON_CLICK.get() {
        f(hit);
    }
}

/// Writes the round picture as `toast-<hash>.png` (a new name per picture: Windows caches
/// toast images by path) and drops the previous ones.
fn write_picture(dir: &Path, p: &Picture) -> Option<PathBuf> {
    let rgba = match p {
        Picture::Photo(b) => crate::avatar::round(b, PICTURE_PX).map_err(|e| tracing::debug!("toast photo: {e:#}")).ok()?,
        Picture::Initial { text, rgb } => identicon(text, *rgb, PICTURE_PX)?,
    };
    let png = sigwin_thumb::png_rgba(&rgba).ok()?;
    let name = format!("toast-{:016x}.png", hash(&png));
    std::fs::create_dir_all(dir).ok()?;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with("toast-") && n.ends_with(".png") && *n != name {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = dir.join(&name);
    if !path.exists() {
        std::fs::write(&path, png).ok()?;
    }
    Some(path)
}

fn hash(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// The thread list's placeholder avatar: white initial on a colored circle.
#[cfg(windows)]
fn identicon(text: &str, rgb: u32, px: u32) -> Option<image::RgbaImage> {
    let size = px as usize;
    let cov = crate::badge::win::text_coverage(text, size, "Malgun Gothic", 600, 0.42);
    let bg = [(rgb >> 16) as u8 as f32, (rgb >> 8) as u8 as f32, rgb as u8 as f32];
    let r = size as f32 / 2.0;
    let mut img = image::RgbaImage::new(px, px);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - r, y as f32 + 0.5 - r);
        let a = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
        let t = cov[y as usize * size + x as usize] as f32 / 255.0;
        let c = |i: usize| (bg[i] + (255.0 - bg[i]) * t).round() as u8;
        *p = image::Rgba([c(0), c(1), c(2), (a * 255.0).round() as u8]);
    }
    Some(img)
}

#[cfg(not(windows))]
fn identicon(_: &str, _: u32, _: u32) -> Option<image::RgbaImage> {
    None
}

/// Signal Desktop's toast XML: ToastImageAndText02 with the photo, else ToastText02. The IM
/// sound is always asked for; whether it plays is the app's switch in Windows' notification
/// settings (Signal's own "sound" setting is an extra in-app sound, not this).
fn toast_xml(title: &str, body: &str, picture: Option<&Path>, token: &str) -> String {
    let (template, image) = match picture {
        Some(p) => ("ToastImageAndText02", format!(r#"<image id="1" src="{}" hint-crop="circle"/>"#, esc(&file_uri(p)))),
        None => ("ToastText02", String::new()),
    };
    format!(
        r#"<toast launch="{}" activationType="foreground"><visual><binding template="{template}">{image}<text id="1">{}</text><text id="2">{}</text></binding></visual><audio src="ms-winsoundevent:Notification.IM"/></toast>"#,
        esc(token),
        esc(title),
        esc(body),
    )
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // not allowed in XML 1.0
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

/// `file:///C:/...` with everything outside encodeURI's safe set percent-encoded.
fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut o = String::from("file:///");
    for b in s.trim_start_matches('/').bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'();/?:@&=+$,#".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
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

    use anyhow::Context;
    use windows::core::{IInspectable, Interface, HSTRING};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};

    const ICON: &[u8] = include_bytes!("../assets/icon.png");

    pub fn show(xml: &str) -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        toast.SetTag(&HSTRING::from("tag"))?;
        toast.SetGroup(&HSTRING::from("group"))?;
        toast.Activated(&TypedEventHandler::new(|_, args| {
            if let Some(token) = launch_args(&args) {
                super::clicked(&token);
            }
            Ok(())
        }))?;
        // one at a time
        clear()?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(super::APP_ID))?.Show(&toast)
    }

    fn launch_args(args: &Option<IInspectable>) -> Option<String> {
        let a = args.as_ref()?.cast::<ToastActivatedEventArgs>().ok()?;
        Some(a.Arguments().ok()?.to_string())
    }

    pub fn clear() -> windows::core::Result<()> {
        ToastNotificationManager::History()?.ClearWithId(&HSTRING::from(super::APP_ID))
    }

    pub fn register() -> anyhow::Result<()> {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA")?).join("Sigwin");
        std::fs::create_dir_all(&dir)?;
        let icon = dir.join("icon.png");
        if std::fs::metadata(&icon).map(|m| m.len() != ICON.len() as u64).unwrap_or(true) {
            std::fs::write(&icon, ICON).context("writing icon")?;
        }
        let key = super::aumid_key(super::APP_ID);
        crate::reg::set_str(&key, "DisplayName", "Sigwin")?;
        crate::reg::set_str(&key, "IconUri", &icon.to_string_lossy())?;
        Ok(())
    }
}

/// Registry key of an AppUserModelID (the uninstaller removes it).
pub fn aumid_key(id: &str) -> String {
    format!(r"Software\Classes\AppUserModelId\{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escapes_and_picks_template() {
        let x = toast_xml("A & <B>", "\"hi\"\u{1}", None, "1-2");
        assert!(x.contains(r#"template="ToastText02""#));
        assert!(x.contains("Notification.IM"));
        assert!(x.contains("<text id=\"1\">A &amp; &lt;B&gt;</text>"));
        assert!(x.contains("<text id=\"2\">&quot;hi&quot;</text>"));
        assert!(x.contains(r#"launch="1-2""#));
        let x = toast_xml("t", "b", Some(Path::new(r"C:\Users\김 철수\open\toast-1.png")), "1");
        assert!(x.contains(r#"template="ToastImageAndText02""#));
        assert!(x.contains(r#"src="file:///C:/Users/%EA%B9%80%20%EC%B2%A0%EC%88%98/open/toast-1.png""#));
    }

    /// WinRT accepts the XML (builds the toast object; nothing is shown).
    #[cfg(windows)]
    #[test]
    fn winrt_parses_the_xml() {
        use windows::core::HSTRING;
        use windows::Data::Xml::Dom::XmlDocument;
        use windows::UI::Notifications::ToastNotification;
        for pic in [None, Some(Path::new(r"C:\tmp\a b\toast-1.png"))] {
            let doc = XmlDocument::new().unwrap();
            doc.LoadXml(&HSTRING::from(toast_xml("김 & <철수>", "\"안녕\" 👍", pic, "1-2"))).unwrap();
            ToastNotification::CreateToastNotification(&doc).unwrap();
        }
    }

    /// `SIGWIN_TOAST_PNG=<dir>` saves the pictures for a look.
    #[cfg(windows)]
    #[test]
    fn identicon_draws_the_initial() {
        for (i, text) in ["김", "A", "😀"].into_iter().enumerate() {
            let img = identicon(text, 0x3a77c9, PICTURE_PX).unwrap();
            if text != "😀" {
                assert!(img.pixels().any(|p| p[3] == 255 && p[0] > 240 && p[1] > 240 && p[2] > 240), "{text}: no white ink");
            }
            assert_eq!(img.get_pixel(0, 0)[3], 0, "corners are transparent");
            if let Some(dir) = std::env::var_os("SIGWIN_TOAST_PNG") {
                let dir = PathBuf::from(dir);
                write_picture(&dir.join(i.to_string()), &Picture::Initial { text: text.into(), rgb: 0x3a77c9 }).unwrap();
            }
        }
    }
}
