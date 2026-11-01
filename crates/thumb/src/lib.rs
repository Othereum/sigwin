//! Image decoding and resampling. Lives in its own crate because the `fast` profile builds the
//! main crate at opt-level 0 (slint's generated code), and generic/inlined pixel code is compiled
//! in the crate that calls it: unoptimized, a 260px Lanczos resample took ~37ms instead of ~1ms.

pub use image::{DynamicImage, ImageResult, RgbaImage};

/// Decodes, sniffing the format (file names and content types may lie).
pub fn decode(bytes: &[u8]) -> ImageResult<DynamicImage> {
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.decode()
}

/// Decodes and shrinks to fit in `max` x `max` (never upscales).
pub fn decode_thumb(bytes: &[u8], max: u32) -> ImageResult<DynamicImage> {
    Ok(decode(bytes)?.thumbnail(max, max))
}

/// Decodes to RGBA.
pub fn decode_rgba(bytes: &[u8]) -> ImageResult<RgbaImage> {
    Ok(decode(bytes)?.to_rgba8())
}

/// Encodes as PNG in memory (cache files are encrypted before they reach the disk).
pub fn png(img: &DynamicImage) -> ImageResult<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

pub fn png_rgba(img: &RgbaImage) -> ImageResult<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)?;
    Ok(out)
}

pub fn to_rgba(img: &DynamicImage) -> RgbaImage {
    img.to_rgba8()
}

/// High-quality downscale so the longer side is `px` (never upscales). Filters in premultiplied
/// alpha, otherwise transparent pixels' colors bleed into sticker edges as dark fringes.
pub fn downscale(img: RgbaImage, px: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= px || w == 0 || h == 0 {
        return img;
    }
    resample(img, px, image::imageops::FilterType::Lanczos3)
}

/// Resamples so the longer side is exactly `px`: down with Lanczos3, up with Catmull-Rom (no
/// ringing). For images drawn at a fixed size: the renderer would scale them nearest-neighbor.
pub fn fit(img: RgbaImage, px: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w.max(h) == px || w == 0 || h == 0 {
        return img;
    }
    if w.max(h) > px {
        return resample(img, px, image::imageops::FilterType::Lanczos3);
    }
    resample(img, px, image::imageops::FilterType::CatmullRom)
}

/// Resamples in premultiplied alpha so the longer side is `px`.
fn resample(img: RgbaImage, px: u32, filter: image::imageops::FilterType) -> RgbaImage {
    let (w, h) = img.dimensions();
    let s = px as f32 / w.max(h) as f32;
    let (nw, nh) = (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1));
    let mut pm = image::Rgba32FImage::new(w, h);
    for (d, p) in pm.pixels_mut().zip(img.pixels()) {
        let a = p[3] as f32 / 255.0;
        *d = image::Rgba([p[0] as f32 / 255.0 * a, p[1] as f32 / 255.0 * a, p[2] as f32 / 255.0 * a, a]);
    }
    let small = image::imageops::resize(&pm, nw, nh, filter);
    let mut out = RgbaImage::new(nw, nh);
    for (d, p) in out.pixels_mut().zip(small.pixels()) {
        let a = p[3].clamp(0.0, 1.0);
        let un = |c: f32| if a > 0.0 { (c / a).clamp(0.0, 1.0) * 255.0 + 0.5 } else { 0.0 };
        *d = image::Rgba([un(p[0]) as u8, un(p[1]) as u8, un(p[2]) as u8, (a * 255.0 + 0.5) as u8]);
    }
    out
}

/// Center-crops to a square and resamples it to exactly `px` (smaller sources are upscaled
/// smoothly instead of letting the renderer pixelate them).
pub fn square(img: &DynamicImage, px: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let s = w.min(h);
    let sq = img.crop_imm((w - s) / 2, (h - s) / 2, s, s).to_rgba8();
    let out = downscale(sq, px);
    if out.width() == px {
        return out;
    }
    image::imageops::resize(&out, px, px, image::imageops::FilterType::CatmullRom)
}

