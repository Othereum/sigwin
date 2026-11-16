//! Turns headless shots into the README's pictures (`scripts/readme-media.ps1` runs it): rounded
//! corners, a border and a shadow, like a window on the desktop; an animated PNG from numbered
//! frames, with the windows where they were on the headless screen and the pointer drawn as the
//! system's arrow. Crops are in the shots' pixels; SCALE is the shots' scale.
//!
//!   readme_media SCALE frame IN OUT [X,Y,W,H]
//!   readme_media SCALE apng DIR OUT [crop=X,Y,W,H] [cursor] [dump=DIR]
//!       DIR holds NNNN-<ms>.png, each shown <ms>, cropped; NNNN-<ms>-2.png beside one is the
//!       second window (the image viewer) at that moment. `cursor` draws the pointer where the
//!       shots say it was. `dump` also saves each frame as a PNG,
//!       for looking at one (viewers show an animated PNG's first frame only)

use anyhow::{bail, Context, Result};
use image::{Rgba, RgbaImage};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "readme_media SCALE frame|apng ...";
    let scale: f32 = args.first().context(usage)?.parse()?;
    let look = Look::new(scale);
    let a = |i: usize| args.get(i).map(String::as_str).context(usage);
    match a(1)? {
        "frame" => {
            let crop = args.get(4).map(|c| parse_rect(c)).transpose()?;
            look.frame(&cut(&image::open(a(2)?)?.to_rgba8(), crop)).save(a(3)?)?
        }
        "apng" => {
            let opt = |key: &str| args.iter().skip(4).find_map(|o| o.strip_prefix(key));
            let crop = opt("crop=").map(parse_rect).transpose()?;
            let cursor = args.iter().skip(4).any(|o| o == "cursor");
            apng(&look, a(2)?, a(3)?, crop, cursor, opt("dump="))?
        }
        _ => bail!(usage),
    }
    Ok(())
}

fn parse_rect(s: &str) -> Result<[u32; 4]> {
    let v: Vec<u32> = s.split(',').map(str::parse).collect::<Result<_, _>>()?;
    v.try_into().map_err(|_| anyhow::anyhow!("X,Y,W,H"))
}

fn cut(img: &RgbaImage, rect: Option<[u32; 4]>) -> RgbaImage {
    match rect {
        Some([x, y, w, h]) => image::imageops::crop_imm(img, x, y, w, h).to_image(),
        None => img.clone(),
    }
}

/// The window's outline and shadow, in the shots' pixels.
struct Look {
    scale: f32,
    radius: f32,
    margin: u32,
    shadow_y: f32,
    blur: f32,
    border: f32,
}

impl Look {
    fn new(s: f32) -> Self {
        Look { scale: s, radius: 8.0 * s, margin: (28.0 * s) as u32, shadow_y: 6.0 * s, blur: 14.0 * s, border: s }
    }

    /// The shot as a window: rounded, bordered, shadowed, on a transparent margin.
    fn frame(&self, shot: &RgbaImage) -> RgbaImage {
        let (w, h) = shot.dimensions();
        let m = self.margin;
        let (half_w, half_h) = (w as f32 / 2.0, h as f32 / 2.0);
        // signed distance to the window's rounded outline (negative inside)
        let dist = |x: f32, y: f32| {
            let qx = (x - half_w).abs() - (half_w - self.radius);
            let qy = (y - half_h).abs() - (half_h - self.radius);
            qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - self.radius
        };
        RgbaImage::from_fn(w + 2 * m, h + 2 * m, |ox, oy| {
            let (x, y) = (ox as f32 - m as f32 + 0.5, oy as f32 - m as f32 + 0.5);
            let d = dist(x, y);
            let cover = (0.5 - d).clamp(0.0, 1.0);
            let ds = dist(x, y - self.shadow_y).max(0.0);
            let shadow = 0.28 * (-(ds / self.blur).powi(2)).exp();
            let mut c = [0.0f32; 3];
            if cover > 0.0 {
                let p = shot.get_pixel((ox - m).min(w - 1), (oy - m).min(h - 1));
                // a thin grey edge: shows on light and dark pages alike
                let edge = (self.border + 0.5 + d).clamp(0.0, 1.0) * 0.35;
                for i in 0..3 {
                    c[i] = p[i] as f32 * (1.0 - edge) + 128.0 * edge;
                }
            }
            let alpha = cover + shadow * (1.0 - cover);
            if alpha <= 0.0 {
                return Rgba([0, 0, 0, 0]);
            }
            // the shadow is black: only the window adds color
            let px = |i: usize| (c[i] * cover / alpha).round() as u8;
            Rgba([px(0), px(1), px(2), (alpha * 255.0).round() as u8])
        })
    }

