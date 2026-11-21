//! Image decoding and resampling. Lives in its own crate because the `fast` profile builds the
//! main crate at opt-level 0 (slint's generated code), and generic/inlined pixel code is compiled
//! in the crate that calls it: unoptimized, a 260px Lanczos resample took ~37ms instead of ~1ms.

mod avif;
pub mod photo;

pub use image::{DynamicImage, ImageResult, RgbaImage};

/// Pixels ready for the UI. Filled here because their code is generic too.
pub type Pixels = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// A ring as wide as `size`, its line `width` thick: white clockwise from the top for `percent` of
/// the way, faint for the rest, smooth at the edges and clear inside and out. It's drawn here
/// because the software renderer can't cut a gradient to a circle.
pub fn progress_ring(size: u32, width: f32, percent: u8) -> Pixels {
    use std::f32::consts::TAU;
    const FAINT: f32 = 0.25;
    let mut out = Pixels::new(size, size);
    let outer = size as f32 / 2.0;
    let inner = outer - width;
    let done = percent.min(100) as f32 / 100.0 * TAU;
    for (i, px) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32 + 0.5 - outer, (i as u32 / size) as f32 + 0.5 - outer);
        let r = x.hypot(y);
        let cover = (outer - r + 0.5).clamp(0.0, 1.0) * (r - inner + 0.5).clamp(0.0, 1.0);
        // how far along the line this pixel is inside the done part, in pixels
        let angle = x.atan2(-y).rem_euclid(TAU);
        let along = match percent {
            0 => 0.0,
            100.. => 1.0,
            _ => ((done - angle) * r + 0.5).clamp(0.0, 1.0),
        };
        let alpha = cover * (FAINT + (1.0 - FAINT) * along);
        *px = slint::Rgba8Pixel { r: 255, g: 255, b: 255, a: (alpha * 255.0 + 0.5) as u8 };
    }
    out
}

/// The most pixels anything here decodes. A header can claim any size in a few bytes, and the
/// buffer for it is allocated before any pixel is read: one that can't be allocated aborts the
/// app. As large as the viewer and sending take.
pub const MAX_PIXELS: u64 = 100_000_000;

/// What an animation decoder may allocate at once: a canvas, the one kept for disposal, a frame
/// and the composite, each up to `MAX_PIXELS` in RGBA. GIF frames may be larger than the screen.
const ANIMATION_MAX_ALLOC: u64 = 4 * 4 * MAX_PIXELS;

fn too_big() -> image::ImageError {
    image::ImageError::Limits(image::error::LimitError::from_kind(image::error::LimitErrorKind::DimensionError))
}

/// Refuses a decoder whose image has more than `max_pixels` (never more than `MAX_PIXELS`).
fn within<D: image::ImageDecoder + ?Sized>(decoder: &D, max_pixels: u64) -> ImageResult<()> {
    let (w, h) = decoder.dimensions();
    if w as u64 * h as u64 > max_pixels.min(MAX_PIXELS) { Err(too_big()) } else { Ok(()) }
}

/// A decoder for what `bytes` hold, sniffing the format (file names and content types may lie).
/// Header only: check its size before decoding, or use `decoder`.
fn open(bytes: &[u8]) -> ImageResult<Box<dyn image::ImageDecoder + '_>> {
    if avif::is_avif(bytes) {
        return Ok(Box::new(avif::AvifDecoder::new(bytes)?));
    }
    Ok(Box::new(image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.into_decoder()?))
}

/// `open`, refusing more than `max_pixels`.
fn decoder(bytes: &[u8], max_pixels: u64) -> ImageResult<Box<dyn image::ImageDecoder + '_>> {
    let d = open(bytes)?;
    within(&*d, max_pixels)?;
    Ok(d)
}

/// `open` for a file, reading only what the format needs.
fn open_file(path: &std::path::Path) -> ImageResult<Box<dyn image::ImageDecoder>> {
    use std::io::Read;
    let mut head = Vec::with_capacity(256);
    std::fs::File::open(path)?.take(256).read_to_end(&mut head)?;
    if avif::is_avif(&head) {
        return Ok(Box::new(avif::AvifDecoder::new(&std::fs::read(path)?)?));
    }
    Ok(Box::new(image::ImageReader::open(path)?.with_guessed_format()?.into_decoder()?))
}

/// Decodes, sniffing the format (file names and content types may lie).
pub fn decode(bytes: &[u8]) -> ImageResult<DynamicImage> {
    decode_within(bytes, MAX_PIXELS)
}

/// `decode`, refusing more than `max_pixels`.
pub fn decode_within(bytes: &[u8], max_pixels: u64) -> ImageResult<DynamicImage> {
    DynamicImage::from_decoder(decoder(bytes, max_pixels)?)
}

/// Decodes upright and resizes to fit in `max` x `max`, enlarging a smaller image too.
pub fn decode_thumb(bytes: &[u8], max: u32) -> ImageResult<DynamicImage> {
    decode_thumb_for(bytes, max, |_| max)
}