/// Frames of an animated image, resampled, with each frame's display time in µs (APNG times
/// are fractions: 1/30 s must stay 33333µs, not 33ms, or the cadence drifts).
pub struct Animation {
    pub frames: Vec<RgbaImage>,
    pub delays_us: Vec<u32>,
}

/// Decoding stops here: long animations would take seconds to resample.
const MAX_FRAMES: usize = 500;

/// Decodes an animated WebP / APNG / GIF, each frame resampled to `px` (see `fit`).
/// `None` when the image is a still (one frame, or a format without animation). If the frames
/// would take more than `max_bytes`, every other frame is dropped (its time goes to the one before)
/// until they fit.
pub fn animation(bytes: &[u8], px: u32, max_bytes: usize) -> ImageResult<Option<Animation>> {
    use image::AnimationDecoder;
    use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let cur = || std::io::Cursor::new(bytes);
    let frames = match reader.format() {
        Some(image::ImageFormat::WebP) => {
            let d = WebPDecoder::new(cur())?;
            if !d.has_animation() {
                return Ok(None);
            }
            d.into_frames()
        }
        Some(image::ImageFormat::Png) => {
            let d = PngDecoder::new(cur())?;
            if !d.is_apng()? {
                return Ok(None);
            }
            d.apng()?.into_frames()
        }
        Some(image::ImageFormat::Gif) => GifDecoder::new(cur())?.into_frames(),
        _ => return Ok(None),
    };
    let mut anim = Animation { frames: Vec::new(), delays_us: Vec::new() };
    for f in frames.take(MAX_FRAMES) {
        let f = f?;
        anim.delays_us.push(delay_us(f.delay()));
        anim.frames.push(fit(f.into_buffer(), px));
    }
    if anim.frames.len() < 2 {
        return Ok(None);
    }
    let size = |a: &Animation| a.frames.iter().map(|f| f.as_raw().len()).sum::<usize>();
    while size(&anim) > max_bytes && anim.frames.len() > 1 {
        let (mut frames, mut delays) = (Vec::new(), Vec::new());
        for (i, (f, d)) in anim.frames.into_iter().zip(anim.delays_us).enumerate() {
            if i % 2 == 0 {
                frames.push(f);
                delays.push(d);
            } else if let Some(last) = delays.last_mut() {
                *last += d;
            }
        }
        anim = Animation { frames, delays_us: delays };
    }
    Ok(Some(anim))
}

/// A frame's display time in µs. Like browsers, "as fast as possible" (<= 10ms) plays at 10 fps.
fn delay_us(delay: image::Delay) -> u32 {
    let (n, d) = delay.numer_denom_ms();
    let us = if d == 0 { 0 } else { (n as u64 * 1000 / d as u64).min(u32::MAX as u64) as u32 };
    if us <= 10_000 { 100_000 } else { us }
}

/// An endlessly looping GIF (demo stickers, tests).
pub fn gif(frames: Vec<RgbaImage>, delay_ms: u32) -> ImageResult<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite)?;
        let delay = image::Delay::from_numer_denom_ms(delay_ms, 1);
        enc.encode_frames(frames.into_iter().map(|f| image::Frame::from_parts(f, 0, 0, delay)))?;
    }
    Ok(out)
}