    /// The system's arrow cursor with its tip at (x, y): white, outlined in black.
    fn cursor(&self, dst: &mut RgbaImage, x: f32, y: f32) {
        // the Windows arrow, 1x, tip at 0,0
        const ARROW: [(f32, f32); 7] = [(0.0, 0.0), (0.0, 16.0), (4.0, 12.5), (7.0, 19.0), (9.5, 18.0), (6.5, 11.5), (11.5, 11.5)];
        let s = self.scale;
        let pts: Vec<(f32, f32)> = ARROW.iter().map(|&(px, py)| (x + px * s, y + py * s)).collect();
        let inside = |qx: f32, qy: f32| {
            let mut odd = false;
            for i in 0..pts.len() {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                if (a.1 > qy) != (b.1 > qy) && qx < a.0 + (qy - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                    odd = !odd;
                }
            }
            odd
        };
        let edge = |qx: f32, qy: f32| {
            (0..pts.len())
                .map(|i| {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                    let t = (((qx - a.0) * dx + (qy - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                    (qx - a.0 - t * dx).hypot(qy - a.1 - t * dy)
                })
                .fold(f32::MAX, f32::min)
        };
        let (x0, y0) = (x.floor() as i64 - 2, y.floor() as i64 - 2);
        let (x1, y1) = (x0 + (13.0 * s) as i64 + 4, y0 + (21.0 * s) as i64 + 4);
        const N: usize = 4;
        for py in y0.max(0)..y1.min(dst.height() as i64) {
            for px in x0.max(0)..x1.min(dst.width() as i64) {
                // 4x4 samples a pixel: inside and near the edge is black, inside is white
                let (mut white, mut black) = (0.0, 0.0);
                for sy in 0..N {
                    for sx in 0..N {
                        let (qx, qy) = (px as f32 + (sx as f32 + 0.5) / N as f32, py as f32 + (sy as f32 + 0.5) / N as f32);
                        if inside(qx, qy) {
                            if edge(qx, qy) < s { black += 1.0 } else { white += 1.0 }
                        }
                    }
                }
                let n = (N * N) as f32;
                let a = (white + black) / n;
                if a <= 0.0 {
                    continue;
                }
                let v = 255.0 * white / (white + black);
                let d = dst.get_pixel_mut(px as u32, py as u32);
                let da = d[3] as f32 / 255.0;
                let out = a + da * (1.0 - a);
                for i in 0..3 {
                    d[i] = ((v * a + d[i] as f32 * da * (1.0 - a)) / out).round() as u8;
                }
                d[3] = (out * 255.0).round() as u8;
            }
        }
    }
}

fn over(dst: &mut RgbaImage, src: &RgbaImage, dx: i64, dy: i64) {
    for (x, y, s) in src.enumerate_pixels() {
        let (tx, ty) = (x as i64 + dx, y as i64 + dy);
        if tx < 0 || ty < 0 || tx >= dst.width() as i64 || ty >= dst.height() as i64 {
            continue;
        }
        let d = dst.get_pixel_mut(tx as u32, ty as u32);
        let sa = s[3] as f32 / 255.0;
        let da = d[3] as f32 / 255.0;
        let a = sa + da * (1.0 - sa);
        if a <= 0.0 {
            continue;
        }
        for i in 0..3 {
            d[i] = ((s[i] as f32 * sa + d[i] as f32 * da * (1.0 - sa)) / a).round() as u8;
        }
        d[3] = (a * 255.0).round() as u8;
    }
}

/// What headless noted in a shot: the window's place on its screen and the pointer's.
fn notes(path: &Path) -> Result<((i64, i64), Option<(i64, i64)>)> {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let reader = dec.read_info()?;
    let get = |key: &str| {
        reader.info().uncompressed_latin1_text.iter().find(|t| t.keyword == key).and_then(|t| {
            let (x, y) = t.text.split_once(',')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        })
    };
    Ok((get("sigwin-pos").unwrap_or((0, 0)), get("sigwin-cursor")))
}

/// A window in a frame: its shot (cropped) and where that is on the screen.
struct Layer {
    path: PathBuf,
    crop: Option<[u32; 4]>,
    at: (i64, i64),
    size: (u32, u32),
}

/// Frames from `dir` (`NNNN-<ms>.png`) as an animated PNG that loops. Each frame after the first
/// holds only the box that changed, so a few moving lines cost little.
fn apng(look: &Look, dir: &str, out: &str, crop: Option<[u32; 4]>, draw_cursor: bool, dump: Option<&str>) -> Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png") && !p.to_string_lossy().ends_with("-2.png"))
        .collect();
    names.sort();
    // where everything goes first: the canvas holds every window of every frame
    let mut plan = Vec::new();
    for path in names {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let ms: u32 = stem.split_once('-').context("frame names are NNNN-<ms>.png")?.1.parse()?;
        let (pos, cursor) = notes(&path)?;
        let (w, h) = image::image_dimensions(&path)?;
        let (cx, cy, w, h) = crop.map_or((0, 0, w, h), |[x, y, w, h]| (x, y, w, h));
        let mut layers = vec![Layer { path: path.clone(), crop, at: (pos.0 + cx as i64, pos.1 + cy as i64), size: (w, h) }];
        let second = path.with_file_name(format!("{stem}-2.png"));
        if second.exists() {
            let (pos, _) = notes(&second)?;
            layers.push(Layer { size: image::image_dimensions(&second)?, path: second, crop: None, at: pos });
        }
        plan.push((layers, cursor, ms));
    }
    let m = look.margin as i64;
    let rects = plan.iter().flat_map(|p| p.0.iter()).map(|l| (l.at.0 - m, l.at.1 - m, l.at.0 + l.size.0 as i64 + m, l.at.1 + l.size.1 as i64 + m));
    let (x0, y0, x1, y1) = rects.fold((i64::MAX, i64::MAX, i64::MIN, i64::MIN), |a, r| (a.0.min(r.0), a.1.min(r.1), a.2.max(r.2), a.3.max(r.3)));
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let mut frames: Vec<(RgbaImage, u32)> = Vec::new();
    for (layers, cursor, ms) in plan {
        let mut img = RgbaImage::new(w, h);
        for l in &layers {
            let shot = look.frame(&cut(&image::open(&l.path)?.to_rgba8(), l.crop));
            over(&mut img, &shot, l.at.0 - m - x0, l.at.1 - m - y0);
        }
        // only over a window: a pointer outside the crop isn't shown
        if let Some((cx, cy)) = cursor.filter(|_| draw_cursor) {
            if layers.iter().any(|l| cx >= l.at.0 && cy >= l.at.1 && cx < l.at.0 + l.size.0 as i64 && cy < l.at.1 + l.size.1 as i64) {
                look.cursor(&mut img, (cx - x0) as f32, (cy - y0) as f32);
            }
        }
        match frames.last_mut() {
            // nothing moved: the frame before stays longer
            Some((last, t)) if *last == img => *t += ms,
            _ => frames.push((img, ms)),
        }
    }
    if let Some(d) = dump {
        std::fs::create_dir_all(d)?;
        for (i, (img, ms)) in frames.iter().enumerate() {
            img.save(Path::new(d).join(format!("{i:04}-{ms}.png")))?;
        }
    }
    let Some((first, _)) = frames.first() else { bail!("no frames in {dir}") };
    let (w, h) = first.dimensions();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(out)?), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    enc.set_animated(frames.len() as u32, 0)?;
    let mut wr = enc.write_header()?;
    let mut prev: Option<&RgbaImage> = None;
    for (img, ms) in &frames {
        let [x, y, fw, fh] = match prev {
            Some(p) => changed(p, img),
            None => [0, 0, w, h],
        };
        wr.reset_frame_position()?;
        wr.set_frame_dimension(fw, fh)?;
        wr.set_frame_position(x, y)?;
        wr.set_frame_delay((*ms).min(u16::MAX as u32) as u16, 1000)?;
        wr.set_blend_op(png::BlendOp::Source)?;
        wr.set_dispose_op(png::DisposeOp::None)?;
        wr.write_image_data(image::imageops::crop_imm(img, x, y, fw, fh).to_image().as_raw())?;
        prev = Some(img);
    }
    wr.finish()?;
    Ok(())
}

/// The box around every pixel that differs (at least one pixel).
fn changed(a: &RgbaImage, b: &RgbaImage) -> [u32; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in b.enumerate_pixels() {
        if a.get_pixel(x, y) != p {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    if x0 == u32::MAX {
        return [0, 0, 1, 1];
    }
    [x0, y0, x1 - x0 + 1, y1 - y0 + 1]
}
