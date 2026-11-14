use std::fs::File;
use std::path::Path;

/// Frames Windows asks for at 100-250% scale (taskbar, tray, title bar, Explorer).
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn render(svg: &str, n: u32) -> ico::IconImage {
    let tree = resvg::usvg::Tree::from_str(svg, &Default::default()).unwrap();
    let mut pm = resvg::tiny_skia::Pixmap::new(n, n).unwrap();
    let s = n as f32 / tree.size().width();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(s, s), &mut pm.as_mut());
    let rgba = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    ico::IconImage::from_rgba_data(n, n, rgba)
}

fn build(assets: &Path, name: &str, png: bool) {
    let svg = std::fs::read_to_string(assets.join(format!("{name}.svg"))).unwrap();
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for n in SIZES {
        // PNG frames: BMP ones are several times larger and land in the exe
        dir.add_entry(ico::IconDirEntry::encode_as_png(&render(&svg, n)).unwrap());
    }
    dir.write(File::create(assets.join(format!("{name}.ico"))).unwrap()).unwrap();
    if png {
        render(&svg, 256).write_png(File::create(assets.join(format!("{name}.png"))).unwrap()).unwrap();
    }
}

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    build(&assets, "icon", true);
    build(&assets, "icon-dev", false);
}
