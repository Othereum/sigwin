//! Image decoding and resampling. Lives in its own crate because the `fast` profile builds the
//! main crate at opt-level 0 (slint's generated code), and generic/inlined pixel code is compiled
//! in the crate that calls it: unoptimized, a 260px Lanczos resample took ~37ms instead of ~1ms.

pub use image::{DynamicImage, ImageResult, RgbaImage};

/// Pixels ready for the UI. Filled here because their code is generic too.
pub type Pixels = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

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

/// Size of a `w` x `h` image shrunk to fit in `box_w` x `box_h` (never upscales).
pub fn fit_size((w, h): (u32, u32), (box_w, box_h): (u32, u32)) -> (u32, u32) {
    let s = (box_w as f64 / w.max(1) as f64).min(box_h as f64 / h.max(1) as f64).min(1.0);
    if s >= 1.0 {
        return (w, h);
    }
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

/// A still image for the viewer.
pub struct ViewImage {
    /// upright (EXIF), at full size
    pub original: Pixels,
    /// shrunk to fit the box asked for; `None` when the original already does
    pub fitted: Option<Pixels>,
    /// can't have transparent pixels
    pub opaque: bool,
}

/// Decodes a still image for the viewer. `None` when it is animated or has more than
/// `max_pixels`: those go to an outside app.
pub fn decode_view(bytes: &[u8], box_w: u32, box_h: u32, max_pixels: u64) -> ImageResult<Option<ViewImage>> {
    use image::ImageDecoder;
    let reader = || image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format();
    let (w, h) = reader()?.into_dimensions()?;
    if w as u64 * h as u64 > max_pixels || is_animated(bytes) {
        return Ok(None);
    }
    let mut decoder = reader()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let full = (img.width(), img.height());
    let opaque = !img.color().has_alpha();
    let out = fit_size(full, (box_w, box_h));
    let original = to_pixels(img);
    let crop = (0.0, 0.0, full.0 as f64, full.1 as f64);
    let fitted = if out == full { None } else { resize_view(original.as_bytes(), full, crop, out, opaque) };
    Ok(Some(ViewImage { original, fitted, opaque }))
}

/// The one conversion from a decoded image to what the UI draws: photos decode to RGB, and going
/// through RGBA first would write every pixel twice.
fn to_pixels(img: DynamicImage) -> Pixels {
    let mut out = Pixels::new(img.width(), img.height());
    let bytes = out.make_mut_bytes();
    match img {
        DynamicImage::ImageRgb8(rgb) => {
            for (d, s) in bytes.chunks_exact_mut(4).zip(rgb.as_raw().chunks_exact(3)) {
                d.copy_from_slice(&[s[0], s[1], s[2], 255]);
            }
        }
        other => bytes.copy_from_slice(other.into_rgba8().as_raw()),
    }
    out
}

/// Resamples the `crop` (left, top, width, height) of an RGBA image of `size` to exactly `out`:
/// Lanczos3 down, Catmull-Rom up (Lanczos rings visibly when enlarging). Integer SIMD, so much
/// faster than `image`'s resize and without its float buffers. `opaque` skips premultiplying,
/// which needs a copy of the source.
pub fn resize_view(src: &[u8], size: (u32, u32), crop: (f64, f64, f64, f64), out: (u32, u32), opaque: bool) -> Option<Pixels> {
    let mut dst = Pixels::new(out.0, out.1);
    resize_into(src, size, crop, out, dst.make_mut_bytes(), opaque)?;
    Some(dst)
}

fn resize_into(src: &[u8], size: (u32, u32), crop: (f64, f64, f64, f64), out: (u32, u32), dst: &mut [u8], opaque: bool) -> Option<()> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let shrinks = (out.0 as f64) * (out.1 as f64) < crop.2 * crop.3;
    let filter = if shrinks { FilterType::Lanczos3 } else { FilterType::CatmullRom };
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(filter))
        .crop(crop.0, crop.1, crop.2, crop.3)
        .use_alpha(!opaque);
    let src = ImageRef::new(size.0, size.1, src, PixelType::U8x4).ok()?;
    let mut dst = Image::from_slice_u8(out.0, out.1, dst, PixelType::U8x4).ok()?;
    Resizer::new().resize(&src, &mut dst, &options).ok()
}

/// High-quality downscale so the longer side is `px` (never upscales). Filters in premultiplied
/// alpha, otherwise transparent pixels' colors bleed into sticker edges as dark fringes.
pub fn downscale(img: RgbaImage, px: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= px || w == 0 || h == 0 {
        return img;
    }
    resample(img, px)
}

/// Resamples so the longer side is exactly `px`. For images drawn at a fixed size: the renderer
/// would scale them nearest-neighbor.
pub fn fit(img: RgbaImage, px: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w.max(h) == px || w == 0 || h == 0 {
        return img;
    }
    resample(img, px)
}

/// Resamples in premultiplied alpha so the longer side is `px`.
fn resample(img: RgbaImage, px: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let s = px as f32 / w.max(h) as f32;
    let (nw, nh) = (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1));
    let mut out = RgbaImage::new(nw, nh);
    match resize_into(img.as_raw(), (w, h), (0.0, 0.0, w as f64, h as f64), (nw, nh), &mut out, false) {
        Some(()) => out,
        None => img,
    }
}