/// Decodes upright and resizes so its longer side is what `side` asks for its size, as far as
/// the image has it, and at least `floor` (enlarging a smaller image to that, like `decode_thumb`).
pub fn decode_thumb_for(bytes: &[u8], floor: u32, side: impl FnOnce((u32, u32)) -> u32) -> ImageResult<DynamicImage> {
    let mut decoder = decoder(bytes, MAX_PIXELS)?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation()?;
    let own = w.max(h);
    let max = side(if turns(orientation) { (h, w) } else { (w, h) }).min(own).max(floor);
    let mut img = match jpeg_at_least(bytes, (w, h), max) {
        Some(img) => img,
        None => DynamicImage::from_decoder(decoder)?,
    };
    img.apply_orientation(orientation);
    Ok(if img.width().max(img.height()) == max { img } else { img.thumbnail(max, max) })
}

/// A JPEG of `size` decoded at the smallest of 1/8, 1/4 and 1/2 that keeps `long` px on its
/// longer side, never whole first: a 12MP photo is 36MB decoded, a quarter of it 2MB. `None`
/// when no smaller scale keeps that, or it isn't a JPEG this decodes (CMYK, lossless, broken):
/// it is decoded whole then.
fn jpeg_at_least(bytes: &[u8], (w, h): (u32, u32), long: u32) -> Option<DynamicImage> {
    use jpeg_decoder::PixelFormat;
    let own = w.max(h);
    if !bytes.starts_with(&[0xFF, 0xD8]) || long.saturating_mul(2) > own {
        return None;
    }
    let at = |side: u32| u16::try_from((u64::from(side) * u64::from(long)).div_ceil(u64::from(own))).ok();
    let mut d = jpeg_decoder::Decoder::new(bytes);
    let (sw, sh) = d.scale(at(w)?, at(h)?).ok()?;
    let px = d.decode().ok()?;
    let (sw, sh) = (u32::from(sw), u32::from(sh));
    match d.info()?.pixel_format {
        PixelFormat::RGB24 => image::RgbImage::from_raw(sw, sh, px).map(DynamicImage::ImageRgb8),
        PixelFormat::L8 => image::GrayImage::from_raw(sw, sh, px).map(DynamicImage::ImageLuma8),
        PixelFormat::L16 | PixelFormat::CMYK32 => None,
    }
}

/// Whether an image stored with `o` is shown turned a quarter, its sides swapped.
fn turns(o: image::metadata::Orientation) -> bool {
    use image::metadata::Orientation::{Rotate90, Rotate90FlipH, Rotate270, Rotate270FlipH};
    matches!(o, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH)
}

/// The longer side an image of `size` needs to fill `w` x `h` (cut around its center, as
/// `cover` does) without being enlarged.
pub fn cover_side((sw, sh): (u32, u32), (w, h): (u32, u32)) -> u32 {
    if sw == 0 || sh == 0 {
        return w.max(h);
    }
    let long = u64::from(sw.max(sh));
    let need = (long * u64::from(w)).div_ceil(u64::from(sw)).max((long * u64::from(h)).div_ceil(u64::from(sh)));
    u32::try_from(need).unwrap_or(u32::MAX)
}

#[test]
fn cover_side_is_just_enough() {
    // a wide photo in a taller box is held by its height
    assert_eq!(cover_side((4000, 3000), (600, 450)), 600);
    assert_eq!(cover_side((4000, 2000), (600, 450)), 900);
    assert_eq!(cover_side((450, 600), (300, 300)), 600 * 300 / 450);
    // made at that size, cutting it never enlarges it
    for (size, target) in [((4000, 3000), (601, 449)), ((1000, 7), (150, 150)), ((333, 1000), (253, 450))] {
        let side = cover_side(size, target);
        // what `thumbnail` gives
        let ratio = f64::from(side) / f64::from(size.0.max(size.1));
        let (w, h) = ((f64::from(size.0) * ratio).round() as u32, (f64::from(size.1) * ratio).round() as u32);
        assert!(w >= target.0 && h >= target.1, "{size:?} {target:?}: {w}x{h}");
    }
}

/// A photo being sent: a file read as it is, or one already in memory.
pub enum Photo<'a> {
    File(&'a std::path::Path),
    Bytes(&'a [u8]),
}

impl Photo<'_> {
    /// Header only, like `open`.
    fn open(&self) -> ImageResult<Box<dyn image::ImageDecoder + '_>> {
        match self {
            Photo::File(path) => open_file(path),
            Photo::Bytes(bytes) => open(bytes),
        }
    }
}

/// A photo's size as shown (EXIF applied), from its header alone.
pub fn photo_size(photo: Photo) -> ImageResult<(u32, u32)> {
    let mut decoder = photo.open()?;
    let (w, h) = decoder.dimensions();
    Ok(if turns(decoder.orientation()?) { (h, w) } else { (w, h) })
}

/// A photo's size as shown (EXIF applied) and its blurhash. Past `max_pixels` (or `MAX_PIXELS`)
/// it isn't decoded and has no blurhash, nor does one that turns out broken after its header.
pub fn photo_facts(photo: Photo, max_pixels: u64) -> ImageResult<((u32, u32), Option<String>)> {
    let mut decoder = photo.open()?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation()?;
    let size = if turns(orientation) { (h, w) } else { (w, h) };
    if within(&*decoder, max_pixels).is_err() {
        return Ok((size, None));
    }
    let Ok(mut img) = DynamicImage::from_decoder(decoder) else { return Ok((size, None)) };
    img.apply_orientation(orientation);
    Ok((size, encode_blurhash(&img.thumbnail(64, 64).to_rgba8())))
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

/// An image from rows of BGRX pixels, the order Windows video decoders give.
pub fn from_bgrx(mut px: Vec<u8>, w: u32, h: u32) -> Option<RgbaImage> {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 255;
    }
    RgbaImage::from_raw(w, h, px)
}

