//! Editing the text a page shows: its text objects, grouped into paragraphs, and
//! changes to them (new text, position, size, colour, removal). A change is applied
//! to the loaded page without writing its content: the caller writes it
//! (`Page::generate`) or drops the loaded page (`Doc::discard_page`) to undo it,
//! which is how previews work.

use super::*;

const FPDF_PAGEOBJ_TEXT: c_int = 1;
const INVISIBLE: FPDF_TEXT_RENDERMODE = 3;
const IDENTITY: FS_MATRIX = FS_MATRIX { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

/// The text of a text object (on the page `text_page` was loaded from).
unsafe fn text_of(lib: Lib, obj: FPDF_PAGEOBJECT, text_page: FPDF_TEXTPAGE) -> String {
    unsafe {
        let len = lib.FPDFTextObj_GetText(obj, text_page, ptr::null_mut(), 0) as usize;
        let mut buf = vec![0u16; len.div_ceil(2).max(1)];
        lib.FPDFTextObj_GetText(obj, text_page, buf.as_mut_ptr(), (buf.len() * 2) as c_ulong);
        utf16_buf_to_string(&buf)
    }
}

/// A character the standard fonts can show (Windows-1252, the WinAnsi encoding
/// PDFium gives them).
pub(crate) fn in_win_ansi(c: char) -> bool {
    matches!(c, ' '..='~' | '\u{a0}'..='\u{ff}') || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c)
}

/// The standard font closest to `font`: Times for serif fonts, Courier for
/// monospaced ones, else Helvetica; bold and italic as the font is.
unsafe fn standard_font_for(lib: Lib, font: FPDF_FONT) -> &'static str {
    let (mut serif, mut mono, mut bold, mut italic) = (false, false, false, false);
    if !font.is_null() {
        unsafe {
            let name = font_name(lib, font).to_lowercase();
            // Flags (PDF 1.7, 9.8.2): FixedPitch 1, Serif 2, Italic 64.
            let flags = lib.FPDFFont_GetFlags(font);
            mono = flags & 1 != 0 || name.contains("courier") || name.contains("mono");
            serif = flags & 2 != 0 || name.contains("times") || name.contains("serif") && !name.contains("sans");
            let mut angle = 0;
            let has_angle = lib.is_true(lib.FPDFFont_GetItalicAngle(font, &mut angle));
            italic = flags & 64 != 0 || has_angle && angle != 0 || name.contains("italic") || name.contains("oblique");
            bold = lib.FPDFFont_GetWeight(font) >= 600 || name.contains("bold");
        }
    }
    match (mono, serif, bold, italic) {
        (true, _, false, false) => "Courier",
        (true, _, true, false) => "Courier-Bold",
        (true, _, false, true) => "Courier-Oblique",
        (true, _, true, true) => "Courier-BoldOblique",
        (false, true, false, false) => "Times-Roman",
        (false, true, true, false) => "Times-Bold",
        (false, true, false, true) => "Times-Italic",
        (false, true, true, true) => "Times-BoldItalic",
        (false, false, false, false) => "Helvetica",
        (false, false, true, false) => "Helvetica-Bold",
        (false, false, false, true) => "Helvetica-Oblique",
        (false, false, true, true) => "Helvetica-BoldOblique",
    }
}

unsafe fn font_name(lib: Lib, font: FPDF_FONT) -> String {
    if font.is_null() {
        return String::new();
    }
    unsafe {
        let len = lib.FPDFFont_GetBaseFontName(font, ptr::null_mut(), 0);
        let mut buf = vec![0u8; len];
        lib.FPDFFont_GetBaseFontName(font, buf.as_mut_ptr().cast(), len);
        String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string()
    }
}

unsafe fn bounds(lib: Lib, obj: FPDF_PAGEOBJECT) -> Option<PdfBox> {
    let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    unsafe { lib.is_true(lib.FPDFPageObj_GetBounds(obj, &mut l, &mut b, &mut r, &mut t)) }.then_some(PdfBox {
        left: l,
        bottom: b,
        right: r,
        top: t,
    })
}

