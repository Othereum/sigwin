//! Unread count on the taskbar button: `ITaskbarList3::SetOverlayIcon` with a small red
//! circle and the number, drawn with GDI at the small-icon size for the window's DPI.
//! Badge notifications (`BadgeUpdateManager`) would need package identity, so no.
//! The notification-area icon (`tray`) gets a smaller badge drawn into the app icon.
//!
//! The overlay only sticks once the taskbar button exists, and Explorer drops it when it
//! restarts: both send "TaskbarButtonCreated", on which `winhook` calls `refresh`.

use std::cell::Cell;

thread_local! {
    static HWND: Cell<isize> = const { Cell::new(0) };
    static COUNT: Cell<u32> = const { Cell::new(0) };
    static NOUN: Cell<&'static str> = const { Cell::new("메시지") };
}

/// The native window exists: remember it and show the current count.
pub fn set_hwnd(hwnd: isize) {
    HWND.set(hwnd);
    refresh();
}

/// `noun` is what's being counted, for the descriptions.
pub fn set_count(n: u32, noun: &'static str) {
    let noun_changed = NOUN.replace(noun) != noun;
    if COUNT.replace(n) != n || noun_changed {
        refresh();
    }
}

pub fn count() -> u32 {
    COUNT.get()
}

/// The count in words, for the tooltip and screen readers.
pub fn description() -> String {
    format!("읽지 않은 {} {}개", NOUN.get(), COUNT.get())
}

/// Re-applies both badges (count changed, taskbar button (re)created, DPI changed).
pub fn refresh() {
    // the tray icon exists without the main window: an autostart launch starts hidden
    crate::tray::update();
    let hwnd = HWND.get();
    if hwnd == 0 {
        return;
    }
    #[cfg(windows)]
    if let Err(e) = win::apply(hwnd, count()) {
        tracing::debug!("taskbar badge: {e:#}");
    }
}

/// Text shown in the badge: up to two digits, then "99+".
fn label(n: u32) -> String {
    if n > 99 { "99+".into() } else { n.to_string() }
}

/// Text in the tray badge: only one character stays readable in its small circle, so it
/// stops at 9. The tooltip has the exact count.
#[cfg(windows)]
fn tray_label(n: u32) -> String {
    n.min(9).to_string()
}

#[cfg(windows)]
pub(crate) mod win {
    use std::cell::Cell;
    use std::ffi::c_void;