/// Turns clockwise by 90, 180 or 270 degrees; other angles leave it as it is.
pub fn rotated(img: RgbaImage, degrees: u32) -> RgbaImage {
    match degrees {
        90 => image::imageops::rotate90(&img),
        180 => image::imageops::rotate180(&img),
        270 => image::imageops::rotate270(&img),
        _ => img,
    }
}

const BASE83: &[u8; 83] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~";

fn base83(s: &[u8]) -> Option<u32> {
    s.iter().try_fold(0u32, |n, c| Some(n * 83 + BASE83.iter().position(|b| b == c)? as u32))
}

fn push_base83(out: &mut String, v: u32, digits: u32) {
    for d in (0..digits).rev() {
        out.push(BASE83[(v / 83u32.pow(d) % 83) as usize] as char);
    }
}

fn srgb_to_linear(v: u8) -> f32 {
    let v = v as f32 / 255.0;
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let v = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (v * 255.0 + 0.5) as u8
}

/// Per pixel, `n` cosines: the basis of a blurhash along one side.
fn cos_table(n: usize, len: u32) -> Vec<f32> {
    use std::f32::consts::PI;
    (0..len).flat_map(|p| (0..n).map(move |i| (PI * p as f32 * i as f32 / len as f32).cos())).collect()
}

/// A blurhash of 4 x 3 components, what Signal's clients send. Costs a little for every pixel, so
/// give it a small image. `None` for an empty one.
pub fn encode_blurhash(img: &RgbaImage) -> Option<String> {
    const NX: usize = 4;
    const NY: usize = 3;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let (cos_x, cos_y) = (cos_table(NX, w), cos_table(NY, h));
    let mut sums = [[0f32; 3]; NX * NY];
    for (x, y, p) in img.enumerate_pixels() {
        let rgb = [srgb_to_linear(p[0]), srgb_to_linear(p[1]), srgb_to_linear(p[2])];
        for (n, sum) in sums.iter_mut().enumerate() {
            let basis = cos_x[x as usize * NX + n % NX] * cos_y[y as usize * NY + n / NX];
            for (s, v) in sum.iter_mut().zip(rgb) {
                *s += v * basis;
            }
        }
    }
    let area = w as f32 * h as f32;
    let dc = sums[0].map(|v| v / area);
    let ac = sums[1..].iter().map(|c| c.map(|v| v * 2.0 / area));
    let largest = ac.clone().flatten().fold(0f32, |m, v| m.max(v.abs()));
    let max_digit = ((largest * 166.0 - 0.5).floor() as i32).clamp(0, 82) as u32;
    let max = (max_digit + 1) as f32 / 166.0;
    let level = |v: f32| {
        let v = v / max;
        ((v.signum() * v.abs().sqrt() * 9.0 + 9.5).floor() as i32).clamp(0, 18) as u32
    };

    let mut out = String::with_capacity(4 + 2 * NX * NY);
    push_base83(&mut out, (NX - 1 + (NY - 1) * 9) as u32, 1);
    push_base83(&mut out, max_digit, 1);
    let [r, g, b] = dc.map(|v| linear_to_srgb(v) as u32);
    push_base83(&mut out, r << 16 | g << 8 | b, 4);
    for [r, g, b] in ac {
        push_base83(&mut out, level(r) * 19 * 19 + level(g) * 19 + level(b), 2);
    }
    Some(out)
}

/// Draws a blurhash at `w` x `h`. It comes from the sender, so anything malformed is `None`.
pub fn blurhash(hash: &str, w: u32, h: u32) -> Option<RgbaImage> {
    let s = hash.as_bytes();
    let size = base83(s.get(..1)?)? as usize;
    let (nx, ny) = (size % 9 + 1, size / 9 + 1);
    if s.len() != 4 + 2 * nx * ny || w == 0 || h == 0 {
        return None;
    }
    let max = (base83(&s[1..2])? + 1) as f32 / 166.0;
    let dc = base83(&s[2..6])?;
    let mut colors = vec![[srgb_to_linear((dc >> 16) as u8), srgb_to_linear((dc >> 8) as u8), srgb_to_linear(dc as u8)]];
    for pair in s[6..].chunks_exact(2) {
        let v = base83(pair)?;
        let ac = |q: u32| {
            let q = (q as f32 - 9.0) / 9.0;
            q * q.abs() * max
        };
        colors.push([ac(v / (19 * 19)), ac(v / 19 % 19), ac(v % 19)]);
    }
    let (cos_x, cos_y) = (cos_table(nx, w), cos_table(ny, h));
    Some(RgbaImage::from_fn(w, h, |x, y| {
        let mut c = [0f32; 3];
        for j in 0..ny {
            for i in 0..nx {
                let basis = cos_x[x as usize * nx + i] * cos_y[y as usize * ny + j];
                for (out, v) in c.iter_mut().zip(colors[i + j * nx]) {
                    *out += v * basis;
                }
            }
        }
        image::Rgba([linear_to_srgb(c[0]), linear_to_srgb(c[1]), linear_to_srgb(c[2]), 255])
    }))
}

