//! Color emoji for the software renderer, which can only draw monochrome glyphs.
//!
//! Windows draws them for us: DirectWrite + a *software* Direct2D render target over a WIC
//! bitmap (no GPU driver is loaded). That covers COLRv1 gradients and sequences (skin tones,
//! ZWJ families, flags) exactly as the rest of Windows shows them.

#[cfg(windows)]
mod imp {
    use windows::core::{w, Result};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
        D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
        DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_WORD_WRAPPING_NO_WRAP,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICImagingFactory,
        WICBitmapCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};

    pub struct Renderer {
        wic: IWICImagingFactory,
        d2d: ID2D1Factory,
        dwrite: IDWriteFactory,
    }

    impl Renderer {
        pub fn new() -> Result<Self> {
            // SAFETY: plain COM/factory creation on the calling thread
            unsafe {
                // already initialized by winit/rfd on the UI thread is fine (S_FALSE / mode mismatch)
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
                let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
                Ok(Self { wic, d2d, dwrite })
            }
        }

        fn format(&self, px: f32) -> Result<IDWriteTextFormat> {
            // SAFETY: COM calls with valid arguments
            unsafe {
                let f = self.dwrite.CreateTextFormat(
                    w!("Segoe UI Emoji"),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    px,
                    w!("en-us"),
                )?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                Ok(f)
            }
        }

        /// Renders one emoji (a single grapheme cluster: ZWJ sequences, skin tones and
        /// flags are one cluster) into a `size`×`size` premultiplied RGBA buffer.
        pub fn render(&self, text: &str, size: u32) -> Result<(u32, u32, Vec<u8>)> {
            let (w, h) = (size, size);
            // SAFETY: COM calls; the WIC bitmap outlives the render target and the lock
            unsafe {
                let bmp: IWICBitmap = self.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand)?;
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                };
                let rt: ID2D1RenderTarget = self.d2d.CreateWicBitmapRenderTarget(&bmp, &props)?;
                let brush = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }, None)?;
                let format = self.format(size as f32 * 0.78)?;
                let wide: Vec<u16> = text.encode_utf16().collect();
                rt.BeginDraw();
                rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
                rt.DrawText(
                    &wide,
                    &format,
                    &D2D_RECT_F { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 },
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                rt.EndDraw(None, None)?;
                let mut px = vec![0u8; (w * h * 4) as usize];
                bmp.CopyPixels(std::ptr::null(), w * 4, &mut px)?;
                // BGRA -> RGBA (still premultiplied)
                for p in px.chunks_exact_mut(4) {
                    p.swap(0, 2);
                }
                Ok((w, h, px))
            }
        }
    }
}

#[cfg(windows)]
pub use imp::Renderer;

/// Pixel size emoji are rendered at; the UI scales them down (reactions ~18px, big ~44px).
pub const SIZE: u32 = 64;

const CHILD_ARG: &str = "--render-emoji";

/// Cache file for one emoji cluster: code points in hex, e.g. `1f44d-1f3fd.png`.
pub fn cache_path(dir: &std::path::Path, cluster: &str) -> std::path::PathBuf {
    let name: Vec<String> = cluster.chars().map(|c| format!("{:x}", c as u32)).collect();
    dir.join(format!("{}.png", name.join("-")))
}

/// Whether a grapheme cluster is an emoji we should draw in color.
pub fn is_emoji(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else { return false };
    let cp = first as u32;
    let pictographic = matches!(cp, 0x1f000..=0x1faff | 0x2600..=0x27bf | 0x2300..=0x23ff | 0x2b00..=0x2bff);
    // text-default symbols (e.g. ❤ ☺) count when followed by the emoji presentation selector
    pictographic && (cp >= 0x1f000 || cluster.contains('\u{fe0f}') || cluster.chars().count() == 1)
}

/// Splits `text` into emoji clusters if it consists only of 1..=max emoji (whitespace allowed).
pub fn only_emoji(text: &str, max: usize) -> Option<Vec<String>> {
    use unicode_segmentation::UnicodeSegmentation;
    let clusters: Vec<String> =
        text.graphemes(true).filter(|g| !g.trim().is_empty()).map(str::to_string).collect();
    (!clusters.is_empty() && clusters.len() <= max && clusters.iter().all(|c| is_emoji(c))).then_some(clusters)
}

/// Renders the missing clusters into `dir` in a child process: DirectWrite/Direct2D keep
/// ~20MB of font caches per process, which we don't want in the long-running app.
pub fn render_missing(dir: &std::path::Path, clusters: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let missing: Vec<&String> = clusters.iter().filter(|c| !cache_path(dir, c).exists()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg(CHILD_ARG)
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        for c in missing {
            writeln!(stdin, "{c}")?;
        }
    }
    child.wait()?;
    Ok(())
}