    use anyhow::{bail, Result};
    use windows_sys::core::{GUID, HRESULT, PCWSTR};
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, DrawIconEx, DI_NORMAL, HICON, ICONINFO, SM_CXSMICON};

    const CLSID_TASKBAR_LIST: GUID = GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);
    const IID_ITASKBAR_LIST3: GUID = GUID::from_u128(0xea1afb91_9e28_4b86_90e9_9e9f8a5eefaf);

    /// ITaskbarList3 vtable up to SetOverlayIcon (slot 18).
    #[repr(C)]
    struct Vtbl {
        _query_interface: usize,
        _add_ref: usize,
        _release: usize,
        hr_init: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        _skip: [usize; 14],
        set_overlay_icon: unsafe extern "system" fn(*mut c_void, HWND, HICON, PCWSTR) -> HRESULT,
    }

    thread_local! {
        /// The ITaskbarList3 instance, created once and kept for the app's lifetime.
        static LIST: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
    }

    fn list() -> Result<*mut c_void> {
        let p = LIST.get();
        if !p.is_null() {
            return Ok(p);
        }
        let mut p = std::ptr::null_mut();
        // SAFETY: standard COM creation on the UI thread (winit already made it an STA;
        // CoInitializeEx then just returns S_FALSE or RPC_E_CHANGED_MODE).
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let hr = CoCreateInstance(&CLSID_TASKBAR_LIST, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ITASKBAR_LIST3, &mut p);
            if hr < 0 || p.is_null() {
                bail!("CoCreateInstance(TaskbarList): {hr:#x}");
            }
            let vt = &**(p as *const *const Vtbl);
            let hr = (vt.hr_init)(p);
            if hr < 0 {
                bail!("HrInit: {hr:#x}");
            }
        }
        LIST.set(p);
        Ok(p)
    }

    pub fn apply(hwnd: isize, n: u32) -> Result<()> {
        let list = list()?;
        let hwnd = hwnd as HWND;
        let (icon, desc) = if n == 0 {
            (std::ptr::null_mut(), Vec::new())
        } else {
            // SAFETY: plain queries on our own window
            let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(hwnd)) }.max(16);
            let px = super::render(&super::label(n), size as usize, super::RED);
            let desc: Vec<u16> = super::description().encode_utf16().chain(Some(0)).collect();
            (make_icon(&px, size), desc)
        };
        // SAFETY: valid interface pointer; the taskbar copies the icon, so we free it right after
        let hr = unsafe {
            let vt = &**(list as *const *const Vtbl);
            let hr = (vt.set_overlay_icon)(list, hwnd, icon, if desc.is_empty() { std::ptr::null() } else { desc.as_ptr() });
            if !icon.is_null() {
                DestroyIcon(icon);
            }
            hr
        };
        if hr < 0 {
            bail!("SetOverlayIcon: {hr:#x}");
        }
        Ok(())
    }

    /// `base` with the tray badge for `n`; the caller frees it. Null if drawing failed.
    pub fn tray_icon(base: HICON, size: i32, n: u32) -> HICON {
        let Some(mut px) = icon_pixels(base, size) else {
            return std::ptr::null_mut();
        };
        super::add_badge(&mut px, size as usize, n);
        make_icon(&px, size)
    }

    /// HICON -> straight-alpha BGRA (top-down, `size`x`size`).
    fn icon_pixels(icon: HICON, size: i32) -> Option<Vec<u8>> {
        // SAFETY: GDI objects are created, used and released locally
        let mut px = unsafe {
            let (dc, bmp, bits) = dib(size);
            if bmp.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bmp);
            let ok = DrawIconEx(dc, 0, 0, icon, size, size, 0, std::ptr::null_mut(), DI_NORMAL) != 0;
            GdiFlush();
            let px = std::slice::from_raw_parts(bits, (size * size * 4) as usize).to_vec();
            SelectObject(dc, old);
            DeleteObject(bmp);
            DeleteDC(dc);
            if !ok {
                return None;
            }
            px
        };
        // drawn over transparent black: the colors came out premultiplied
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut p[..3] {
                    *c = (*c as u32 * 255 / a).min(255) as u8;
                }
            }
        }
        Some(px)
    }

    #[cfg(test)]
    pub fn test_icon(bgra: &[u8], size: i32) -> HICON {
        make_icon(bgra, size)
    }

    #[cfg(test)]
    pub fn test_pixels(icon: HICON, size: i32) -> Vec<u8> {
        icon_pixels(icon, size).unwrap()
    }

    /// Straight-alpha BGRA (top-down, `size`x`size`) -> HICON.
    fn make_icon(bgra: &[u8], size: i32) -> HICON {
        // SAFETY: GDI objects are created, used and released locally
        unsafe {
            let (dc, bmp, bits) = dib(size);
            if bmp.is_null() {
                DeleteDC(dc);
                return std::ptr::null_mut();
            }
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits, bgra.len());
            let mask = CreateBitmap(size, size, 1, 1, std::ptr::null());
            let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: bmp };
            let icon = CreateIconIndirect(&info);
            DeleteObject(mask);
            DeleteObject(bmp);
            DeleteDC(dc);
            icon
        }
    }

    /// A memory DC and a 32bpp top-down DIB section (zeroed).
    unsafe fn dib(size: i32) -> (HDC, HBITMAP, *mut u8) {
        let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = size;
        bi.bmiHeader.biHeight = -size;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits = std::ptr::null_mut();
        // SAFETY: bi is a valid header; bits receives the section's memory
        unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            (dc, bmp, bits as *mut u8)
        }
    }

    /// Grayscale coverage (0..=255, `size`x`size`) of `text`, centered: the largest font up to
    /// `frac` of the size whose text fits the circle's width.
    pub(crate) fn text_coverage(text: &str, size: usize, face: &str, weight: i32, frac: f32) -> Vec<u8> {
        let s = size as i32;
        let mut out = vec![0u8; size * size];
        // SAFETY: GDI objects are created, used and released locally
        unsafe {
            let (dc, bmp, bits) = dib(s);
            if bmp.is_null() {
                DeleteDC(dc);
                return out;
            }
            let old_bmp = SelectObject(dc, bmp);
            let face: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
            let t: Vec<u16> = text.encode_utf16().collect();
            let max_w = (s as f32 * 0.76).round() as i32;
            let mut h = (s as f32 * frac).round() as i32;
            let (font, old_font) = loop {
                let font = CreateFontW(
                    -h, 0, 0, 0, weight, 0, 0, 0,
                    DEFAULT_CHARSET as u32, OUT_TT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
                    ANTIALIASED_QUALITY as u32, 0, face.as_ptr(),
                );
                let old = SelectObject(dc, font);
                let mut ext = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32W(dc, t.as_ptr(), t.len() as i32, &mut ext);
                if ext.cx <= max_w || h <= 6 {
                    break (font, old);
                }
                SelectObject(dc, old);
                DeleteObject(font);
                h -= 1;
            };
            SetTextColor(dc, 0x00ff_ffff);
            SetBkMode(dc, TRANSPARENT as i32);
            let mut rc = RECT { left: 0, top: 0, right: s, bottom: s };
            DrawTextW(dc, t.as_ptr(), t.len() as i32, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            GdiFlush();
            let px = std::slice::from_raw_parts(bits, size * size * 4);
            for (o, p) in out.iter_mut().zip(px.chunks_exact(4)) {
                *o = p[0].max(p[1]).max(p[2]);
            }
            SelectObject(dc, old_font);
            DeleteObject(font);
            SelectObject(dc, old_bmp);
            DeleteObject(bmp);
            DeleteDC(dc);
        }
        center_ink(&mut out, size);
        out
    }

    /// Shifts the coverage so its ink box is centered (whole pixels).
    fn center_ink(cov: &mut [u8], size: usize) {
        let (mut x0, mut y0, mut x1, mut y1) = (size, size, 0, 0);
        for y in 0..size {
            for x in 0..size {
                if cov[y * size + x] > 40 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x0 > x1 {
            return;
        }
        let dx = ((size - 1) as i32 - (x0 + x1) as i32) / 2;
        let dy = ((size - 1) as i32 - (y0 + y1) as i32) / 2;
        if dx == 0 && dy == 0 {
            return;
        }
        let src = cov.to_vec();
        cov.fill(0);
        for y in 0..size as i32 {
            for x in 0..size as i32 {
                let (sx, sy) = (x - dx, y - dy);
                if sx >= 0 && sy >= 0 && (sx as usize) < size && (sy as usize) < size {
                    cov[y as usize * size + x as usize] = src[sy as usize * size + sx as usize];
                }
            }
        }
    }
}

#[cfg(windows)]
const RED: [f32; 3] = [0xe0 as f32, 0x30 as f32, 0x2c as f32]; // r, g, b
/// The tray badge is small and sits on a colored icon: it needs to catch the eye more.
#[cfg(windows)]
const BRIGHT_RED: [f32; 3] = [0xff as f32, 0x3b as f32, 0x30 as f32]; // r, g, b

/// Straight-alpha BGRA pixels of the badge: red circle, white text.
#[cfg(windows)]
fn render(text: &str, size: usize, color: [f32; 3]) -> Vec<u8> {
    let cov = win::text_coverage(text, size, "Segoe UI", 700, 0.78);
    let r = size as f32 / 2.0;
    let mut out = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 + 0.5 - r, y as f32 + 0.5 - r);
            let a = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if a == 0.0 {
                continue;
            }
            let t = cov[y * size + x] as f32 / 255.0;
            let c = |i: usize| (color[i] + (255.0 - color[i]) * t).round() as u8;
            let o = (y * size + x) * 4;
            out[o..o + 4].copy_from_slice(&[c(2), c(1), c(0), (a * 255.0).round() as u8]);
        }
    }
    out
}