/// Center-crops to the shape of `w` x `h` and resamples to exactly that. Cropped first: enlarging
/// from a crop can make `fast_image_resize` read out of bounds (see `viewer::patch_crop`).
pub fn cover(img: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let (sw, sh) = img.dimensions();
    if sw == 0 || sh == 0 || w == 0 || h == 0 {
        return img.clone();
    }
    let (cw, ch) = if sw as u64 * h as u64 > sh as u64 * w as u64 {
        (((sh as u64 * w as u64 / h as u64) as u32).max(1), sh)
    } else {
        (sw, ((sw as u64 * h as u64 / w as u64) as u32).max(1))
    };
    let cropped = image::imageops::crop_imm(img, (sw - cw) / 2, (sh - ch) / 2, cw, ch).to_image();
    let mut out = RgbaImage::new(w, h);
    match resize_into(cropped.as_raw(), (cw, ch), (0.0, 0.0, cw as f64, ch as f64), (w, h), &mut out, false) {
        Some(()) => out,
        None => cropped,
    }
}

#[test]
fn cover_fills_the_size_from_the_center() {
    // red | green | red: only the middle survives a square cut of a wide image
    let wide = RgbaImage::from_fn(300, 100, |x, _| image::Rgba(if (100..200).contains(&x) { [0, 255, 0, 255] } else { [255, 0, 0, 255] }));
    let cell = cover(&wide, 50, 50);
    assert_eq!(cell.dimensions(), (50, 50));
    assert_eq!(cell.get_pixel(25, 25).0, [0, 255, 0, 255]);
    assert_eq!(cell.get_pixel(1, 25).0, [0, 255, 0, 255]);
    // enlarges, and takes extreme shapes
    for (w, h, sw, sh) in [(400, 402, 37, 41), (200, 100, 1, 500), (100, 200, 500, 1), (301, 150, 300, 150)] {
        let src = RgbaImage::from_pixel(sw, sh, image::Rgba([9, 9, 9, 255]));
        assert_eq!(cover(&src, w, h).dimensions(), (w, h));
    }
}

/// Center-crops to a square and resamples it to exactly `px` (smaller sources are upscaled
/// smoothly instead of letting the renderer pixelate them).
pub fn square(img: &DynamicImage, px: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let s = w.min(h);
    fit(img.crop_imm((w - s) / 2, (h - s) / 2, s, s).to_rgba8(), px)
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
    fn fit_survives_every_ratio() {
        // the resizer has crashed on sizes that line pixels up exactly
        for w in 1..40u32 {
            for h in [1, 7, w, w * 3] {
                let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 200, (x * 9 + y * 5) as u8]));
                for px in 1..90 {
                    let out = fit(img.clone(), px);
                    assert_eq!(out.width().max(out.height()), px, "{w}x{h} to {px}");
                }
            }
        }
    }

    #[test]
    fn decode_view_fits_never_upscales_and_refuses() {
        let png = png_rgba(&RgbaImage::new(400, 200)).unwrap();
        let v = decode_view(&png, 100, 100, u64::MAX).unwrap().unwrap();
        let size = |p: &Pixels| (p.width(), p.height());
        assert_eq!((size(&v.original), size(&v.fitted.unwrap()), v.opaque), ((400, 200), (100, 50), false));
        let v = decode_view(&png, 1000, 1000, u64::MAX).unwrap().unwrap();
        assert_eq!((size(&v.original), v.fitted.is_none()), ((400, 200), true));
        assert!(decode_view(&png, 100, 100, 79_999).unwrap().is_none());
        assert!(decode_view(&gif(3, 50), 100, 100, u64::MAX).unwrap().is_none());
        assert!(decode_view(b"junk", 100, 100, u64::MAX).is_err());
    }

    #[test]
    fn resize_view_takes_the_crop_at_the_exact_size() {
        // left half red, right half blue
        let src = RgbaImage::from_fn(40, 20, |x, _| image::Rgba(if x < 20 { [255, 0, 0, 255] } else { [0, 0, 255, 255] }));
        for out in [(10, 10), (80, 80)] {
            let img = resize_view(src.as_raw(), (40, 20), (20.0, 0.0, 20.0, 20.0), out, true).unwrap();
            assert_eq!((img.width(), img.height()), out);
            let middle = (out.1 / 2 * out.0 + out.0 / 2) as usize * 4;
            assert_eq!(img.as_bytes()[middle..middle + 4], [0, 0, 255, 255]);
        }
    }

    #[test]
    fn decode_view_converts_to_the_same_pixels() {
        let rgb = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8 * 10, y as u8 * 20, 30]));
        let rgba = DynamicImage::ImageRgb8(rgb.clone()).into_rgba8();
        assert_eq!(to_pixels(DynamicImage::ImageRgb8(rgb)).as_bytes(), rgba.as_raw().as_slice());
        let gray = image::GrayImage::from_pixel(2, 2, image::Luma([7]));
        assert_eq!(to_pixels(DynamicImage::ImageLuma8(gray)).as_bytes(), [7, 7, 7, 255].repeat(4).as_slice());
        let clear = RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 4]));
        assert_eq!(to_pixels(DynamicImage::ImageRgba8(clear)).as_bytes(), [1, 2, 3, 4]);
    }

    #[test]
    fn square_is_exact_size() {
        assert_eq!(square(&DynamicImage::new_rgba8(300, 200), 40).dimensions(), (40, 40));
        assert_eq!(square(&DynamicImage::new_rgba8(20, 30), 40).dimensions(), (40, 40));
    }
}