#[test]
fn blurhash_draws_and_refuses_malformed() {
    // one component: 0xFF8000 everywhere (base83 of 16744448)
    let n = 0xFF8000u32;
    let digits: Vec<u8> = [83u32.pow(3), 83u32.pow(2), 83, 1].iter().map(|d| BASE83[(n / d % 83) as usize]).collect();
    let flat = format!("00{}", String::from_utf8(digits).unwrap());
    let img = blurhash(&flat, 4, 3).unwrap();
    assert_eq!(img.dimensions(), (4, 3));
    assert_eq!(img.get_pixel(3, 2).0, [255, 128, 0, 255]);
    // the format's own example: dark at the bottom right, no panic on 4 x 3 components
    let img = blurhash("LEHV6nWB2yk8pyo0adR*.7kCMdnj", 32, 32).unwrap();
    assert_eq!(img.dimensions(), (32, 32));
    for bad in ["", "L", "LEHV6nWB2yk8pyo0adR*.7kCMdn", "LEHV6nWB2yk8pyo0adR*.7kCMdnjj", "LEHV6nWB2yk8pyo0adR*.7kCMd한", "\u{1F600}0000000"] {
        assert!(blurhash(bad, 32, 32).is_none(), "{bad}");
    }
    assert!(blurhash("LEHV6nWB2yk8pyo0adR*.7kCMdnj", 0, 32).is_none());
}

#[test]
fn progress_ring_fills_clockwise_from_the_top() {
    let alpha = |percent, x: usize, y: usize| progress_ring(40, 3.0, percent).as_slice()[y * 40 + x].a;
    // on the line: right, bottom and left of the center
    let (right, bottom, left) = ((38, 20), (20, 38), (1, 20));
    let at = |percent, (x, y)| alpha(percent, x, y);
    assert_eq!(at(0, right), 64);
    assert_eq!(at(0, left), 64);
    assert_eq!(at(30, right), 255);
    assert_eq!(at(30, bottom), 64);
    assert_eq!(at(60, bottom), 255);
    assert_eq!(at(60, left), 64);
    assert_eq!(at(100, left), 255);
    // clear in the corners and the middle, whatever the percent
    for percent in [0, 50, 100] {
        assert_eq!(alpha(percent, 0, 0), 0);
        assert_eq!(alpha(percent, 39, 39), 0);
        assert_eq!(alpha(percent, 20, 20), 0);
    }
    // nothing bright just left of the top at 0, and the start of the line isn't cut at 100
    assert_eq!(alpha(0, 19, 1), 64);
    assert_eq!(alpha(100, 19, 1), 255);
}

#[test]
fn blurhash_encodes_like_signal_desktop() {
    let gradient = |w, h| RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 31) as u8, (y * 27) as u8, (x * y * 5) as u8, 255]));
    // from the library Signal Desktop uses (blurhash 2.0.5), same pixels, but with the largest
    // absolute component as the format's C and Swift encoders have it: that library takes the
    // largest signed one, which clips strong negatives (these two would start "LC" and "LE")
    assert_eq!(encode_blurhash(&gradient(8, 6)).unwrap(), "LgFNSP2,a{xZz=NffQnje.f8fQf6");
    assert_eq!(encode_blurhash(&gradient(5, 9)).unwrap(), "Lf8?ZQKtSKx@l[Vza~iyf%fjfQfj");
    let flat = RgbaImage::from_pixel(2, 2, image::Rgba([255, 128, 0, 255]));
    assert_eq!(encode_blurhash(&flat).unwrap(), "L~TNoS}.fQ}.}.}VfQ}VfQfQfQfQ");
    assert!(encode_blurhash(&RgbaImage::new(0, 0)).is_none());
    // what we send, we can draw
    assert!(blurhash(&encode_blurhash(&gradient(8, 6)).unwrap(), 4, 3).is_some());
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
/// `max_pixels` (or `MAX_PIXELS`): those go to an outside app.
pub fn decode_view(bytes: &[u8], box_w: u32, box_h: u32, max_pixels: u64) -> ImageResult<Option<ViewImage>> {
    let mut decoder = open(bytes)?;
    if within(&*decoder, max_pixels).is_err() || is_animated(bytes) {
        return Ok(None);
    }
    let orientation = decoder.orientation()?;
    let (original, opaque) = match jpeg_rgba(bytes, orientation) {
        Some(p) => (p, true),
        None => {
            let mut img = DynamicImage::from_decoder(decoder)?;
            img.apply_orientation(orientation);
            let opaque = !img.color().has_alpha();
            (to_pixels(img), opaque)
        }
    };
    let full = (original.width(), original.height());
    let out = fit_size(full, (box_w, box_h));
    let crop = (0.0, 0.0, full.0 as f64, full.1 as f64);
    let fitted = if out == full { None } else { resize_view(original.as_bytes(), full, crop, out, opaque) };
    Ok(Some(ViewImage { original, fitted, opaque }))
}