/// Child side of [`render_missing`]: returns true if this process was the renderer child.
pub fn run_if_child() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(CHILD_ARG) {
        return false;
    }
    #[cfg(windows)]
    if let (Some(dir), Ok(r)) = (args.get(2), Renderer::new()) {
        use std::io::BufRead;
        let dir = std::path::Path::new(dir);
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let cluster = line.trim_end_matches(['\r', '\n']);
            if cluster.is_empty() {
                continue;
            }
            if let Ok((w, h, mut px)) = r.render(cluster, SIZE) {
                unpremultiply(&mut px);
                if let Some(img) = image::RgbaImage::from_raw(w, h, px) {
                    let _ = img.save(cache_path(dir, cluster));
                }
            }
        }
    }
    true
}

fn unpremultiply(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// `--emoji-probe <out.png>`: render a sample grid and report what it cost.
pub fn probe(out: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let samples = ["😀", "🎉", "👍", "👍🏽", "❤️", "🙂", "😂", "🥹", "👨‍👩‍👧", "🇰🇷", "🐶", "🍕", "✅", "🔥", "🫠", "🧑🏻‍💻"];
        let before = crate::alloc::private_bytes();
        let t = std::time::Instant::now();
        let r = Renderer::new().map_err(|e| anyhow::anyhow!("init: {e}"))?;
        let after_init = crate::alloc::private_bytes();
        let size = 48u32;
        let cols = 8u32;
        let mut grid = image::RgbaImage::new(size * cols, size * 2);
        for (i, s) in samples.iter().enumerate() {
            let (w, h, px) = r.render(s, size).map_err(|e| anyhow::anyhow!("render {s}: {e}"))?;
            let img = image::RgbaImage::from_raw(w, h, px).expect("buffer size");
            let (x, y) = ((i as u32 % cols) * size, (i as u32 / cols) * size);
            image::imageops::overlay(&mut grid, &image::imageops::crop_imm(&img, 0, 0, size, size).to_image(), x as i64, y as i64);
        }
        let elapsed = t.elapsed();
        let after = crate::alloc::private_bytes();
        // un-premultiply for saving
        for p in grid.pixels_mut() {
            let a = p[3] as u32;
            if a > 0 && a < 255 {
                for c in 0..3 {
                    p[c] = ((p[c] as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        grid.save(out)?;
        let mb = |b: usize| b as f64 / 1048576.0;
        println!(
            "rendered {} emoji in {:?}; private: before {:.1}MB, after init {:.1}MB, after render {:.1}MB",
            samples.len(),
            elapsed,
            mb(before),
            mb(after_init),
            mb(after)
        );
        // marginal cost: 100 more distinct emoji
        let t = std::time::Instant::now();
        for cp in 0x1f600u32..0x1f664 {
            let s = char::from_u32(cp).unwrap().to_string();
            let _ = r.render(&s, size);
        }
        println!("+100 emoji in {:?}: private {:.1}MB", t.elapsed(), mb(crate::alloc::private_bytes()));
        drop(r);
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("after dropping the renderer: private {:.1}MB", mb(crate::alloc::private_bytes()));
        let r = Renderer::new().map_err(|e| anyhow::anyhow!("init: {e}"))?;
        let t = std::time::Instant::now();
        let _ = r.render("😀", size);
        println!("re-create + 1 emoji in {:?}: private {:.1}MB", t.elapsed(), mb(crate::alloc::private_bytes()));
    }
    Ok(())
}

/// Makes sure the clusters are rendered (child process), then decodes them for the UI.
/// Blocking: call from a worker thread.
pub fn load(dir: &std::path::Path, clusters: &[String]) -> Vec<crate::types::Thumb> {
    if let Err(e) = render_missing(dir, clusters) {
        tracing::warn!("emoji render: {e}");
    }
    clusters
        .iter()
        .filter_map(|c| {
            let img = image::open(cache_path(dir, c)).ok()?.to_rgba8();
            Some(crate::types::Thumb {
                key: format!("{}{c}", crate::types::EMOJI_PREFIX),
                width: img.width(),
                height: img.height(),
                rgba: img.into_raw(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_emoji_only_messages() {
        assert_eq!(only_emoji("😀", 5).unwrap().len(), 1);
        assert_eq!(only_emoji("🎉🥳👍🏽", 5).unwrap(), vec!["🎉", "🥳", "👍🏽"]);
        assert_eq!(only_emoji(" ❤️ ", 5).unwrap(), vec!["❤️"]);
        assert_eq!(only_emoji("👨‍👩‍👧", 5).unwrap().len(), 1, "ZWJ family is one cluster");
        assert!(only_emoji("hi 😀", 5).is_none());
        assert!(only_emoji("ㅋㅋ", 5).is_none());
        assert!(only_emoji("123", 5).is_none());
        assert!(only_emoji("", 5).is_none());
        assert!(only_emoji("😀😀😀😀😀😀", 5).is_none(), "more than max");
    }

    #[test]
    fn cache_names_are_code_points() {
        let p = cache_path(std::path::Path::new("x"), "👍🏽");
        assert_eq!(p.file_name().unwrap(), "1f44d-1f3fd.png");
    }
}
