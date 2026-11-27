//! Text box (FreeText) annotations. PDFium cannot generate their appearance, so we
//! lay the (rich) text out ourselves and draw it as glyph outlines: no font is
//! embedded in the document and any viewer can display it. The text itself stays in
//! /Contents and /RC, and the style in /DA and /DS, so other PDF editors can still
//! edit the box.

use super::commands::{PdfBox, Rgb};
use super::rich::{Align, CharStyle, RichText};

/// Inner padding between border and text, in points.
pub const PADDING: f32 = 4.0;
/// Line height relative to the largest font size on the line.
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

/// The fonts of one family.
pub trait FontSet {
    fn font(&self, bold: bool, italic: bool) -> &dyn GlyphSource;
}

/// Border and background of a box (text styles are part of the rich text).
#[derive(Clone, Debug, PartialEq)]
pub struct BoxStyle {
    /// Border colour and width (no border if width is 0).
    pub border: (Rgb, f32),
    pub fill: Option<Rgb>,
}

/// One laid-out line: characters with their x offset from the line start.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub glyphs: Vec<(char, CharStyle, f32)>,
    /// Width without trailing spaces.
    pub width: f32,
    pub height: f32,
    pub ascent: f32,
}

fn advance(fonts: &dyn FontSet, c: char, s: &CharStyle) -> f32 {
    fonts.font(s.bold, s.italic).advance(c) * s.size
}

fn finish_line(chars: &[(char, CharStyle)], fonts: &dyn FontSet, empty_style: CharStyle) -> Line {
    let mut x = 0.0;
    let mut glyphs = Vec::with_capacity(chars.len());
    let mut width = 0.0;
    for &(c, s) in chars {
        glyphs.push((c, s, x));
        x += advance(fonts, c, &s);
        if !c.is_whitespace() {
            width = x;
        }
    }
    let styles: Vec<CharStyle> = if chars.is_empty() { vec![empty_style] } else { chars.iter().map(|c| c.1).collect() };
    let size = styles.iter().map(|s| s.size).fold(0.0, f32::max);
    let ascent = styles.iter().map(|s| fonts.font(s.bold, s.italic).ascent() * s.size).fold(0.0, f32::max);
    Line { glyphs, width, height: size * LINE_HEIGHT, ascent }
}

/// Breaks rich text into lines no wider than `max_width`. Paragraphs (newlines) are
/// kept; words wider than a line are broken by characters.
pub fn layout(rich: &RichText, max_width: f32, fonts: &dyn FontSet) -> Vec<Line> {
    let chars = rich.chars();
    let default = rich.first_style(CharStyle::default());
    let mut lines = Vec::new();
    let mut paragraphs: Vec<(Vec<(char, CharStyle)>, CharStyle)> = vec![(Vec::new(), default)];
    for &(c, s) in &chars {
        if c == '\n' {
            paragraphs.push((Vec::new(), s));
        } else {
            paragraphs.last_mut().unwrap().0.push((c, s));
        }
    }
    for (para, para_style) in paragraphs {
        // Words: non-space characters followed by their spaces.
        let mut words: Vec<Vec<(char, CharStyle)>> = Vec::new();
        for (c, s) in para {
            let new_word = match words.last() {
                None => true,
                Some(w) => !c.is_whitespace() && w.last().is_some_and(|(p, _)| p.is_whitespace()),
            };
            if new_word {
                words.push(Vec::new());
            }
            words.last_mut().unwrap().push((c, s));
        }
        let mut line: Vec<(char, CharStyle)> = Vec::new();
        let mut line_w = 0.0;
        for word in words {
            let ink: f32 = word.iter().filter(|(c, _)| !c.is_whitespace()).map(|(c, s)| advance(fonts, *c, s)).sum();
            let full: f32 = word.iter().map(|(c, s)| advance(fonts, *c, s)).sum();
            if line_w + ink <= max_width || line.is_empty() && ink <= max_width {
                line.extend(word);
                line_w += full;
                continue;
            }
            if !line.is_empty() {
                lines.push(finish_line(&line, fonts, para_style));
                line.clear();
                line_w = 0.0;
            }
            // Fits on a line of its own, or has to be broken by characters.
            for (c, s) in word {
                let w = advance(fonts, c, &s);
                if line_w + w > max_width && !line.is_empty() && !c.is_whitespace() {
                    lines.push(finish_line(&line, fonts, para_style));
                    line.clear();
                    line_w = 0.0;
                }
                line.push((c, s));
                line_w += w;
            }
        }
        lines.push(finish_line(&line, fonts, para_style));
    }
    lines
}

