//! Text recognition (OCR) with the Tesseract command-line program.
//!
//! Tesseract is run as a separate program rather than linked: it keeps magic-pdf
//! small, Tesseract's language data stays where the user's installation keeps it,
//! and a crash in recognition cannot take the viewer down. Pages are rendered by
//! PDFium (on the worker thread), written as a greyscale PGM image, recognised to
//! hOCR, and the words are placed back onto the page as invisible text.

use crate::i18n::{t, tf};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolution pages are rendered at for recognition.
pub const DPI: f32 = 300.0;
/// Longest side of a rendered page in pixels (very large pages are rendered smaller).
pub const MAX_SIDE: f32 = 8000.0;
/// Pages with at least this many non-space characters already have text and are
/// skipped unless recognition is forced.
pub const HAS_TEXT_CHARS: usize = 20;

/// How to install Tesseract, shown when it is missing.
pub const INSTALL_HELP: &str = "Text recognition uses the free Tesseract OCR program, which was not found.\n\n\
Linux (Debian/Ubuntu): sudo apt install tesseract-ocr tesseract-ocr-deu\n\
Linux (Fedora): sudo dnf install tesseract tesseract-langpack-deu\n\
Windows: install it from github.com/UB-Mannheim/tesseract/wiki (choose the languages you need)\n\
macOS: brew install tesseract tesseract-lang\n\n\
Then open this dialog again.";

/// A recognised word placed on a page (PDF page space, points). The word's text runs
/// along the baseline from `origin` to `end`; `up` is the unit vector towards the top
/// of the letters (it differs from "up the page" on rotated pages).
#[derive(Clone, Debug, PartialEq)]
pub struct OcrWord {
    pub text: String,
    pub origin: (f32, f32),
    pub end: (f32, f32),
    pub up: (f32, f32),
    pub size: f32,
}

/// A word as Tesseract reports it, in image pixels (origin top-left).
#[derive(Clone, Debug, PartialEq)]
pub struct HocrWord {
    pub text: String,
    /// x0, y0, x1, y1.
    pub bbox: [f32; 4],
    /// y of the baseline at the word's centre.
    pub baseline: f32,
    /// Text height (em size) in pixels.
    pub size: f32,
}

