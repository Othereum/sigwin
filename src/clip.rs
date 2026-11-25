//! Pasting files and images from the Windows clipboard into the composer (Ctrl+V), and copying a
//! received image (`set_image`) or a text (`set_text`) to it.
//!
//! Text paste stays with Slint's TextInput: we only step in when the clipboard holds no text.
//! Images are encoded row by row into a PNG, so a 4K screenshot never sits in memory as a
//! decoded copy, and sealed under `paste/`: nothing outside Sigwin reads them (`attached`).

use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// text (or nothing we handle): let the text input paste it
    Other,
    Files,
    Image,
}

#[cfg(windows)]
mod win {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Context};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    use super::Probe;

    const CF_DIB: u32 = 8;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;
    const CF_DIBV5: u32 = 17;

    fn png_format() -> u32 {
        let name: Vec<u16> = "PNG".encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated UTF-16 string
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    fn available(f: u32) -> bool {
        // SAFETY: plain query, no clipboard ownership needed
        f != 0 && unsafe { IsClipboardFormatAvailable(f) } != 0
    }

    pub fn probe() -> Probe {
        if available(CF_UNICODETEXT) {
            Probe::Other
        } else if available(CF_HDROP) {
            Probe::Files
        } else if available(png_format()) || available(CF_DIBV5) || available(CF_DIB) {
            Probe::Image
        } else {
            Probe::Other
        }
    }

    /// Open clipboard; closed on drop. Retries briefly: another app may hold it.
    struct Open;

    impl Open {
        fn new() -> anyhow::Result<Self> {
            for _ in 0..10 {
                // SAFETY: no owner window; closed again in Drop
                if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                    return Ok(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            bail!(tr!("Couldn't open the clipboard"))
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: we opened it
            unsafe { CloseClipboard() };
        }
    }

    /// A clipboard memory block, locked while this lives.
    struct Locked {
        handle: *mut core::ffi::c_void,
        ptr: *const u8,
        len: usize,
    }

    impl Locked {
        fn get(format: u32) -> Option<Self> {
            // SAFETY: clipboard is open; the handle stays owned by the clipboard
            unsafe {
                let handle = GetClipboardData(format);
                if handle.is_null() {
                    return None;
                }
                let ptr = GlobalLock(handle) as *const u8;
                if ptr.is_null() {
                    return None;
                }
                Some(Locked { handle, ptr, len: GlobalSize(handle) })
            }
        }

        fn bytes(&self) -> &[u8] {
            // SAFETY: GlobalSize bytes are readable while locked
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    impl Drop for Locked {
        fn drop(&mut self) {
            // SAFETY: locked in `get`
            unsafe { GlobalUnlock(self.handle) };
        }
    }

    pub fn files() -> Vec<PathBuf> {
        let Ok(_open) = Open::new() else { return Vec::new() };
        // SAFETY: clipboard open; HDROP is read, never freed (the clipboard owns it)
        unsafe {
            let h = GetClipboardData(CF_HDROP);
            if h.is_null() {
                return Vec::new();
            }
            let n = DragQueryFileW(h, u32::MAX, std::ptr::null_mut(), 0);
            let mut out = Vec::new();
            for i in 0..n {
                let len = DragQueryFileW(h, i, std::ptr::null_mut(), 0) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32) as usize;
                let p = PathBuf::from(String::from_utf16_lossy(&buf[..got]));
                // folders can't be attached
                if p.is_file() {
                    out.push(p);
                }
            }
            out
        }
    }

    pub fn save_image(dir: &Path) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("clipboard-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S%3f")));
        let _open = Open::new()?;
        // browsers put the original PNG on the clipboard: keep it as is (alpha included)
        if let Some(png) = Locked::get(png_format()) {
            sigwin_vault::write(&path, png.bytes())?;
            return Ok(path);
        }
        let dib = Locked::get(CF_DIBV5).or_else(|| Locked::get(CF_DIB)).context(tr!("No image on the clipboard"))?;
        let png = dib_png(dib.bytes())?;
        sigwin_vault::write(&path, &png)?;
        Ok(path)
    }

    /// Puts a picture on the clipboard as PNG (what browsers and Office take, alpha kept) and as a
    /// DIB (what older apps take). `rgba` is `w` x `h` straight RGBA; `png` is the same picture.
    pub fn set_image(png: &[u8], rgba: &[u8], w: u32, h: u32) -> anyhow::Result<()> {
        let (dib, dib_format) = rgba_dib(rgba, w, h)?;
        let _open = Open::new()?;
        // SAFETY: clipboard is open; EmptyClipboard makes us its owner for the formats set below
        if unsafe { EmptyClipboard() } == 0 {
            bail!(tr!("Couldn't open the clipboard"));
        }
        put(png_format(), png)?;
        put(dib_format, &dib)
    }

    pub fn set_text(text: &str) -> anyhow::Result<()> {
        let wide: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        let _open = Open::new()?;
        // SAFETY: clipboard is open; EmptyClipboard makes us its owner for the format set below
        if unsafe { EmptyClipboard() } == 0 {
            bail!(tr!("Couldn't open the clipboard"));
        }
        put(CF_UNICODETEXT, &wide)
    }

    /// Hands a copy of `data` to the clipboard, which owns it from then on.
    fn put(format: u32, data: &[u8]) -> anyhow::Result<()> {
        // SAFETY: the block is filled before it is handed over; on failure it is still ours to free
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, data.len());
            if handle.is_null() {
                bail!(tr!("Out of memory"));
            }
            let ptr = GlobalLock(handle) as *mut u8;
            if ptr.is_null() {
                GlobalFree(handle);
                bail!(tr!("Out of memory"));
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            GlobalUnlock(handle);
            if SetClipboardData(format, handle).is_null() {
                GlobalFree(handle);
                bail!(tr!("Couldn't open the clipboard"));
            }
        }
        Ok(())
    }

    /// A packed bottom-up DIB of straight RGBA: 24bpp when it is opaque, else a V5 one with an alpha
    /// mask.
    pub(super) fn rgba_dib(rgba: &[u8], w: u32, h: u32) -> anyhow::Result<(Vec<u8>, u32)> {
        let (w, h) = (w as usize, h as usize);
        if w == 0 || h == 0 || rgba.len() != w * h * 4 {
            bail!(tr!("Truncated image"));
        }
        let alpha = rgba.chunks_exact(4).any(|p| p[3] != 255);
        let (bpp, hsize) = if alpha { (32usize, 124usize) } else { (24, 40) };
        let stride = (w * bpp).div_ceil(32) * 4;
        let mut out = vec![0u8; hsize + stride * h];
        let mut set = |o: usize, v: u32| out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        set(0, hsize as u32);
        set(4, w as u32);
        set(8, h as u32);
        set(20, (stride * h) as u32);
        let mut set16 = |o: usize, v: u16| out[o..o + 2].copy_from_slice(&v.to_le_bytes());
        set16(12, 1);
        set16(14, bpp as u16);
        if alpha {
            let mut set = |o: usize, v: u32| out[o..o + 4].copy_from_slice(&v.to_le_bytes());
            set(16, 3); // BI_BITFIELDS
            set(40, 0x00ff_0000);
            set(44, 0x0000_ff00);
            set(48, 0x0000_00ff);
            set(52, 0xff00_0000);
            set(56, 0x7352_4742); // LCS_sRGB
        }
        for y in 0..h {
            let dst = &mut out[hsize + (h - 1 - y) * stride..][..stride];
            for (x, p) in rgba[y * w * 4..][..w * 4].chunks_exact(4).enumerate() {
                let d = &mut dst[x * bpp / 8..][..bpp / 8];
                d[0] = p[2];
                d[1] = p[1];
                d[2] = p[0];
                if alpha {
                    d[3] = p[3];
                }
            }
        }
        Ok((out, if alpha { CF_DIBV5 } else { CF_DIB }))
    }

    fn u32_at(b: &[u8], o: usize) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(b.get(o..o + 4).context(tr!("Truncated image"))?.try_into()?))
    }

    /// Encodes a packed DIB (BITMAPINFOHEADER or V4/V5, 24 or 32 bpp) as a PNG, a row at a time.
    pub(super) fn dib_png(b: &[u8]) -> anyhow::Result<Vec<u8>> {
        let hsize = u32_at(b, 0)? as usize;
        let w = u32_at(b, 4)? as i32;
        let h = u32_at(b, 8)? as i32;
        let bpp = u16::from_le_bytes(b.get(14..16).context(tr!("Truncated image"))?.try_into()?);
        let compression = u32_at(b, 16)?;
        let clr_used = u32_at(b, 32)? as usize;
        if hsize < 40 || w <= 0 || h == 0 || !matches!(bpp, 24 | 32) || !matches!(compression, 0 | 3) {
            bail!(tr!("Unsupported image format ({}bpp)", bpp));
        }
        let (width, height, top_down) = (w as usize, h.unsigned_abs() as usize, h < 0);
        // BI_BITFIELDS: masks follow a plain 40-byte header, or are part of a V4/V5 header
        let masks = if compression == 3 {
            Some([u32_at(b, 40)?, u32_at(b, 44)?, u32_at(b, 48)?, if hsize >= 56 { u32_at(b, 52)? } else { 0 }])
        } else {
            None
        };
        let offset = hsize + if compression == 3 && hsize == 40 { 12 } else { 0 } + clr_used * 4;
        let stride = (width * bpp as usize).div_ceil(32) * 4;
        let pixels = b.get(offset..offset + stride * height).context(tr!("Truncated image"))?;
        let [rm, gm, bm, am] = masks.unwrap_or([0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0]);
        let chan = |px: u32, m: u32| -> u8 { if m == 0 { 0 } else { ((px & m) >> m.trailing_zeros()) as u8 } };
        // BI_RGB 32bpp leaves alpha undefined (usually 0): only trust it if some pixel uses it
        let alpha = bpp == 32
            && (am != 0 || masks.is_none())
            && pixels.chunks_exact(stride).any(|row| row[..width * 4].chunks_exact(4).any(|p| p[3] != 0));
        let am = if am != 0 { am } else { 0xff00_0000 };

        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(if alpha { png::ColorType::Rgba } else { png::ColorType::Rgb });
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header()?;
        let mut stream = writer.stream_writer()?;
        let mut line = Vec::with_capacity(width * 4);
        for y in 0..height {
            let src = if top_down { y } else { height - 1 - y };
            let row = &pixels[src * stride..src * stride + stride];
            line.clear();
            for x in 0..width {
                let (r, g, b_, a) = if bpp == 24 {
                    (row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255)
                } else {
                    let px = u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into()?);
                    (chan(px, rm), chan(px, gm), chan(px, bm), chan(px, am))
                };
                line.extend_from_slice(&[r, g, b_]);
                if alpha {
                    line.push(a);
                }
            }
            stream.write_all(&line)?;
        }
        stream.finish()?;
        writer.finish()?;
        Ok(out)
    }
}