unsafe fn matrix(lib: Lib, obj: FPDF_PAGEOBJECT) -> FS_MATRIX {
    let mut m = IDENTITY;
    unsafe { lib.FPDFPageObj_GetMatrix(obj, &mut m) };
    m
}

unsafe fn font_size(lib: Lib, obj: FPDF_PAGEOBJECT) -> f32 {
    let mut size = 0.0f32;
    unsafe { lib.FPDFTextObj_GetFontSize(obj, &mut size) };
    size
}

fn union(a: Option<PdfBox>, b: PdfBox) -> PdfBox {
    match a {
        None => b,
        Some(a) => PdfBox {
            left: a.left.min(b.left),
            bottom: a.bottom.min(b.bottom),
            right: a.right.max(b.right),
            top: a.top.max(b.top),
        },
    }
}

/// What a text change did: where the text was and where it is now (None: removed).
#[derive(Clone, Copy, Debug)]
pub struct TextChanged {
    pub before: PdfBox,
    pub after: Option<PdfBox>,
}

enum Applied {
    Done(Option<PdfBox>),
    /// The text's own font cannot write the new text.
    NeedsStandardFont,
}

/// What paragraph grouping needs to know about a line of text.
#[derive(Clone, Debug)]
pub(crate) struct LineInfo {
    pub index: usize,
    pub left: f32,
    pub right: f32,
    pub baseline: f32,
    /// Font size as drawn.
    pub size: f32,
    pub font: String,
    /// Not rotated or skewed.
    pub upright: bool,
}

/// Groups lines into paragraphs: lines in the same font and size, starting at the
/// same left edge (the first one may be indented), evenly spaced up to double spacing.
/// Text that shares its baseline with other text (a line in several pieces) stays
/// on its own. Returns object indices, top to bottom; every line is in one group.
pub(crate) fn group_paragraphs(lines: &[LineInfo]) -> Vec<Vec<usize>> {
    // Other text on the same baseline next to it (a column further on does not count).
    let alone = |l: &LineInfo| {
        l.upright
            && !lines.iter().any(|o| {
                o.index != l.index
                    && (o.baseline - l.baseline).abs() < 0.3 * l.size.max(o.size)
                    && o.left < l.right + 0.8 * l.size
                    && o.right > l.left - 0.8 * l.size
            })
    };
    let mut order: Vec<&LineInfo> = lines.iter().collect();
    order.sort_by(|a, b| b.baseline.total_cmp(&a.baseline).then(a.left.total_cmp(&b.left)));
    // Each group: its lines (top to bottom) and whether it can take more.
    let mut groups: Vec<(Vec<&LineInfo>, bool)> = Vec::new();
    for line in order {
        let joinable = alone(line);
        let fits = |g: &Vec<&LineInfo>| {
            let last = g[g.len() - 1];
            let gap = last.baseline - line.baseline;
            let size = last.size;
            let same_font = last.font == line.font && (last.size - line.size).abs() <= 0.05 * size;
            let spaced = (0.9 * size..=2.5 * size).contains(&gap)
                && (g.len() < 2 || {
                    let first_gap = g[0].baseline - g[1].baseline;
                    (gap - first_gap).abs() <= 0.15 * first_gap
                });
            let aligned = if g.len() == 1 {
                // The first line may be indented.
                line.left <= last.left + 0.6 * size && line.left >= last.left - 4.0 * size
            } else {
                (line.left - last.left).abs() <= 0.6 * size
            };
            same_font && spaced && aligned
        };
        let target = joinable
            .then(|| {
                groups
                    .iter()
                    .enumerate()
                    .filter(|(_, (g, open))| *open && fits(g))
                    .min_by(|(_, (a, _)), (_, (b, _))| {
                        let d = |g: &Vec<&LineInfo>| (g[g.len() - 1].left - line.left).abs();
                        d(a).total_cmp(&d(b))
                    })
                    .map(|(i, _)| i)
            })
            .flatten();
        match target {
            Some(i) => groups[i].0.push(line),
            None => groups.push((vec![line], joinable)),
        }
    }
    groups.into_iter().map(|(g, _)| g.into_iter().map(|l| l.index).collect()).collect()
}