/// Height a box needs for `lines`.
pub fn box_height(lines: &[Line]) -> f32 {
    2.0 * PADDING + lines.iter().map(|l| l.height).sum::<f32>()
}

fn rgb(c: Rgb) -> String {
    format!("{:.3} {:.3} {:.3}", c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0)
}

/// Lays out `rich` in a box at `rect`. The box keeps its top edge, width and height,
/// but grows downwards if the text needs more room. Returns the box rectangle and the
/// appearance stream.
pub fn appearance(rect: PdfBox, rich: &RichText, style: &BoxStyle, fonts: &dyn FontSet) -> (PdfBox, String) {
    let inner_w = (rect.right - rect.left - 2.0 * PADDING).max(1.0);
    let lines = layout(rich, inner_w, fonts);
    let r = PdfBox { bottom: rect.top - box_height(&lines).max(rect.top - rect.bottom), ..rect };
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
    let mut top = r.top - PADDING;
    for line in &lines {
        let baseline = top - line.ascent - (line.height - line.ascent * LINE_HEIGHT).max(0.0) / 2.0;
        let shift = match rich.align {
            Align::Left => 0.0,
            Align::Center => (inner_w - line.width) / 2.0,
            Align::Right => inner_w - line.width,
        };
        let x0 = r.left + PADDING + shift.max(0.0);
        let mut color: Option<Rgb> = None;
        for &(c, cs, x) in &line.glyphs {
            if c.is_whitespace() {
                continue;
            }
            let mut glyph = String::new();
            fonts.font(cs.bold, cs.italic).outline(c, cs.size, x0 + x, baseline, &mut glyph);
            if glyph.is_empty() {
                continue;
            }
            if color != Some(cs.color) {
                s += &format!("{} rg\n", rgb(cs.color));
                color = Some(cs.color);
            }
            s += &glyph;
            s += "f\n";
        }
        // Underline and strikethrough as thin bars under/through each character.
        for &(c, cs, x) in &line.glyphs {
            let adv = advance(fonts, c, &cs);
            let thickness = (cs.size * 0.06).max(0.5);
            for (on, dy) in [(cs.underline, -cs.size * 0.12), (cs.strike, cs.size * 0.28)] {
                if on && !(c.is_whitespace() && x >= line.width) {
                    s += &format!(
                        "{} rg {:.2} {:.2} {:.2} {:.2} re f\n",
                        rgb(cs.color),
                        x0 + x,
                        baseline + dy - thickness / 2.0,
                        adv,
                        thickness
                    );
                }
            }
        }
        top -= line.height;
    }
    s += "Q\n";
    (r, s)
}

/// Whole-box style changes from the annotation popup. The box's size is the first
/// character's: a new size scales every character by the same factor (so larger or
/// smaller words keep their proportions). A new colour replaces the box's main colour
/// (the first character's) and leaves differently coloured words alone.
pub fn restyle_whole_box(rich: &mut RichText, size: Option<f32>, color: Option<Rgb>) {
    let base = rich.first_style(CharStyle::default());
    for span in &mut rich.spans {
        if let Some(size) = size {
            let scaled = span.style.size * size / base.size.max(0.1);
            span.style.size = ((scaled * 2.0).round() / 2.0).clamp(1.0, 400.0);
        }
        if let Some(color) = color
            && span.style.color == base.color
        {
            span.style.color = color;
        }
    }
}

/// The /DA (default appearance) string other PDF editors use to restyle the box.
pub fn default_appearance(size: f32, color: Rgb) -> String {
    format!("/Helv {size:.1} Tf {} rg", rgb(color))
}

/// The text colour of a /DS default style string ("…; color:#ff0000; …").
pub fn parse_default_style_color(ds: &str) -> Option<Rgb> {
    let lower = ds.to_ascii_lowercase();
    let at = lower.find("color:")? + "color:".len();
    let hex = lower[at..].trim_start().strip_prefix('#')?;
    let hex: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
    let v = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        6 => Some([v(0)?, v(2)?, v(4)?]),
        3 => {
            let c = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|d| d * 17);
            Some([c(0)?, c(1)?, c(2)?])
        }
        _ => None,
    }
}

