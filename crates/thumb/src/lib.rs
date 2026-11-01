//! Image decoding and resampling. Lives in its own crate because the `fast` profile builds the
//! main crate at opt-level 0 (slint's generated code), and generic/inlined pixel code is compiled
//! in the crate that calls it: unoptimized, a 260px Lanczos resample took ~37ms instead of ~1ms.

use std::path::Path;

pub use image::{DynamicImage, ImageResult, RgbaImage};

/// Decodes, sniffing the format (file names and content types may lie).
pub fn decode(bytes: &[u8]) -> ImageResult<DynamicImage> {
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.decode()
}

/// Decodes and shrinks to fit in `max` x `max` (never upscales).
pub fn decode_thumb(bytes: &[u8], max: u32) -> ImageResult<DynamicImage> {
    Ok(decode(bytes)?.thumbnail(max, max))
}

/// Reads an image file as RGBA.
pub fn open_rgba(path: &Path) -> ImageResult<RgbaImage> {
    Ok(image::open(path)?.to_rgba8())
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
    let s = px as f32 / w.max(h) as f32;
    let (nw, nh) = (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1));
    let mut pm = image::Rgba32FImage::new(w, h);
    for (d, p) in pm.pixels_mut().zip(img.pixels()) {
        let a = p[3] as f32 / 255.0;
        *d = image::Rgba([p[0] as f32 / 255.0 * a, p[1] as f32 / 255.0 * a, p[2] as f32 / 255.0 * a, a]);
    }
    let small = image::imageops::resize(&pm, nw, nh, image::imageops::FilterType::Lanczos3);
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn square_is_exact_size() {
        assert_eq!(square(&DynamicImage::new_rgba8(300, 200), 40).dimensions(), (40, 40));
        assert_eq!(square(&DynamicImage::new_rgba8(20, 30), 40).dimensions(), (40, 40));
    }
}