/// An upright JPEG decoded straight into what the UI draws. Decoded to RGB and widened, both were
/// held at once: 7 bytes a pixel instead of 4 (88MB instead of 50 for a 12MP photo). `None`
/// when its EXIF turns it (turning takes a second buffer anyway) or it isn't YCbCr or RGB: it is
/// decoded as other images are then.
fn jpeg_rgba(bytes: &[u8], orientation: image::metadata::Orientation) -> Option<Pixels> {
    use zune_core::colorspace::ColorSpace;
    if orientation != image::metadata::Orientation::NoTransforms || !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    // as `image` sets it up; the output can't change once the headers are read
    let options = zune_core::options::DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(usize::MAX)
        .set_max_height(usize::MAX)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(bytes), options);
    d.decode_headers().ok()?;
    if !matches!(d.input_colorspace()?, ColorSpace::YCbCr | ColorSpace::RGB) {
        return None;
    }
    let (w, h) = d.dimensions()?;
    let mut out = Pixels::new(u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    d.decode_into(out.make_mut_bytes()).ok()?;
    Some(out)
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

/// Center-crops to the shape of `w` x `h` and resamples to exactly that.
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
    let (left, top) = ((sw - cw) / 2, (sh - ch) / 2);
    let mut out = RgbaImage::new(w, h);
    match resize_into(img.as_raw(), (sw, sh), (left as f64, top as f64, cw as f64, ch as f64), (w, h), &mut out, false) {
        Some(()) => out,
        None => image::imageops::crop_imm(img, left, top, cw, ch).to_image(),
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

/// Decoding stops here: long animations would take seconds to resample.
const MAX_FRAMES: usize = 500;

type Cursor<'a> = std::io::Cursor<&'a [u8]>;

/// For the animation decoders below, which refuse a canvas past `MAX_PIXELS`: their own
/// constructors set no limits, and their frame iterators allocate a canvas per frame.
fn animation_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(ANIMATION_MAX_ALLOC);
    limits
}

fn gif_decoder(bytes: &[u8]) -> ImageResult<image::codecs::gif::GifDecoder<Cursor<'_>>> {
    use image::ImageDecoder;
    let mut d = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))?;
    within(&d, MAX_PIXELS)?;
    d.set_limits(animation_limits())?;
    Ok(d)
}

fn png_decoder(bytes: &[u8]) -> ImageResult<image::codecs::png::PngDecoder<Cursor<'_>>> {
    let d = image::codecs::png::PngDecoder::with_limits(std::io::Cursor::new(bytes), animation_limits())?;
    within(&d, MAX_PIXELS)?;
    Ok(d)
}

fn webp_decoder(bytes: &[u8]) -> ImageResult<image::codecs::webp::WebPDecoder<Cursor<'_>>> {
    let d = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes))?;
    within(&d, MAX_PIXELS)?;
    Ok(d)
}

/// The frames of an animated WebP / APNG / GIF; `None` for a still or another format.
fn frames(bytes: &[u8]) -> ImageResult<Option<image::Frames<'_>>> {
    use image::AnimationDecoder;
    Ok(match image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.format() {
        Some(image::ImageFormat::WebP) => {
            let d = webp_decoder(bytes)?;
            d.has_animation().then(|| d.into_frames())
        }
        Some(image::ImageFormat::Png) => {
            let d = png_decoder(bytes)?;
            if d.is_apng()? { Some(d.apng()?.into_frames()) } else { None }
        }
        Some(image::ImageFormat::Gif) => Some(gif_decoder(bytes)?.into_frames()),
        _ => None,
    })
}

/// An animated WebP / APNG / GIF as an APNG with every frame resampled to `px` (see `fit`),
/// looping forever. Slint's `Image` plays it, decoding one frame at a time, but draws frames
/// nearest-neighbor: they must come at the size shown. `None` for a still (one frame, or a
/// format without animation). Holds one frame at a time: a first pass only counts them, which
/// the APNG header needs before the first.
pub fn animation_png(bytes: &[u8], px: u32) -> ImageResult<Option<Vec<u8>>> {
    let Some(counted) = frames(bytes)? else { return Ok(None) };
    let n = counted.take(MAX_FRAMES).count();
    if n < 2 {
        return Ok(None);
    }
    let io = |e: png::EncodingError| image::ImageError::IoError(std::io::Error::other(e));
    // APNG keeps fractions: 1/30 s stays 33.3ms, so the cadence doesn't drift
    let resampled = |f: image::Frame| ((delay_us(f.delay()) / 100).min(u16::MAX as u32) as u16, fit(f.into_buffer(), px));
    let mut src = frames(bytes)?.into_iter().flatten().take(n);
    let Some(first) = src.next() else { return Ok(None) };
    let (delay, first) = resampled(first?);
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, first.width(), first.height());
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    // a few times faster than the default, ~15% bigger
    e.set_compression(png::Compression::Fast);
    e.set_animated(n as u32, 0).map_err(io)?;
    let mut w = e.write_header().map_err(io)?;
    w.set_frame_delay(delay, 10_000).map_err(io)?;
    w.write_image_data(first.as_raw()).map_err(io)?;
    for f in src {
        let (delay, img) = resampled(f?);
        w.set_frame_delay(delay, 10_000).map_err(io)?;
        w.write_image_data(img.as_raw()).map_err(io)?;
    }
    w.finish().map_err(io)?;
    Ok(Some(out))
}