/// The lines for `text` in a paragraph that had the lines `old`. Without a `width`
/// each line of `text` is one line. With one, lines at the start and the end that
/// did not change keep their breaks and the rest is wrapped to `width` (as
/// `measure` measures). A line break in `text` always starts a new line; an empty
/// line stays empty.
pub(crate) fn layout_lines(
    old: &[String],
    text: &str,
    width: Option<f32>,
    measure: &dyn Fn(&str) -> f32,
) -> Vec<String> {
    let hard: Vec<&str> = text.split('\n').map(|l| l.trim()).collect();
    let Some(width) = width else {
        return hard.iter().map(|l| l.to_string()).collect();
    };
    // Words, with None for a line break.
    let mut tokens: Vec<Option<&str>> = Vec::new();
    for (i, line) in hard.iter().enumerate() {
        if i > 0 {
            tokens.push(None);
        }
        tokens.extend(line.split_whitespace().map(Some));
    }
    let old_words: Vec<Vec<&str>> = old.iter().map(|l| l.split_whitespace().collect()).collect();
    let matches_at = |pos: usize, words: &[&str]| {
        !words.is_empty()
            && pos + words.len() <= tokens.len()
            && words.iter().enumerate().all(|(k, w)| tokens[pos + k] == Some(*w))
    };
    // Unchanged lines at the start (not the last line, unless the text ends there).
    let (mut kept_start, mut pos) = (0, 0);
    while kept_start < old_words.len() && matches_at(pos, &old_words[kept_start]) {
        let end = pos + old_words[kept_start].len();
        if kept_start + 1 == old_words.len() && end != tokens.len() {
            break;
        }
        pos = end;
        kept_start += 1;
    }
    // Unchanged lines at the end.
    let (mut kept_end, mut end) = (0, tokens.len());
    while kept_start + kept_end < old_words.len() {
        let words = &old_words[old_words.len() - 1 - kept_end];
        if end < pos + words.len() || !matches_at(end - words.len(), words) {
            break;
        }
        end -= words.len();
        kept_end += 1;
    }
    // The last unchanged line takes changed words after it when they fit.
    if kept_start > 0 && matches!(tokens.get(pos), Some(Some(_))) && pos < end {
        kept_start -= 1;
        pos -= old_words[kept_start].len();
    }
    let mut lines: Vec<String> = old[..kept_start].iter().map(|l| l.trim().to_string()).collect();
    let middle = &tokens[pos..end];
    if !middle.is_empty() || kept_start + kept_end == 0 {
        // Pieces between line breaks. A break right after the kept start or right
        // before the kept end only ends that line.
        let mut pieces: Vec<Vec<&str>> =
            middle.split(|t| t.is_none()).map(|p| p.iter().flatten().copied().collect()).collect();
        let n = pieces.len();
        if kept_start > 0 && n > 1 && pieces[0].is_empty() {
            pieces.remove(0);
        }
        if kept_end > 0 && n > 1 && pieces.last().is_some_and(|p| p.is_empty()) {
            pieces.pop();
        }
        for piece in pieces {
            let mut line = String::new();
            for word in piece {
                let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
                if !line.is_empty() && measure(&candidate) > width {
                    lines.push(std::mem::replace(&mut line, word.to_string()));
                } else {
                    line = candidate;
                }
            }
            lines.push(line);
        }
    }
    lines.extend(old[old.len() - kept_end..].iter().map(|l| l.trim().to_string()));
    lines
}

