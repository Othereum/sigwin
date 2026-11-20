//! AVIF stills: mp4parse finds the AV1 data in the container, rav1d decodes it. `image` decodes
//! AVIF only through the dav1d C library, which needs its own build on Windows. Animated AVIF
//! shows its still (primary) image; grid (tiled) images aren't read by mp4parse yet.

use image::error::{DecodingError, ImageFormatHint, LimitError, LimitErrorKind};
use image::metadata::Orientation;
use image::{ColorType, ImageDecoder, ImageError, ImageResult};
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::*;
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::*;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

/// Whether `bytes` start with an ISO-BMFF `ftyp` box naming an AVIF brand.
pub fn is_avif(bytes: &[u8]) -> bool {
    let Some(size) = bytes.get(0..4).map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize) else { return false };
    if bytes.get(4..8) != Some(b"ftyp") || size < 16 || bytes.len() < 16 {
        return false;
    }
    // major brand, minor version, then compatible brands (as many as `bytes` hold)
    let (compatible, _) = bytes[16..size.min(bytes.len())].as_chunks::<4>();
    std::iter::once(&bytes[8..12]).chain(compatible.iter().map(|b| &b[..])).any(|b| b == b"avif" || b == b"avis")
}

fn error(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormatHint::Exact(image::ImageFormat::Avif), e))
}

pub struct AvifDecoder {
    color: Vec<u8>,
    alpha: Option<Vec<u8>>,
    premultiplied: bool,
    size: (u32, u32),
    orientation: Orientation,
}

impl AvifDecoder {
    /// Reads the container and the AV1 sequence header; pixels are decoded in `read_image`.
    pub fn new(bytes: &[u8]) -> ImageResult<Self> {
        let ctx = mp4parse::read_avif(&mut std::io::Cursor::new(bytes), mp4parse::ParseStrictness::Normal).map_err(|e| error(format!("{e:?}")))?;
        let color = ctx.primary_item_coded_data().filter(|d| !d.is_empty()).ok_or_else(|| error("no AV1 image"))?.to_vec();
        let alpha = ctx.alpha_item_coded_data().filter(|d| !d.is_empty()).map(<[u8]>::to_vec);
        let header = sequence_header(&color)?;
        let size = (header.max_width as u32, header.max_height as u32);
        let mirror = ctx.image_mirror_ptr().ok().filter(|p| !p.is_null()).map(|p| unsafe { std::ptr::read(p) });
        let orientation = orientation(ctx.image_rotation().unwrap_or(mp4parse::ImageRotation::D0), mirror);
        Ok(Self { color, alpha, premultiplied: ctx.premultiplied_alpha, size, orientation })
    }
}

fn sequence_header(obus: &[u8]) -> ImageResult<Dav1dSequenceHeader> {
    let mut out = MaybeUninit::<Dav1dSequenceHeader>::uninit();
    // SAFETY: `obus` is valid for its length; the header is written in full on success.
    let r = unsafe { dav1d_parse_sequence_header(NonNull::new(out.as_mut_ptr()), NonNull::new(obus.as_ptr().cast_mut()), obus.len()) };
    if r.0 != 0 {
        return Err(error("bad AV1 sequence header"));
    }
    // SAFETY: success above
    let header = unsafe { out.assume_init() };
    if header.max_width <= 0 || header.max_height <= 0 {
        return Err(ImageError::Limits(LimitError::from_kind(LimitErrorKind::DimensionError)));
    }
    Ok(header)
}

