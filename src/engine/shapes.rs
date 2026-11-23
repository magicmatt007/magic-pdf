//! Appearance streams for annotations magic-pdf draws itself: lines, arrows,
//! polygons, clouds and text stamps.
//!
//! PDFium cannot create Line or Polygon annotations, so these shapes are stored as
//! Ink annotations (their strokes are a fallback for other viewers that redraw ink)
//! with the exact shape in private keys and an appearance stream drawn here.
//! Coordinates are PDF page space.

use super::commands::{PdfBox, Rgb};
use super::textbox::FontSet;

/// Private annotation keys.
pub const SHAPE_KEY: &str = "MagicPdfShape";
pub const POINTS_KEY: &str = "MagicPdfPoints";
pub const STAMP_KEY: &str = "MagicPdfStamp";
/// Measurements: real units per PDF point and the unit, e.g. "0.0353 m".
pub const MEASURE_KEY: &str = "MagicPdfMeasure";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Line,
    Arrow,
    Polygon,
    Cloud,
    /// A measured distance (line with end marks and its length).
    Distance,
    /// A measured area (polygon with its area).
    Area,
}

/// A measuring scale: real `unit`s per PDF point.
#[derive(Clone, Debug, PartialEq)]
pub struct Scale {
    pub per_point: f64,
    pub unit: String,
}

impl Scale {
    /// The scale for a drawing at 1:`ratio` measured in `unit` (mm, cm, m, km, in, ft).
    pub fn new(ratio: f64, unit: &str) -> Scale {
        let mm_per_unit = match unit {
            "cm" => 10.0,
            "m" => 1000.0,
            "km" => 1_000_000.0,
            "in" => 25.4,
            "ft" => 304.8,
            _ => 1.0,
        };
        Scale {
            per_point: 25.4 / 72.0 * ratio / mm_per_unit,
            unit: unit.to_string(),
        }
    }

    pub fn encode(&self) -> String {
        format!("{} {}", self.per_point, self.unit)
    }

    pub fn decode(s: &str) -> Option<Scale> {
        let (n, unit) = s.split_once(' ')?;
        Some(Scale {
            per_point: n.parse().ok()?,
            unit: unit.to_string(),
        })
    }
}

fn number(v: f64) -> String {
    let s = if v >= 100.0 {
        format!("{v:.1}")
    } else if v >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    };
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The length of a line or the area of a polygon, e.g. "12.5 m" or "3.2 m²".
pub fn measure_label(shape: Shape, pts: &[(f32, f32)], scale: &Scale) -> String {
    match shape {
        Shape::Area if pts.len() >= 3 => {
            let twice: f64 = (0..pts.len())
                .map(|i| {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    a.0 as f64 * b.1 as f64 - b.0 as f64 * a.1 as f64
                })
                .sum();
            format!("{} {}²", number(twice.abs() / 2.0 * scale.per_point * scale.per_point), scale.unit)
        }
        _ if pts.len() >= 2 => {
            let len: f64 = pts.windows(2).map(|w| ((w[1].0 - w[0].0) as f64).hypot((w[1].1 - w[0].1) as f64)).sum();
            format!("{} {}", number(len * scale.per_point), scale.unit)
        }
        _ => String::new(),
    }
}

/// Text drawn with glyph outlines along the direction `dir` (unit vector), centred
/// on `at`, its baseline `lift` points away from the line.
fn label(fonts: &dyn FontSet, text: &str, size: f32, at: (f32, f32), dir: (f32, f32), lift: f32) -> String {
    let f = fonts.font(true, false);
    let w: f32 = text.chars().map(|c| f.advance(c) * size).sum();
    // Keep text upright.
    let dir = if dir.0 < -1e-3 || (dir.0.abs() <= 1e-3 && dir.1 < 0.0) { (-dir.0, -dir.1) } else { dir };
    let up = (-dir.1, dir.0);
    let origin = (at.0 - dir.0 * w / 2.0 + up.0 * lift, at.1 - dir.1 * w / 2.0 + up.1 * lift);
    let mut s = format!(
        "q {:.4} {:.4} {:.4} {:.4} {:.2} {:.2} cm\n",
        dir.0, dir.1, up.0, up.1, origin.0, origin.1
    );
    let mut x = 0.0;
    for c in text.chars() {
        if !c.is_whitespace() {
            f.outline(c, size, x, 0.0, &mut s);
            s += "f\n";
        }
        x += f.advance(c) * size;
    }
    s += "Q\n";
    s
}