/// The Tesseract program: `MAGIC_PDF_TESSERACT`, `tesseract` on PATH, or the usual
/// install locations (Windows installer, Homebrew).
pub fn find_tesseract() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MAGIC_PDF_TESSERACT") {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "tesseract.exe" } else { "tesseract" };
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                dirs.push(base.join("Tesseract-OCR"));
                dirs.push(base.join("Programs").join("Tesseract-OCR"));
            }
        }
    } else {
        // GUI apps on macOS do not get the shell's PATH.
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/opt/local/bin"].map(PathBuf::from));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

fn command(exe: &Path) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Installed recognition languages (Tesseract codes like `eng`, `deu`), without the
/// orientation-detection pseudo language `osd`.
pub fn languages(exe: &Path) -> Result<Vec<String>, String> {
    let out = command(exe)
        .arg("--list-langs")
        .output()
        .map_err(|e| tf("Could not run {file}: {e}", &[("e", &e), ("file", &exe.display())]))?;
    // Older versions print the list on stderr.
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let mut langs: Vec<String> = text
        .lines()
        .skip_while(|l| !l.starts_with("List of available languages"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "osd" && !l.contains(' '))
        .map(String::from)
        .collect();
    langs.sort();
    langs.dedup();
    if langs.is_empty() {
        return Err(t("Tesseract is installed but has no language data.").into());
    }
    Ok(langs)
}

/// Runs Tesseract on an image file and returns its hOCR output.
pub fn recognise(exe: &Path, image: &Path, langs: &str) -> Result<String, String> {
    let out = command(exe)
        .arg(image)
        .arg("stdout")
        .args(["-l", langs, "--dpi"])
        .arg(format!("{DPI:.0}"))
        .arg("hocr")
        .output()
        .map_err(|e| tf("Could not run Tesseract: {e}", &[("e", &e)]))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("unknown error");
        return Err(tf("Tesseract failed: {msg}", &[("msg", &msg)]));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Writes 8-bit greyscale pixels as a binary PGM (which Tesseract reads natively).
pub fn write_pgm(path: &Path, width: usize, height: usize, gray: &[u8]) -> std::io::Result<()> {
    let mut data = format!("P5\n{width} {height}\n255\n").into_bytes();
    data.extend_from_slice(gray);
    std::fs::write(path, data)
}

/// Reads `key v1 v2 …` from an hOCR title attribute (`bbox 1 2 3 4; x_size 30`).
fn title_values(title: &str, key: &str) -> Option<Vec<f32>> {
    title.split(';').find_map(|part| {
        let mut it = part.split_whitespace();
        (it.next() == Some(key)).then(|| it.filter_map(|v| v.parse().ok()).collect())
    })
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let start = tag.find(&format!("{name}="))? + name.len() + 1;
    let quote = tag[start..].chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &tag[start + 1..];
    Some(&rest[..rest.find(quote)?])
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Extracts words with their boxes and baselines from Tesseract's hOCR output.
pub fn parse_hocr(hocr: &str) -> Vec<HocrWord> {
    // (bbox, baseline slope, baseline offset, size) of the current line.
    let mut line: Option<([f32; 4], f32, f32, Option<f32>)> = None;
    let mut words = Vec::new();
    let mut rest = hocr;
    while let Some(i) = rest.find("<span") {
        rest = &rest[i..];
        let Some(tag_end) = rest.find('>') else { break };
        let tag = &rest[..tag_end];
        rest = &rest[tag_end + 1..];
        let class = attr(tag, "class").unwrap_or("");
        let title = attr(tag, "title").unwrap_or("");
        let Some(bbox) = title_values(title, "bbox").and_then(|v| <[f32; 4]>::try_from(v).ok()) else {
            continue;
        };
        if class == "ocrx_word" {
            let Some(end) = rest.find("</span>") else { break };
            // Words may contain <strong>/<em> markup.
            let text = decode_entities(&strip_tags(&rest[..end])).trim().to_string();
            rest = &rest[end + "</span>".len()..];
            if text.is_empty() || bbox[2] <= bbox[0] || bbox[3] <= bbox[1] {
                continue;
            }
            let height = bbox[3] - bbox[1];
            let (baseline, size) = match line {
                Some((lb, slope, offset, size)) => {
                    let cx = (bbox[0] + bbox[2]) / 2.0;
                    let y = lb[3] + offset + slope * (cx - lb[0]);
                    (y.clamp(bbox[1], bbox[3]), size.unwrap_or(lb[3] - lb[1]))
                }
                None => (bbox[3] - height * 0.2, height),
            };
            words.push(HocrWord { text, bbox, baseline, size: size.max(1.0) });
        } else if matches!(class, "ocr_line" | "ocr_header" | "ocr_caption" | "ocr_textfloat") {
            let (slope, offset) = match title_values(title, "baseline").as_deref() {
                Some([s, o]) => (*s, *o),
                _ => (0.0, 0.0),
            };
            let size = title_values(title, "x_size").and_then(|v| v.first().copied());
            line = Some((bbox, slope, offset, size));
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hocr_words_and_baselines() {
        let hocr = r#"
     <span class='ocr_line' id='line_1_2' title="bbox 241 588 1508 649; baseline 0.01 -15; x_size 60; x_descenders 15">
      <span class='ocrx_word' id='word_1_5' title='bbox 241 589 340 634; x_wconf 96'>The</span>
      <span class='ocrx_word' id='word_1_6' title='bbox 363 589 509 649; x_wconf 96'><strong>Q&amp;A</strong></span>
      <span class='ocrx_word' id='word_1_7' title='bbox 531 589 700 634; x_wconf 96'> </span>
     </span>
     <span class='ocr_header' id='line_1_3' title="bbox 10 700 200 760; baseline 0 -10">
      <span class='ocrx_word' id='word_1_8' title='bbox 10 700 90 750; x_wconf 90'>&#252;ber&lt;</span>
     </span>"#;
        let w = parse_hocr(hocr);
        assert_eq!(w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["The", "Q&A", "über<"]);
        assert_eq!(w[0].bbox, [241.0, 589.0, 340.0, 634.0]);
        assert_eq!(w[0].size, 60.0);
        // 649 - 15 + 0.01 * (290.5 - 241), kept within the word's box.
        assert_eq!(w[0].baseline, 634.0);
        assert!((w[1].baseline - 635.95).abs() < 0.01);
        // No x_size: the line height.
        assert_eq!(w[2].size, 60.0);
        assert_eq!(w[2].baseline, 750.0);
    }

    #[test]
    fn parses_hocr_attributes() {
        assert_eq!(title_values("bbox 1 2 3 4; x_wconf 9", "x_wconf"), Some(vec![9.0]));
        assert_eq!(attr("<span class='a' title=\"b c\"", "title"), Some("b c"));
        assert_eq!(decode_entities("a &amp b &#x41;"), "a &amp b A");
    }
}
