//! Unread count on the taskbar button: `ITaskbarList3::SetOverlayIcon` with a small red
//! circle and the number, drawn with GDI at the small-icon size for the window's DPI.
//! Badge notifications (`BadgeUpdateManager`) would need package identity, so no.
//!
//! The overlay only sticks once the taskbar button exists, and Explorer drops it when it
//! restarts: both send "TaskbarButtonCreated", on which `winhook` calls `refresh`.

use std::cell::Cell;

thread_local! {
    static HWND: Cell<isize> = const { Cell::new(0) };
    static COUNT: Cell<u32> = const { Cell::new(0) };
}

/// The native window exists: remember it and show the current count.
pub fn set_hwnd(hwnd: isize) {
    HWND.set(hwnd);
    refresh();
}

pub fn set_count(n: u32) {
    if COUNT.replace(n) != n {
        refresh();
    }
}

/// Re-applies the overlay (taskbar button (re)created, DPI changed).
pub fn refresh() {
    let hwnd = HWND.get();
    if hwnd == 0 {
        return;
    }
    #[cfg(windows)]
    if let Err(e) = win::apply(hwnd, COUNT.get()) {
        tracing::debug!("taskbar badge: {e:#}");
    }
}

/// Text shown in the badge: one or two characters fit the 16px circle.
fn label(n: u32) -> String {
    if n > 9 { "9+".into() } else { n.to_string() }
}

#[cfg(windows)]
mod win {
    use std::cell::Cell;
    use std::ffi::c_void;

    use anyhow::{bail, Result};
    use windows_sys::core::{GUID, HRESULT, PCWSTR};
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO, SM_CXSMICON};

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
            let px = super::render(&super::label(n), size as usize);
            let desc: Vec<u16> = format!("읽지 않은 메시지 {n}개").encode_utf16().chain(Some(0)).collect();
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

    /// Grayscale coverage (0..=255, `size`x`size`) of `text` in bold Segoe UI, centered.
    pub fn text_coverage(text: &str, size: usize) -> Vec<u8> {
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
            let em = if text.chars().count() > 1 { 0.6 } else { 0.78 };
            let face: Vec<u16> = "Segoe UI".encode_utf16().chain(Some(0)).collect();
            let font = CreateFontW(
                -((s as f32 * em).round() as i32),
                0, 0, 0, FW_BOLD as i32, 0, 0, 0,
                DEFAULT_CHARSET as u32, OUT_TT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32, 0, face.as_ptr(),
            );
            let old_font = SelectObject(dc, font);
            SetTextColor(dc, 0x00ff_ffff);
            SetBkMode(dc, TRANSPARENT as i32);
            let t: Vec<u16> = text.encode_utf16().collect();
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

/// Straight-alpha BGRA pixels of the badge: red circle, white text.
#[cfg(windows)]
fn render(text: &str, size: usize) -> Vec<u8> {
    const RED: [f32; 3] = [0xe0 as f32, 0x30 as f32, 0x2c as f32]; // r, g, b
    let cov = win::text_coverage(text, size);
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
            let c = |i: usize| (RED[i] + (255.0 - RED[i]) * t).round() as u8;
            let o = (y * size + x) * 4;
            out[o..o + 4].copy_from_slice(&[c(2), c(1), c(0), (a * 255.0).round() as u8]);
        }
    }
    out
}

#[cfg(all(test, windows))]
mod tests {
    /// Renders the badge at common sizes; `SIGWIN_BADGE_PNG=<dir>` saves them for a look.
    #[test]
    fn renders() {
        for size in [16usize, 20, 24, 32] {
            for n in [1u32, 7, 23] {
                let px = super::render(&super::label(n), size);
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