/// Appearance of a measurement: the shape, its label, and the rectangle.
pub fn measure_appearance(
    shape: Shape,
    pts: &[(f32, f32)],
    color: Rgb,
    width: f32,
    fill: Option<Rgb>,
    text: &str,
    fonts: &dyn FontSet,
) -> (PdfBox, String) {
    let size = (width * 3.0 + 8.0).min(24.0);
    let mut s = format!("q 1 J 1 j {width:.2} w {} RG {} rg\n", rgb(color), rgb(color));
    let mut extent: Vec<(f32, f32)> = pts.to_vec();
    match shape {
        Shape::Distance if pts.len() >= 2 => {
            let (a, b) = (pts[0], pts[pts.len() - 1]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt().max(1e-3);
            let dir = (dx / len, dy / len);
            let n = (-dir.1 * size * 0.5, dir.0 * size * 0.5);
            s += &format!("{:.2} {:.2} m {:.2} {:.2} l S\n", a.0, a.1, b.0, b.1);
            for p in [a, b] {
                // End marks across the line.
                s += &format!("{:.2} {:.2} m {:.2} {:.2} l S\n", p.0 - n.0, p.1 - n.1, p.0 + n.0, p.1 + n.1);
                extent.extend([(p.0 - n.0, p.1 - n.1), (p.0 + n.0, p.1 + n.1)]);
            }
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            s += &label(fonts, text, size, mid, dir, size * 0.35 + width);
            let w: f32 = text.chars().map(|c| fonts.font(true, false).advance(c) * size).sum::<f32>() / 2.0 + 2.0;
            let up = size * 1.3 + width;
            extent.extend([
                (mid.0 - w, mid.1 - up),
                (mid.0 + w, mid.1 + up),
                (mid.0 - w, mid.1 + up),
                (mid.0 + w, mid.1 - up),
            ]);
        }
        Shape::Area if pts.len() >= 3 => {
            s += &format!("{:.2} {:.2} m ", pts[0].0, pts[0].1);
            for p in &pts[1..] {
                s += &format!("{:.2} {:.2} l ", p.0, p.1);
            }
            match fill {
                Some(f) => s += &format!("h q {} rg B Q\n", rgb(f)),
                None => s += "h S\n",
            }
            let n = pts.len() as f32;
            let c = (pts.iter().map(|p| p.0).sum::<f32>() / n, pts.iter().map(|p| p.1).sum::<f32>() / n);
            s += &format!("{} rg\n", rgb(color));
            s += &label(fonts, text, size, c, (1.0, 0.0), -size * 0.35);
        }
        _ => {}
    }
    s += "Q\n";
    let rect = bounds(extent).unwrap_or(PdfBox {
        left: 0.0,
        bottom: 0.0,
        right: 1.0,
        top: 1.0,
    });
    (rect.expand(width / 2.0 + 1.0), s)
}

impl Shape {
    pub fn name(self) -> &'static str {
        match self {
            Shape::Line => "line",
            Shape::Arrow => "arrow",
            Shape::Polygon => "polygon",
            Shape::Cloud => "cloud",
            Shape::Distance => "distance",
            Shape::Area => "area",
        }
    }

    pub fn parse(s: &str) -> Option<Shape> {
        [Shape::Line, Shape::Arrow, Shape::Polygon, Shape::Cloud, Shape::Distance, Shape::Area]
            .into_iter()
            .find(|k| k.name() == s)
    }

    pub fn closed(self) -> bool {
        matches!(self, Shape::Polygon | Shape::Cloud | Shape::Area)
    }
}

pub fn encode_points(pts: &[(f32, f32)]) -> String {
    pts.iter().map(|(x, y)| format!("{x:.2} {y:.2}")).collect::<Vec<_>>().join(" ")
}

pub fn decode_points(s: &str) -> Vec<(f32, f32)> {
    let nums: Vec<f32> = s.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    nums.as_chunks::<2>().0.iter().map(|c| (c[0], c[1])).collect()
}

fn rgb(c: Rgb) -> String {
    format!("{:.3} {:.3} {:.3}", c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0)
}

