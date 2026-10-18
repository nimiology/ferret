mod app;
mod theme;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Ferret")
            .with_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Ferret",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cc.egui_ctx.clone())))
        }),
    )
}

/// SF Pro / SF Mono when present, egui defaults otherwise.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, path, family) in [
        ("sf", "/System/Library/Fonts/SFNS.ttf", egui::FontFamily::Proportional),
        ("sfmono", "/System/Library/Fonts/SFNSMono.ttf", egui::FontFamily::Monospace),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(name.into(), egui::FontData::from_owned(bytes));
            fonts.families.entry(family).or_default().insert(0, name.into());
        }
    }
    ctx.set_fonts(fonts);
}