#[cfg(windows)]
pub use win::{files, probe, save_image, set_image, set_text};

#[cfg(not(windows))]
pub fn probe() -> Probe {
    Probe::Other
}
#[cfg(not(windows))]
pub fn files() -> Vec<std::path::PathBuf> {
    Vec::new()
}
#[cfg(not(windows))]
pub fn save_image(_: &Path) -> anyhow::Result<std::path::PathBuf> {
    anyhow::bail!("not supported")
}

#[cfg(not(windows))]
pub fn set_image(_: &[u8], _: &[u8], _: u32, _: u32) -> anyhow::Result<()> {
    anyhow::bail!("not supported")
}

#[cfg(not(windows))]
pub fn set_text(_: &str) -> anyhow::Result<()> {
    anyhow::bail!("not supported")
}

/// Removes pasted images left from earlier runs.
pub fn clean(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn rgba_to_dib_and_back() {
        // 2x1: red, then half-transparent blue
        let rgba = [255, 0, 0, 255, 0, 0, 255, 128];
        let (dib, format) = super::win::rgba_dib(&rgba, 2, 1).unwrap();
        assert_eq!(format, 17);
        let png = super::win::dib_png(&dib).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255, 128]);
        // opaque: plain 24bpp
        let (dib, format) = super::win::rgba_dib(&[1, 2, 3, 255], 1, 1).unwrap();
        assert_eq!(format, 8);
        let img = image::load_from_memory(&super::win::dib_png(&dib).unwrap()).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [1, 2, 3]);
    }

    #[test]
    fn dib_to_png() {
        // 2x2 bottom-up 24bpp: rows padded to 8 bytes
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        // bottom row: blue, green; top row: red, white (BGR order)
        dib.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0]);
        dib.extend_from_slice(&[0, 0, 255, 255, 255, 255, 0, 0]);
        let png = super::win::dib_png(&dib).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0]);
    }
}