/// Arrow head length for a stroke width.
fn head_len(width: f32) -> f32 {
    (width * 4.0 + 6.0).min(40.0)
}

/// Radius of a cloud's scallops for a stroke width.
fn scallop(width: f32) -> f32 {
    (width * 2.5 + 4.0).min(20.0)
}

/// Points of an arrow head: tip, left and right corner.
fn arrow_head(from: (f32, f32), tip: (f32, f32), width: f32) -> [(f32, f32); 3] {
    let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
    let len = (dx * dx + dy * dy).sqrt().max(1e-3);
    let (ux, uy) = (dx / len, dy / len);
    let l = head_len(width).min(len);
    let half = l * 0.45;
    let base = (tip.0 - ux * l, tip.1 - uy * l);
    [tip, (base.0 - uy * half, base.1 + ux * half), (base.0 + uy * half, base.1 - ux * half)]
}

/// Cubic Bézier segments (start, c1, c2, end) of a cloud around the polygon `pts`.
fn cloud_segments(pts: &[(f32, f32)], width: f32) -> Vec<[(f32, f32); 4]> {
    let r = scallop(width);
    // Signed area tells the winding, so the scallops bulge outwards.
    let area: f32 = (0..pts.len())
        .map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum();
    let outward = if area >= 0.0 { 1.0 } else { -1.0 };
    let mut out = Vec::new();
    for i in 0..pts.len() {
        let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-3 {
            continue;
        }
        let n = (len / (2.0 * r)).round().max(1.0) as usize;
        let (nx, ny) = (dy / len * outward, -dx / len * outward);
        for k in 0..n {
            let t0 = k as f32 / n as f32;
            let t1 = (k + 1) as f32 / n as f32;
            let p0 = (a.0 + dx * t0, a.1 + dy * t0);
            let p1 = (a.0 + dx * t1, a.1 + dy * t1);
            let h = len / n as f32 / 2.0 * 4.0 / 3.0;
            out.push([p0, (p0.0 + nx * h, p0.1 + ny * h), (p1.0 + nx * h, p1.1 + ny * h), p1]);
        }
    }
    out
}

/// Bounding box of points.
fn bounds(pts: impl IntoIterator<Item = (f32, f32)>) -> Option<PdfBox> {
    pts.into_iter()
        .map(|(x, y)| PdfBox {
            left: x,
            bottom: y,
            right: x,
            top: y,
        })
        .reduce(PdfBox::union)
}

/// The annotation rectangle and appearance stream of a shape.
pub fn appearance(shape: Shape, pts: &[(f32, f32)], color: Rgb, width: f32, fill: Option<Rgb>) -> (PdfBox, String) {
    let mut s = format!("q 1 J 1 j {width:.2} w {} RG {} rg\n", rgb(color), rgb(fill.unwrap_or(color)));
    let mut extent: Vec<(f32, f32)> = pts.to_vec();
    match shape {
        Shape::Line | Shape::Arrow if pts.len() >= 2 => {
            let (a, b) = (pts[0], pts[pts.len() - 1]);
            if shape == Shape::Arrow {
                let head = arrow_head(a, b, width);
                // The shaft stops inside the head so its round cap does not poke out.
                let base = ((head[1].0 + head[2].0) / 2.0, (head[1].1 + head[2].1) / 2.0);
                s += &format!("{:.2} {:.2} m {:.2} {:.2} l S\n", a.0, a.1, base.0, base.1);
                s += &format!("{} rg ", rgb(color));
                s += &format!(
                    "{:.2} {:.2} m {:.2} {:.2} l {:.2} {:.2} l h b\n",
                    head[0].0, head[0].1, head[1].0, head[1].1, head[2].0, head[2].1
                );
                extent.extend(head);
            } else {
                s += &format!("{:.2} {:.2} m {:.2} {:.2} l S\n", a.0, a.1, b.0, b.1);
            }
        }
        Shape::Polygon if pts.len() >= 2 => {
            s += &format!("{:.2} {:.2} m ", pts[0].0, pts[0].1);
            for p in &pts[1..] {
                s += &format!("{:.2} {:.2} l ", p.0, p.1);
            }
            s += if fill.is_some() { "h B\n" } else { "h S\n" };
        }
        Shape::Cloud if pts.len() >= 2 => {
            let segs = cloud_segments(pts, width);
            if let Some(first) = segs.first() {
                s += &format!("{:.2} {:.2} m ", first[0].0, first[0].1);
                for [_, c1, c2, e] in &segs {
                    s += &format!("{:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c ", c1.0, c1.1, c2.0, c2.1, e.0, e.1);
                    extent.extend([*c1, *c2]);
                }
                s += if fill.is_some() { "h B\n" } else { "h S\n" };
            }
        }
        _ => {}
    }
    s += "Q\n";
    let rect = bounds(extent).unwrap_or(PdfBox {
        left: 0.0,
        bottom: 0.0,
        right: 1.0,
        top: 1.0,
    });
    (rect.expand(width / 2.0 + 1.0), s)
}

