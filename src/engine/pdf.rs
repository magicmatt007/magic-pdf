//! Thin safe-ish wrapper over the raw PDFium C API (via pdfium-render's bindings).
//! Everything here must only be used from the PDFium worker thread.

use std::ffi::c_void;
use std::os::raw::{c_int, c_ulong};
use std::ptr;

use pdfium_render::prelude::*;

use super::commands::*;

pub type Lib = &'static dyn PdfiumLibraryBindings;

// Constants from the PDFium public headers (not re-exported by pdfium-render).
const FPDF_ANNOT: c_int = 0x01;
const FPDF_REVERSE_BYTE_ORDER: c_int = 0x10;
const FPDF_BITMAP_BGRA: c_int = 4;
const FPDF_NO_INCREMENTAL: FPDF_DWORD = 2;
const FPDF_ERR_PASSWORD: c_ulong = 4;
const FPDF_ANNOT_FLAG_HIDDEN: c_int = 1 << 1;
const FPDF_ANNOT_FLAG_PRINT: c_int = 1 << 2;
const FPDF_ANNOT_APPEARANCEMODE_NORMAL: FPDF_ANNOT_APPEARANCEMODE = 0;
const COLORTYPE_COLOR: FPDFANNOT_COLORTYPE = 0;
const COLORTYPE_INTERIOR: FPDFANNOT_COLORTYPE = 1;
const PDFACTION_GOTO: c_ulong = 1;
const PDFACTION_URI: c_ulong = 3;
const FPDF_MATCHCASE: c_ulong = 0x1;
const FPDF_MATCHWHOLEWORD: c_ulong = 0x2;

const SUBTYPE_TEXT: c_int = 1;
const SUBTYPE_LINK: c_int = 2;
const SUBTYPE_FREETEXT: c_int = 3;
const SUBTYPE_LINE: c_int = 4;
const SUBTYPE_SQUARE: c_int = 5;
const SUBTYPE_CIRCLE: c_int = 6;
const SUBTYPE_HIGHLIGHT: c_int = 9;
const SUBTYPE_UNDERLINE: c_int = 10;
const SUBTYPE_SQUIGGLY: c_int = 11;
const SUBTYPE_STRIKEOUT: c_int = 12;
const SUBTYPE_STAMP: c_int = 13;
const SUBTYPE_INK: c_int = 15;
const SUBTYPE_POPUP: c_int = 16;
const SUBTYPE_WIDGET: c_int = 20;

pub enum OpenError {
    Password,
    Other(String),
}

pub struct Doc {
    lib: Lib,
    handle: FPDF_DOCUMENT,
    form: FPDF_FORMHANDLE,
    // Must outlive the form handle and the document respectively.
    _form_info: Box<FPDF_FORMFILLINFO>,
    _data: Vec<u8>,
}

impl Drop for Doc {
    fn drop(&mut self) {
        unsafe {
            if !self.form.is_null() {
                self.lib.FPDFDOC_ExitFormFillEnvironment(self.form);
            }
            self.lib.FPDF_CloseDocument(self.handle);
        }
    }
}

/// A loaded page; closed on drop.
pub struct Page<'d> {
    doc: &'d Doc,
    pub handle: FPDF_PAGE,
}

impl Drop for Page<'_> {
    fn drop(&mut self) {
        unsafe {
            if !self.doc.form.is_null() {
                self.doc.lib.FORM_OnBeforeClosePage(self.handle, self.doc.form);
            }
            self.doc.lib.FPDF_ClosePage(self.handle);
        }
    }
}

struct TextPage<'p> {
    lib: Lib,
    handle: FPDF_TEXTPAGE,
    _page: std::marker::PhantomData<&'p ()>,
}

impl Drop for TextPage<'_> {
    fn drop(&mut self) {
        unsafe { self.lib.FPDFText_ClosePage(self.handle) }
    }
}

struct Annot {
    lib: Lib,
    handle: FPDF_ANNOTATION,
}

