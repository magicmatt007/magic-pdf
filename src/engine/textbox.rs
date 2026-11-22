//! Text box (FreeText) annotations. PDFium cannot generate their appearance, so we
//! lay the text out ourselves and draw it as glyph outlines (no font is embedded in
//! the document; any viewer can display it). The text itself stays in /Contents and
//! the style in /DA, so other PDF editors can still edit the box.

use super::commands::{PdfBox, Rgb};

/// Inner padding between border and text, in points.
pub const PADDING: f32 = 4.0;
/// Line height relative to the font size.
pub const LINE_HEIGHT: f32 = 1.25;
/// Width of a box created with a single click.
pub const DEFAULT_WIDTH: f32 = 200.0;

/// Glyph metrics and outlines (implemented with PDFium; faked in tests).
pub trait GlyphSource {
    /// Advance width of `ch` at font size 1.
    fn advance(&self, ch: char) -> f32;
    /// Ascent at font size 1.
    fn ascent(&self) -> f32;
    /// Appends the outline of `ch` at `size`, placed with its origin at (x, y), as
    /// PDF path operators (without the final fill operator).
    fn outline(&self, ch: char, size: f32, x: f32, y: f32, out: &mut String);
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoxStyle {
    pub font_size: f32,
    pub text_color: Rgb,
    /// Border colour and width (no border if width is 0).
    pub border: (Rgb, f32),
    pub fill: Option<Rgb>,
}

/// Splits `text` into lines no wider than `max_width` (font size `size`). Paragraphs
/// (newlines) are kept; words wider than a line are broken by characters.
pub fn wrap(text: &str, max_width: f32, size: f32, font: &dyn GlyphSource) -> Vec<String> {
    let width = |s: &str| s.chars().map(|c| font.advance(c)).sum::<f32>() * size;
    let max_width = max_width.max(size);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if width(&candidate) <= max_width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // The word alone may still be too wide: break it.
            for ch in word.chars() {
                let mut next = line.clone();
                next.push(ch);
                if width(&next) > max_width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    line.push(ch);
                } else {
                    line = next;
                }
            }
        }
        lines.push(line);
    }
    lines
}

/// Height a box needs for `lines` lines of text.
pub fn box_height(lines: usize, size: f32) -> f32 {
    2.0 * PADDING + lines.max(1) as f32 * size * LINE_HEIGHT
}

fn rgb(c: Rgb) -> String {
    format!(
        "{:.3} {:.3} {:.3}",
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0
    )
}

/// Lays out `text` in a box whose top-left corner and width come from `rect`. Returns
/// the box rectangle (height fitted to the text) and the appearance content stream.
pub fn appearance(rect: PdfBox, text: &str, style: &BoxStyle, font: &dyn GlyphSource) -> (PdfBox, String) {
    let size = style.font_size.max(1.0);
    let inner_w = (rect.right - rect.left - 2.0 * PADDING).max(size);
    let lines = wrap(text, inner_w, size, font);
    let height = box_height(lines.len(), size);
    let r = PdfBox {
        bottom: rect.top - height,
        ..rect
    };
    let (w, h) = (r.right - r.left, r.top - r.bottom);
    let mut s = String::from("q\n");
    if let Some(fill) = style.fill {
        s += &format!("{} rg {:.2} {:.2} {:.2} {:.2} re f\n", rgb(fill), r.left, r.bottom, w, h);
    }
    let (border, bw) = style.border;
    if bw > 0.0 {
        s += &format!(
            "{} RG {bw:.2} w {:.2} {:.2} {:.2} {:.2} re S\n",
            rgb(border),
            r.left + bw / 2.0,
            r.bottom + bw / 2.0,
            w - bw,
            h - bw
        );
    }
    s += &format!("{} rg\n", rgb(style.text_color));
    let mut baseline = r.top - PADDING - font.ascent() * size;
    for line in &lines {
        let mut x = r.left + PADDING;
        for ch in line.chars() {
            if !ch.is_whitespace() {
                let mut glyph = String::new();
                font.outline(ch, size, x, baseline, &mut glyph);
                if !glyph.is_empty() {
                    s += &glyph;
                    s += "f\n";
                }
            }
            x += font.advance(ch) * size;
        }
        baseline -= size * LINE_HEIGHT;
    }
    s += "Q\n";
    (r, s)
}

