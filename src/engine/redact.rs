//! Geometry for redaction: which characters go, and which pixels of an image an
//! area covers. The PDFium work is in `pdf.rs` (`Page::redact`).

use super::commands::PdfBox;

/// Whether a character with box `b` is removed: its centre lies in an area.
pub fn removes_char(b: &PdfBox, areas: &[PdfBox]) -> bool {
    let (x, y) = b.center();
    areas.iter().any(|a| a.contains(x, y))
}

/// Whether a whole object with bounds `b` lies inside one of the areas.
pub fn removes_object(b: &PdfBox, areas: &[PdfBox]) -> bool {
    areas.iter().any(|a| a.expand(0.5).encloses(b))
}

/// The pixel rectangle `[x0, y0, x1)` × `[y0, y1)` of a `w`×`h` image drawn with the
/// matrix `m` (`[a, b, c, d, e, f]`, unit square to page space) that `area` covers.
pub fn image_pixels(m: [f32; 6], w: u32, h: u32, area: &PdfBox) -> Option<(u32, u32, u32, u32)> {
    let [a, b, c, d, e, f] = m.map(f64::from);
    let det = a * d - b * c;
    if det.abs() < 1e-12 || w == 0 || h == 0 {
        return None;
    }
    // Page point -> unit square (u right, v up) -> pixels (y down).
    let to_px = |x: f32, y: f32| {
        let (x, y) = (f64::from(x) - e, f64::from(y) - f);
        let u = (d * x - c * y) / det;
        let v = (a * y - b * x) / det;
        (u * f64::from(w), (1.0 - v) * f64::from(h))
    };
    let corners = [
        to_px(area.left, area.bottom),
        to_px(area.right, area.bottom),
        to_px(area.left, area.top),
        to_px(area.right, area.top),
    ];
    let clamp = |v: f64, max: u32| v.clamp(0.0, f64::from(max)) as u32;
    let x0 = clamp(corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min).floor(), w);
    let x1 = clamp(corners.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max).ceil(), w);
    let y0 = clamp(corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min).floor(), h);
    let y1 = clamp(corners.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max).ceil(), h);
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

/// Paints the pixel rectangle opaque black in RGBA pixels of width `w`.
pub fn fill_black(rgba: &mut [u8], w: u32, (x0, y0, x1, y1): (u32, u32, u32, u32)) {
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * w + x) * 4) as usize;
            if let Some(px) = rgba.get_mut(i..i + 4) {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(left: f32, bottom: f32, right: f32, top: f32) -> PdfBox {
        PdfBox { left, bottom, right, top }
    }

    #[test]
    fn characters_go_when_their_centre_is_covered() {
        let areas = [bx(100.0, 100.0, 200.0, 120.0)];
        assert!(removes_char(&bx(95.0, 105.0, 106.0, 115.0), &areas));
        assert!(!removes_char(&bx(90.0, 105.0, 101.0, 115.0), &areas));
        assert!(removes_object(&bx(110.0, 101.0, 190.0, 119.0), &areas));
        assert!(!removes_object(&bx(110.0, 101.0, 210.0, 119.0), &areas));
    }

    #[test]
    fn image_pixels_follow_the_image_matrix() {
        // A 100×50 px image drawn at (100, 200), 200×100 pt.
        let m = [200.0, 0.0, 0.0, 100.0, 100.0, 200.0];
        // The left half, top 20 pt of the image.
        let r = image_pixels(m, 100, 50, &bx(50.0, 280.0, 200.0, 400.0));
        assert_eq!(r, Some((0, 0, 50, 10)));
        // Outside the image.
        assert_eq!(image_pixels(m, 100, 50, &bx(0.0, 0.0, 50.0, 50.0)), None);
        // Rotated a quarter turn: u runs up the page, v to the left.
        let m = [0.0, 100.0, -50.0, 0.0, 300.0, 100.0];
        let r = image_pixels(m, 10, 10, &bx(250.0, 100.0, 300.0, 150.0)).unwrap();
        assert_eq!(r, (0, 0, 5, 10));
        let mut px = vec![255u8; 4 * 4 * 4];
        fill_black(&mut px, 4, (1, 1, 3, 2));
        assert_eq!(&px[(4 + 1) * 4..][..4], &[0, 0, 0, 255]);
        assert_eq!(&px[0..4], &[255, 255, 255, 255]);
    }
}
