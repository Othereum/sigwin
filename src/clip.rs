//! Pasting files and images from the Windows clipboard into the composer (Ctrl+V).
//!
//! Text paste stays with Slint's TextInput: we only step in when the clipboard holds no text.
//! Images are streamed row by row into a PNG under `paste/`, so a 4K screenshot never sits in
//! memory as a decoded copy.

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
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Context};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
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
            bail!("클립보드를 열 수 없습니다")
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
            std::fs::write(&path, png.bytes())?;
            return Ok(path);
        }
        let dib = Locked::get(CF_DIBV5).or_else(|| Locked::get(CF_DIB)).context("클립보드에 이미지가 없습니다")?;
        write_dib_png(dib.bytes(), &path)?;
        Ok(path)
    }

    fn u32_at(b: &[u8], o: usize) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(b.get(o..o + 4).context("잘린 이미지")?.try_into()?))
    }

    /// Streams a packed DIB (BITMAPINFOHEADER or V4/V5, 24 or 32 bpp) into a PNG file.
    pub(super) fn write_dib_png(b: &[u8], path: &Path) -> anyhow::Result<()> {
        let hsize = u32_at(b, 0)? as usize;
        let w = u32_at(b, 4)? as i32;
        let h = u32_at(b, 8)? as i32;
        let bpp = u16::from_le_bytes(b.get(14..16).context("잘린 이미지")?.try_into()?);
        let compression = u32_at(b, 16)?;
        let clr_used = u32_at(b, 32)? as usize;
        if hsize < 40 || w <= 0 || h == 0 || !matches!(bpp, 24 | 32) || !matches!(compression, 0 | 3) {
            bail!("지원하지 않는 이미지 형식입니다 ({bpp}bpp)");
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
        let pixels = b.get(offset..offset + stride * height).context("잘린 이미지")?;
        let [rm, gm, bm, am] = masks.unwrap_or([0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0]);
        let chan = |px: u32, m: u32| -> u8 { if m == 0 { 0 } else { ((px & m) >> m.trailing_zeros()) as u8 } };
        // BI_RGB 32bpp leaves alpha undefined (usually 0): only trust it if some pixel uses it
        let alpha = bpp == 32
            && (am != 0 || masks.is_none())
            && pixels.chunks_exact(stride).any(|row| row[..width * 4].chunks_exact(4).any(|p| p[3] != 0));
        let am = if am != 0 { am } else { 0xff00_0000 };

        let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), width as u32, height as u32);
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
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{files, probe, save_image};

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

/// Removes pasted images left from earlier runs.
pub fn clean(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(test, windows))]
mod tests {
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
        let dir = std::env::temp_dir().join(format!("ls-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.png");
        super::win::write_dib_png(&dib, &p).unwrap();
        let img = image::open(&p).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
