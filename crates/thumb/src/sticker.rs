//! What a Signal sticker may be, and turning an image into one (the rules of Signal Desktop's
//! sticker creator). Here and not in the main crate so the lossless encoder stays optimized.

use image::{ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, RgbaImage};

/// A sticker's canvas, and the longest side a PNG or WebP may have to be sent as it is.
pub const SIDE: u32 = 512;
/// An animated sticker's shortest side.
pub const MIN_APNG_SIDE: u32 = 10;
pub const MAX_BYTES: usize = 300 * 1024;
/// A source file bigger than this isn't read.
pub const MAX_SOURCE_BYTES: u64 = 64 << 20;
/// A source picture with more pixels than this isn't decoded: converting holds it whole.
const MAX_SOURCE_PIXELS: u64 = 16_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Unreadable,
    /// a GIF or WebP animation: only APNG is taken as it is
    Animated,
    NotSquare,
    TooSmall,
    TooBig,
    MustLoop,
    TooLarge,
}

/// What the headers say about a file that may become a sticker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub width: u32,
    pub height: u32,
    pub apng: bool,
    /// goes up as it is: no conversion
    pub as_is: bool,
}

/// An APNG's `acTL`: (frames, plays), plays 0 meaning forever.
fn apng_control(bytes: &[u8]) -> Option<(u32, u32)> {
    let reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().ok()?;
    reader.info().animation_control.map(|a| (a.num_frames, a.num_plays))
}

/// Checks a file against the rules from its headers alone, without decoding it.
pub fn inspect(bytes: &[u8]) -> Result<Facts, Reject> {
    let reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|_| Reject::Unreadable)?;
    let format = reader.format();
    let (width, height) = reader.into_dimensions().map_err(|_| Reject::Unreadable)?;
    if width == 0 || height == 0 {
        return Err(Reject::Unreadable);
    }
    if format == Some(ImageFormat::Png) {
        if let Some((_, plays)) = apng_control(bytes) {
            if width != height {
                return Err(Reject::NotSquare);
            }
            if bytes.len() > MAX_BYTES {
                return Err(Reject::TooLarge);
            }
            if width < MIN_APNG_SIDE {
                return Err(Reject::TooSmall);
            }
            if width > SIDE {
                return Err(Reject::TooBig);
            }
            if plays != 0 {
                return Err(Reject::MustLoop);
            }
            return Ok(Facts { width, height, apng: true, as_is: true });
        }
    }
    if super::is_animated(bytes) {
        return Err(Reject::Animated);
    }
    if width as u64 * height as u64 > MAX_SOURCE_PIXELS {
        return Err(Reject::TooBig);
    }
    let as_is = matches!(format, Some(ImageFormat::Png | ImageFormat::WebP)) && width.max(height) <= SIDE && bytes.len() <= MAX_BYTES;
    Ok(Facts { width, height, apng: false, as_is })
}

/// The bytes to upload: the file itself when it is as good as one, otherwise the picture fitted
/// into a transparent 512x512 canvas and encoded as lossless WebP.
pub fn prepare(bytes: Vec<u8>) -> Result<Vec<u8>, Reject> {
    let facts = inspect(&bytes)?;
    if facts.as_is {
        return Ok(bytes);
    }
    let img = super::decode_within(&bytes, MAX_SOURCE_PIXELS).map_err(|_| Reject::Unreadable)?;
    drop(bytes);
    let fitted = super::fit(img.to_rgba8(), SIDE);
    drop(img);
    let mut canvas = RgbaImage::new(SIDE, SIDE);
    let (x, y) = ((SIDE - fitted.width()) / 2, (SIDE - fitted.height()) / 2);
    image::imageops::replace(&mut canvas, &fitted, x as i64, y as i64);
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .write_image(canvas.as_raw(), SIDE, SIDE, ExtendedColorType::Rgba8)
        .map_err(|_| Reject::Unreadable)?;
    if out.len() > MAX_BYTES {
        return Err(Reject::TooLarge);
    }
    Ok(out)
}