impl Drop for Annot {
    fn drop(&mut self) {
        unsafe { self.lib.FPDFPage_CloseAnnot(self.handle) }
    }
}

fn utf16_buf_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Calls a PDFium "get UTF-16LE string" function using the two-call pattern
/// (`f(buffer, len_in_bytes) -> required_bytes`).
fn read_utf16(mut f: impl FnMut(*mut c_void, c_ulong) -> c_ulong) -> String {
    let needed = f(ptr::null_mut(), 0) as usize;
    if needed <= 2 {
        return String::new();
    }
    let mut buf = vec![0u16; needed.div_ceil(2)];
    f(buf.as_mut_ptr() as *mut c_void, (buf.len() * 2) as c_ulong);
    utf16_buf_to_string(&buf)
}

fn to_box(r: &FS_RECTF) -> PdfBox {
    PdfBox {
        left: r.left.min(r.right),
        bottom: r.bottom.min(r.top),
        right: r.left.max(r.right),
        top: r.bottom.max(r.top),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// PDF date string for "now" in UTC, e.g. `D:20261122101500Z`.
pub fn pdf_date_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "D:{y:04}{m:02}{d:02}{:02}{:02}{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

#[repr(C)]
struct VecWriter {
    base: FPDF_FILEWRITE,
    out: Vec<u8>,
}

unsafe extern "C" fn write_block(this: *mut FPDF_FILEWRITE, data: *const c_void, size: c_ulong) -> c_int {
    // SAFETY: `this` is the first field of a live VecWriter (repr(C)).
    let writer = unsafe { &mut *(this as *mut VecWriter) };
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    writer.out.extend_from_slice(bytes);
    1
}

impl Doc {
    pub fn open_bytes(lib: Lib, data: Vec<u8>, password: Option<&str>) -> Result<Doc, OpenError> {
        let handle = unsafe { lib.FPDF_LoadMemDocument64(&data, password) };
        if handle.is_null() {
            let code = unsafe { lib.FPDF_GetLastError() };
            return Err(if code == FPDF_ERR_PASSWORD {
                OpenError::Password
            } else {
                OpenError::Other(match code {
                    2 => "file not found or could not be opened".into(),
                    3 => "not a PDF file or the file is corrupted".into(),
                    5 => "unsupported security scheme".into(),
                    _ => format!("PDFium error {code}"),
                })
            });
        }
        // SAFETY: FPDF_FORMFILLINFO is a plain C struct of ints and optional fn pointers.
        let mut form_info: Box<FPDF_FORMFILLINFO> = Box::new(unsafe { std::mem::zeroed() });
        form_info.version = 2;
        let form = unsafe { lib.FPDFDOC_InitFormFillEnvironment(handle, form_info.as_mut()) };
        Ok(Doc {
            lib,
            handle,
            form,
            _form_info: form_info,
            _data: data,
        })
    }

    pub fn page_count(&self) -> usize {
        unsafe { self.lib.FPDF_GetPageCount(self.handle).max(0) as usize }
    }

    pub fn page_size(&self, index: usize) -> (f32, f32) {
        let mut size = FS_SIZEF {
            width: 0.0,
            height: 0.0,
        };
        unsafe {
            self.lib
                .FPDF_GetPageSizeByIndexF(self.handle, index as c_int, &mut size);
        }
        (size.width.abs().max(1.0), size.height.abs().max(1.0))
    }

    pub fn page(&self, index: usize) -> Option<Page<'_>> {
        let handle = unsafe { self.lib.FPDF_LoadPage(self.handle, index as c_int) };
        if handle.is_null() {
            return None;
        }
        if !self.form.is_null() {
            unsafe { self.lib.FORM_OnAfterLoadPage(handle, self.form) };
        }
        Some(Page { doc: self, handle })
    }

    pub fn title(&self) -> Option<String> {
        let t = read_utf16(|buf, len| unsafe { self.lib.FPDF_GetMetaText(self.handle, "Title", buf, len) });
        let t = t.trim().to_string();
        (!t.is_empty()).then_some(t)
    }

    fn dest_page(&self, dest: FPDF_DEST) -> Option<usize> {
        if dest.is_null() {
            return None;
        }
        let p = unsafe { self.lib.FPDFDest_GetDestPageIndex(self.handle, dest) };
        (p >= 0).then_some(p as usize)
    }

    pub fn outline(&self) -> Vec<OutlineItem> {
        let first = unsafe { self.lib.FPDFBookmark_GetFirstChild(self.handle, ptr::null_mut()) };
        let mut count = 0;
        self.outline_level(first, 0, &mut count)
    }

    fn outline_level(&self, first: FPDF_BOOKMARK, depth: usize, count: &mut usize) -> Vec<OutlineItem> {
        const MAX_ITEMS: usize = 20_000;
        const MAX_DEPTH: usize = 64;
        let mut items = Vec::new();
        let mut bm = first;
        while !bm.is_null() && *count < MAX_ITEMS {
            *count += 1;
            let title = read_utf16(|buf, len| unsafe { self.lib.FPDFBookmark_GetTitle(bm, buf, len) });
            let mut page = self.dest_page(unsafe { self.lib.FPDFBookmark_GetDest(self.handle, bm) });
            if page.is_none() {
                let action = unsafe { self.lib.FPDFBookmark_GetAction(bm) };
                if !action.is_null() && unsafe { self.lib.FPDFAction_GetType(action) } == PDFACTION_GOTO {
                    page = self.dest_page(unsafe { self.lib.FPDFAction_GetDest(self.handle, action) });
                }
            }
            let child = unsafe { self.lib.FPDFBookmark_GetFirstChild(self.handle, bm) };
            let children = if depth < MAX_DEPTH {
                self.outline_level(child, depth + 1, count)
            } else {
                Vec::new()
            };
            items.push(OutlineItem {
                title,
                page,
                children,
            });
            bm = unsafe { self.lib.FPDFBookmark_GetNextSibling(self.handle, bm) };
        }
        items
    }

    pub fn save_to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut writer = VecWriter {
            base: FPDF_FILEWRITE {
                version: 1,
                WriteBlock: Some(write_block),
            },
            out: Vec::new(),
        };
        let ok = unsafe {
            self.lib
                .FPDF_SaveAsCopy(self.handle, &mut writer.base, FPDF_NO_INCREMENTAL)
        };
        if self.lib.is_true(ok) {
            Ok(writer.out)
        } else {
            Err("PDFium could not write the document".into())
        }
    }
}