/// `text` with `find` replaced by `replace`, and how many times.
pub(crate) fn replace_all(text: &str, find: &str, replace: &str, match_case: bool) -> (String, usize) {
    if find.is_empty() {
        return (text.to_string(), 0);
    }
    let fold = |c: char| if match_case { c.to_string() } else { c.to_lowercase().collect() };
    let hay: Vec<char> = text.chars().collect();
    let needle: Vec<String> = find.chars().map(fold).collect();
    let (mut out, mut count, mut i) = (String::new(), 0, 0);
    while i < hay.len() {
        if i + needle.len() <= hay.len() && needle.iter().enumerate().all(|(k, n)| fold(hay[i + k]) == *n) {
            out.push_str(replace);
            count += 1;
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    (out, count)
}

impl Doc {
    /// Drops the loaded copy of a page: changes to its objects that were not
    /// written to its content are gone when it is loaded again.
    pub fn discard_page(&self, index: usize) {
        let mut pages = self.pages.borrow_mut();
        if let Some(pos) = pages.iter().position(|(i, _)| *i == index) {
            let (_, handle) = pages.remove(pos);
            drop(pages);
            unsafe { self.close_page(handle) };
        }
    }

    /// Applies `change` to page `index` without writing the page's content (see the
    /// module documentation). Uses the closest standard font where the text's own
    /// font cannot write the new text.
    pub fn change_text(&self, index: usize, change: &TextChange) -> Result<TextChanged, String> {
        let cannot = || t("some of these characters cannot be written in this text's font").to_string();
        let before = self.page(index).ok_or(t("page not found"))?.objects_bounds(&change.objects)?;
        let first = self.page(index).ok_or(t("page not found"))?.apply_change(change, false);
        match first {
            Ok(Applied::Done(after)) => Ok(TextChanged { before, after }),
            Ok(Applied::NeedsStandardFont) => {
                self.discard_page(index);
                let writable =
                    change.text.as_deref().unwrap_or("").chars().all(|c| c.is_whitespace() || in_win_ansi(c));
                if !writable {
                    return Err(cannot());
                }
                let page = self.page(index).ok_or(t("page not found"))?;
                match page.apply_change(change, true) {
                    Ok(Applied::Done(after)) => Ok(TextChanged { before, after }),
                    Ok(Applied::NeedsStandardFont) => {
                        self.discard_page(index);
                        Err(cannot())
                    }
                    Err(e) => {
                        self.discard_page(index);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                self.discard_page(index);
                Err(e)
            }
        }
    }

    /// Replaces `find` with `replace` in every text object of the document and writes
    /// the changed pages. Returns how many places were replaced and how many could
    /// not be (the font cannot write the new text). Text split over several objects
    /// is not found.
    pub fn replace_text(&self, find: &str, replace: &str, match_case: bool) -> Result<(usize, usize), String> {
        let (mut replaced, mut failed) = (0, 0);
        for index in 0..self.page_count() {
            let Some(objects) = self.page(index).map(|p| p.text_objects().0) else { continue };
            for obj in objects {
                let (text, n) = replace_all(&obj.text, find, replace, match_case);
                if n == 0 {
                    continue;
                }
                let change = TextChange { objects: vec![obj.index], text: Some(text), ..Default::default() };
                match self.change_text(index, &change) {
                    Ok(_) => {
                        // Written right away: a failed change later on this page drops
                        // the page's unwritten changes.
                        self.page(index).ok_or(t("page not found"))?.generate()?;
                        replaced += n;
                    }
                    Err(_) => failed += n,
                }
            }
        }
        Ok((replaced, failed))
    }
}

impl<'d> Page<'d> {
    /// The page's visible text objects (not inside form XObjects) and their
    /// paragraphs (see `group_paragraphs`).
    pub fn text_objects(&self) -> (Vec<TextObject>, Vec<Vec<usize>>) {
        let lib = self.lib();
        let Some(tp) = self.text_page() else { return (Vec::new(), Vec::new()) };
        let (mut out, mut lines) = (Vec::new(), Vec::new());
        unsafe {
            for i in 0..lib.FPDFPage_CountObjects(self.handle).max(0) {
                let obj = lib.FPDFPage_GetObject(self.handle, i);
                if obj.is_null()
                    || lib.FPDFPageObj_GetType(obj) != FPDF_PAGEOBJ_TEXT
                    || lib.FPDFTextObj_GetTextRenderMode(obj) == INVISIBLE
                {
                    continue;
                }
                let text = text_of(lib, obj, tp.handle);
                if text.trim().is_empty() {
                    continue;
                }
                let Some(rect) = bounds(lib, obj) else { continue };
                let m = matrix(lib, obj);
                let scale = (m.c * m.c + m.d * m.d).sqrt();
                let size = font_size(lib, obj) * if scale > 0.0 { scale } else { 1.0 };
                let (mut cr, mut cg, mut cb, mut ca) = (0, 0, 0, 255);
                lib.FPDFPageObj_GetFillColor(obj, &mut cr, &mut cg, &mut cb, &mut ca);
                lines.push(LineInfo {
                    index: i as usize,
                    left: rect.left,
                    right: rect.right,
                    baseline: m.f,
                    size,
                    font: font_name(lib, lib.FPDFTextObj_GetFont(obj)),
                    upright: m.a > 0.0 && m.d > 0.0 && m.b.abs() < 1e-3 * m.a && m.c.abs() < 1e-3 * m.d,
                });
                out.push(TextObject { index: i as usize, text, rect, size, color: [cr as u8, cg as u8, cb as u8] });
            }
        }
        (out, group_paragraphs(&lines))
    }

    fn text_object(&self, index: usize) -> Result<FPDF_PAGEOBJECT, String> {
        let lib = self.lib();
        let obj = unsafe { lib.FPDFPage_GetObject(self.handle, index as c_int) };
        if obj.is_null() || unsafe { lib.FPDFPageObj_GetType(obj) } != FPDF_PAGEOBJ_TEXT {
            return Err(t("the text was not found on the page").into());
        }
        Ok(obj)
    }

    fn objects_bounds(&self, indices: &[usize]) -> Result<PdfBox, String> {
        let mut all = None;
        for &i in indices {
            if let Some(b) = unsafe { bounds(self.lib(), self.text_object(i)?) } {
                all = Some(union(all, b));
            }
        }
        all.ok_or_else(|| t("the text was not found on the page").into())
    }

    fn index_of(&self, obj: FPDF_PAGEOBJECT) -> Option<usize> {
        let lib = self.lib();
        let n = unsafe { lib.FPDFPage_CountObjects(self.handle) }.max(0);
        (0..n).find(|&i| unsafe { lib.FPDFPage_GetObject(self.handle, i) } == obj).map(|i| i as usize)
    }

    /// A new text object in `font` looking like `like` (colours, render mode), at `m`.
    unsafe fn new_text_object(
        &self,
        font: FPDF_FONT,
        size: f32,
        like: FPDF_PAGEOBJECT,
        m: &FS_MATRIX,
    ) -> FPDF_PAGEOBJECT {
        let lib = self.lib();
        unsafe {
            let new = lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, size);
            if new.is_null() {
                return new;
            }
            lib.FPDFPageObj_SetMatrix(new, m);
            lib.FPDFTextObj_SetTextRenderMode(new, lib.FPDFTextObj_GetTextRenderMode(like));
            let (mut r, mut g, mut b, mut a) = (0, 0, 0, 255);
            if lib.is_true(lib.FPDFPageObj_GetFillColor(like, &mut r, &mut g, &mut b, &mut a)) {
                lib.FPDFPageObj_SetFillColor(new, r, g, b, a);
            }
            if lib.is_true(lib.FPDFPageObj_GetStrokeColor(like, &mut r, &mut g, &mut b, &mut a)) {
                lib.FPDFPageObj_SetStrokeColor(new, r, g, b, a);
            }
            new
        }
    }

    fn insert_at(&self, obj: FPDF_PAGEOBJECT, index: usize) {
        let lib = self.lib();
        unsafe {
            if !lib.is_true(lib.FPDFPage_InsertObjectAtIndex(self.handle, obj, index)) {
                lib.FPDFPage_InsertObject(self.handle, obj);
            }
        }
    }

    fn remove(&self, obj: FPDF_PAGEOBJECT) {
        let lib = self.lib();
        unsafe {
            if lib.is_true(lib.FPDFPage_RemoveObject(self.handle, obj)) {
                lib.FPDFPageObj_Destroy(obj);
            }
        }
    }

    fn apply_change(&self, change: &TextChange, standard: bool) -> Result<Applied, String> {
        let lib = self.lib();
        let objs = change.objects.iter().map(|&i| self.text_object(i)).collect::<Result<Vec<_>, _>>()?;
        let Some(&first) = objs.first() else { return Err(t("the text was not found on the page").into()) };
        let before = self.objects_bounds(&change.objects)?;
        // The size as drawn, before any change.
        let m0 = unsafe { matrix(lib, first) };
        let raw_size = unsafe { font_size(lib, first) };
        let scale_y = (m0.c * m0.c + m0.d * m0.d).sqrt().max(1e-6);
        let mut current = objs.clone();
        if let Some(text) = &change.text {
            let tp = self.text_page().ok_or(t("could not render the page"))?;
            let old: Vec<String> = objs.iter().map(|&o| unsafe { text_of(lib, o, tp.handle) }).collect();
            drop(tp);
            let font = unsafe {
                let own = lib.FPDFTextObj_GetFont(first);
                if standard { lib.FPDFText_LoadStandardFont(self.doc.handle, standard_font_for(lib, own)) } else { own }
            };
            if font.is_null() {
                return Ok(Applied::NeedsStandardFont);
            }
            let scale_x = (m0.a * m0.a + m0.b * m0.b).sqrt().max(1e-6);
            let measure = |s: &str| -> f32 {
                if s.is_empty() {
                    return 0.0;
                }
                unsafe {
                    let probe = lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, raw_size);
                    if probe.is_null() {
                        return 0.0;
                    }
                    lib.FPDFText_SetText(probe, wide(s).as_ptr());
                    let w = bounds(lib, probe).map_or(0.0, |b| b.right - b.left);
                    lib.FPDFPageObj_Destroy(probe);
                    w
                }
            };
            let width = (objs.len() > 1).then(|| {
                objs.iter().filter_map(|&o| unsafe { bounds(lib, o) }).map(|b| b.right - b.left).fold(0.0, f32::max)
                    / scale_x
                    * 1.02
            });
            let lines = layout_lines(&old, text, width, &measure);
            let matrices: Vec<FS_MATRIX> = objs.iter().map(|&o| unsafe { matrix(lib, o) }).collect();
            let spacing = if objs.len() > 1 {
                (matrices[0].f - matrices[matrices.len() - 1].f) / (objs.len() - 1) as f32
            } else {
                1.2 * raw_size * scale_y
            };
            let mut placed: Vec<(FPDF_PAGEOBJECT, &str)> = Vec::new();
            let mut remove: Vec<FPDF_PAGEOBJECT> = Vec::new();
            let mut after = first;
            for (i, line) in lines.iter().enumerate() {
                let own = objs.get(i).copied();
                if line.is_empty() {
                    remove.extend(own);
                    continue;
                }
                let obj = match own {
                    Some(o) if !standard => o,
                    _ => {
                        let like = objs[i.min(objs.len() - 1)];
                        let mut m = matrices[i.min(objs.len() - 1)];
                        if i >= objs.len() {
                            m.f -= spacing * (i + 1 - objs.len()) as f32;
                        }
                        let new = unsafe { self.new_text_object(font, raw_size, like, &m) };
                        if new.is_null() {
                            return Ok(Applied::NeedsStandardFont);
                        }
                        let at = match own {
                            Some(o) => {
                                remove.push(o);
                                self.index_of(o)
                            }
                            None => self.index_of(after).map(|i| i + 1),
                        };
                        self.insert_at(new, at.unwrap_or(usize::MAX));
                        new
                    }
                };
                unsafe { lib.FPDFText_SetText(obj, wide(line).as_ptr()) };
                placed.push((obj, line));
                after = obj;
            }
            remove.extend(objs.iter().skip(lines.len()).copied());
            for &o in &remove {
                self.remove(o);
            }
            if !standard {
                // The font shows the new text only if reading it back gives it.
                let tp = self.text_page().ok_or(t("could not render the page"))?;
                let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
                if placed.iter().any(|(o, line)| squash(&unsafe { text_of(lib, *o, tp.handle) }) != squash(line)) {
                    return Ok(Applied::NeedsStandardFont);
                }
            }
            current = placed.into_iter().map(|(o, _)| o).collect();
        }
        if change.delete {
            for &o in &current {
                self.remove(o);
            }
            return Ok(Applied::Done(None));
        }
        if let Some(size) = change.size {
            let k = size / (raw_size * scale_y).max(1e-3);
            // Grows from the top left corner.
            let (x0, y0) = (before.left, before.top);
            for &o in &current {
                unsafe {
                    lib.FPDFPageObj_Transform(
                        o,
                        k as f64,
                        0.0,
                        0.0,
                        k as f64,
                        (x0 * (1.0 - k)) as f64,
                        (y0 * (1.0 - k)) as f64,
                    )
                };
            }
        }
        if let Some([r, g, b]) = change.color {
            for &o in &current {
                unsafe {
                    lib.FPDFPageObj_SetFillColor(o, r.into(), g.into(), b.into(), 255);
                    lib.FPDFPageObj_SetStrokeColor(o, r.into(), g.into(), b.into(), 255);
                }
            }
        }
        let (dx, dy) = change.offset;
        if dx != 0.0 || dy != 0.0 {
            for &o in &current {
                unsafe { lib.FPDFPageObj_Transform(o, 1.0, 0.0, 0.0, 1.0, dx as f64, dy as f64) };
            }
        }
        let mut after = None;
        for &o in &current {
            if let Some(b) = unsafe { bounds(lib, o) } {
                after = Some(union(after, b));
            }
        }
        Ok(Applied::Done(after))
    }

    /// The pixel rectangle (x, y, width, height) that `region` covers when the page is
    /// drawn `full` pixels large turned by `rotation`.
    pub fn region_pixels(
        &self,
        full: (usize, usize),
        rotation: Rotation,
        region: PdfBox,
    ) -> (usize, usize, usize, usize) {
        let lib = self.lib();
        let (w, h) = (full.0 as c_int, full.1 as c_int);
        let rot = rotation.quarter_turns();
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for (x, y) in [
            (region.left, region.bottom),
            (region.left, region.top),
            (region.right, region.bottom),
            (region.right, region.top),
        ] {
            let (mut dx, mut dy) = (0, 0);
            unsafe { lib.FPDF_PageToDevice(self.handle, 0, 0, w, h, rot, x.into(), y.into(), &mut dx, &mut dy) };
            (x0, y0, x1, y1) = (x0.min(dx), y0.min(dy), x1.max(dx), y1.max(dy));
        }
        let x0 = x0.clamp(0, w) as usize;
        let y0 = y0.clamp(0, h) as usize;
        let x1 = x1.clamp(0, w) as usize;
        let y1 = y1.clamp(0, h) as usize;
        (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(index: usize, left: f32, baseline: f32, size: f32) -> LineInfo {
        LineInfo { index, left, right: left + 300.0, baseline, size, font: "Helvetica".into(), upright: true }
    }

    #[test]
    fn lines_group_into_paragraphs() {
        let lines = vec![
            line(0, 72.0, 760.0, 24.0), // heading
            line(1, 72.0, 720.0, 10.0),
            line(2, 72.0, 708.0, 10.0),
            line(3, 72.0, 696.0, 10.0),
            line(4, 72.0, 660.0, 10.0), // a gap: a new paragraph
            line(5, 72.0, 648.0, 10.0),
            line(6, 320.0, 720.0, 10.0), // a second column
            line(7, 320.0, 708.0, 10.0),
        ];
        let mut lines = lines;
        lines[6].right = 560.0;
        lines[7].right = 560.0;
        for l in &mut lines[1..6] {
            l.right = 300.0;
        }
        assert_eq!(group_paragraphs(&lines), vec![vec![0], vec![1, 2, 3], vec![6, 7], vec![4, 5]]);
    }

    #[test]
    fn a_line_in_pieces_is_not_grouped() {
        let mut lines = vec![line(0, 72.0, 720.0, 10.0), line(1, 72.0, 708.0, 10.0), line(2, 72.0, 696.0, 10.0)];
        lines[1].right = 120.0;
        lines.push(LineInfo { index: 3, left: 125.0, right: 200.0, ..line(3, 0.0, 708.0, 10.0) });
        let groups = group_paragraphs(&lines);
        assert!(groups.contains(&vec![1]) && groups.contains(&vec![3]), "{groups:?}");
    }

    fn chars(s: &str) -> f32 {
        s.chars().count() as f32
    }

    fn old(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unchanged_lines_keep_their_breaks() {
        let old = old(&["aaa bbb ccc", "ddd eee", "fff ggg hhh", "iii"]);
        // Only the second line changes: the others stay as they were.
        let lines = layout_lines(&old, "aaa bbb ccc dd eee fff ggg hhh iii", Some(11.0), &chars);
        assert_eq!(lines, ["aaa bbb ccc", "dd eee", "fff ggg hhh", "iii"]);
        // A longer second line wraps into two.
        let lines = layout_lines(&old, "aaa bbb ccc ddd eee xx yy zz fff ggg hhh iii", Some(11.0), &chars);
        assert_eq!(lines, ["aaa bbb ccc", "ddd eee xx", "yy zz", "fff ggg hhh", "iii"]);
        // Nothing changed: nothing moves.
        assert_eq!(layout_lines(&old, &old.join(" "), Some(11.0), &chars), old);
        // Words added at the end flow on from the last line.
        let lines = layout_lines(&old, "aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk", Some(11.0), &chars);
        assert_eq!(lines, ["aaa bbb ccc", "ddd eee", "fff ggg hhh", "iii jjj kkk"]);
        // Text removed from the middle.
        let lines = layout_lines(&old, "aaa bbb ccc fff ggg hhh iii", Some(11.0), &chars);
        assert_eq!(lines, ["aaa bbb ccc", "fff ggg hhh", "iii"]);
    }

    #[test]
    fn line_breaks_and_single_lines() {
        let old = old(&["one two", "three"]);
        assert_eq!(layout_lines(&old, "one two\n\nthree four", Some(20.0), &chars), ["one two", "", "three four"]);
        assert_eq!(layout_lines(&["x".into()], "a long line\nnext", None, &chars), ["a long line", "next"]);
        assert_eq!(layout_lines(&old, "everything new here", Some(9.0), &chars), ["everything", "new here"]);
    }

    #[test]
    fn replaces_with_and_without_case() {
        assert_eq!(replace_all("Page 1, page 2", "page", "Seite", false), ("Seite 1, Seite 2".into(), 2));
        assert_eq!(replace_all("Page 1, page 2", "page", "Seite", true), ("Page 1, Seite 2".into(), 1));
        assert_eq!(replace_all("Grüezi GRÜEZI", "grüezi", "Hoi", false), ("Hoi Hoi".into(), 2));
        assert_eq!(replace_all("abc", "", "x", false), ("abc".into(), 0));
    }
}
