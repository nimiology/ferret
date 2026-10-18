//! The Ferret mark (docs/design/Ferret.dc.html screen 08, assets/icon.svg), rasterized with tiny-skia.
//! Geometry is in the SVG's 100×100 viewBox.
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// `template`: black mark on transparent, cropped tight (menu bar template image).
/// Otherwise: the full app icon with the dark rounded tile.
pub fn render(size: u32, template: bool) -> Pixmap {
    let mut pm = Pixmap::new(size, size).expect("size > 0");
    let paint = |c: Color| {
        let mut p = Paint::default();
        p.set_color(c);
        p.anti_alias = true;
        p
    };
    let rgb = |v: u32| Color::from_rgba8((v >> 16) as u8, (v >> 8) as u8, v as u8, 255);
    let (body, accent) = if template { (Color::BLACK, Color::BLACK) } else { (rgb(0xF1EEE9), rgb(0xFF9259)) };

    let s = size as f32;
    let tf = if template {
        // mark bbox ≈ x 14..79.5, y 24..65.5 -> fill the square with a little air
        let k = s / 70.0;
        Transform::from_translate(-46.75, -44.75).post_scale(k, k).post_translate(s / 2.0, s / 2.0)
    } else {
        Transform::from_scale(s / 100.0, s / 100.0)
    };

    if !template {
        let tile = rounded(0.0, 0.0, 100.0, 100.0, 23.0);
        pm.fill_path(&tile, &paint(rgb(0x1C1B19)), FillRule::Winding, tf, None);
    }
    // capsule body
    pm.fill_path(&rounded(14.0, 52.0, 37.0, 10.0, 5.0), &paint(body), FillRule::Winding, tf, None);
    // ring head
    let ring = PathBuilder::from_circle(64.0, 50.0, 15.0).unwrap();
    pm.stroke_path(&ring, &paint(accent), &Stroke { width: 9.0, ..Default::default() }, tf, None);
    // ears
    for pts in [[(52.0, 38.0), (55.0, 24.0), (63.0, 33.0)], [(66.0, 33.0), (74.0, 24.0), (77.0, 38.0)]] {
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        pb.line_to(pts[1].0, pts[1].1);
        pb.line_to(pts[2].0, pts[2].1);
        pb.close();
        pm.fill_path(&pb.finish().unwrap(), &paint(accent), FillRule::Winding, tf, None);
    }
    pm
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    // tiny-skia has no rrect helper: quads at the corners are close enough at icon sizes.
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(Rect::from_xywh(x, y, w, h).unwrap()))
}

/// Straight (un-premultiplied) RGBA, as tray-icon wants. Black template pixels are identical either way.
#[allow(dead_code)] // used by main.rs only
pub fn tray_rgba() -> (Vec<u8>, u32) {
    let pm = render(32, true); // 16 pt @2x
    (pm.data().to_vec(), 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizes_something() {
        let (rgba, n) = tray_rgba();
        assert_eq!(rgba.len(), (n * n * 4) as usize);
        let opaque = rgba.chunks(4).filter(|p| p[3] > 128).count();
        assert!(opaque > 50, "mark too thin: {opaque}");
        assert!(opaque < (n * n) as usize / 2, "mark fills the square: {opaque}");
        // app icon: corners transparent (rounded tile), centre opaque
        let pm = render(64, false);
        assert_eq!(pm.pixel(0, 0).unwrap().alpha(), 0);
        assert_eq!(pm.pixel(32, 32).unwrap().alpha(), 255);
    }
}