/// Whether `bytes` hold an animation, from the headers (GIFs decode their first two frames).
pub fn is_animated(bytes: &[u8]) -> bool {
    use image::AnimationDecoder;
    use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
    let cur = || std::io::Cursor::new(bytes);
    let Ok(reader) = image::ImageReader::new(cur()).with_guessed_format() else { return false };
    match reader.format() {
        Some(image::ImageFormat::WebP) => WebPDecoder::new(cur()).is_ok_and(|d| d.has_animation()),
        Some(image::ImageFormat::Png) => PngDecoder::new(cur()).is_ok_and(|d| d.is_apng().unwrap_or(false)),
        Some(image::ImageFormat::Gif) => GifDecoder::new(cur()).is_ok_and(|d| d.into_frames().take(2).count() == 2),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif(frames: u32, delay_ms: u32) -> Vec<u8> {
        let fs = (0..frames).map(|i| RgbaImage::from_pixel(64, 64, image::Rgba([(i * 40) as u8, 0, 0, 255])));
        super::gif(fs.collect(), delay_ms).unwrap()
    }

    #[test]
    fn decodes_animated_gif_and_skips_stills() {
        let a = animation(&gif(4, 50), 32, usize::MAX).unwrap().unwrap();
        assert_eq!(a.frames.len(), 4);
        assert_eq!(a.frames[0].dimensions(), (32, 32));
        assert_eq!(a.delays_us, vec![50_000; 4]);
        assert!(animation(&gif(1, 50), 32, usize::MAX).unwrap().is_none());
        let png = png_rgba(&RgbaImage::new(8, 8)).unwrap();
        assert!(animation(&png, 32, usize::MAX).unwrap().is_none());
        assert!(is_animated(&gif(3, 50)));
        assert!(!is_animated(&gif(1, 50)) && !is_animated(&png) && !is_animated(b"junk"));
    }

    #[test]
    fn keeps_fractional_delays() {
        // APNG stores 1/30 s; image reports it in ms as 100/3
        assert_eq!(delay_us(image::Delay::from_numer_denom_ms(100, 3)), 33_333);
        assert_eq!(delay_us(image::Delay::from_numer_denom_ms(0, 1)), 100_000);
        assert_eq!(delay_us(image::Delay::from_numer_denom_ms(20, 1)), 20_000);
    }

    #[test]
    fn drops_frames_to_fit_the_budget() {
        // 4 frames of 32x32 RGBA = 4 x 4096 bytes; room for 2
        let a = animation(&gif(4, 50), 32, 2 * 4096).unwrap().unwrap();
        assert_eq!(a.frames.len(), 2);
        assert_eq!(a.delays_us, vec![100_000, 100_000]);
    }


    #[test]
    fn downscale_fits_long_side_and_keeps_transparency() {
        // opaque red disc on a transparent background with garbage (green) color in the clear pixels
        let img = RgbaImage::from_fn(260, 200, |x, y| {
            let (dx, dy) = (x as f32 - 130.0, y as f32 - 100.0);
            if dx * dx + dy * dy < 80.0 * 80.0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 255, 0, 0]) }
        });
        let out = downscale(img, 130);
        assert_eq!(out.dimensions(), (130, 100));
        assert_eq!(out.get_pixel(0, 0)[3], 0);
        // edge pixels must not pick up the green of transparent neighbours
        assert!(out.pixels().all(|p| p[3] == 0 || p[1] < 8), "green fringe");
        assert_eq!(downscale(RgbaImage::new(50, 40), 130).dimensions(), (50, 40));
    }

    #[test]
    fn fit_scales_both_ways_without_fringes() {
        let disc = |s: u32| {
            RgbaImage::from_fn(s, s, |x, y| {
                let (dx, dy) = (x as f32 - s as f32 / 2.0, y as f32 - s as f32 / 2.0);
                if dx * dx + dy * dy < (s as f32 / 3.0).powi(2) { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 255, 0, 0]) }
            })
        };
        let up = fit(disc(112), 140);
        assert_eq!(up.dimensions(), (140, 140));
        assert!(up.pixels().all(|p| p[3] == 0 || p[1] < 8), "green fringe");
        // smooth edge: some partly transparent pixels
        assert!(up.pixels().any(|p| p[3] > 0 && p[3] < 255));
        assert_eq!(fit(disc(260), 140).dimensions(), (140, 140));
        assert_eq!(fit(RgbaImage::new(70, 35), 140).dimensions(), (140, 70));
    }

    #[test]
    fn square_is_exact_size() {
        assert_eq!(square(&DynamicImage::new_rgba8(300, 200), 40).dimensions(), (40, 40));
        assert_eq!(square(&DynamicImage::new_rgba8(20, 30), 40).dimensions(), (40, 40));
    }
}