/// The /DA (default appearance) string other PDF editors use to restyle the box.
pub fn default_appearance(style: &BoxStyle) -> String {
    format!("/Helv {:.1} Tf {} rg", style.font_size, rgb(style.text_color))
}

/// Reads font size and text colour back from a /DA string.
pub fn parse_default_appearance(da: &str) -> (Option<f32>, Option<Rgb>) {
    let tokens: Vec<&str> = da.split_whitespace().collect();
    let num = |i: usize| tokens.get(i).and_then(|t| t.parse::<f32>().ok());
    let mut size = None;
    let mut color = None;
    for (i, t) in tokens.iter().enumerate() {
        match *t {
            "Tf" if i >= 1 => size = num(i - 1),
            "rg" if i >= 3 => {
                if let (Some(r), Some(g), Some(b)) = (num(i - 3), num(i - 2), num(i - 1)) {
                    color = Some([r, g, b].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8));
                }
            }
            "g" if i >= 1 => {
                if let Some(v) = num(i - 1) {
                    let v = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                    color = Some([v, v, v]);
                }
            }
            _ => {}
        }
    }
    (size.filter(|s| *s > 0.0), color)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character is 0.5 em wide; each glyph outline is a marker.
    struct Mono;
    impl GlyphSource for Mono {
        fn advance(&self, _: char) -> f32 {
            0.5
        }
        fn ascent(&self) -> f32 {
            0.8
        }
        fn outline(&self, ch: char, _size: f32, x: f32, y: f32, out: &mut String) {
            out.push_str(&format!("% {ch} at {x:.1} {y:.1}\n"));
        }
    }

    #[test]
    fn wraps_words_and_keeps_paragraphs() {
        // 10 pt text: 5 pt per character, 40 pt per line = 8 characters.
        let lines = wrap("hello big world\nab", 40.0, 10.0, &Mono);
        assert_eq!(lines, vec!["hello", "big", "world", "ab"]);
        let lines = wrap("abcdefghijk", 40.0, 10.0, &Mono);
        assert_eq!(lines, vec!["abcdefgh", "ijk"]);
        assert_eq!(wrap("", 40.0, 10.0, &Mono), vec![""]);
    }

    #[test]
    fn appearance_fits_height_and_places_text() {
        let style = BoxStyle {
            font_size: 10.0,
            text_color: [0, 0, 0],
            border: ([255, 0, 0], 1.0),
            fill: Some([255, 255, 255]),
        };
        let rect = PdfBox {
            left: 100.0,
            bottom: 0.0,
            right: 148.0,
            top: 500.0,
        };
        let (r, s) = appearance(rect, "hello world", &style, &Mono);
        // Two lines: 2 * 4 padding + 2 * 12.5 line height.
        assert_eq!(r.top, 500.0);
        assert!((r.bottom - (500.0 - 33.0)).abs() < 1e-3);
        assert!(s.contains("1.000 1.000 1.000 rg"), "fill");
        assert!(s.contains("1.000 0.000 0.000 RG 1.00 w"), "border");
        // First glyph at the padding, baseline one ascent below the top padding.
        assert!(s.contains("% h at 104.0 488.0"), "{s}");
        assert!(s.contains("% w at 104.0 475.5"), "{s}");
    }

    #[test]
    fn default_appearance_round_trip() {
        let style = BoxStyle {
            font_size: 14.0,
            text_color: [0, 110, 230],
            border: ([0, 0, 0], 1.0),
            fill: None,
        };
        let da = default_appearance(&style);
        assert_eq!(parse_default_appearance(&da), (Some(14.0), Some([0, 110, 230])));
        assert_eq!(parse_default_appearance("/Helv 0 Tf 0 g"), (None, Some([0, 0, 0])));
    }
}