/// Draws the tray badge over the app icon's pixels (straight BGRA): upper right corner.
#[cfg(windows)]
fn add_badge(icon: &mut [u8], size: usize, n: u32) {
    let d = (size as f32 * 0.6875).round() as usize;
    let badge = render(&tray_label(n), d, BRIGHT_RED);
    let off = size - d;
    for y in 0..d {
        for x in 0..d {
            let b = &badge[(y * d + x) * 4..][..4];
            if b[3] == 0 {
                continue;
            }
            // source over, for the circle's antialiased edge
            let o = (y * size + off + x) * 4;
            let a = b[3] as f32 / 255.0;
            let under = icon[o + 3] as f32 / 255.0 * (1.0 - a);
            let out = a + under;
            for i in 0..3 {
                icon[o + i] = ((b[i] as f32 * a + icon[o + i] as f32 * under) / out).round() as u8;
            }
            icon[o + 3] = (out * 255.0).round() as u8;
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    /// The real pipeline on the real icon (HICON -> pixels -> badge);
    /// `SIGWIN_BADGE_PNG=<dir>` saves the results for a look.
    #[test]
    fn tray_badge() {
        let src = image::open("assets/icon.png").unwrap().into_rgba8();
        for size in [16u32, 20, 24, 32] {
            let small = image::imageops::resize(&src, size, size, image::imageops::FilterType::Lanczos3);
            let bgra: Vec<u8> = small.pixels().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
            let base = super::win::test_icon(&bgra, size as i32);
            let back = super::win::test_pixels(base, size as i32);
            // the icon's alpha survives the round trip through GDI
            let solid = |px: &[u8]| px.chunks_exact(4).filter(|p| p[3] == 255).count();
            assert_eq!(solid(&back), solid(&bgra));
            for n in [1u32, 7, 10, 150] {
                let mut px = back.clone();
                super::add_badge(&mut px, size as usize, n);
                // the circle's red is there, and the icon's own pixels too
                assert!(px.chunks_exact(4).any(|p| p == [0x30, 0x3b, 0xff, 255]));
                assert!(px.chunks_exact(4).any(|p| p[3] != 0 && p[0] > p[2]));
                if let Some(dir) = std::env::var_os("SIGWIN_BADGE_PNG") {
                    let rgba: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
                    let img = image::RgbaImage::from_raw(size, size, rgba).unwrap();
                    let img = image::imageops::resize(&img, size * 8, size * 8, image::imageops::FilterType::Nearest);
                    img.save(std::path::Path::new(&dir).join(format!("tray-{size}-{n}.png"))).unwrap();
                }
            }
        }
    }

    /// Renders the badge at common sizes; `SIGWIN_BADGE_PNG=<dir>` saves them for a look.
    #[test]
    fn renders() {
        for size in [16usize, 20, 24, 32] {
            for n in [1u32, 7, 23, 88, 150] {
                let px = super::render(&super::label(n), size, super::RED);
                assert_eq!(px.len(), size * size * 4);
                // the text left white pixels inside the circle
                assert!(px.chunks_exact(4).any(|p| p[3] == 255 && p[0] > 200 && p[1] > 200));
                if let Some(dir) = std::env::var_os("SIGWIN_BADGE_PNG") {
                    let rgba: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
                    let img = image::RgbaImage::from_raw(size as u32, size as u32, rgba).unwrap();
                    let img = image::imageops::resize(&img, size as u32 * 8, size as u32 * 8, image::imageops::FilterType::Nearest);
                    img.save(std::path::Path::new(&dir).join(format!("badge-{size}-{n}.png"))).unwrap();
                }
            }
        }
    }
}
