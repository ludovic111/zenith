//! Renders `assets/icon/zenith.svg` into the PNG sizes of a macOS icon set and the app's PNG.
//! `cargo run -p zenith-app --example render_icon -- <out-dir>`; `scripts/mac/icon.sh` turns
//! the set into `AppIcon.icns`.

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "target/icon".into()));
    std::fs::create_dir_all(out.join("AppIcon.iconset")).expect("output folder");
    let svg = include_bytes!("../assets/icon/zenith.svg");
    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default()).expect("valid SVG");
    let render = |px: u32| {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(px, px).expect("pixmap");
        let scale = px as f32 / 1024.;
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        pixmap
    };
    for (name, px) in [
        ("icon_16x16.png", 16),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    ] {
        render(px).save_png(out.join("AppIcon.iconset").join(name)).expect("write PNG");
    }
    render(1024).save_png(out.join("zenith.png")).expect("write PNG");
    render(512).save_png(out.join("zenith-512.png")).expect("write PNG");
    println!("{}", out.display());
}
