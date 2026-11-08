//! Photos prepared for sending: scaled and re-encoded like Signal Desktop does, and stripped of
//! EXIF (location and the like), which a re-encode drops by itself.

use super::{DynamicImage, ImageResult, RgbaImage};
use image::{RgbImage, buffer::ConvertBuffer};
use image::{ImageDecoder, ImageFormat};

/// How much a photo is shrunk before it is sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Quality {
    /// Signal Desktop's default level: up to 1600px, about 1MiB.
    #[default]
    Standard,
    /// Signal Desktop's "high quality": up to 4096px, about 3MiB.
    High,
}

impl Quality {
    pub fn from_index(i: u8) -> Self {
        match i {
            1 => Self::High,
            _ => Self::Standard,
        }
    }
}

const MIB: usize = 1024 * 1024;

/// Signal Desktop's `MEDIA_QUALITY_LEVEL_DATA`: (longest side, JPEG quality, target size, size up to
/// which the image keeps its dimensions).
fn level(q: Quality) -> (u32, u8, usize, usize) {
    match q {
        Quality::High => (4096, 75, 3 * MIB, MIB * 2 / 5),
        _ => (1600, 70, MIB, MIB / 5),
    }
}

/// Longest sides tried in turn, as in Signal Desktop; the last resort is `MIN_SIDE`.
const SIDES: [u32; 5] = [3072, 2048, 1600, 1024, 768];
const MIN_SIDE: u32 = 512;

/// A photo ready to upload.
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    /// as shown, when the photo was decoded on the way
    pub size: Option<(u32, u32)>,
    pub blurhash: Option<String>,
}

/// Prepares `bytes` for sending. `None` means to send the file as it is: a GIF, or a format we
/// can't decode. What stays in its format (an animation, an image with transparent parts: a JPEG
/// would lose that) still loses its metadata.
pub fn prepare(bytes: &[u8], quality: Quality) -> ImageResult<Option<Prepared>> {
    let format = image::guess_format(bytes)?;
    if let Some(p) = shrink(bytes, format, quality)? {
        return Ok(Some(p));
    }
    Ok(strip_metadata(bytes, format).map(|bytes| Prepared {
        bytes,
        content_type: if format == ImageFormat::Png { "image/png" } else { "image/webp" },
        size: None,
        blurhash: None,
    }))
}

/// A photo scaled and re-encoded as a JPEG, or just freed of metadata when it is small enough.
/// `None` when it should stay in its format.
fn shrink(bytes: &[u8], format: ImageFormat, quality: Quality) -> ImageResult<Option<Prepared>> {
    if super::is_animated(bytes) || !matches!(format, ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP) {
        return Ok(None);
    }
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let upright = orientation == image::metadata::Orientation::NoTransforms;
    let jpeg = format == ImageFormat::Jpeg;
    // a small JPEG that needs no turning only loses its metadata, untouched otherwise
    let (side, jpeg_quality, target, keep_size) = level(quality);
    if jpeg && upright && bytes.len() <= keep_size {
        return Ok(Some(Prepared { bytes: strip_jpeg_metadata(bytes), content_type: "image/jpeg", size: None, blurhash: None }));
    }
    if !jpeg && bytes.len() <= keep_size {
        return Ok(None);
    }
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let rgba = img.to_rgba8();
    if img.color().has_alpha() && rgba.pixels().any(|p| p.0[3] < 255) {
        return Ok(None);
    }
    let blurhash = super::encode_blurhash(&img.thumbnail(64, 64).to_rgba8());
    drop(img);
    let full = rgba.dimensions();
    let encode = |im: &RgbaImage, q: u8| -> ImageResult<Vec<u8>> {
        let rgb: RgbImage = im.convert();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).encode_image(&rgb)?;
        Ok(out)
    };
    let mut last = None;
    for s in SIDES.into_iter().filter(|s| *s <= side).chain([MIN_SIDE]) {
        let size = super::fit_size(full, (s, s));
        // a bigger limit changed nothing for an image this small: same bytes
        if last == Some(size) {
            continue;
        }
        last = Some(size);
        // always from the full image: shrinking a shrunk copy would stack the blur
        let scaled = if size == full {
            None
        } else {
            let mut out = RgbaImage::new(size.0, size.1);
            super::resize_into(rgba.as_raw(), full, (0.0, 0.0, full.0 as f64, full.1 as f64), size, &mut out, true)
                .ok_or_else(|| image::ImageError::Limits(image::error::LimitError::from_kind(image::error::LimitErrorKind::DimensionError)))?;
            Some(out)
        };
        let out = encode(scaled.as_ref().unwrap_or(&rgba), jpeg_quality)?;
        if out.len() <= target || s == MIN_SIDE {
            // a JPEG that came out bigger than it was, at its own size, stays as it was
            if jpeg && upright && size == full && out.len() >= bytes.len() {
                return Ok(Some(Prepared { bytes: strip_jpeg_metadata(bytes), content_type: "image/jpeg", size: Some(full), blurhash }));
            }
            return Ok(Some(Prepared { bytes: out, content_type: "image/jpeg", size: Some(size), blurhash }));
        }
    }
    unreachable!("MIN_SIDE always ends the loop")
}