/// Ink strokes standing in for a shape (for viewers that redraw ink annotations).
pub fn ink_strokes(shape: Shape, pts: &[(f32, f32)], width: f32) -> Vec<Vec<(f32, f32)>> {
    match shape {
        _ if pts.len() < 2 => Vec::new(),
        Shape::Line | Shape::Distance => vec![vec![pts[0], pts[pts.len() - 1]]],
        Shape::Arrow => {
            let (a, b) = (pts[0], pts[pts.len() - 1]);
            let h = arrow_head(a, b, width);
            vec![vec![a, b], vec![h[1], h[0], h[2]]]
        }
        Shape::Polygon | Shape::Area => {
            let mut v = pts.to_vec();
            v.push(pts[0]);
            vec![v]
        }
        Shape::Cloud => {
            let mut v = Vec::new();
            for [p0, c1, c2, p1] in cloud_segments(pts, width) {
                for i in 0..8 {
                    let t = i as f32 / 8.0;
                    let u = 1.0 - t;
                    let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
                    v.push((
                        w[0] * p0.0 + w[1] * c1.0 + w[2] * c2.0 + w[3] * p1.0,
                        w[0] * p0.1 + w[1] * c1.1 + w[2] * c2.1 + w[3] * p1.1,
                    ));
                }
            }
            if let Some(&f) = v.first() {
                v.push(f);
            }
            vec![v]
        }
    }
}

/// Subtitle size relative to the title, and padding (in title sizes).
const STAMP_SUB: f32 = 0.42;
const STAMP_PAD: f32 = 0.6;

fn text_width(fonts: &dyn FontSet, bold: bool, text: &str) -> f32 {
    let f = fonts.font(bold, false);
    text.chars().map(|c| f.advance(c)).sum()
}

/// Natural size of a text stamp with a title of `size` points.
pub fn stamp_size(title: &str, subtitle: &str, size: f32, fonts: &dyn FontSet) -> (f32, f32) {
    let tw = text_width(fonts, true, title);
    let sw = text_width(fonts, false, subtitle) * STAMP_SUB;
    let lines = if subtitle.is_empty() { 1.0 } else { 1.0 + STAMP_SUB * 1.2 };
    let w = (tw.max(sw) + 2.0 * STAMP_PAD) * size;
    let h = (lines + 2.0 * STAMP_PAD * 0.8) * size;
    (w, h)
}