impl<'d> Page<'d> {
    fn lib(&self) -> Lib {
        self.doc.lib
    }

    pub fn geom(&self) -> PageGeom {
        let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let lib = self.lib();
        let ok = unsafe { lib.FPDFPage_GetCropBox(self.handle, &mut l, &mut b, &mut r, &mut t) };
        if !lib.is_true(ok) {
            unsafe { lib.FPDFPage_GetMediaBox(self.handle, &mut l, &mut b, &mut r, &mut t) };
        }
        let rotation = Rotation::from_quarter_turns(unsafe { lib.FPDFPage_GetRotation(self.handle) });
        PageGeom {
            size: ((r - l).abs().max(1.0), (t - b).abs().max(1.0)),
            origin: (l.min(r), b.min(t)),
            rotation,
        }
    }

    /// Renders into an RGBA buffer `width` x `height` pixels (already rotated by
    /// `rotation`), including annotations and form fields.
    pub fn render(&self, width: usize, height: usize, rotation: Rotation) -> Option<Vec<u8>> {
        let lib = self.lib();
        unsafe {
            let bitmap = lib.FPDFBitmap_CreateEx(width as c_int, height as c_int, FPDF_BITMAP_BGRA, ptr::null_mut(), 0);
            if bitmap.is_null() {
                return None;
            }
            lib.FPDFBitmap_FillRect(bitmap, 0, 0, width as c_int, height as c_int, 0xFFFF_FFFF);
            let rot = rotation.quarter_turns();
            let flags = FPDF_ANNOT | FPDF_REVERSE_BYTE_ORDER;
            lib.FPDF_RenderPageBitmap(bitmap, self.handle, 0, 0, width as c_int, height as c_int, rot, flags);
            if !self.doc.form.is_null() {
                lib.FPDF_FFLDraw(self.doc.form, bitmap, self.handle, 0, 0, width as c_int, height as c_int, rot, flags);
            }
            let stride = lib.FPDFBitmap_GetStride(bitmap) as usize;
            let buf = lib.FPDFBitmap_GetBuffer(bitmap) as *const u8;
            let mut out = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                let row = std::slice::from_raw_parts(buf.add(y * stride), width * 4);
                out.extend_from_slice(row);
            }
            lib.FPDFBitmap_Destroy(bitmap);
            Some(out)
        }
    }

    fn text_page(&self) -> Option<TextPage<'_>> {
        let handle = unsafe { self.lib().FPDFText_LoadPage(self.handle) };
        (!handle.is_null()).then(|| TextPage {
            lib: self.lib(),
            handle,
            _page: std::marker::PhantomData,
        })
    }

    pub fn text(&self) -> PageText {
        let lib = self.lib();
        let mut text = PageText::default();
        if let Some(tp) = self.text_page() {
            let n = unsafe { lib.FPDFText_CountChars(tp.handle) };
            text.chars.reserve(n.max(0) as usize);
            for i in 0..n {
                let code = unsafe { lib.FPDFText_GetUnicode(tp.handle, i) };
                let ch = char::from_u32(code).unwrap_or('\u{FFFD}');
                let mut r = FS_RECTF {
                    left: 0.0,
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                };
                let ok = unsafe { lib.FPDFText_GetLooseCharBox(tp.handle, i, &mut r) };
                let rect = Some(to_box(&r))
                    .filter(|b| lib.is_true(ok) && b.right - b.left > 0.0 && b.top - b.bottom > 0.0);
                text.chars.push(TextChar { ch, rect });
            }
        }
        let mut pos: c_int = 0;
        let mut link: FPDF_LINK = ptr::null_mut();
        while lib.is_true(unsafe { lib.FPDFLink_Enumerate(self.handle, &mut pos, &mut link) }) {
            let mut r = FS_RECTF {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            };
            if !lib.is_true(unsafe { lib.FPDFLink_GetAnnotRect(link, &mut r) }) {
                continue;
            }
            let mut target = self
                .doc
                .dest_page(unsafe { lib.FPDFLink_GetDest(self.doc.handle, link) })
                .map(LinkTarget::Page);
            if target.is_none() {
                let action = unsafe { lib.FPDFLink_GetAction(link) };
                if !action.is_null() {
                    match unsafe { lib.FPDFAction_GetType(action) } {
                        PDFACTION_GOTO => {
                            target = self
                                .doc
                                .dest_page(unsafe { lib.FPDFAction_GetDest(self.doc.handle, action) })
                                .map(LinkTarget::Page);
                        }
                        PDFACTION_URI => {
                            // URI is returned as 7-bit ASCII bytes.
                            let needed = unsafe {
                                lib.FPDFAction_GetURIPath(self.doc.handle, action, ptr::null_mut(), 0)
                            } as usize;
                            if needed > 1 {
                                let mut buf = vec![0u8; needed];
                                unsafe {
                                    lib.FPDFAction_GetURIPath(
                                        self.doc.handle,
                                        action,
                                        buf.as_mut_ptr() as *mut c_void,
                                        needed as c_ulong,
                                    )
                                };
                                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                                target = Some(LinkTarget::Uri(String::from_utf8_lossy(&buf[..end]).into_owned()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Some(target) = target {
                text.links.push(PageLink {
                    rect: to_box(&r),
                    target,
                });
            }
        }
        text
    }

    pub fn search(&self, query: &SearchQuery, limit: usize) -> Vec<Vec<PdfBox>> {
        let lib = self.lib();
        let Some(tp) = self.text_page() else { return Vec::new() };
        let mut flags = 0;
        if query.match_case {
            flags |= FPDF_MATCHCASE;
        }
        if query.whole_word {
            flags |= FPDF_MATCHWHOLEWORD;
        }
        let needle = wide(&query.text);
        let mut hits = Vec::new();
        unsafe {
            let sch = lib.FPDFText_FindStart(tp.handle, needle.as_ptr(), flags, 0);
            if sch.is_null() {
                return hits;
            }
            while lib.is_true(lib.FPDFText_FindNext(sch)) && hits.len() < limit {
                let start = lib.FPDFText_GetSchResultIndex(sch);
                let count = lib.FPDFText_GetSchCount(sch);
                let n = lib.FPDFText_CountRects(tp.handle, start, count);
                let mut rects = Vec::new();
                for i in 0..n {
                    let (mut l, mut t, mut r, mut b) = (0.0, 0.0, 0.0, 0.0);
                    if lib.is_true(lib.FPDFText_GetRect(tp.handle, i, &mut l, &mut t, &mut r, &mut b)) {
                        rects.push(PdfBox {
                            left: l as f32,
                            bottom: b as f32,
                            right: r as f32,
                            top: t as f32,
                        });
                    }
                }
                if !rects.is_empty() {
                    hits.push(rects);
                }
            }
            lib.FPDFText_FindClose(sch);
        }
        hits
    }

    pub fn annot_count(&self) -> usize {
        unsafe { self.lib().FPDFPage_GetAnnotCount(self.handle).max(0) as usize }
    }

    fn annot(&self, index: usize) -> Option<Annot> {
        let handle = unsafe { self.lib().FPDFPage_GetAnnot(self.handle, index as c_int) };
        (!handle.is_null()).then(|| Annot {
            lib: self.lib(),
            handle,
        })
    }

    pub fn annot_flags(&self, index: usize) -> Option<i32> {
        let a = self.annot(index)?;
        Some(unsafe { self.lib().FPDFAnnot_GetFlags(a.handle) })
    }

    pub fn set_annot_flags(&self, index: usize, flags: i32) -> bool {
        let Some(a) = self.annot(index) else { return false };
        self.lib().is_true(unsafe { self.lib().FPDFAnnot_SetFlags(a.handle, flags) })
    }

    pub fn set_annot_hidden(&self, index: usize, hidden: bool) -> Option<i32> {
        let flags = self.annot_flags(index)?;
        let new = if hidden {
            flags | FPDF_ANNOT_FLAG_HIDDEN
        } else {
            flags & !FPDF_ANNOT_FLAG_HIDDEN
        };
        self.set_annot_flags(index, new);
        Some(flags)
    }

    /// Lists user-visible annotations (no links, form widgets, popups or hidden ones).
    pub fn annots(&self, skip: impl Fn(usize) -> bool) -> Vec<AnnotInfo> {
        let lib = self.lib();
        let mut out = Vec::new();
        for index in 0..self.annot_count() {
            if skip(index) {
                continue;
            }
            let Some(a) = self.annot(index) else { continue };
            let subtype = unsafe { lib.FPDFAnnot_GetSubtype(a.handle) };
            if matches!(subtype, SUBTYPE_LINK | SUBTYPE_WIDGET | SUBTYPE_POPUP) {
                continue;
            }
            let flags = unsafe { lib.FPDFAnnot_GetFlags(a.handle) };
            if flags & FPDF_ANNOT_FLAG_HIDDEN != 0 {
                continue;
            }
            let kind = match subtype {
                SUBTYPE_TEXT => AnnotKind::Note,
                SUBTYPE_FREETEXT => AnnotKind::FreeText,
                SUBTYPE_LINE => AnnotKind::Line,
                SUBTYPE_SQUARE => AnnotKind::Square,
                SUBTYPE_CIRCLE => AnnotKind::Circle,
                SUBTYPE_HIGHLIGHT => AnnotKind::Highlight,
                SUBTYPE_UNDERLINE => AnnotKind::Underline,
                SUBTYPE_SQUIGGLY => AnnotKind::Squiggly,
                SUBTYPE_STRIKEOUT => AnnotKind::StrikeOut,
                SUBTYPE_STAMP => AnnotKind::Stamp,
                SUBTYPE_INK => AnnotKind::Ink,
                _ => AnnotKind::Other,
            };
            let mut r = FS_RECTF {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            };
            unsafe { lib.FPDFAnnot_GetRect(a.handle, &mut r) };
            let string = |key: &str| {
                read_utf16(|buf, len| unsafe {
                    lib.FPDFAnnot_GetStringValue(a.handle, key, buf as *mut FPDF_WCHAR, len)
                })
            };
            let (mut hr, mut vr, mut w) = (0.0, 0.0, 1.0);
            unsafe { lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut w) };
            out.push(AnnotInfo {
                index,
                kind,
                rect: to_box(&r),
                color: self.annot_color(&a, COLORTYPE_COLOR),
                interior: self.annot_color(&a, COLORTYPE_INTERIOR),
                width: w,
                contents: string("Contents"),
                author: string("T"),
                modified: string("M"),
            });
        }
        out
    }

    fn annot_color(&self, a: &Annot, kind: FPDFANNOT_COLORTYPE) -> Option<Rgb> {
        let (mut r, mut g, mut b, mut al) = (0, 0, 0, 0);
        let ok = unsafe { self.lib().FPDFAnnot_GetColor(a.handle, kind, &mut r, &mut g, &mut b, &mut al) };
        self.lib().is_true(ok).then_some([r as u8, g as u8, b as u8])
    }

    /// Creates an annotation and returns its index.
    pub fn add_annot(&self, n: &NewAnnot) -> Result<usize, String> {
        let lib = self.lib();
        let subtype = match n.kind {
            AnnotKind::Highlight => SUBTYPE_HIGHLIGHT,
            AnnotKind::Underline => SUBTYPE_UNDERLINE,
            AnnotKind::StrikeOut => SUBTYPE_STRIKEOUT,
            AnnotKind::Squiggly => SUBTYPE_SQUIGGLY,
            AnnotKind::Note => SUBTYPE_TEXT,
            AnnotKind::Ink => SUBTYPE_INK,
            AnnotKind::Square => SUBTYPE_SQUARE,
            AnnotKind::Circle => SUBTYPE_CIRCLE,
            other => return Err(format!("creating {} annotations is not supported", other.label())),
        };
        let handle = unsafe { lib.FPDFPage_CreateAnnot(self.handle, subtype) };
        if handle.is_null() {
            return Err("PDFium could not create the annotation".into());
        }
        let a = Annot { lib, handle };
        let index = unsafe { lib.FPDFPage_GetAnnotIndex(self.handle, a.handle) };
        unsafe {
            let rect = FS_RECTF {
                left: n.rect.left,
                top: n.rect.top,
                right: n.rect.right,
                bottom: n.rect.bottom,
            };
            lib.FPDFAnnot_SetRect(a.handle, &rect);
            lib.FPDFAnnot_SetFlags(a.handle, FPDF_ANNOT_FLAG_PRINT);
            let [r, g, b] = n.color;
            lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_COLOR, r.into(), g.into(), b.into(), 255);
            if let Some([r, g, b]) = n.interior {
                lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_INTERIOR, r.into(), g.into(), b.into(), 255);
            }
            if n.kind.has_border_width() {
                lib.FPDFAnnot_SetBorder(a.handle, 0.0, 0.0, n.width);
            }
            for q in &n.quads {
                let qp = FS_QUADPOINTSF {
                    x1: q[0],
                    y1: q[1],
                    x2: q[2],
                    y2: q[3],
                    x3: q[4],
                    y3: q[5],
                    x4: q[6],
                    y4: q[7],
                };
                lib.FPDFAnnot_AppendAttachmentPoints(a.handle, &qp);
            }
            for stroke in &n.ink {
                let pts: Vec<FS_POINTF> = stroke.iter().map(|&(x, y)| FS_POINTF { x, y }).collect();
                if !pts.is_empty() {
                    lib.FPDFAnnot_AddInkStroke(a.handle, pts.as_ptr(), pts.len() as _);
                }
            }
            let now = pdf_date_now();
            lib.FPDFAnnot_SetStringValue_str(a.handle, "Contents", &n.contents);
            if !n.author.is_empty() {
                lib.FPDFAnnot_SetStringValue_str(a.handle, "T", &n.author);
            }
            lib.FPDFAnnot_SetStringValue_str(a.handle, "CreationDate", &now);
            lib.FPDFAnnot_SetStringValue_str(a.handle, "M", &now);
            if n.kind == AnnotKind::Note {
                lib.FPDFAnnot_SetStringValue_str(a.handle, "Name", "Comment");
            }
        }
        if index < 0 {
            return Err("annotation index unavailable".into());
        }
        Ok(index as usize)
    }

    /// Applies `change` and returns the change that reverts it.
    pub fn update_annot(&self, index: usize, change: &AnnotChange) -> Result<AnnotChange, String> {
        let lib = self.lib();
        let a = self.annot(index).ok_or("annotation not found")?;
        let string = |key: &str| {
            read_utf16(|buf, len| unsafe { lib.FPDFAnnot_GetStringValue(a.handle, key, buf as *mut FPDF_WCHAR, len) })
        };
        let mut undo = AnnotChange::default();
        let restyle = change.color.is_some() || change.interior.is_some() || change.width.is_some();
        unsafe {
            if restyle {
                // PDFium refuses colour access while an appearance stream exists. Drop it;
                // PDFium regenerates it from the new style when the page is rendered.
                lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null());
            }
            if let Some([r, g, b]) = change.color {
                undo.color = Some(self.annot_color(&a, COLORTYPE_COLOR).unwrap_or([0, 0, 0]));
                lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_COLOR, r.into(), g.into(), b.into(), 255);
            }
            if let Some(interior) = change.interior {
                undo.interior = Some(self.annot_color(&a, COLORTYPE_INTERIOR));
                match interior {
                    Some([r, g, b]) => {
                        lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_INTERIOR, r.into(), g.into(), b.into(), 255);
                    }
                    // There is no API to remove /IC; a non-array value is ignored as a fill.
                    None => {
                        lib.FPDFAnnot_SetStringValue_str(a.handle, "IC", "");
                    }
                }
            }
            if let Some(w) = change.width {
                let (mut hr, mut vr, mut old) = (0.0, 0.0, 1.0);
                lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut old);
                undo.width = Some(old);
                lib.FPDFAnnot_SetBorder(a.handle, hr, vr, w);
            }
            if let Some(text) = &change.contents {
                undo.contents = Some(string("Contents"));
                lib.FPDFAnnot_SetStringValue_str(a.handle, "Contents", text);
            }
            lib.FPDFAnnot_SetStringValue_str(a.handle, "M", &pdf_date_now());
        }
        Ok(undo)
    }

    /// Permanently removes annotations (and their popups) by index.
    pub fn remove_annots(&self, indices: &[usize]) {
        let lib = self.lib();
        let mut all: Vec<usize> = Vec::new();
        for &i in indices {
            all.push(i);
            if let Some(a) = self.annot(i) {
                let popup = unsafe { lib.FPDFAnnot_GetLinkedAnnot(a.handle, "Popup") };
                if !popup.is_null() {
                    let pi = unsafe { lib.FPDFPage_GetAnnotIndex(self.handle, popup) };
                    unsafe { lib.FPDFPage_CloseAnnot(popup) };
                    if pi >= 0 {
                        all.push(pi as usize);
                    }
                }
            }
        }
        all.sort_unstable();
        all.dedup();
        for i in all.into_iter().rev() {
            unsafe { lib.FPDFPage_RemoveAnnot(self.handle, i as c_int) };
        }
    }
}