/// HEIF turns (`irot`, anticlockwise) then mirrors (`imir`), in that order.
fn orientation(rotation: mp4parse::ImageRotation, mirror: Option<mp4parse::ImageMirror>) -> Orientation {
    use mp4parse::{ImageMirror::*, ImageRotation::*};
    use Orientation::*;
    match (rotation, mirror) {
        (D0, None) => NoTransforms,
        (D90, None) => Rotate270,
        (D180, None) => Rotate180,
        (D270, None) => Rotate90,
        (D0, Some(LeftRight)) | (D180, Some(TopBottom)) => FlipHorizontal,
        (D0, Some(TopBottom)) | (D180, Some(LeftRight)) => FlipVertical,
        (D90, Some(LeftRight)) | (D270, Some(TopBottom)) => Rotate270FlipH,
        (D90, Some(TopBottom)) | (D270, Some(LeftRight)) => Rotate90FlipH,
    }
}

/// A decoded picture, released on drop.
struct Picture(Dav1dPicture);

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture came from `dav1d_get_picture`
        unsafe { dav1d_picture_unref(NonNull::new(&mut self.0)) }
    }
}

impl Picture {
    fn header(&self) -> &Dav1dSequenceHeader {
        // SAFETY: a decoded picture always carries its sequence header, alive as long as it is
        unsafe { self.0.seq_hdr.expect("sequence header").as_ref() }
    }

    /// Sample `x`, `y` of `plane` (0 Y, 1 U, 2 V), scaled to 8 bits.
    #[inline(always)]
    fn sample(&self, plane: usize, x: usize, y: usize) -> u8 {
        let stride = self.0.stride[(plane > 0) as usize];
        let base = self.0.data[plane].expect("plane").as_ptr().cast::<u8>();
        // SAFETY: callers stay inside the plane's size, and the stride steps whole rows
        unsafe {
            let row = base.offset(y as isize * stride);
            match self.0.p.bpc {
                8 => *row.add(x),
                bpc => (*row.cast::<u16>().add(x) >> (bpc - 8)) as u8,
            }
        }
    }
}

/// Decodes one AV1 still, single-threaded: a thread pool would cost more than the decode.
fn decode_av1(obus: &[u8]) -> ImageResult<Picture> {
    let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: fills every field
    unsafe { dav1d_default_settings(NonNull::new(settings.as_mut_ptr()).unwrap()) };
    // SAFETY: just filled
    let mut settings = unsafe { settings.assume_init() };
    settings.n_threads = 1;
    settings.max_frame_delay = 1;
    // a safety limit against hostile headers; callers check the pixel count they accept
    settings.frame_size_limit = 16384 * 16384;
    let mut ctx: Option<Dav1dContext> = None;
    // SAFETY: both pointers are valid
    if unsafe { dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut settings)) }.0 != 0 {
        return Err(error("AV1 decoder"));
    }
    struct Close(Option<Dav1dContext>);
    impl Drop for Close {
        fn drop(&mut self) {
            // SAFETY: opened above, closed once
            unsafe { dav1d_close(NonNull::new(&mut self.0)) }
        }
    }
    let ctx = Close(ctx);
    let mut data = Dav1dData::default();
    // SAFETY: allocates `obus.len()` bytes owned by `data`
    let buf = unsafe { dav1d_data_create(NonNull::new(&mut data), obus.len()) };
    if buf.is_null() {
        return Err(error("AV1 data"));
    }
    // SAFETY: `buf` has room for `obus`
    unsafe { std::ptr::copy_nonoverlapping(obus.as_ptr(), buf, obus.len()) };
    let again = -libc::EAGAIN;
    let mut picture = Dav1dPicture::default();
    let result = loop {
        if data.sz > 0 {
            // SAFETY: valid context and data
            let r = unsafe { dav1d_send_data(ctx.0, NonNull::new(&mut data)) }.0;
            if r != 0 && r != again {
                break Err(error("AV1 data rejected"));
            }
        }
        // SAFETY: valid context; `picture` is written on success
        let r = unsafe { dav1d_get_picture(ctx.0, NonNull::new(&mut picture)) }.0;
        if r == 0 {
            break Ok(Picture(std::mem::take(&mut picture)));
        }
        if r != again || data.sz == 0 {
            break Err(error("AV1 frame"));
        }
    };
    // SAFETY: releases whatever wasn't consumed
    unsafe { dav1d_data_unref(NonNull::new(&mut data)) };
    result
}

