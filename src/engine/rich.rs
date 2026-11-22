//! Rich text for text box annotations: styled runs, stored in the PDF as the plain
//! text (/Contents) plus the standard rich text XHTML (/RC) and a default style (/DS),
//! the format Acrobat and PDF-XChange use.

use super::commands::Rgb;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Rgb,
    /// Font size in points.
    pub size: f32,
}

impl Default for CharStyle {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            color: [0, 0, 0],
            size: 12.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    fn css(self) -> &'static str {
        match self {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: CharStyle,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    pub spans: Vec<Span>,
    pub align: Align,
}

impl RichText {
    pub fn plain(text: &str, style: CharStyle) -> RichText {
        RichText {
            spans: vec![Span {
                text: text.to_string(),
                style,
            }],
            align: Align::Left,
        }
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// One style per character (for editing).
    pub fn chars(&self) -> Vec<(char, CharStyle)> {
        self.spans
            .iter()
            .flat_map(|s| s.text.chars().map(move |c| (c, s.style)))
            .collect()
    }

    /// Builds rich text from per-character styles, merging equal neighbours.
    pub fn from_chars(chars: &[(char, CharStyle)], align: Align) -> RichText {
        let mut spans: Vec<Span> = Vec::new();
        for &(c, style) in chars {
            match spans.last_mut() {
                Some(last) if last.style == style => last.text.push(c),
                _ => spans.push(Span {
                    text: c.to_string(),
                    style,
                }),
            }
        }
        RichText { spans, align }
    }

    /// The style of the first character (or `fallback` for empty text).
    pub fn first_style(&self, fallback: CharStyle) -> CharStyle {
        self.spans.first().map_or(fallback, |s| s.style)
    }

    /// The /RC value.
    pub fn to_xhtml(&self) -> String {
        let base = self.first_style(CharStyle::default());
        let mut out = format!(
            "<?xml version=\"1.0\"?><body xmlns=\"http://www.w3.org/1999/xhtml\" \
             xmlns:xfa=\"http://www.xfa.org/schema/xfa-data/1.0/\" xfa:APIVersion=\"Acrobat:11.0.0\" \
             xfa:spec=\"2.0.2\" style=\"font-family:Noto Sans,Helvetica,sans-serif;{}text-align:{}\"><p dir=\"ltr\">",
            style_css(&base),
            self.align.css()
        );
        for span in &self.spans {
            for (i, part) in span.text.split('\n').enumerate() {
                if i > 0 {
                    out += "</p><p dir=\"ltr\">";
                }
                if !part.is_empty() {
                    out += &format!("<span style=\"{}\">{}</span>", style_css(&span.style), escape(part));
                }
            }
        }
        out += "</p></body>";
        out
    }

    /// The /DS (default style) value.
    pub fn default_style_string(&self) -> String {
        let s = self.first_style(CharStyle::default());
        format!(
            "font: {}{}{:.1}pt Helvetica; color:{}; text-align:{}",
            if s.italic { "italic " } else { "" },
            if s.bold { "bold " } else { "" },
            s.size,
            hex(s.color),
            self.align.css()
        )
    }

    /// Reads an /RC value. Styles not given fall back to `default`.
    pub fn parse_xhtml(xhtml: &str, default: CharStyle) -> Option<RichText> {
        let mut stack: Vec<CharStyle> = vec![default];
        let mut chars: Vec<(char, CharStyle)> = Vec::new();
        let mut align = Align::Left;
        let mut paragraphs = 0;
        // A paragraph break waiting for the next text (it takes that text's style).
        let mut pending_break = false;
        let mut rest = xhtml;
        let mut saw_body = false;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('<') {
                let end = after.find('>')?;
                let tag = &after[..end];
                rest = &after[end + 1..];
                if tag.starts_with('?') || tag.starts_with('!') {
                    continue;
                }
                let closing = tag.starts_with('/');
                let self_closing = tag.ends_with('/');
                let name = tag
                    .trim_start_matches('/')
                    .trim_end_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if closing {
                    if stack.len() > 1 && !matches!(name.as_str(), "br") {
                        stack.pop();
                    }
                    continue;
                }
                if name == "br" {
                    chars.push(('\n', *stack.last().unwrap()));
                    continue;
                }
                let mut style = *stack.last().unwrap();
                match name.as_str() {
                    "b" | "strong" => style.bold = true,
                    "i" | "em" => style.italic = true,
                    "u" => style.underline = true,
                    "s" | "strike" | "del" => style.strike = true,
                    "body" => saw_body = true,
                    "p" | "div" => {
                        if paragraphs > 0 {
                            if pending_break {
                                chars.push(('\n', style));
                            }
                            pending_break = true;
                        }
                        paragraphs += 1;
                    }
                    _ => {}
                }
                if let Some(css) = attr(tag, "style") {
                    apply_css(&mut style, &mut align, &css);
                }
                if !self_closing {
                    stack.push(style);
                }
            } else {
                let end = rest.find('<').unwrap_or(rest.len());
                let text = unescape(&rest[..end]);
                rest = &rest[end..];
                // Whitespace between block tags is formatting, not text.
                if stack.len() <= 2 && text.trim().is_empty() {
                    continue;
                }
                let style = *stack.last().unwrap();
                if std::mem::take(&mut pending_break) {
                    chars.push(('\n', style));
                }
                chars.extend(text.chars().filter(|c| *c != '\r').map(|c| (if c == '\n' { ' ' } else { c }, style)));
            }
        }
        if pending_break {
            chars.push(('\n', *stack.last().unwrap()));
        }
        saw_body.then(|| RichText::from_chars(&chars, align))
    }
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn style_css(s: &CharStyle) -> String {
    let mut decor = Vec::new();
    if s.underline {
        decor.push("underline");
    }
    if s.strike {
        decor.push("line-through");
    }
    format!(
        "font-size:{:.1}pt;color:{};font-weight:{};font-style:{};text-decoration:{};",
        s.size,
        hex(s.color),
        if s.bold { "bold" } else { "normal" },
        if s.italic { "italic" } else { "normal" },
        if decor.is_empty() { "none".to_string() } else { decor.join(" ") }
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out += &rest[..i];
        let after = &rest[i + 1..];
        match after.find(';').filter(|&e| e <= 10) {
            Some(e) => {
                let ent = &after[..e];
                let ch = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{a0}'),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                        u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                    }
                    _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                    _ => None,
                };
                match ch {
                    Some(c) => {
                        out.push(c);
                        rest = &after[e + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = after;
                    }
                }
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out + rest
}

/// Value of attribute `name` in a tag's text (single or double quotes).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let start = from + i;
        let before_ok = start == 0 || lower.as_bytes()[start - 1].is_ascii_whitespace();
        let after = lower[start + name.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            let value = tag[tag.len() - after.len() + 1..].trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let inner = &value[1..];
                return inner.find(quote).map(|e| unescape(&inner[..e]));
            }
        }
        from = start + name.len();
    }
    None
}

fn parse_color(v: &str) -> Option<Rgb> {
    let v = v.trim();
    if let Some(h) = v.strip_prefix('#') {
        let full: String = if h.len() == 3 { h.chars().flat_map(|c| [c, c]).collect() } else { h.to_string() };
        let n = u32::from_str_radix(full.get(..6)?, 16).ok()?;
        return Some([(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    let inner = v.strip_prefix("rgb(")?.strip_suffix(')')?;
    let parts: Vec<u8> = inner.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    (parts.len() == 3).then(|| [parts[0], parts[1], parts[2]])
}

fn parse_size(v: &str) -> Option<f32> {
    let v = v.trim().trim_end_matches("pt").trim_end_matches("px");
    v.parse::<f32>().ok().filter(|s| *s > 0.0)
}

fn apply_css(style: &mut CharStyle, align: &mut Align, css: &str) {
    for decl in css.split(';') {
        let Some((key, value)) = decl.split_once(':') else { continue };
        let value = value.trim().to_ascii_lowercase();
        match key.trim().to_ascii_lowercase().as_str() {
            "font-size" => {
                if let Some(s) = parse_size(&value) {
                    style.size = s;
                }
            }
            "color" => {
                if let Some(c) = parse_color(&value) {
                    style.color = c;
                }
            }
            "font-weight" => {
                style.bold = value == "bold" || value == "bolder" || value.parse::<u32>().is_ok_and(|w| w >= 600)
            }
            "font-style" => style.italic = value == "italic" || value == "oblique",
            "text-decoration" => {
                style.underline = value.contains("underline");
                style.strike = value.contains("line-through");
            }
            "text-align" => {
                *align = match value.as_str() {
                    "center" => Align::Center,
                    "right" => Align::Right,
                    _ => Align::Left,
                }
            }
            "font" => {
                for tok in value.split_whitespace() {
                    match tok {
                        "bold" => style.bold = true,
                        "italic" | "oblique" => style.italic = true,
                        t if t.ends_with("pt") => {
                            if let Some(s) = parse_size(t) {
                                style.size = s;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(bold: bool, color: Rgb) -> CharStyle {
        CharStyle {
            bold,
            color,
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_through_xhtml() {
        let rich = RichText {
            spans: vec![
                Span {
                    text: "Hello ".into(),
                    style: st(false, [0, 0, 0]),
                },
                Span {
                    text: "bold & <red>".into(),
                    style: st(true, [220, 30, 30]),
                },
                Span {
                    text: "\nПривет".into(),
                    style: CharStyle {
                        italic: true,
                        underline: true,
                        strike: true,
                        size: 18.0,
                        ..Default::default()
                    },
                },
            ],
            align: Align::Center,
        };
        let back = RichText::parse_xhtml(&rich.to_xhtml(), CharStyle::default()).unwrap();
        assert_eq!(back, rich);
        assert_eq!(back.text(), "Hello bold & <red>\nПривет");
    }

    #[test]
    fn reads_acrobat_style_rich_text() {
        let rc = r#"<?xml version="1.0"?><body xmlns="http://www.w3.org/1999/xhtml" style="font-size:10.0pt;text-align:right;color:#FF0000;font-weight:normal;font-style:normal;font-family:Helvetica">
<p dir="ltr"><span style="font-weight:bold">Bold</span> plain <b><i>both</i></b>&#160;x</p>
<p dir="ltr"><span style="text-decoration:word;color:rgb(0,0,255)">blue</span></p></body>"#;
        let r = RichText::parse_xhtml(rc, CharStyle::default()).unwrap();
        assert_eq!(r.text(), "Bold plain both\u{a0}x\nblue");
        assert_eq!(r.align, Align::Right);
        let chars = r.chars();
        assert!(chars[0].1.bold && chars[0].1.color == [255, 0, 0] && chars[0].1.size == 10.0);
        assert!(!chars[5].1.bold);
        let both = chars[11].1;
        assert!(both.bold && both.italic, "{both:?}");
        assert_eq!(chars.last().unwrap().1.color, [0, 0, 255]);
        assert!(RichText::parse_xhtml("not rich text", CharStyle::default()).is_none());
    }

    #[test]
    fn spans_merge_and_default_style() {
        let a = st(false, [0, 0, 0]);
        let b = st(true, [0, 0, 0]);
        let r = RichText::from_chars(&[('a', a), ('b', a), ('c', b)], Align::Left);
        assert_eq!(r.spans.len(), 2);
        assert_eq!(r.spans[0].text, "ab");
        assert_eq!(r.default_style_string(), "font: 12.0pt Helvetica; color:#000000; text-align:left");
    }
}