/// The JPEG without its metadata segments (EXIF, XMP, IPTC), byte for byte otherwise. The color
/// profile (APP2) stays: dropping it would shift colors. Returns the input if it isn't one.
pub fn strip_jpeg_metadata(jpeg: &[u8]) -> Vec<u8> {
    if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] {
        return jpeg.to_vec();
    }
    let mut out = Vec::with_capacity(jpeg.len());
    out.extend_from_slice(&jpeg[..2]);
    let mut i = 2;
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let marker = jpeg[i + 1];
        // fill bytes
        if marker == 0xFF {
            i += 1;
            continue;
        }
        // the image data starts here; markers without a length (RSTn, TEM) can't precede it
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        let end = (i + 2 + len).min(jpeg.len());
        // APP1 (EXIF, XMP) and APP13 (Photoshop / IPTC)
        if marker != 0xE1 && marker != 0xED {
            out.extend_from_slice(&jpeg[i..end]);
        }
        i = end;
    }
    out.extend_from_slice(&jpeg[i.min(jpeg.len())..]);
    out
}

/// A PNG or WebP without its metadata chunks, or `None` for another format. One that EXIF turns
/// is left as it is: the turn would go with the chunk.
fn strip_metadata(bytes: &[u8], format: ImageFormat) -> Option<Vec<u8>> {
    if !matches!(format, ImageFormat::Png | ImageFormat::WebP) {
        return None;
    }
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_decoder().ok()?;
    if decoder.orientation().ok()? != image::metadata::Orientation::NoTransforms {
        return None;
    }
    if format == ImageFormat::Png { strip_png_metadata(bytes) } else { strip_webp_metadata(bytes) }
}

/// The PNG without its EXIF and text chunks (they hold comments and XMP too). Chunks are copied as
/// they are, so the pixels and the color information stay.
pub fn strip_png_metadata(png: &[u8]) -> Option<Vec<u8>> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if png.get(..8)? != SIGNATURE {
        return None;
    }
    let mut out = Vec::with_capacity(png.len());
    out.extend_from_slice(&SIGNATURE);
    let mut i = 8;
    while i < png.len() {
        let len = u32::from_be_bytes(png.get(i..i + 4)?.try_into().ok()?) as usize;
        let end = i.checked_add(12)?.checked_add(len)?;
        let chunk = png.get(i..end)?;
        if !matches!(&chunk[4..8], b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt") {
            out.extend_from_slice(chunk);
        }
        i = end;
    }
    Some(out)
}