/// Y'CbCr to R'G'B' for one matrix: R = Y + cr*V, G = Y - g_cb*U - g_cr*V, B = Y + cb*U.
struct Matrix {
    cr: f32,
    g_cb: f32,
    g_cr: f32,
    cb: f32,
}

impl Matrix {
    fn from_kr_kb(kr: f32, kb: f32) -> Self {
        let kg = 1.0 - kr - kb;
        Self { cr: 2.0 * (1.0 - kr), g_cb: 2.0 * kb * (1.0 - kb) / kg, g_cr: 2.0 * kr * (1.0 - kr) / kg, cb: 2.0 * (1.0 - kb) }
    }
}

enum Conversion {
    Matrix(Matrix),
    /// planes hold G, B, R
    Identity,
    YCgCo,
}

fn conversion(mtrx: Dav1dMatrixCoefficients) -> Conversion {
    match mtrx {
        DAV1D_MC_IDENTITY => Conversion::Identity,
        DAV1D_MC_SMPTE_YCGCO => Conversion::YCgCo,
        DAV1D_MC_BT470BG | DAV1D_MC_BT601 => Conversion::Matrix(Matrix::from_kr_kb(0.299, 0.114)),
        DAV1D_MC_FCC => Conversion::Matrix(Matrix::from_kr_kb(0.30, 0.11)),
        DAV1D_MC_SMPTE240 => Conversion::Matrix(Matrix::from_kr_kb(0.212, 0.087)),
        DAV1D_MC_BT2020_NCL | DAV1D_MC_BT2020_CL => Conversion::Matrix(Matrix::from_kr_kb(0.2627, 0.0593)),
        // BT.709 and unspecified, as browsers do
        _ => Conversion::Matrix(Matrix::from_kr_kb(0.2126, 0.0722)),
    }
}

impl ImageDecoder for AvifDecoder {
    fn dimensions(&self) -> (u32, u32) {
        self.size
    }