/// Appearance of a text stamp filling `rect`: rounded double frame, bold title and an
/// optional smaller second line (e.g. author and date).
pub fn stamp_appearance(rect: PdfBox, title: &str, subtitle: &str, color: Rgb, fonts: &dyn FontSet) -> String {
    let (w, h) = (rect.right - rect.left, rect.top - rect.bottom);
    let (nw, nh) = stamp_size(title, subtitle, 1.0, fonts);
    let size = (w / nw).min(h / nh).max(0.1);
    let bw = (size * 0.12).max(0.5);
    let radius = (size * 0.45).min(h / 2.0);
    let mut s = format!("q {} RG {} rg {bw:.2} w\n", rgb(color), rgb(color));
    let rr = |s: &mut String, l: f32, b: f32, r: f32, t: f32, rad: f32| {
        let k = rad * 0.4477;
        *s += &format!(
            "{:.2} {:.2} m {:.2} {:.2} l {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c {:.2} {:.2} l {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c \
             {:.2} {:.2} l {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c {:.2} {:.2} l {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c h S\n",
            l + rad, b,
            r - rad, b,
            r - k, b, r, b + k, r, b + rad,
            r, t - rad,
            r, t - k, r - k, t, r - rad, t,
            l + rad, t,
            l + k, t, l, t - k, l, t - rad,
            l, b + rad,
            l, b + k, l + k, b, l + rad, b,
        );
    };
    rr(&mut s, rect.left + bw, rect.bottom + bw, rect.right - bw, rect.top - bw, radius);
    // Text, centred.
    let title_w = text_width(fonts, true, title) * size;
    let sub_size = size * STAMP_SUB;
    let sub_w = text_width(fonts, false, subtitle) * sub_size;
    let block_h = if subtitle.is_empty() { size * 0.72 } else { size * 0.72 + sub_size * 1.25 };
    let top = rect.bottom + h / 2.0 + block_h / 2.0;
    let cx = rect.left + w / 2.0;
    let mut text = |t: &str, bold: bool, sz: f32, width: f32, baseline: f32| {
        let f = fonts.font(bold, false);
        let mut x = cx - width / 2.0;
        for c in t.chars() {
            if !c.is_whitespace() {
                let mut g = String::new();
                f.outline(c, sz, x, baseline, &mut g);
                if !g.is_empty() {
                    s += &g;
                    s += "f\n";
                }
            }
            x += f.advance(c) * sz;
        }
    };
    text(title, true, size, title_w, top - size * 0.72);
    if !subtitle.is_empty() {
        text(subtitle, false, sub_size, sub_w, top - size * 0.72 - sub_size * 1.25);
    }
    s += "Q\n";
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurements_in_real_units() {
        // 1:100 in metres: 72 pt (one inch on paper) is 2.54 m.
        let scale = Scale::new(100.0, "m");
        assert_eq!(measure_label(Shape::Distance, &[(0.0, 0.0), (72.0, 0.0)], &scale), "2.54 m");
        let square = [(0.0, 0.0), (72.0, 0.0), (72.0, 72.0), (0.0, 72.0)];
        assert_eq!(measure_label(Shape::Area, &square, &scale), "6.45 m²");
        assert_eq!(measure_label(Shape::Distance, &[(0.0, 0.0), (0.0, 72.0)], &Scale::new(1.0, "mm")), "25.4 mm");
        assert_eq!(Scale::decode(&scale.encode()), Some(scale));
    }

    #[test]
    fn points_round_trip() {
        let pts = vec![(1.5, 2.25), (-3.0, 400.25)];
        assert_eq!(decode_points(&encode_points(&pts)), pts);
        assert_eq!(Shape::parse("cloud"), Some(Shape::Cloud));
        assert_eq!(Shape::parse("blob"), None);
    }

    #[test]
    fn arrow_rect_includes_the_head() {
        let pts = [(100.0, 100.0), (200.0, 100.0)];
        let (line_rect, _) = appearance(Shape::Line, &pts, [0, 0, 0], 2.0, None);
        let (arrow_rect, ap) = appearance(Shape::Arrow, &pts, [255, 0, 0], 2.0, None);
        assert!(arrow_rect.top > line_rect.top, "the head is wider than the line");
        assert!(ap.contains(" b\n"), "filled head: {ap}");
        assert_eq!(ink_strokes(Shape::Arrow, &pts, 2.0).len(), 2);
    }

    #[test]
    fn clouds_bulge_outwards_whatever_the_winding() {
        let square = [(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)];
        let mut reversed = square;
        reversed.reverse();
        for pts in [square, reversed] {
            let (rect, ap) = appearance(Shape::Cloud, &pts, [0, 0, 255], 1.0, Some([200, 200, 255]));
            assert!(rect.left < -3.0 && rect.right > 103.0 && rect.bottom < -3.0 && rect.top > 103.0, "{rect:?}");
            assert!(ap.contains(" c ") && ap.ends_with("h B\nQ\n"), "{ap}");
            // The fallback stroke stays outside the polygon as well.
            let ink = &ink_strokes(Shape::Cloud, &pts, 1.0)[0];
            assert!(ink.iter().all(|&(x, y)| !(x > 1.0 && x < 99.0 && y > 1.0 && y < 99.0)));
        }
    }

    #[test]
    fn polygon_without_fill_is_only_stroked() {
        let (_, ap) = appearance(Shape::Polygon, &[(0.0, 0.0), (50.0, 0.0), (25.0, 40.0)], [0, 0, 0], 1.0, None);
        assert!(ap.contains("h S\n"));
    }
}