/// The WebP without its EXIF and XMP chunks, with the flags in its header (VP8X) and the file size
/// brought along.
pub fn strip_webp_metadata(webp: &[u8]) -> Option<Vec<u8>> {
    if webp.get(..4)? != b"RIFF" || webp.get(8..12)? != b"WEBP" {
        return None;
    }
    let mut out = webp.get(..12)?.to_vec();
    let mut i = 12;
    while i < webp.len() {
        let len = u32::from_le_bytes(webp.get(i + 4..i + 8)?.try_into().ok()?) as usize;
        // chunks are padded to an even size
        let end = i.checked_add(8)?.checked_add(len)?.checked_add(len & 1)?.min(webp.len());
        let chunk = webp.get(i..end)?;
        match &chunk[..4] {
            b"EXIF" | b"XMP " => {}
            b"VP8X" => {
                let mut c = chunk.to_vec();
                // flags: bit 3 EXIF, bit 2 XMP
                c[8] &= !0b0000_1100;
                out.extend_from_slice(&c);
            }
            _ => out.extend_from_slice(chunk),
        }
        i = end;
    }
    let size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    /// Noisy, so that it doesn't compress to nothing.
    fn noise(w: u32, h: u32) -> RgbImage {
        let mut seed = 12345u32;
        RgbImage::from_fn(w, h, |_, _| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        })
    }

    fn jpeg(img: &RgbImage, q: u8) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).encode_image(img).unwrap();
        out
    }

    /// `jpeg` with an APP1 segment (as EXIF has) after SOI.
    fn with_exif(jpeg: &[u8]) -> Vec<u8> {
        let body = b"Exif\0\0secret location";
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn has(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn strips_exif_without_touching_the_image() {
        let plain = jpeg(&noise(32, 32), 90);
        let tagged = with_exif(&plain);
        assert!(has(&tagged, b"secret"));
        let stripped = strip_jpeg_metadata(&tagged);
        assert_eq!(stripped, plain);
        assert_eq!(strip_jpeg_metadata(b"not a jpeg"), b"not a jpeg");
    }

    #[test]
    fn standard_shrinks_a_big_photo() {
        let big = jpeg(&noise(2400, 1800), 95);
        assert!(big.len() > MIB);
        let p = prepare(&big, Quality::Standard).unwrap().unwrap();
        assert!(p.bytes.len() <= MIB, "{}", p.bytes.len());
        let (w, h) = p.size.unwrap();
        assert!(w.max(h) <= 1600 && w > h);
        assert_eq!(crate::decode(&p.bytes).unwrap().width(), w);
        assert!(p.blurhash.is_some());
    }

    #[test]
    fn small_photos_keep_their_size() {
        let small = with_exif(&jpeg(&noise(200, 100), 80));
        let p = prepare(&small, Quality::Standard).unwrap().unwrap();
        assert!(!has(&p.bytes, b"secret"));
        assert_eq!(crate::decode(&p.bytes).unwrap().width(), 200);
    }

    #[test]
    fn keeps_transparent_images_in_their_format_and_gifs_as_they_are() {
        let mut clear = RgbaImage::from_pixel(2000, 2000, Rgba([10, 20, 30, 255]));
        clear.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        let kept = prepare(&crate::png_rgba(&clear).unwrap(), Quality::Standard).unwrap().unwrap();
        assert_eq!(kept.content_type, "image/png");
        assert_eq!(crate::decode(&kept.bytes).unwrap().to_rgba8(), clear);
        let frames = (0..3).map(|i| RgbaImage::from_pixel(64, 64, Rgba([i * 40, 0, 0, 255]))).collect();
        assert!(prepare(&crate::gif(frames, 50).unwrap(), Quality::Standard).unwrap().is_none());
        assert!(prepare(b"junk", Quality::Standard).is_err());
    }

    #[test]
    fn big_opaque_png_becomes_jpeg() {
        let rgba = DynamicImage::ImageRgb8(noise(1800, 1200)).to_rgba8();
        let png = crate::png_rgba(&rgba).unwrap();
        let p = prepare(&png, Quality::Standard).unwrap().unwrap();
        assert_eq!(p.content_type, "image/jpeg");
        assert!(p.bytes.len() < png.len());
    }

    /// A chunk of a PNG (the checksum isn't checked by the decoder).
    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0; 4]);
        c
    }

    #[test]
    fn strips_png_metadata_and_keeps_the_pixels() {
        let plain = crate::png_rgba(&DynamicImage::ImageRgb8(noise(40, 30)).to_rgba8()).unwrap();
        // right after IHDR (8 signature + 25 header)
        let mut tagged = plain[..33].to_vec();
        tagged.extend(png_chunk(b"eXIf", b"secret exif"));
        tagged.extend(png_chunk(b"tEXt", b"Comment secret text"));
        tagged.extend_from_slice(&plain[33..]);
        let out = prepare(&tagged, Quality::Standard).unwrap().unwrap();
        assert_eq!(out.content_type, "image/png");
        assert!(!has(&out.bytes, b"secret"));
        assert_eq!(crate::decode(&out.bytes).unwrap().to_rgba8(), crate::decode(&plain).unwrap().to_rgba8());
        assert!(strip_png_metadata(b"nope").is_none());
    }

    #[test]
    fn strips_webp_metadata_and_keeps_the_pixels() {
        let img = DynamicImage::ImageRgb8(noise(40, 30)).to_rgba8();
        let mut lossless = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
            .encode(img.as_raw(), 40, 30, image::ExtendedColorType::Rgba8)
            .unwrap();
        // the extended layout: VP8X (EXIF flag), the image chunk, then EXIF
        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0b0000_1000, 0, 0, 0];
        vp8x.extend_from_slice(&[39, 0, 0, 29, 0, 0]);
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&(vp8x.len() as u32).to_le_bytes());
        body.extend(vp8x);
        body.extend_from_slice(&lossless[12..]);
        let secret = b"secret exif!";
        body.extend_from_slice(b"EXIF");
        body.extend_from_slice(&(secret.len() as u32).to_le_bytes());
        body.extend_from_slice(secret);
        let mut tagged = b"RIFF".to_vec();
        tagged.extend_from_slice(&(body.len() as u32).to_le_bytes());
        tagged.extend(body);
        assert!(crate::decode(&tagged).is_ok());
        let out = prepare(&tagged, Quality::Standard).unwrap().unwrap();
        assert_eq!(out.content_type, "image/webp");
        assert!(!has(&out.bytes, b"secret") && !has(&out.bytes, b"EXIF"));
        assert_eq!(out.bytes[20] & 0b0000_1100, 0);
        assert_eq!(u32::from_le_bytes(out.bytes[4..8].try_into().unwrap()) as usize, out.bytes.len() - 8);
        assert_eq!(crate::decode(&out.bytes).unwrap().to_rgba8(), img);
    }
}