    fn color_type(&self) -> ColorType {
        if self.alpha.is_some() { ColorType::Rgba8 } else { ColorType::Rgb8 }
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(self.orientation)
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let pic = decode_av1(&self.color)?;
        let (w, h) = (pic.0.p.w as usize, pic.0.p.h as usize);
        if (w as u32, h as u32) != self.size {
            return Err(error("AV1 frame size differs from its header"));
        }
        let header = pic.header();
        let full = header.color_range != 0;
        let layout = pic.0.p.layout;
        let (sx, sy) = match layout {
            DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
            DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
            _ => (0, 0),
        };
        let mono = layout == DAV1D_PIXEL_LAYOUT_I400;
        let conv = conversion(header.mtrx);
        if matches!(conv, Conversion::Identity) && layout != DAV1D_PIXEL_LAYOUT_I444 {
            return Err(error("identity matrix needs 4:4:4"));
        }
        // limited range: Y 16..235, chroma 16..240 around 128
        let (y_off, y_scale, c_scale) = if full { (0.0, 1.0, 1.0) } else { (16.0, 255.0 / 219.0, 255.0 / 224.0) };
        let alpha = self.alpha.as_deref().map(decode_av1).transpose()?;
        if let Some(a) = &alpha
            && (a.0.p.w as usize, a.0.p.h as usize) != (w, h)
        {
            return Err(error("alpha size differs"));
        }
        let alpha_full = alpha.as_ref().is_some_and(|a| a.header().color_range != 0);
        let channels = if alpha.is_some() { 4 } else { 3 };
        let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        for (y, row) in buf.chunks_exact_mut(w * channels).enumerate().take(h) {
            for (x, px) in row.chunks_exact_mut(channels).enumerate() {
                let luma = (pic.sample(0, x, y) as f32 - y_off) * y_scale;
                let rgb = if mono {
                    [clamp(luma); 3]
                } else {
                    let (cx, cy) = (x >> sx, y >> sy);
                    let u = (pic.sample(1, cx, cy) as f32 - 128.0) * c_scale;
                    let v = (pic.sample(2, cx, cy) as f32 - 128.0) * c_scale;
                    match &conv {
                        Conversion::Matrix(m) => [clamp(luma + m.cr * v), clamp(luma - m.g_cb * u - m.g_cr * v), clamp(luma + m.cb * u)],
                        Conversion::Identity => [clamp(v + 128.0 * c_scale), clamp(luma), clamp(u + 128.0 * c_scale)],
                        Conversion::YCgCo => {
                            let t = luma - u;
                            [clamp(t + v), clamp(luma + u), clamp(t - v)]
                        }
                    }
                };
                px[..3].copy_from_slice(&rgb);
                if let Some(a) = &alpha {
                    let raw = a.sample(0, x, y);
                    let av = if alpha_full { raw } else { clamp((raw as f32 - 16.0) * 255.0 / 219.0) };
                    px[3] = av;
                    if self.premultiplied && av > 0 && av < 255 {
                        for c in &mut px[..3] {
                            *c = ((*c as u32 * 255 + av as u32 / 2) / av as u32).min(255) as u8;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

#[cfg(test)]
mod tests {
    // made with Pillow (libavif): see each test for what they hold
    const OPAQUE: &[u8] = include_bytes!("../tests/fixtures/opaque.avif");
    const ALPHA: &[u8] = include_bytes!("../tests/fixtures/alpha.avif");
    const ROTATED: &[u8] = include_bytes!("../tests/fixtures/rotated.avif");

    fn near(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 12)
    }

    #[test]
    fn sniffs_only_avif() {
        assert!(super::is_avif(OPAQUE));
        assert!(!super::is_avif(b"\x89PNG\r\n\x1a\n...."));
        assert!(!super::is_avif(&OPAQUE[..10]));
    }

    #[test]
    fn decodes_colors_alpha_and_turn() {
        // 48x32, red left half, blue right half, 4:2:0
        let img = crate::decode_rgba(OPAQUE).unwrap();
        assert_eq!(img.dimensions(), (48, 32));
        assert!(near(img.get_pixel(4, 16).0, [220, 30, 30, 255]), "{:?}", img.get_pixel(4, 16));
        assert!(near(img.get_pixel(44, 16).0, [30, 30, 220, 255]), "{:?}", img.get_pixel(44, 16));
        // 16x16, clear top half, green bottom
        let img = crate::decode_rgba(ALPHA).unwrap();
        assert_eq!(img.get_pixel(8, 2).0[3], 0);
        assert!(near(img.get_pixel(8, 13).0, [20, 200, 20, 255]), "{:?}", img.get_pixel(8, 13));
        // stored 40x20 with a red left edge, `irot` turns it to stand 20x40 with the edge on top
        let img = crate::decode_thumb(ROTATED, 40).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (20, 40));
        assert!(near(img.get_pixel(10, 1).0, [220, 30, 30, 255]), "{:?}", img.get_pixel(10, 1));
        assert!(near(img.get_pixel(10, 38).0, [230, 230, 230, 255]), "{:?}", img.get_pixel(10, 38));
    }

    #[test]
    fn view_and_send_take_avif() {
        let v = crate::decode_view(OPAQUE, 1000, 1000, u64::MAX).unwrap().unwrap();
        assert!(v.opaque && v.fitted.is_none());
        let sent = crate::photo::prepare(OPAQUE, crate::photo::Quality::Standard).unwrap().unwrap();
        assert_eq!(sent.content_type, "image/jpeg");
        let sent = crate::photo::prepare(ALPHA, crate::photo::Quality::Standard).unwrap().unwrap();
        assert_eq!(sent.content_type, "image/png");
    }
}