/// The colour a drawing fills its first rectangle with (a text box's background),
/// as opposed to the colour of its text.
pub fn rectangle_fill(content: &str) -> Option<Rgb> {
    let tokens: Vec<&str> = content.split_whitespace().collect();
    let num = |t: &str| t.parse::<f32>().ok().map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    let mut fill = Some([0, 0, 0]);
    let mut after_re = false;
    for (i, t) in tokens.iter().enumerate() {
        match *t {
            "rg" if i >= 3 => fill = (|| Some([num(tokens[i - 3])?, num(tokens[i - 2])?, num(tokens[i - 1])?]))(),
            "g" if i >= 1 => fill = num(tokens[i - 1]).map(|v| [v, v, v]),
            "re" => after_re = true,
            "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" if after_re => return fill,
            "S" | "s" | "n" | "W" | "W*" => after_re = false,
            _ => {}
        }
    }
    None
}

/// Whether a /DS string is the one magic-pdf wrote before text boxes used the
/// Acrobat colour convention ("font: 12.0pt Helvetica; color:#…; text-align:…").
pub fn is_legacy_magic_style(ds: &str) -> bool {
    ds.starts_with("font: ") && ds.contains("pt Helvetica; color:#") && ds.contains("text-align:")
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

    #[test]
    fn a_box_background_is_the_colour_its_rectangle_is_filled_with() {
        // PDF-XChange: filled and stroked in one go.
        assert_eq!(
            rectangle_fill("1 1 0 rg 1 0 0 RG 1 w 79 671 78 49 re B BT 0 0 0 rg (Box 1) Tj ET"),
            Some([255, 255, 0])
        );
        // magic-pdf: fill, then border.
        assert_eq!(rectangle_fill("0 1 0 rg 300 600 100 60 re f 0 0 1 RG 300 600 100 60 re S"), Some([0, 255, 0]));
        // Only a stroked rectangle and red text: no background.
        assert_eq!(rectangle_fill("1 1 0 RG 1 w 239 619 78 49 re S 1 0 0 rg 246 661 m 247 661 l h f"), None);
        assert_eq!(parse_default_style_color("font-family:Arial;font-size:12pt;color:#00FF80;"), Some([0, 255, 128]));
        assert!(is_legacy_magic_style("font: 12.0pt Helvetica; color:#000000; text-align:left"));
        assert!(!is_legacy_magic_style("font: Helvetica,sans-serif 12.0pt; text-align:left; color:#000000"));
    }
    use crate::engine::rich::Span;

    /// Regular characters are 0.5 em wide, bold ones 0.6 em; glyphs are markers.
    struct Mono {
        bold: bool,
    }
    impl GlyphSource for Mono {
        fn advance(&self, _: char) -> f32 {
            if self.bold { 0.6 } else { 0.5 }
        }
        fn ascent(&self) -> f32 {
            0.8
        }
        fn outline(&self, ch: char, _size: f32, x: f32, y: f32, out: &mut String) {
            out.push_str(&format!("% {ch} at {x:.1} {y:.1}\n"));
        }
    }
    struct Fonts(Mono, Mono);
    impl FontSet for Fonts {
        fn font(&self, bold: bool, _italic: bool) -> &dyn GlyphSource {
            if bold { &self.1 } else { &self.0 }
        }
    }
    fn fonts() -> Fonts {
        Fonts(Mono { bold: false }, Mono { bold: true })
    }

    fn plain(text: &str, size: f32) -> RichText {
        RichText::plain(text, CharStyle { size, ..Default::default() })
    }

    fn line_texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.glyphs.iter().map(|g| g.0).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn wraps_words_and_keeps_paragraphs() {
        // 10 pt text: 5 pt per character, 40 pt per line = 8 characters.
        let lines = layout(&plain("hello big world\nab", 10.0), 40.0, &fonts());
        assert_eq!(line_texts(&lines), vec!["hello", "big", "world", "ab"]);
        let lines = layout(&plain("abcdefghijk", 10.0), 40.0, &fonts());
        assert_eq!(line_texts(&lines), vec!["abcdefgh", "ijk"]);
        assert_eq!(layout(&plain("", 10.0), 40.0, &fonts()).len(), 1);
    }

    #[test]
    fn mixed_styles_measure_and_size_lines() {
        let normal = CharStyle::default();
        let big_bold = CharStyle { bold: true, size: 20.0, ..Default::default() };
        let rich = RichText {
            spans: vec![Span { text: "ab ".into(), style: normal }, Span { text: "CD".into(), style: big_bold }],
            align: Align::Left,
        };
        let lines = layout(&rich, 1000.0, &fonts());
        assert_eq!(lines.len(), 1);
        // 3 * 6 (12 pt regular) + 2 * 12 (20 pt bold) = 42.
        assert!((lines[0].width - 42.0).abs() < 1e-3, "{}", lines[0].width);
        assert_eq!(lines[0].height, 25.0, "the biggest size sets the line height");
        assert_eq!(lines[0].glyphs[3].2, 18.0, "bold text starts after the regular text");
    }

    #[test]
    fn appearance_fits_height_aligns_and_decorates() {
        let style = BoxStyle { border: ([255, 0, 0], 1.0), fill: Some([255, 255, 255]) };
        // A box only 1 pt tall grows to fit the text.
        let rect = PdfBox { left: 100.0, bottom: 499.0, right: 148.0, top: 500.0 };
        let (r, s) = appearance(rect, &plain("hello world", 10.0), &style, &fonts());
        // Two lines: 2 * 4 padding + 2 * 12.5 line height.
        assert_eq!(r.top, 500.0);
        assert!((r.bottom - (500.0 - 33.0)).abs() < 1e-3);
        assert!(s.contains("1.000 1.000 1.000 rg"), "fill");
        assert!(s.contains("1.000 0.000 0.000 RG 1.00 w"), "border");
        // Baseline: top padding, then half the extra line spacing (12.5 - 10) / 2,
        // then the ascent (8): 500 - 4 - 1.25 - 8.
        assert!(s.contains("% h at 104.0 486.8"), "{s}");
        assert!(s.contains("% w at 104.0 474.2"), "{s}");

        // Centred: "ab" (10 pt wide) in a 40 pt wide inner area starts 15 pt in.
        let mut centred = plain("ab", 10.0);
        centred.align = Align::Center;
        let (_, s) = appearance(rect, &centred, &style, &fonts());
        assert!(s.contains("% a at 119.0"), "{s}");

        // Underline: a bar under the characters.
        let mut u = plain("ab", 10.0);
        u.spans[0].style.underline = true;
        let (_, s) = appearance(rect, &u, &style, &fonts());
        // 1.2 pt below the baseline (486.75), 0.6 pt thick.
        assert!(s.contains("104.00 485.25 5.00 0.60 re f"), "{s}");
    }

    #[test]
    fn a_box_taller_than_its_text_keeps_its_height() {
        let style = BoxStyle { border: ([0, 0, 0], 1.0), fill: None };
        let rect = PdfBox { left: 100.0, bottom: 300.0, right: 300.0, top: 500.0 };
        let (r, _) = appearance(rect, &plain("short", 10.0), &style, &fonts());
        assert_eq!(r, rect);
    }

    #[test]
    fn whole_box_changes_keep_individual_styles() {
        let red = [220, 30, 30];
        let mut rich = RichText {
            spans: vec![
                Span { text: "normal ".into(), style: CharStyle::default() },
                Span { text: "big red".into(), style: CharStyle { size: 18.0, color: red, ..Default::default() } },
            ],
            align: Align::Left,
        };
        restyle_whole_box(&mut rich, Some(24.0), Some([0, 0, 255]));
        assert_eq!(rich.spans[0].style.size, 24.0);
        assert_eq!(rich.spans[1].style.size, 36.0, "keeps its 1.5x size");
        assert_eq!(rich.spans[0].style.color, [0, 0, 255]);
        assert_eq!(rich.spans[1].style.color, red, "accent colour kept");
    }

    #[test]
    fn default_appearance_round_trip() {
        let da = default_appearance(14.0, [0, 110, 230]);
        assert_eq!(parse_default_appearance(&da), (Some(14.0), Some([0, 110, 230])));
        assert_eq!(parse_default_appearance("/Helv 0 Tf 0 g"), (None, Some([0, 0, 0])));
    }
}