/// The picture as a `px` x `px` thumbnail (longer side `px`), of a file `inspect` took.
pub fn thumb(bytes: &[u8], px: u32) -> Result<RgbaImage, Reject> {
    let img = super::decode_within(bytes, MAX_SOURCE_PIXELS.max(SIDE as u64 * SIDE as u64)).map_err(|_| Reject::Unreadable)?;
    Ok(super::fit(img.to_rgba8(), px))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apng(side: u32, plays: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, side, side);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(2, plays).unwrap();
        enc.set_frame_delay(1, 10).unwrap();
        let mut w = enc.write_header().unwrap();
        for i in 0..2u8 {
            w.write_image_data(&[i * 100, 0, 0, 255].repeat((side * side) as usize)).unwrap();
        }
        w.finish().unwrap();
        out
    }

    fn still(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]));
        super::super::png_rgba(&img).unwrap()
    }

    #[test]
    fn an_apng_is_checked_and_never_changed() {
        let ok = apng(64, 0);
        assert_eq!(inspect(&ok), Ok(Facts { width: 64, height: 64, apng: true, as_is: true }));
        assert_eq!(prepare(ok.clone()).unwrap(), ok);
        assert_eq!(inspect(&apng(64, 3)), Err(Reject::MustLoop));
        assert_eq!(inspect(&apng(8, 0)), Err(Reject::TooSmall));
        assert_eq!(inspect(&apng(600, 0)), Err(Reject::TooBig));
    }

    #[test]
    fn an_apng_must_be_square_and_small() {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 40, 20);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(1, 0).unwrap();
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[0; 40 * 20 * 4]).unwrap();
        w.finish().unwrap();
        assert_eq!(inspect(&out), Err(Reject::NotSquare));
        // noise doesn't compress: a 300 KiB limit it exceeds
        let mut big = Vec::new();
        let mut enc = png::Encoder::new(&mut big, 400, 400);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(1, 0).unwrap();
        let mut w = enc.write_header().unwrap();
        let mut x = 12345u32;
        let noise: Vec<u8> = (0..400 * 400 * 4)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 24) as u8
            })
            .collect();
        w.write_image_data(&noise).unwrap();
        w.finish().unwrap();
        assert_eq!(inspect(&big), Err(Reject::TooLarge));
    }

    #[test]
    fn a_small_png_goes_up_as_it_is() {
        let p = still(200, 120);
        assert_eq!(inspect(&p).unwrap().as_is, true);
        assert_eq!(prepare(p.clone()).unwrap(), p);
    }

    #[test]
    fn other_pictures_become_a_centered_lossless_webp_canvas() {
        // wider than the canvas, and not square
        let p = still(1024, 400);
        let facts = inspect(&p).unwrap();
        assert!(!facts.as_is && !facts.apng);
        let out = prepare(p).unwrap();
        assert!(out.len() <= MAX_BYTES);
        let back = super::super::decode(&out).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (512, 512));
        // transparent above and below the 512x200 picture, opaque in its middle
        assert_eq!(back.get_pixel(256, 10)[3], 0);
        assert_eq!(back.get_pixel(256, 256)[3], 255);
        assert_eq!(back.get_pixel(256, 411)[3], 0);
        // a picture within the canvas but not a PNG or WebP is converted too
        let gif = super::super::gif(vec![RgbaImage::from_pixel(32, 32, image::Rgba([9, 9, 9, 255]))], 50).unwrap();
        assert!(!inspect(&gif).unwrap().as_is);
        assert_eq!(super::super::decode(&prepare(gif).unwrap()).unwrap().width(), 512);
    }

    #[test]
    fn a_png_too_heavy_to_send_is_converted() {
        // noise: lossless WebP can't shrink it below 300 KiB either
        let mut x = 7u32;
        let img = RgbaImage::from_fn(512, 512, |_, _| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            image::Rgba([(x >> 24) as u8, (x >> 16) as u8, (x >> 8) as u8, 255])
        });
        let p = super::super::png_rgba(&img).unwrap();
        assert!(p.len() > MAX_BYTES);
        assert!(!inspect(&p).unwrap().as_is);
        assert_eq!(prepare(p), Err(Reject::TooLarge));
    }

    #[test]
    fn junk_and_gif_animations_are_refused() {
        assert_eq!(inspect(b"not a picture"), Err(Reject::Unreadable));
        let frames = vec![RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255])), RgbaImage::from_pixel(16, 16, image::Rgba([9, 2, 3, 255]))];
        let gif = super::super::gif(frames, 50).unwrap();
        assert_eq!(inspect(&gif), Err(Reject::Animated));
    }

    #[test]
    fn a_thumbnail_has_the_asked_size() {
        let t = thumb(&still(300, 100), 64).unwrap();
        assert_eq!(t.dimensions(), (64, 21));
    }
}
