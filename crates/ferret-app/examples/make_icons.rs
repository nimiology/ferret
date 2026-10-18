//! Writes assets/AppIcon.iconset/*.png and assets/tray@2x.png from the same rasterizer the app uses.
//! Then: `iconutil -c icns crates/ferret-app/assets/AppIcon.iconset -o crates/ferret-app/assets/AppIcon.icns`
//!
//!     cargo run -p ferret-app --example make_icons
#[path = "../src/icon.rs"]
mod icon;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let set = dir.join("AppIcon.iconset");
    std::fs::create_dir_all(&set).unwrap();
    for pt in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let name = if scale == 1 { format!("icon_{pt}x{pt}.png") } else { format!("icon_{pt}x{pt}@2x.png") };
            icon::render(pt * scale, false).save_png(set.join(name)).unwrap();
        }
    }
    icon::render(32, true).save_png(dir.join("tray@2x.png")).unwrap();
    println!("wrote {}", set.display());
}