/// Whether a GIF has `n` frames, counted without decoding them: `image`'s frames are each a
/// whole decoded canvas, megabytes for one picture just to be counted.
fn gif_frames_at_least(bytes: &[u8], n: usize) -> bool {
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let Ok(mut d) = options.read_info(bytes) else { return false };
    (0..n).all(|_| d.read_next_frame().is_ok_and(|f| f.is_some()))
}

/// Whether `bytes` are an animation (GIF, animated WebP or APNG) with more than one frame.
pub fn is_animated_gif(bytes: &[u8]) -> bool {
    is_animated(bytes)
}

/// An animation's size, from its header.
pub fn gif_size(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()
}

/// Decodes a GIF one frame at a time, each resampled to `px` (see `fit`) and handed to `on_frame`
/// with its display time in µs, looping until it returns false or the GIF can't go on. Nothing
/// is kept: playing costs one frame plus the decoder, however long the GIF is. The caller paces.
/// Returns the frames of the first loop (a still, or a broken file, has fewer than 2 and stops).
pub fn stream_gif(bytes: &[u8], px: u32, mut on_frame: impl FnMut(RgbaImage, u32) -> bool) -> ImageResult<usize> {
    use image::AnimationDecoder;
    let format = image::guess_format(bytes)?;
    let mut first_loop = 0;
    loop {
        let mut n = 0;
        let frames = match format {
            image::ImageFormat::WebP => webp_decoder(bytes)?.into_frames(),
            image::ImageFormat::Png => png_decoder(bytes)?.apng()?.into_frames(),
            _ => gif_decoder(bytes)?.into_frames(),
        };
        for f in frames {
            let f = f?;
            let delay = delay_us(f.delay());
            n += 1;
            if !on_frame(fit(f.into_buffer(), px), delay) {
                return Ok(first_loop.max(n));
            }
        }
        if first_loop == 0 {
            first_loop = n;
        }
        if n < 2 {
            return Ok(n);
        }
    }
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

/// Whether `bytes` hold an animation, from the headers (GIFs: whether a second frame follows).
pub fn is_animated(bytes: &[u8]) -> bool {
    let Ok(reader) = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format() else { return false };
    match reader.format() {
        Some(image::ImageFormat::WebP) => webp_decoder(bytes).is_ok_and(|d| d.has_animation()),
        Some(image::ImageFormat::Png) => png_decoder(bytes).is_ok_and(|d| d.is_apng().unwrap_or(false)),
        // counted without decoding them
        Some(image::ImageFormat::Gif) => gif_frames_at_least(bytes, 2),
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
    fn streams_a_gif_in_loops_until_told_to_stop() {
        let g = gif(3, 50);
        assert!(is_animated_gif(&g) && !is_animated_gif(&gif(1, 50)));
        assert_eq!(gif_size(&g), Some((64, 64)));
        let mut seen = Vec::new();
        let n = stream_gif(&g, 32, |f, d| {
            assert_eq!((f.dimensions(), d), ((32, 32), 50_000));
            seen.push(f.get_pixel(0, 0)[0]);
            seen.len() < 7
        })
        .unwrap();
        assert_eq!(n, 3);
        // 3 frames, looped: the red value of the frame index
        assert_eq!(seen, [0, 40, 80, 0, 40, 80, 0]);
        // a still doesn't loop
        assert_eq!(stream_gif(&gif(1, 50), 32, |_, _| true).unwrap(), 1);
    }

    fn apng(frames: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 16, 16);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(frames, 0).unwrap();
        enc.set_frame_delay(1, 20).unwrap();
        let mut w = enc.write_header().unwrap();
        for i in 0..frames {
            w.write_image_data(&[(i * 60) as u8, 0, 0, 255].repeat(16 * 16)).unwrap();
        }
        w.finish().unwrap();
        out
    }

    #[test]
    fn streams_an_apng() {
        let a = apng(3);
        assert!(is_animated_gif(&a) && !is_animated_gif(&png_rgba(&RgbaImage::new(8, 8)).unwrap()));
        // a one-frame APNG says it animates, but stops after one frame
        assert_eq!(stream_gif(&apng(1), 16, |_, _| true).unwrap(), 1);
        assert_eq!(gif_size(&a), Some((16, 16)));
        let mut seen = Vec::new();
        stream_gif(&a, 16, |f, d| {
            assert_eq!(d, 50_000);
            seen.push(f.get_pixel(0, 0)[0]);
            seen.len() < 4
        })
        .unwrap();
        assert_eq!(seen, [0, 60, 120, 0]);
    }

    #[test]
    fn makes_a_resampled_apng_and_skips_stills() {
        let a = animation_png(&gif(4, 50), 32).unwrap().unwrap();
        let mut seen = Vec::new();
        stream_gif(&a, 32, |f, d| {
            assert_eq!((f.dimensions(), d), ((32, 32), 50_000));
            seen.push(f.get_pixel(0, 0)[0]);
            seen.len() < 4
        })
        .unwrap();
        assert_eq!(seen, [0, 40, 80, 120]);
        assert!(animation_png(&gif(1, 50), 32).unwrap().is_none());
        let png = png_rgba(&RgbaImage::new(8, 8)).unwrap();
        assert!(animation_png(&png, 32).unwrap().is_none());
        assert!(is_animated(&gif(3, 50)));
        assert!(!is_animated(&gif(1, 50)) && !is_animated(&png) && !is_animated(b"junk"));
    }

    #[test]
    fn apng_keeps_fractional_delays() {
        // 1/30 s in the source stays 33.3ms
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 4, 4);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(2, 0).unwrap();
        enc.set_frame_delay(1, 30).unwrap();
        let mut w = enc.write_header().unwrap();
        for _ in 0..2 {
            w.write_image_data(&[0; 64]).unwrap();
        }
        w.finish().unwrap();
        let a = animation_png(&out, 4).unwrap().unwrap();
        let mut delays = Vec::new();
        stream_gif(&a, 4, |_, d| {
            delays.push(d);
            delays.len() < 2
        })
        .unwrap();
        assert_eq!(delays, [33_300, 33_300]);
    }

    #[test]
    fn keeps_fractional_delays() {
        // APNG stores 1/30 s; image reports it in ms as 100/3
        assert_eq!(delay_us(image::Delay::from_numer_denom_ms(100, 3)), 33_333);
        assert_eq!(delay_us(image::Delay::from_numer_denom_ms(0, 1)), 100_000);
        assert_eq!(delay_us(image::Delay::from_numer_denom_ms(20, 1)), 20_000);
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

    /// A JPEG of 40 x 20 whose EXIF says to turn it 90 degrees clockwise to show it: dark on the
    /// left, bright on the right.
    fn sideways_jpeg() -> Vec<u8> {
        sideways_jpeg_of(40, 20)
    }

    /// `sideways_jpeg` at `w` x `h`.
    fn sideways_jpeg_of(w: u32, h: u32) -> Vec<u8> {
        use image::ImageEncoder;
        let img = RgbaImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgba([0, 0, 0, 255]) } else { image::Rgba([255, 255, 255, 255]) });
        let rgb = DynamicImage::ImageRgba8(img).to_rgb8();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        // little-endian TIFF with one entry: Orientation (0x0112), a short, 6
        let tiff = [b"II*\0".as_slice(), &[8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]].concat();
        let exif = [b"Exif\0\0".as_slice(), &tiff].concat();
        let len = (exif.len() + 2) as u16;
        let app1 = [[0xFF, 0xE1].as_slice(), &len.to_be_bytes(), &exif].concat();
        jpeg.splice(2..2, app1);
        jpeg
    }

    #[test]
    fn a_big_jpeg_is_decoded_small_for_its_thumbnail() {
        let jpeg = sideways_jpeg_of(1600, 800);
        // stored 1600 x 800: a quarter (400 x 200) still has the 300 px asked for
        assert!(jpeg_at_least(&jpeg, (1600, 800), 300).is_some_and(|i| (i.width(), i.height()) == (400, 200)));
        // half of it wouldn't have it: decoded whole
        assert!(jpeg_at_least(&jpeg, (1600, 800), 900).is_none());
        let thumb = decode_thumb_for(&jpeg, 0, |size| {
            assert_eq!(size, (800, 1600), "asked with the size as shown");
            300
        })
        .unwrap()
        .to_rgba8();
        assert_eq!(thumb.dimensions(), (150, 300));
        // upright: the dark half on top
        assert!(thumb.get_pixel(75, 40)[0] < 50 && thumb.get_pixel(75, 260)[0] > 200);
        // as the whole decode would have made it
        let mut whole = decode(&jpeg).unwrap();
        whole.apply_orientation(image::metadata::Orientation::Rotate90);
        let whole = whole.thumbnail(300, 300).to_rgba8();
        let diff = thumb.as_raw().iter().zip(whole.as_raw()).map(|(a, b)| u64::from(a.abs_diff(*b))).sum::<u64>();
        assert!(diff / thumb.as_raw().len() as u64 <= 2, "mean difference {}", diff as f64 / thumb.as_raw().len() as f64);
    }

    #[test]
    fn thumbs_and_facts_come_out_upright() {
        let jpeg = sideways_jpeg();
        let thumb = decode_thumb(&jpeg, 40).unwrap().to_rgba8();
        assert_eq!(thumb.dimensions(), (20, 40));
        // the dark half turned to the top
        assert!(thumb.get_pixel(10, 5)[0] < 50 && thumb.get_pixel(10, 35)[0] > 200);

        let dir = std::env::temp_dir().join(format!("sigwin-thumb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jpg");
        std::fs::write(&path, &jpeg).unwrap();
        let (size, hash) = photo_facts(Photo::File(&path), u64::MAX).unwrap();
        assert_eq!(size, (20, 40));
        let drawn = blurhash(&hash.unwrap(), 8, 16).unwrap();
        assert!(drawn.get_pixel(4, 1)[0] < drawn.get_pixel(4, 14)[0]);
        // the same from memory
        assert_eq!(photo_facts(Photo::Bytes(&jpeg), u64::MAX).unwrap().0, (20, 40));
        // too large to decode: the size alone
        assert_eq!(photo_facts(Photo::File(&path), 799).unwrap(), ((20, 40), None));
        // cut after its header: the size alone too
        let scan = jpeg.windows(2).position(|m| m == [0xFF, 0xDA]).unwrap();
        std::fs::write(&path, &jpeg[..scan + 14]).unwrap();
        assert_eq!(photo_facts(Photo::File(&path), u64::MAX).unwrap().0, (20, 40));
        std::fs::write(&path, b"junk").unwrap();
        assert!(photo_facts(Photo::File(&path), u64::MAX).is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
    fn an_upright_jpeg_decodes_straight_to_what_is_drawn() {
        use image::ImageEncoder;
        let rgb = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([(x * 4) as u8, (y * 5) as u8, 128]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .write_image(rgb.as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
            .unwrap();
        let direct = jpeg_rgba(&jpeg, image::metadata::Orientation::NoTransforms).expect("decoded directly");
        // the same pixels the RGB decode gives
        assert_eq!(direct.as_bytes(), to_pixels(decode(&jpeg).unwrap()).as_bytes());
        let v = decode_view(&jpeg, 1000, 1000, u64::MAX).unwrap().unwrap();
        assert!(v.opaque && v.original.as_bytes() == direct.as_bytes());
        // turned by its EXIF: decoded as before, still upright
        let sideways = sideways_jpeg();
        assert!(jpeg_rgba(&sideways, image::metadata::Orientation::Rotate90).is_none());
        let v = decode_view(&sideways, 1000, 1000, u64::MAX).unwrap().unwrap();
        assert_eq!((v.original.width(), v.original.height()), (20, 40));
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

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let crc = [kind.as_slice(), data].concat().iter().fold(!0u32, |mut c, b| {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 == 1 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
            c
        });
        [&(data.len() as u32).to_be_bytes(), kind.as_slice(), data, &(!crc).to_be_bytes()].concat()
    }

    /// A PNG of a few bytes whose header claims `side` x `side` RGBA, with `extra` chunks before
    /// its (empty) image data.
    fn claimed_png(side: u32, extra: &[Vec<u8>]) -> Vec<u8> {
        let ihdr = [&side.to_be_bytes()[..], &side.to_be_bytes(), &[8, 6, 0, 0, 0]].concat();
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend(png_chunk(b"IHDR", &ihdr));
        extra.iter().for_each(|c| out.extend_from_slice(c));
        out.extend(png_chunk(b"IDAT", &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]));
        out.extend(png_chunk(b"IEND", &[]));
        out
    }

    /// A GIF of a few bytes: a `screen` logical screen with one `frame`-sized image.
    fn claimed_gif(screen: u16, frame: u16) -> Vec<u8> {
        let mut out = b"GIF89a".to_vec();
        out.extend_from_slice(&[&screen.to_le_bytes()[..], &screen.to_le_bytes(), &[0, 0, 0]].concat());
        out.push(b',');
        out.extend_from_slice(&[&[0, 0, 0, 0][..], &frame.to_le_bytes(), &frame.to_le_bytes(), &[0]].concat());
        out.extend_from_slice(&[2, 2, 0x4C, 0x01, 0, b';']);
        out
    }

    // each of these would allocate gigabytes or terabytes, and abort, if decoded
    #[test]
    fn refuses_a_huge_png_header() {
        let png = claimed_png(1 << 20, &[]);
        assert_eq!(png.len(), 65);
        assert!(matches!(decode(&png), Err(image::ImageError::Limits(_))));
        assert!(decode_thumb(&png, 100).is_err() && decode_rgba(&png).is_err());
        assert!(decode_within(&claimed_png(2049, &[]), 2048 * 2048).is_err());
        assert!(decode_view(&png, 100, 100, u64::MAX).unwrap().is_none());
        assert!(photo::prepare(&png, photo::Quality::Standard).is_err());
        assert!(animation_png(&png, 32).is_err());
    }

    #[test]
    fn refuses_a_huge_gif_screen_or_frame() {
        let gif = claimed_gif(u16::MAX, 1);
        assert_eq!(gif.len(), 29);
        assert!(decode(&gif).is_err());
        assert!(decode_view(&gif, 100, 100, u64::MAX).unwrap().is_none());
        assert!(!is_animated(&gif));
        assert!(stream_gif(&gif, 32, |_, _| true).is_err());
        assert!(animation_png(&gif, 32).is_err());
        // a tiny screen doesn't make a huge frame safe
        let gif = claimed_gif(1, u16::MAX);
        assert!(decode(&gif).is_err());
        assert!(!is_animated(&gif));
        assert!(stream_gif(&gif, 32, |_, _| true).is_err());
    }

    #[test]
    fn refuses_a_huge_apng() {
        let actl = png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]);
        let apng = claimed_png(1 << 20, &[actl.clone()]);
        assert!(!is_animated(&apng));
        assert!(animation_png(&apng, 32).is_err());
        assert!(stream_gif(&apng, 32, |_, _| true).is_err());
        assert!(decode(&apng).is_err());
        // the same file at a sane size gets past the size check
        assert!(png_decoder(&claimed_png(16, &[actl])).is_ok());
    }

    #[test]
    fn square_is_exact_size() {
        assert_eq!(square(&DynamicImage::new_rgba8(300, 200), 40).dimensions(), (40, 40));
        assert_eq!(square(&DynamicImage::new_rgba8(20, 30), 40).dimensions(), (40, 40));
    }
}
