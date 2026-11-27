//! Thin safe-ish wrapper over the raw PDFium C API (via pdfium-render's bindings).
//! Everything here must only be used from the PDFium worker thread.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::os::raw::{c_int, c_ulong};
use std::ptr;

use pdfium_render::prelude::*;

use super::commands::*;
use super::rich::{CharStyle, RichText};
use super::shapes::{self, MEASURE_KEY, POINTS_KEY, SHAPE_KEY, STAMP_KEY, Shape};
use super::textbox::{self, BoxStyle, FontSet, GlyphSource};

/// Private keys for comment threads and review status.
use super::lopdf_ops::REPLIES_KEY;
use super::lopdf_ops::STATUS_KEY;

pub type Lib = &'static dyn PdfiumLibraryBindings;

// Constants from the PDFium public headers (not re-exported by pdfium-render).
const FPDF_ANNOT: c_int = 0x01;
const FPDF_REVERSE_BYTE_ORDER: c_int = 0x10;
const FPDF_BITMAP_BGRA: c_int = 4;
const FPDF_INCREMENTAL: FPDF_DWORD = 1;
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

/// Marked-content tag of header and footer texts magic-pdf adds.
const HEADER_FOOTER_MARK: &str = "MagicPdfHeaderFooter";
/// Set on text boxes whose colours follow the Acrobat convention (/C fill, /DA border,
/// /DS text). Boxes without it that carry magic-pdf's old /DS used /C border, /IC fill.
const BOX_STYLE_KEY: &str = "MagicPdfBoxStyle";
/// Marked-content tag of watermarks magic-pdf adds.
const WATERMARK_MARK: &str = "MagicPdfWatermark";
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

/// Form-fill environment handed to PDFium. `info` must be the first field: PDFium
/// passes a pointer to it back into the callbacks, which we cast to `FormEnv`.
#[repr(C)]
struct FormEnv {
    info: FPDF_FORMFILLINFO,
    /// Pages PDFium asked to repaint since the last `take_events`.
    invalidated: RefCell<Vec<FPDF_PAGE>>,
    /// A form field value changed.
    changed: Cell<bool>,
}

unsafe extern "C" fn ffi_invalidate(this: *mut FPDF_FORMFILLINFO, page: FPDF_PAGE, _l: f64, _t: f64, _r: f64, _b: f64) {
    // SAFETY: `this` points at the `info` field of a live, boxed FormEnv (repr(C)).
    let env = unsafe { &*(this as *const FormEnv) };
    let mut v = env.invalidated.borrow_mut();
    if !v.contains(&page) {
        v.push(page);
    }
}

unsafe extern "C" fn ffi_on_change(this: *mut FPDF_FORMFILLINFO) {
    let env = unsafe { &*(this as *const FormEnv) };
    env.changed.set(true);
}

/// Pages kept loaded. Form editing needs the page to stay loaded while a field has
/// focus, and reusing loaded pages also saves re-parsing.
const PAGE_CACHE: usize = 8;

pub struct Doc {
    lib: Lib,
    handle: FPDF_DOCUMENT,
    form: FPDF_FORMHANDLE,
    /// Loaded pages, least recently used first.
    pages: RefCell<Vec<(usize, FPDF_PAGE)>>,
    // Must outlive the form handle and the document respectively.
    env: Box<FormEnv>,
    _data: Vec<u8>,
}

impl Drop for Doc {
    fn drop(&mut self) {
        unsafe {
            for (_, page) in self.pages.borrow_mut().drain(..) {
                self.close_page(page);
            }
            if !self.form.is_null() {
                self.lib.FPDFDOC_ExitFormFillEnvironment(self.form);
            }
            self.lib.FPDF_CloseDocument(self.handle);
        }
    }
}

/// A loaded page, owned by the document's page cache.
pub struct Page<'d> {
    doc: &'d Doc,
    pub handle: FPDF_PAGE,
}

/// Form events collected from PDFium callbacks.
#[derive(Default)]
pub struct FormEvents {
    pub invalidated_pages: Vec<usize>,
    pub changed: bool,
}

/// Field types reported by FPDFAnnot_GetFormFieldType.
pub const FIELD_TEXT: i32 = 6;

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

/// PDF date string for "now" in local time, e.g. `D:20261127150000+01'00'`.
pub fn pdf_date_now() -> String {
    pdf_date(chrono::Local::now().fixed_offset())
}

/// A PDF date in the time's own zone (`Z` for UTC).
fn pdf_date(time: chrono::DateTime<chrono::FixedOffset>) -> String {
    let offset = time.offset().local_minus_utc();
    let stamp = time.format("%Y%m%d%H%M%S");
    if offset == 0 {
        return format!("D:{stamp}Z");
    }
    let (sign, minutes) = (if offset < 0 { '-' } else { '+' }, offset.abs() / 60);
    format!("D:{stamp}{sign}{:02}'{:02}'", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod date_tests {
    #[test]
    fn pdf_dates_carry_their_zone() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap();
        assert_eq!(super::pdf_date(at("2026-11-27T15:04:05+01:00")), "D:20261127150405+01'00'");
        assert_eq!(super::pdf_date(at("2026-07-01T08:00:00-05:30")), "D:20260701080000-05'30'");
        assert_eq!(super::pdf_date(at("2026-11-27T14:04:05Z")), "D:20261127140405Z");
    }
}

#[repr(C)]
struct VecWriter {
    base: FPDF_FILEWRITE,
    out: Vec<u8>,
}

/// FPDF_FILEACCESS reader over a `Vec<u8>`.
unsafe extern "C" fn read_block(param: *mut c_void, position: c_ulong, buf: *mut u8, size: c_ulong) -> c_int {
    let data = unsafe { &*(param as *const Vec<u8>) };
    let (start, len) = (position as usize, size as usize);
    match data.get(start..start + len) {
        Some(src) => {
            unsafe { ptr::copy_nonoverlapping(src.as_ptr(), buf, len) };
            1
        }
        None => 0,
    }
}

/// Copies a PDFium bitmap into RGBA pixels.
/// A PDFium BGRA bitmap from RGBA pixels (caller destroys it).
fn rgba_bitmap(lib: Lib, w: u32, h: u32, rgba: &[u8]) -> Option<FPDF_BITMAP> {
    if w == 0 || h == 0 || rgba.len() < (w * h * 4) as usize {
        return None;
    }
    // Only pixels that are not fully opaque need an alpha channel (a soft mask).
    let alpha = rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 255);
    unsafe {
        let bmp = lib.FPDFBitmap_Create(w as c_int, h as c_int, alpha.into());
        if bmp.is_null() {
            return None;
        }
        let stride = lib.FPDFBitmap_GetStride(bmp) as usize;
        let buf = lib.FPDFBitmap_GetBuffer(bmp) as *mut u8;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = &rgba[(y * w as usize + x) * 4..][..4];
                let d = buf.add(y * stride + x * 4);
                *d = s[2];
                *d.add(1) = s[1];
                *d.add(2) = s[0];
                *d.add(3) = s[3];
            }
        }
        Some(bmp)
    }
}

fn bitmap_rgba(lib: Lib, bmp: FPDF_BITMAP) -> (u32, u32, Vec<u8>) {
    unsafe {
        let (w, h) = (lib.FPDFBitmap_GetWidth(bmp) as usize, lib.FPDFBitmap_GetHeight(bmp) as usize);
        let stride = lib.FPDFBitmap_GetStride(bmp) as usize;
        let format = lib.FPDFBitmap_GetFormat(bmp);
        let buf = lib.FPDFBitmap_GetBuffer(bmp) as *const u8;
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let px = |bpp: usize, i: usize| *buf.add(y * stride + x * bpp + i);
                let p = match format {
                    1 => {
                        let v = px(1, 0);
                        [v, v, v, 255]
                    }
                    2 => [px(3, 2), px(3, 1), px(3, 0), 255],
                    3 => [px(4, 2), px(4, 1), px(4, 0), 255],
                    _ => [px(4, 2), px(4, 1), px(4, 0), px(4, 3)],
                };
                out.extend(p);
            }
        }
        (w as u32, h as u32, out)
    }
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
        // SAFETY: FPDF_FORMFILLINFO is a plain C struct of ints, pointers and optional fn
        // pointers, for which all-zero is valid. No JavaScript platform is provided, so
        // document scripts never run.
        let mut info: FPDF_FORMFILLINFO = unsafe { std::mem::zeroed() };
        info.version = 1;
        info.FFI_Invalidate = Some(ffi_invalidate);
        info.FFI_OnChange = Some(ffi_on_change);
        let mut env = Box::new(FormEnv { info, invalidated: RefCell::new(Vec::new()), changed: Cell::new(false) });
        let form = unsafe { lib.FPDFDOC_InitFormFillEnvironment(handle, &mut env.info) };
        if !form.is_null() {
            unsafe {
                // Light blue backgrounds make fields easy to spot (like browsers do).
                lib.FPDF_SetFormFieldHighlightColor(form, 0, 0x00FF_E4DD);
                lib.FPDF_SetFormFieldHighlightAlpha(form, 90);
            }
        }
        Ok(Doc { lib, handle, form, pages: RefCell::new(Vec::new()), env, _data: data })
    }

    pub fn page_count(&self) -> usize {
        unsafe { self.lib.FPDF_GetPageCount(self.handle).max(0) as usize }
    }

    pub fn page_size(&self, index: usize) -> (f32, f32) {
        let mut size = FS_SIZEF { width: 0.0, height: 0.0 };
        unsafe {
            self.lib.FPDF_GetPageSizeByIndexF(self.handle, index as c_int, &mut size);
        }
        (size.width.abs().max(1.0), size.height.abs().max(1.0))
    }

    pub fn page(&self, index: usize) -> Option<Page<'_>> {
        let mut pages = self.pages.borrow_mut();
        if let Some(pos) = pages.iter().position(|(i, _)| *i == index) {
            let entry = pages.remove(pos);
            pages.push(entry);
            return Some(Page { doc: self, handle: entry.1 });
        }
        let handle = unsafe { self.lib.FPDF_LoadPage(self.handle, index as c_int) };
        if handle.is_null() {
            return None;
        }
        if !self.form.is_null() {
            unsafe { self.lib.FORM_OnAfterLoadPage(handle, self.form) };
        }
        pages.push((index, handle));
        if pages.len() > PAGE_CACHE {
            // Evict the least recently used page, but never the one holding form focus.
            let focused = self.focused_field().map(|(p, _)| p);
            if let Some(pos) = pages.iter().position(|(i, _)| Some(*i) != focused) {
                let (_, old) = pages.remove(pos);
                unsafe { self.close_page(old) };
            }
        }
        Some(Page { doc: self, handle })
    }

    unsafe fn close_page(&self, page: FPDF_PAGE) {
        unsafe {
            if !self.form.is_null() {
                self.lib.FORM_OnBeforeClosePage(page, self.form);
            }
            self.lib.FPDF_ClosePage(page);
        }
    }

    /// Returns and clears events reported by PDFium's form callbacks.
    pub fn take_form_events(&self) -> FormEvents {
        let pages = self.pages.borrow();
        let invalidated = std::mem::take(&mut *self.env.invalidated.borrow_mut());
        FormEvents {
            invalidated_pages: invalidated
                .iter()
                .filter_map(|h| pages.iter().find(|(_, p)| p == h).map(|(i, _)| *i))
                .collect(),
            changed: self.env.changed.replace(false),
        }
    }

    /// (page index, field type) of the focused form field.
    pub fn focused_field(&self) -> Option<(usize, i32)> {
        if self.form.is_null() {
            return None;
        }
        let mut page: c_int = -1;
        let mut annot: FPDF_ANNOTATION = ptr::null_mut();
        let ok = unsafe { self.lib.FORM_GetFocusedAnnot(self.form, &mut page, &mut annot) };
        if !self.lib.is_true(ok) || annot.is_null() {
            return None;
        }
        let kind = unsafe { self.lib.FPDFAnnot_GetFormFieldType(self.form, annot) };
        unsafe { self.lib.FPDFPage_CloseAnnot(annot) };
        (page >= 0).then_some((page as usize, kind))
    }

    /// Closes all cached pages (required before changing the page structure).
    pub fn close_all_pages(&self) {
        let pages: Vec<_> = self.pages.borrow_mut().drain(..).collect();
        for (_, page) in pages {
            unsafe { self.close_page(page) };
        }
    }

    /// The document's signatures (raw), checked against the bytes the document was
    /// opened from.
    pub fn signatures(&self) -> Vec<SignatureInfo> {
        let lib = self.lib;
        let data: &[u8] = &self._data;
        let n = unsafe { lib.FPDF_GetSignatureCount(self.handle) };
        (0..n.max(0))
            .filter_map(|i| unsafe {
                let sig = lib.FPDF_GetSignatureObject(self.handle, i);
                if sig.is_null() {
                    return None;
                }
                let len = lib.FPDFSignatureObj_GetContents(sig, ptr::null_mut(), 0) as usize;
                let mut contents = vec![0u8; len];
                lib.FPDFSignatureObj_GetContents(sig, contents.as_mut_ptr().cast(), len as c_ulong);
                let n = lib.FPDFSignatureObj_GetByteRange(sig, ptr::null_mut(), 0) as usize;
                let mut range = vec![0 as c_int; n];
                lib.FPDFSignatureObj_GetByteRange(sig, range.as_mut_ptr(), n as c_ulong);
                let ranges: Vec<(usize, usize)> =
                    range.as_chunks::<2>().0.iter().map(|p| (p[0].max(0) as usize, p[1].max(0) as usize)).collect();
                let ascii = |f: &dyn Fn(*mut std::os::raw::c_char, c_ulong) -> c_ulong| {
                    let len = f(ptr::null_mut(), 0) as usize;
                    let mut buf = vec![0u8; len];
                    f(buf.as_mut_ptr().cast(), len as c_ulong);
                    String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string()
                };
                let subfilter = ascii(&|b, l| lib.FPDFSignatureObj_GetSubFilter(sig, b, l));
                let time = ascii(&|b, l| lib.FPDFSignatureObj_GetTime(sig, b, l));
                let rlen = lib.FPDFSignatureObj_GetReason(sig, ptr::null_mut(), 0) as usize;
                let mut rbuf = vec![0u16; rlen.div_ceil(2)];
                lib.FPDFSignatureObj_GetReason(sig, rbuf.as_mut_ptr().cast(), rlen as c_ulong);
                Some(SignatureInfo {
                    check: super::signatures::check(data, &ranges, &contents, &subfilter),
                    reason: utf16_buf_to_string(&rbuf),
                    time,
                    permission: lib.FPDFSignatureObj_GetDocMDPPermission(sig),
                })
            })
            .collect()
    }

    /// An empty document (for extracting pages into).
    pub fn create(lib: Lib) -> Result<Doc, String> {
        let handle = unsafe { lib.FPDF_CreateNewDocument() };
        if handle.is_null() {
            return Err("PDFium could not create a document".into());
        }
        let info: FPDF_FORMFILLINFO = unsafe { std::mem::zeroed() };
        Ok(Doc {
            lib,
            handle,
            form: ptr::null_mut(),
            pages: RefCell::new(Vec::new()),
            env: Box::new(FormEnv { info, invalidated: RefCell::new(Vec::new()), changed: Cell::new(false) }),
            _data: Vec::new(),
        })
    }

    /// Applies a page operation. Returns the number of pages inserted (InsertFile).
    pub fn apply_page_op(&self, op: &super::pages::PageOp) -> Result<usize, String> {
        use super::pages::PageOp;
        self.kill_form_focus();
        self.close_all_pages();
        let lib = self.lib;
        match op {
            PageOp::Delete { pages } => {
                let mut p = pages.clone();
                p.sort_unstable();
                p.dedup();
                for &i in p.iter().rev() {
                    unsafe { lib.FPDFPage_Delete(self.handle, i as c_int) };
                }
                Ok(0)
            }
            PageOp::Move { pages, to } => {
                let mut p: Vec<c_int> = pages.iter().map(|&i| i as c_int).collect();
                p.sort_unstable();
                p.dedup();
                let ok = unsafe { lib.FPDF_MovePages(self.handle, p.as_ptr(), p.len() as c_ulong, *to as c_int) };
                if lib.is_true(ok) { Ok(0) } else { Err("PDFium could not move the pages".into()) }
            }
            PageOp::Rotate { pages, quarter_turns } => {
                for &i in pages {
                    let page = self.page(i).ok_or("page not found")?;
                    let current = unsafe { lib.FPDFPage_GetRotation(page.handle) };
                    unsafe { lib.FPDFPage_SetRotation(page.handle, (current + quarter_turns).rem_euclid(4)) };
                }
                // Page sizes and geometry changed; reload pages on next use.
                self.close_all_pages();
                Ok(0)
            }
            PageOp::InsertBlank { at, size } => {
                let page = unsafe { lib.FPDFPage_New(self.handle, *at as c_int, size.0 as f64, size.1 as f64) };
                if page.is_null() {
                    return Err("PDFium could not create a page".into());
                }
                unsafe { lib.FPDF_ClosePage(page) };
                Ok(1)
            }
            PageOp::InsertFile { at, source, pages, .. } => {
                let src = Doc::open_bytes(lib, source.as_ref().clone(), None).map_err(|e| match e {
                    OpenError::Password => "password-protected files cannot be inserted".to_string(),
                    OpenError::Other(m) => m,
                })?;
                self.import_pages(&src, pages.as_deref(), *at)
            }
            PageOp::SetOutline { .. } | PageOp::SetProperties { .. } => Err("written by the worker".into()),
            PageOp::Watermark { pages, spec } => {
                const FPDF_FONT_TRUETYPE: c_int = 2;
                let ttf = crate::fonts::BOLD;
                let font =
                    unsafe { lib.FPDFText_LoadFont(self.handle, ttf.as_ptr(), ttf.len() as _, FPDF_FONT_TRUETYPE, 1) };
                if font.is_null() {
                    return Err("PDFium could not load the watermark font".into());
                }
                let result = pages.iter().try_for_each(|&i| {
                    let page = self.page(i).ok_or("page not found")?;
                    page.remove_marked(WATERMARK_MARK);
                    page.add_watermark(spec, font);
                    page.generate_content()
                });
                unsafe { lib.FPDFFont_Close(font) };
                self.close_all_pages();
                result.map(|_| 0)
            }
            PageOp::RemoveWatermark { pages } => {
                for &i in pages {
                    let page = self.page(i).ok_or("page not found")?;
                    if page.remove_marked(WATERMARK_MARK) > 0 {
                        page.generate_content()?;
                    }
                }
                self.close_all_pages();
                Ok(0)
            }
            PageOp::Flatten { pages } => {
                const FLAT_NORMALDISPLAY: c_int = 0;
                const FLATTEN_FAIL: c_int = 0;
                for &i in pages {
                    let page = self.page(i).ok_or("page not found")?;
                    if unsafe { lib.FPDFPage_Flatten(page.handle, FLAT_NORMALDISPLAY) } == FLATTEN_FAIL {
                        return Err(format!("PDFium could not flatten page {}", i + 1));
                    }
                }
                self.close_all_pages();
                Ok(0)
            }
            PageOp::Redact { areas } => {
                for (i, boxes) in areas {
                    let page = self.page(*i).ok_or("page not found")?;
                    page.redact(boxes)?;
                }
                self.close_all_pages();
                Ok(0)
            }
            PageOp::Crop { pages, margins } => {
                for &i in pages {
                    let page = self.page(i).ok_or("page not found")?;
                    page.crop(*margins)?;
                }
                self.close_all_pages();
                Ok(0)
            }
            PageOp::HeaderFooter { pages, spec } => {
                const FPDF_FONT_TRUETYPE: c_int = 2;
                let ttf = crate::fonts::REGULAR;
                let font =
                    unsafe { lib.FPDFText_LoadFont(self.handle, ttf.as_ptr(), ttf.len() as _, FPDF_FONT_TRUETYPE, 1) };
                if font.is_null() {
                    return Err("PDFium could not load the header font".into());
                }
                let count = self.page_count();
                let result = pages.iter().try_for_each(|&i| {
                    let page = self.page(i).ok_or("page not found")?;
                    page.remove_marked(HEADER_FOOTER_MARK);
                    page.add_header_footer(spec, i, count, font);
                    if lib.is_true(unsafe { lib.FPDFPage_GenerateContent(page.handle) }) {
                        Ok(())
                    } else {
                        Err("PDFium could not write the page content".to_string())
                    }
                });
                unsafe { lib.FPDFFont_Close(font) };
                self.close_all_pages();
                result.map(|_| 0)
            }
            PageOp::RemoveHeaderFooter { pages } => {
                for &i in pages {
                    let page = self.page(i).ok_or("page not found")?;
                    if page.remove_marked(HEADER_FOOTER_MARK) > 0
                        && !lib.is_true(unsafe { lib.FPDFPage_GenerateContent(page.handle) })
                    {
                        return Err("PDFium could not write the page content".into());
                    }
                }
                self.close_all_pages();
                Ok(0)
            }
            PageOp::TextLayer { pages, replace } => {
                const FPDF_FONT_TRUETYPE: c_int = 2;
                let ttf = crate::fonts::REGULAR;
                // One embedded font (as CID font, so any Unicode text can be stored)
                // shared by all text objects of this operation.
                let font =
                    unsafe { lib.FPDFText_LoadFont(self.handle, ttf.as_ptr(), ttf.len() as _, FPDF_FONT_TRUETYPE, 1) };
                if font.is_null() {
                    return Err("PDFium could not load the text layer font".into());
                }
                let result = pages.iter().try_for_each(|(i, words)| {
                    let page = self.page(*i).ok_or("page not found")?;
                    if *replace {
                        page.remove_invisible_text();
                    }
                    page.add_invisible_words(words, font);
                    if lib.is_true(unsafe { lib.FPDFPage_GenerateContent(page.handle) }) {
                        Ok(())
                    } else {
                        Err("PDFium could not write the page content".to_string())
                    }
                });
                unsafe { lib.FPDFFont_Close(font) };
                // Text pages are re-read on next use.
                self.close_all_pages();
                result.map(|()| 0)
            }
        }
    }

    /// Imports pages of `src` (all if None) at index `at`. Returns how many.
    pub fn import_pages(&self, src: &Doc, pages: Option<&[usize]>, at: usize) -> Result<usize, String> {
        let lib = self.lib;
        let indices: Vec<c_int> = match pages {
            Some(p) => p.iter().map(|&i| i as c_int).collect(),
            None => (0..src.page_count() as c_int).collect(),
        };
        let ok = unsafe {
            lib.FPDF_ImportPagesByIndex(
                self.handle,
                src.handle,
                indices.as_ptr(),
                indices.len() as c_ulong,
                at as c_int,
            )
        };
        if lib.is_true(ok) { Ok(indices.len()) } else { Err("PDFium could not import the pages".into()) }
    }

    pub fn kill_form_focus(&self) {
        if !self.form.is_null() {
            unsafe { self.lib.FORM_ForceToKillFocus(self.form) };
        }
    }

    pub fn properties(&self) -> DocProperties {
        let meta = |tag: &str| {
            read_utf16(|buf, len| unsafe { self.lib.FPDF_GetMetaText(self.handle, tag, buf, len) }).trim().to_string()
        };
        let mut v = 0;
        let version = self.lib.is_true(unsafe { self.lib.FPDF_GetFileVersion(self.handle, &mut v) }).then_some(v);
        DocProperties {
            title: meta("Title"),
            author: meta("Author"),
            subject: meta("Subject"),
            keywords: meta("Keywords"),
            creator: meta("Creator"),
            producer: meta("Producer"),
            created: meta("CreationDate"),
            modified: meta("ModDate"),
            version,
        }
    }

    pub fn attachments(&self) -> Vec<AttachmentInfo> {
        let lib = self.lib;
        let n = unsafe { lib.FPDFDoc_GetAttachmentCount(self.handle) }.max(0);
        (0..n)
            .filter_map(|i| {
                let a = unsafe { lib.FPDFDoc_GetAttachment(self.handle, i) };
                if a.is_null() {
                    return None;
                }
                let name = read_utf16(|buf, len| unsafe { lib.FPDFAttachment_GetName(a, buf as *mut FPDF_WCHAR, len) });
                let mut size: c_ulong = 0;
                unsafe { lib.FPDFAttachment_GetFile(a, ptr::null_mut(), 0, &mut size) };
                Some(AttachmentInfo { name, size: size as usize })
            })
            .collect()
    }

    /// Name and content of embedded file `index`.
    pub fn attachment(&self, index: usize) -> Option<(String, Vec<u8>)> {
        let lib = self.lib;
        unsafe {
            let a = lib.FPDFDoc_GetAttachment(self.handle, index as c_int);
            if a.is_null() {
                return None;
            }
            let name = read_utf16(|buf, len| lib.FPDFAttachment_GetName(a, buf as *mut FPDF_WCHAR, len));
            let mut size: c_ulong = 0;
            if !lib.is_true(lib.FPDFAttachment_GetFile(a, ptr::null_mut(), 0, &mut size)) {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            let ok = lib.FPDFAttachment_GetFile(a, data.as_mut_ptr() as *mut c_void, size, &mut size);
            lib.is_true(ok).then(|| {
                data.truncate(size as usize);
                (name, data)
            })
        }
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
            let index = *count;
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
            let children = if depth < MAX_DEPTH { self.outline_level(child, depth + 1, count) } else { Vec::new() };
            items.push(OutlineItem { title, page, children, origin: Some(OutlineOrigin { index, page }) });
            bm = unsafe { self.lib.FPDFBookmark_GetNextSibling(self.handle, bm) };
        }
        items
    }

    pub fn save_to_bytes(&self) -> Result<Vec<u8>, String> {
        self.save_with(FPDF_NO_INCREMENTAL)
    }

    /// The opened bytes unchanged, with the changes appended (an incremental update),
    /// so the signatures in them stay intact.
    pub fn save_incremental(&self) -> Result<Vec<u8>, String> {
        self.save_with(FPDF_INCREMENTAL)
    }

    fn save_with(&self, flags: FPDF_DWORD) -> Result<Vec<u8>, String> {
        let mut writer =
            VecWriter { base: FPDF_FILEWRITE { version: 1, WriteBlock: Some(write_block) }, out: Vec::new() };
        let ok = unsafe { self.lib.FPDF_SaveAsCopy(self.handle, &mut writer.base, flags) };
        if self.lib.is_true(ok) { Ok(writer.out) } else { Err("PDFium could not write the document".into()) }
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
        PageGeom { size: ((r - l).abs().max(1.0), (t - b).abs().max(1.0)), origin: (l.min(r), b.min(t)), rotation }
    }

    /// Renders into an RGBA buffer `width` x `height` pixels (already rotated by
    /// `rotation`), including annotations and interactive form fields.
    pub fn render(&self, width: usize, height: usize, rotation: Rotation) -> Option<Vec<u8>> {
        self.render_bitmap((width, height), (0, 0, width, height), rotation, true, false)
    }

    /// Renders for printing: BGRA byte order (as GDI expects) or RGBA, with print
    /// appearances and without form field highlights.
    #[allow(dead_code)]
    pub fn render_for_print(&self, width: usize, height: usize, rgba: bool) -> Option<Vec<u8>> {
        self.render_bitmap((width, height), (0, 0, width, height), Rotation::R0, rgba, true)
    }

    /// Renders the part `region` (x, y, width, height in pixels) of the page drawn at
    /// `full` size.
    pub fn render_region(
        &self,
        full: (usize, usize),
        region: (usize, usize, usize, usize),
        rotation: Rotation,
    ) -> Option<Vec<u8>> {
        self.render_bitmap(full, region, rotation, true, false)
    }

    fn render_bitmap(
        &self,
        full: (usize, usize),
        region: (usize, usize, usize, usize),
        rotation: Rotation,
        rgba: bool,
        printing: bool,
    ) -> Option<Vec<u8>> {
        const FPDF_PRINTING: c_int = 0x800;
        let lib = self.lib();
        let (rx, ry, width, height) = region;
        let (fw, fh) = (full.0 as c_int, full.1 as c_int);
        let (ox, oy) = (-(rx as c_int), -(ry as c_int));
        unsafe {
            let bitmap = lib.FPDFBitmap_CreateEx(width as c_int, height as c_int, FPDF_BITMAP_BGRA, ptr::null_mut(), 0);
            if bitmap.is_null() {
                return None;
            }
            lib.FPDFBitmap_FillRect(bitmap, 0, 0, width as c_int, height as c_int, 0xFFFF_FFFF);
            let rot = rotation.quarter_turns();
            let mut flags = FPDF_ANNOT;
            if rgba {
                flags |= FPDF_REVERSE_BYTE_ORDER;
            }
            if printing {
                flags |= FPDF_PRINTING;
            }
            lib.FPDF_RenderPageBitmap(bitmap, self.handle, ox, oy, fw, fh, rot, flags);
            if !printing && !self.doc.form.is_null() {
                lib.FPDF_FFLDraw(self.doc.form, bitmap, self.handle, ox, oy, fw, fh, rot, flags);
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

    /// Renders the page content alone (no annotations or form fields) as 8-bit grey
    /// pixels, for text recognition.
    pub fn render_gray(&self, width: usize, height: usize) -> Option<Vec<u8>> {
        let lib = self.lib();
        unsafe {
            let bitmap = lib.FPDFBitmap_CreateEx(width as c_int, height as c_int, FPDF_BITMAP_BGRA, ptr::null_mut(), 0);
            if bitmap.is_null() {
                return None;
            }
            lib.FPDFBitmap_FillRect(bitmap, 0, 0, width as c_int, height as c_int, 0xFFFF_FFFF);
            lib.FPDF_RenderPageBitmap(bitmap, self.handle, 0, 0, width as c_int, height as c_int, 0, 0);
            let stride = lib.FPDFBitmap_GetStride(bitmap) as usize;
            let buf = lib.FPDFBitmap_GetBuffer(bitmap) as *const u8;
            let mut out = Vec::with_capacity(width * height);
            for y in 0..height {
                let row = std::slice::from_raw_parts(buf.add(y * stride), width * 4);
                out.extend(
                    row.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|px| ((px[0] as u32 * 114 + px[1] as u32 * 587 + px[2] as u32 * 299) / 1000) as u8),
                );
            }
            lib.FPDFBitmap_Destroy(bitmap);
            Some(out)
        }
    }

    /// Number of characters of the page's text that are not white space.
    pub fn text_char_count(&self) -> usize {
        let lib = self.lib();
        let Some(tp) = self.text_page() else { return 0 };
        let n = unsafe { lib.FPDFText_CountChars(tp.handle) };
        (0..n)
            .filter(|&i| {
                char::from_u32(unsafe { lib.FPDFText_GetUnicode(tp.handle, i) }).is_some_and(|c| !c.is_whitespace())
            })
            .count()
    }

    /// Maps a point of a `width` x `height` rendering (as from `render_gray`) to page space.
    fn device_to_page(&self, width: usize, height: usize, x: f32, y: f32) -> (f32, f32) {
        let (mut px, mut py) = (0.0, 0.0);
        unsafe {
            self.lib().FPDF_DeviceToPage(
                self.handle,
                0,
                0,
                width as c_int,
                height as c_int,
                0,
                x.round() as c_int,
                y.round() as c_int,
                &mut px,
                &mut py,
            )
        };
        (px as f32, py as f32)
    }

    /// Places words recognised on a `width` x `height` rendering of this page.
    pub fn place_ocr_words(
        &self,
        width: usize,
        height: usize,
        words: Vec<super::ocr::HocrWord>,
    ) -> Vec<super::ocr::OcrWord> {
        words
            .into_iter()
            .filter_map(|w| {
                let origin = self.device_to_page(width, height, w.bbox[0], w.baseline);
                let end = self.device_to_page(width, height, w.bbox[2], w.baseline);
                let top = self.device_to_page(width, height, w.bbox[0], w.baseline - w.size);
                let (ux, uy) = (top.0 - origin.0, top.1 - origin.1);
                let size = ux.hypot(uy);
                (size > 0.0).then(|| super::ocr::OcrWord {
                    text: w.text,
                    origin,
                    end,
                    up: (ux / size, uy / size),
                    size,
                })
            })
            .collect()
    }

    /// Adds words as invisible text (render mode 3), each stretched along its baseline
    /// to the width the word has in the image. Call FPDFPage_GenerateContent after.
    fn add_invisible_words(&self, words: &[super::ocr::OcrWord], font: FPDF_FONT) {
        const INVISIBLE: FPDF_TEXT_RENDERMODE = 3;
        let lib = self.lib();
        for w in words {
            unsafe {
                let obj = lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, w.size);
                if obj.is_null() {
                    continue;
                }
                let text = wide(&w.text);
                lib.FPDFText_SetText(obj, text.as_ptr());
                lib.FPDFTextObj_SetTextRenderMode(obj, INVISIBLE);
                let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                let ok = lib.FPDFPageObj_GetBounds(obj, &mut l, &mut b, &mut r, &mut t);
                let natural = r - l;
                if !lib.is_true(ok) || natural <= 0.0 {
                    lib.FPDFPageObj_Destroy(obj);
                    continue;
                }
                // Text space x runs along the baseline (scaled to the word's width),
                // y along `up`; the left edge of the glyphs lands on `origin`.
                let a = (w.end.0 - w.origin.0) / natural;
                let bb = (w.end.1 - w.origin.1) / natural;
                let (c, d) = w.up;
                let e = w.origin.0 - a * l;
                let f = w.origin.1 - bb * l;
                lib.FPDFPageObj_Transform(obj, a as f64, bb as f64, c as f64, d as f64, e as f64, f as f64);
                lib.FPDFPage_InsertObject(self.handle, obj);
            }
        }
    }

    /// The visible area (crop box, or media box without one), unrotated.
    fn crop_box(&self) -> PdfBox {
        let lib = self.lib();
        let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        unsafe {
            if !lib.is_true(lib.FPDFPage_GetCropBox(self.handle, &mut l, &mut b, &mut r, &mut t)) {
                lib.FPDFPage_GetMediaBox(self.handle, &mut l, &mut b, &mut r, &mut t);
            }
        }
        PdfBox { left: l.min(r), bottom: b.min(t), right: l.max(r), top: b.max(t) }
    }

    fn rotation_turns(&self) -> usize {
        unsafe { self.lib().FPDFPage_GetRotation(self.handle) }.rem_euclid(4) as usize
    }

    /// Trims the visible area by margins given as the page is displayed.
    fn crop(&self, margins: super::pages::Margins) -> Result<(), String> {
        let disp = margins.as_array();
        let r = self.rotation_turns();
        // Unrotated sides (top, right, bottom, left) from the displayed ones.
        let m: [f32; 4] = std::array::from_fn(|i| disp[(i + r) % 4]);
        let b = self.crop_box();
        let new = PdfBox { left: b.left + m[3], bottom: b.bottom + m[2], right: b.right - m[1], top: b.top - m[0] };
        if new.right - new.left < 36.0 || new.top - new.bottom < 36.0 {
            return Err("The margins leave less than half an inch of the page.".into());
        }
        unsafe { self.lib().FPDFPage_SetCropBox(self.handle, new.left, new.bottom, new.right, new.top) };
        Ok(())
    }

    /// Maps a point given as the page is displayed (origin bottom-left of the visible
    /// area) to unrotated page space, with the displayed x and y directions.
    fn displayed_to_page(&self, u: f32, v: f32) -> ((f32, f32), (f32, f32), (f32, f32)) {
        let b = self.crop_box();
        match self.rotation_turns() {
            0 => ((b.left + u, b.bottom + v), (1.0, 0.0), (0.0, 1.0)),
            1 => ((b.right - v, b.bottom + u), (0.0, 1.0), (-1.0, 0.0)),
            2 => ((b.right - u, b.top - v), (-1.0, 0.0), (0.0, -1.0)),
            _ => ((b.left + v, b.top - u), (0.0, -1.0), (1.0, 0.0)),
        }
    }

    fn add_header_footer(&self, spec: &super::pages::HeaderFooter, index: usize, count: usize, font: FPDF_FONT) {
        let lib = self.lib();
        let b = self.crop_box();
        let (w, h) = if self.rotation_turns() % 2 == 1 {
            (b.top - b.bottom, b.right - b.left)
        } else {
            (b.right - b.left, b.top - b.bottom)
        };
        let [cr, cg, cb] = spec.color;
        let rows = [(&spec.header, h - spec.margin - spec.size * 0.75), (&spec.footer, spec.margin + spec.size * 0.2)];
        for (texts, baseline) in rows {
            for (slot, text) in texts.iter().enumerate() {
                let text = super::pages::HeaderFooter::resolve(text, index, count, spec.first_number);
                if text.trim().is_empty() {
                    continue;
                }
                unsafe {
                    let obj = lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, spec.size);
                    if obj.is_null() {
                        continue;
                    }
                    let wtext = wide(&text);
                    lib.FPDFText_SetText(obj, wtext.as_ptr());
                    lib.FPDFPageObj_SetFillColor(obj, cr.into(), cg.into(), cb.into(), 255);
                    let (mut l, mut bt, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                    lib.FPDFPageObj_GetBounds(obj, &mut l, &mut bt, &mut r, &mut t);
                    let tw = r - l;
                    let u = match slot {
                        0 => spec.margin,
                        1 => (w - tw) / 2.0,
                        _ => w - spec.margin - tw,
                    };
                    let ((ox, oy), (ax, ay), (cx, cy)) = self.displayed_to_page(u, baseline);
                    lib.FPDFPageObj_Transform(
                        obj,
                        ax as f64,
                        ay as f64,
                        cx as f64,
                        cy as f64,
                        (ox - ax * l) as f64,
                        (oy - ay * l) as f64,
                    );
                    lib.FPDFPageObj_AddMark(obj, HEADER_FOOTER_MARK);
                    lib.FPDFPage_InsertObject(self.handle, obj);
                }
            }
        }
    }

    fn generate_content(&self) -> Result<(), String> {
        if self.lib().is_true(unsafe { self.lib().FPDFPage_GenerateContent(self.handle) }) {
            Ok(())
        } else {
            Err("PDFium could not write the page content".to_string())
        }
    }

    /// Draws the watermark text across the page centre, as the page is displayed.
    fn add_watermark(&self, spec: &super::pages::Watermark, font: FPDF_FONT) {
        let lib = self.lib();
        if spec.text.trim().is_empty() {
            return;
        }
        let b = self.crop_box();
        let (w, h) = if self.rotation_turns() % 2 == 1 {
            (b.top - b.bottom, b.right - b.left)
        } else {
            (b.right - b.left, b.top - b.bottom)
        };
        let angle = spec.angle.map_or_else(|| h.atan2(w), f32::to_radians);
        unsafe {
            // Measure at 1 pt, then pick the size that fits 80 % of the page along the angle.
            let probe = lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, 1.0);
            if probe.is_null() {
                return;
            }
            let text = wide(&spec.text);
            lib.FPDFText_SetText(probe, text.as_ptr());
            let (mut l, mut bt, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            lib.FPDFPageObj_GetBounds(probe, &mut l, &mut bt, &mut r, &mut t);
            lib.FPDFPageObj_Destroy(probe);
            let unit_width = (r - l).max(0.01);
            let (sin, cos) = angle.sin_cos();
            // Longest line through the centre at this angle that stays on the page.
            let reach = (if cos.abs() > 1e-3 { w / cos.abs() } else { f32::MAX }).min(if sin.abs() > 1e-3 {
                h / sin.abs()
            } else {
                f32::MAX
            });
            let size = spec.size.min(0.8 * reach / unit_width).max(4.0);
            let obj = lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, size);
            if obj.is_null() {
                return;
            }
            lib.FPDFText_SetText(obj, text.as_ptr());
            let [cr, cg, cb] = spec.color;
            let alpha = (spec.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
            lib.FPDFPageObj_SetFillColor(obj, cr.into(), cg.into(), cb.into(), alpha);
            lib.FPDFPageObj_GetBounds(obj, &mut l, &mut bt, &mut r, &mut t);
            let (xc, yc) = ((l + r) / 2.0, (bt + t) / 2.0);
            // Text space -> displayed page (rotated about the centre) -> PDF page space.
            let (o, (ax, ay), (cx, cy)) = self.displayed_to_page(0.0, 0.0);
            let (du, dv) = (w / 2.0 - cos * xc + sin * yc, h / 2.0 - sin * xc - cos * yc);
            let m = FS_MATRIX {
                a: ax * cos + cx * sin,
                b: ay * cos + cy * sin,
                c: -ax * sin + cx * cos,
                d: -ay * sin + cy * cos,
                e: o.0 + ax * du + cx * dv,
                f: o.1 + ay * du + cy * dv,
            };
            lib.FPDFPageObj_SetMatrix(obj, &m);
            lib.FPDFPageObj_AddMark(obj, WATERMARK_MARK);
            if !(spec.behind && lib.is_true(lib.FPDFPage_InsertObjectAtIndex(self.handle, obj, 0))) {
                lib.FPDFPage_InsertObject(self.handle, obj);
            }
        }
    }

    /// Removes page objects carrying the marked-content tag `name`. Returns how many.
    fn remove_marked(&self, name: &str) -> usize {
        let lib = self.lib();
        let mut removed = 0;
        unsafe {
            for i in (0..lib.FPDFPage_CountObjects(self.handle)).rev() {
                let obj = lib.FPDFPage_GetObject(self.handle, i);
                if obj.is_null() {
                    continue;
                }
                let marked = (0..lib.FPDFPageObj_CountMarks(obj)).any(|m| {
                    let mark = lib.FPDFPageObj_GetMark(obj, m as _);
                    let mut len: c_ulong = 0;
                    let mut buf = [0u16; 64];
                    !mark.is_null()
                        && lib.is_true(lib.FPDFPageObjMark_GetName(
                            mark,
                            buf.as_mut_ptr(),
                            (buf.len() * 2) as c_ulong,
                            &mut len,
                        ))
                        && utf16_buf_to_string(&buf) == name
                });
                if marked && lib.is_true(lib.FPDFPage_RemoveObject(self.handle, obj)) {
                    lib.FPDFPageObj_Destroy(obj);
                    removed += 1;
                }
            }
        }
        removed
    }

    /// Scales images shown above `dpi` down to `dpi` and stores them as JPEG, where
    /// that makes them smaller. Images with transparency and black-and-white images
    /// are left alone. Returns the number of images changed.
    pub fn downsample_images(&self, dpi: f32) -> usize {
        const FPDF_PAGEOBJ_IMAGE: c_int = 3;
        let lib = self.lib();
        let mut changed = 0;
        unsafe {
            for i in 0..lib.FPDFPage_CountObjects(self.handle) {
                let obj = lib.FPDFPage_GetObject(self.handle, i);
                if obj.is_null() || lib.FPDFPageObj_GetType(obj) != FPDF_PAGEOBJ_IMAGE {
                    continue;
                }
                let mut meta: FPDF_IMAGEOBJ_METADATA = std::mem::zeroed();
                if !lib.is_true(lib.FPDFImageObj_GetImageMetadata(obj, self.handle, &mut meta)) {
                    continue;
                }
                let shown_dpi = meta.horizontal_dpi.min(meta.vertical_dpi);
                if meta.bits_per_pixel < 8
                    || shown_dpi <= dpi * 1.25
                    || (meta.width as u64 * meta.height as u64) < 100_000
                {
                    continue;
                }
                let raw_len = lib.FPDFImageObj_GetImageDataRaw(obj, ptr::null_mut(), 0) as usize;
                let Some((w, h, rgba)) = self.object_image(obj) else { continue };
                if rgba.as_chunks::<4>().0.iter().any(|p| p[3] < 250) {
                    continue;
                }
                let scale = dpi / shown_dpi;
                let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
                let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else { continue };
                let small = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
                let rgb = image::DynamicImage::ImageRgba8(small).to_rgb8();
                let mut jpeg = Vec::new();
                let encoded = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&rgb);
                if encoded.is_err() || jpeg.len() >= raw_len {
                    continue;
                }
                let mut access = FPDF_FILEACCESS {
                    m_FileLen: jpeg.len() as c_ulong,
                    m_GetBlock: Some(read_block),
                    m_Param: &jpeg as *const Vec<u8> as *mut c_void,
                };
                let mut page = self.handle;
                if lib.is_true(lib.FPDFImageObj_LoadJpegFileInline(&mut page, 1, obj, &mut access)) {
                    changed += 1;
                }
            }
            if changed > 0 {
                lib.FPDFPage_GenerateContent(self.handle);
            }
        }
        changed
    }

    /// The pixels of an image object (with its transparency mask) as RGBA.
    fn object_image(&self, obj: FPDF_PAGEOBJECT) -> Option<(u32, u32, Vec<u8>)> {
        let lib = self.lib();
        unsafe {
            let (mut pw, mut ph) = (0, 0);
            lib.FPDFImageObj_GetImagePixelSize(obj, &mut pw, &mut ph);
            let mut saved = FS_MATRIX { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };
            lib.FPDFPageObj_GetMatrix(obj, &mut saved);
            let native = FS_MATRIX { a: pw.max(1) as f32, b: 0.0, c: 0.0, d: ph.max(1) as f32, e: 0.0, f: 0.0 };
            lib.FPDFPageObj_SetMatrix(obj, &native);
            let mut bmp = lib.FPDFImageObj_GetRenderedBitmap(self.doc.handle, self.handle, obj);
            lib.FPDFPageObj_SetMatrix(obj, &saved);
            if bmp.is_null() {
                bmp = lib.FPDFImageObj_GetBitmap(obj);
            }
            if bmp.is_null() {
                return None;
            }
            let out = bitmap_rgba(lib, bmp);
            lib.FPDFBitmap_Destroy(bmp);
            Some(out)
        }
    }

    /// Redacts the page: removes text, image pixels, shapes and annotations under
    /// `areas` (PDF page space) and paints the areas black. Characters whose centre
    /// is covered go; the rest of their text object is rewritten glyph by glyph at
    /// the same positions. Glyphs that cannot be rewritten in their font, and form
    /// XObjects touching an area, are replaced by a picture of the page taken before
    /// the redaction (with the areas already black).
    fn redact(&self, areas: &[PdfBox]) -> Result<(), String> {
        use super::redact::{removes_char, removes_object};
        const IMAGE: c_int = 3;
        const FORM: c_int = 5;
        const FILL_ALTERNATE: c_int = 1;
        let lib = self.lib();
        let mut black: Vec<PdfBox> = areas.to_vec();
        let mut patches: Vec<PdfBox> = Vec::new();
        let snapshot = self.redaction_snapshot();

        struct Glyph {
            ch: u32,
            origin: (f64, f64),
            bbox: PdfBox,
            remove: bool,
        }
        let objects: Vec<FPDF_PAGEOBJECT> = unsafe {
            (0..lib.FPDFPage_CountObjects(self.handle)).map(|i| lib.FPDFPage_GetObject(self.handle, i)).collect()
        };
        let bounds =
            |obj: FPDF_PAGEOBJECT| {
                let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                unsafe { lib.is_true(lib.FPDFPageObj_GetBounds(obj, &mut l, &mut b, &mut r, &mut t)) }
                    .then_some(PdfBox { left: l, bottom: b, right: r, top: t })
            };

        // 1. Text: characters per text object.
        let mut glyphs: Vec<(FPDF_PAGEOBJECT, Vec<Glyph>)> = Vec::new();
        if let Some(tp) = self.text_page() {
            let n = unsafe { lib.FPDFText_CountChars(tp.handle) };
            for i in 0..n {
                unsafe {
                    let obj = lib.FPDFText_GetTextObject(tp.handle, i);
                    let ch = lib.FPDFText_GetUnicode(tp.handle, i);
                    if obj.is_null() || ch < 0x20 {
                        continue;
                    }
                    let (mut l, mut r, mut b, mut t) = (0.0, 0.0, 0.0, 0.0);
                    let (mut x, mut y) = (0.0, 0.0);
                    if !lib.is_true(lib.FPDFText_GetCharBox(tp.handle, i, &mut l, &mut r, &mut b, &mut t))
                        || !lib.is_true(lib.FPDFText_GetCharOrigin(tp.handle, i, &mut x, &mut y))
                    {
                        continue;
                    }
                    let bbox = PdfBox { left: l as f32, bottom: b as f32, right: r as f32, top: t as f32 };
                    let g = Glyph { ch, origin: (x, y), bbox, remove: removes_char(&bbox, areas) };
                    match glyphs.iter_mut().find(|(o, _)| *o == obj) {
                        Some((_, v)) => v.push(g),
                        None => glyphs.push((obj, vec![g])),
                    }
                }
            }
        }
        // Rewrites the kept glyphs of affected top-level text objects.
        let mut created: Vec<(FPDF_PAGEOBJECT, u32, PdfBox)> = Vec::new();
        for (obj, gs) in &glyphs {
            if !gs.iter().any(|g| g.remove) {
                continue;
            }
            black.extend(gs.iter().filter(|g| g.remove).map(|g| g.bbox));
            let Some(index) = objects.iter().position(|o| o == obj) else {
                // Inside a form XObject: handled with the form below.
                continue;
            };
            unsafe {
                let font = lib.FPDFTextObj_GetFont(*obj);
                let mut size = 0.0f32;
                lib.FPDFTextObj_GetFontSize(*obj, &mut size);
                let mut m = FS_MATRIX { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };
                lib.FPDFPageObj_GetMatrix(*obj, &mut m);
                let mode = lib.FPDFTextObj_GetTextRenderMode(*obj);
                let (mut fr, mut fg, mut fb, mut fa) = (0, 0, 0, 255);
                let fill = lib.is_true(lib.FPDFPageObj_GetFillColor(*obj, &mut fr, &mut fg, &mut fb, &mut fa));
                let (mut sr, mut sg, mut sb, mut sa) = (0, 0, 0, 255);
                let stroke = lib.is_true(lib.FPDFPageObj_GetStrokeColor(*obj, &mut sr, &mut sg, &mut sb, &mut sa));
                let mut width = 0.0f32;
                let has_width = lib.is_true(lib.FPDFPageObj_GetStrokeWidth(*obj, &mut width));
                for g in gs.iter().filter(|g| !g.remove && g.ch != 0x20) {
                    let new = if font.is_null() {
                        ptr::null_mut()
                    } else {
                        lib.FPDFPageObj_CreateTextObj(self.doc.handle, font, size)
                    };
                    let text: Vec<u16> = char::from_u32(g.ch).map(|c| wide(&c.to_string())).unwrap_or_default();
                    if new.is_null() || text.is_empty() || !lib.is_true(lib.FPDFText_SetText(new, text.as_ptr())) {
                        if !new.is_null() {
                            lib.FPDFPageObj_Destroy(new);
                        }
                        patches.push(g.bbox);
                        continue;
                    }
                    let gm = FS_MATRIX { e: g.origin.0 as f32, f: g.origin.1 as f32, ..m };
                    lib.FPDFPageObj_SetMatrix(new, &gm);
                    lib.FPDFTextObj_SetTextRenderMode(new, mode);
                    if fill {
                        lib.FPDFPageObj_SetFillColor(new, fr, fg, fb, fa);
                    }
                    if stroke {
                        lib.FPDFPageObj_SetStrokeColor(new, sr, sg, sb, sa);
                    }
                    if has_width {
                        lib.FPDFPageObj_SetStrokeWidth(new, width);
                    }
                    if !lib.is_true(lib.FPDFPage_InsertObjectAtIndex(self.handle, new, index)) {
                        lib.FPDFPage_InsertObject(self.handle, new);
                    }
                    created.push((new, g.ch, g.bbox));
                }
                if lib.is_true(lib.FPDFPage_RemoveObject(self.handle, *obj)) {
                    lib.FPDFPageObj_Destroy(*obj);
                }
            }
        }
        // Glyphs whose font cannot encode the character again become part of a picture.
        if !created.is_empty()
            && let Some(tp) = self.text_page()
        {
            for (obj, ch, bbox) in &created {
                let mut buf = [0u16; 8];
                let len =
                    unsafe { lib.FPDFTextObj_GetText(*obj, tp.handle, buf.as_mut_ptr(), (buf.len() * 2) as c_ulong) };
                let got = utf16_buf_to_string(&buf[..(len as usize / 2).min(buf.len())]);
                if got.chars().next().map(u32::from) != Some(*ch) {
                    patches.push(*bbox);
                    unsafe {
                        if lib.is_true(lib.FPDFPage_RemoveObject(self.handle, *obj)) {
                            lib.FPDFPageObj_Destroy(*obj);
                        }
                    }
                }
            }
        }

        // 2. Images, shapes and form XObjects (top level; text objects were handled above).
        let hit = |b: &PdfBox| areas.iter().any(|a| a.intersects(b));
        for &obj in objects.iter().rev() {
            if glyphs.iter().any(|(o, gs)| *o == obj && gs.iter().any(|g| g.remove)) {
                continue; // already replaced
            }
            let Some(b) = bounds(obj) else { continue };
            if !hit(&b) {
                continue;
            }
            let kind = unsafe { lib.FPDFPageObj_GetType(obj) };
            let remove = if removes_object(&b, areas) {
                true
            } else if kind == FORM {
                patches.push(b);
                true
            } else if kind == IMAGE {
                !self.redact_image(obj, areas)
            } else {
                // Text none of whose characters is covered; shapes only partly covered.
                false
            };
            if remove {
                unsafe {
                    if lib.is_true(lib.FPDFPage_RemoveObject(self.handle, obj)) {
                        lib.FPDFPageObj_Destroy(obj);
                    }
                }
            }
        }

        // 3. Pictures of the page where content could not be kept as it was.
        if !patches.is_empty() {
            let (w, h, rgba) = snapshot.ok_or("could not render the page")?;
            for p in &patches {
                self.insert_patch(w, h, &rgba, &p.expand(0.5), &black)?;
            }
        }

        // 4. Annotations over an area.
        unsafe {
            for i in (0..lib.FPDFPage_GetAnnotCount(self.handle)).rev() {
                let a = lib.FPDFPage_GetAnnot(self.handle, i);
                if a.is_null() {
                    continue;
                }
                let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
                let ok = lib.is_true(lib.FPDFAnnot_GetRect(a, &mut r));
                lib.FPDFPage_CloseAnnot(a);
                let b = PdfBox {
                    left: r.left.min(r.right),
                    bottom: r.bottom.min(r.top),
                    right: r.left.max(r.right),
                    top: r.bottom.max(r.top),
                };
                if ok && hit(&b) {
                    lib.FPDFPage_RemoveAnnot(self.handle, i);
                }
            }
        }

        // 5. Black boxes on top.
        for b in &black {
            unsafe {
                let obj = lib.FPDFPageObj_CreateNewRect(b.left, b.bottom, b.right - b.left, b.top - b.bottom);
                if obj.is_null() {
                    return Err("PDFium could not draw the redaction".into());
                }
                lib.FPDFPageObj_SetFillColor(obj, 0, 0, 0, 255);
                lib.FPDFPath_SetDrawMode(obj, FILL_ALTERNATE, 0);
                lib.FPDFPage_InsertObject(self.handle, obj);
            }
        }
        if lib.is_true(unsafe { lib.FPDFPage_GenerateContent(self.handle) }) {
            Ok(())
        } else {
            Err("PDFium could not write the page content".into())
        }
    }

    /// The page content (no annotations) as RGBA pixels, about 200 dpi.
    fn redaction_snapshot(&self) -> Option<(u32, u32, Vec<u8>)> {
        let lib = self.lib();
        let (pw, ph) = unsafe { (lib.FPDF_GetPageWidthF(self.handle), lib.FPDF_GetPageHeightF(self.handle)) };
        let scale = (200.0 / 72.0f32).min(6000.0 / pw.max(ph).max(1.0));
        let (w, h) = ((pw * scale).round().max(1.0) as c_int, (ph * scale).round().max(1.0) as c_int);
        unsafe {
            let bmp = lib.FPDFBitmap_CreateEx(w, h, FPDF_BITMAP_BGRA, ptr::null_mut(), 0);
            if bmp.is_null() {
                return None;
            }
            lib.FPDFBitmap_FillRect(bmp, 0, 0, w, h, 0xFFFF_FFFF);
            lib.FPDF_RenderPageBitmap(bmp, self.handle, 0, 0, w, h, 0, FPDF_REVERSE_BYTE_ORDER);
            let stride = lib.FPDFBitmap_GetStride(bmp) as usize;
            let buf = lib.FPDFBitmap_GetBuffer(bmp) as *const u8;
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h as usize {
                out.extend_from_slice(std::slice::from_raw_parts(buf.add(y * stride), w as usize * 4));
            }
            lib.FPDFBitmap_Destroy(bmp);
            Some((w as u32, h as u32, out))
        }
    }

    /// Adds the part `area` of the page snapshot as an image, with `black` painted over.
    fn insert_patch(&self, w: u32, h: u32, rgba: &[u8], area: &PdfBox, black: &[PdfBox]) -> Result<(), String> {
        let lib = self.lib();
        let (wi, hi) = (w as c_int, h as c_int);
        let to_dev = |x: f32, y: f32| {
            let (mut dx, mut dy) = (0, 0);
            unsafe { lib.FPDF_PageToDevice(self.handle, 0, 0, wi, hi, 0, x.into(), y.into(), &mut dx, &mut dy) };
            (dx, dy)
        };
        let dev_rect = |b: &PdfBox| {
            let pts =
                [to_dev(b.left, b.bottom), to_dev(b.right, b.top), to_dev(b.left, b.top), to_dev(b.right, b.bottom)];
            let x0 = pts.iter().map(|p| p.0).min().unwrap_or(0).clamp(0, wi);
            let x1 = pts.iter().map(|p| p.0).max().unwrap_or(0).clamp(0, wi);
            let y0 = pts.iter().map(|p| p.1).min().unwrap_or(0).clamp(0, hi);
            let y1 = pts.iter().map(|p| p.1).max().unwrap_or(0).clamp(0, hi);
            (x0 as u32, y0 as u32, x1 as u32, y1 as u32)
        };
        let (x0, y0, x1, y1) = dev_rect(area);
        if x0 >= x1 || y0 >= y1 {
            return Ok(());
        }
        let (pw, ph) = (x1 - x0, y1 - y0);
        let mut crop = Vec::with_capacity((pw * ph * 4) as usize);
        for y in y0..y1 {
            let start = ((y * w + x0) * 4) as usize;
            crop.extend_from_slice(&rgba[start..start + (pw * 4) as usize]);
        }
        for b in black {
            let (bx0, by0, bx1, by1) = dev_rect(b);
            let (cx0, cy0) = (bx0.max(x0), by0.max(y0));
            let (cx1, cy1) = (bx1.min(x1), by1.min(y1));
            if cx0 < cx1 && cy0 < cy1 {
                super::redact::fill_black(&mut crop, pw, (cx0 - x0, cy0 - y0, cx1 - x0, cy1 - y0));
            }
        }
        let to_page = |dx: u32, dy: u32| {
            let (mut x, mut y) = (0.0, 0.0);
            unsafe { lib.FPDF_DeviceToPage(self.handle, 0, 0, wi, hi, 0, dx as c_int, dy as c_int, &mut x, &mut y) };
            (x as f32, y as f32)
        };
        // Image space: (0, 0) is the bottom-left pixel corner, (1, 1) the top-right.
        let o = to_page(x0, y1);
        let ux = to_page(x1, y1);
        let vy = to_page(x0, y0);
        let m = FS_MATRIX { a: ux.0 - o.0, b: ux.1 - o.1, c: vy.0 - o.0, d: vy.1 - o.1, e: o.0, f: o.1 };
        unsafe {
            let bmp = rgba_bitmap(lib, pw, ph, &crop).ok_or("out of memory for the redaction")?;
            let img = lib.FPDFPageObj_NewImageObj(self.doc.handle);
            let ok = lib.is_true(lib.FPDFImageObj_SetBitmap(ptr::null_mut(), 0, img, bmp));
            lib.FPDFBitmap_Destroy(bmp);
            if !ok {
                lib.FPDFPageObj_Destroy(img);
                return Err("PDFium could not store the redacted picture".into());
            }
            lib.FPDFPageObj_SetMatrix(img, &m);
            lib.FPDFPage_InsertObject(self.handle, img);
        }
        Ok(())
    }

    /// Blackens the pixels of an image object under `areas`. False if the image could
    /// not be read or written (the caller then removes it).
    fn redact_image(&self, obj: FPDF_PAGEOBJECT, areas: &[PdfBox]) -> bool {
        use super::redact::{fill_black, image_pixels};
        let lib = self.lib();
        let Some((w, h, mut rgba)) = self.object_image(obj) else { return false };
        let mut m = FS_MATRIX { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };
        unsafe { lib.FPDFPageObj_GetMatrix(obj, &mut m) };
        let m = [m.a, m.b, m.c, m.d, m.e, m.f];
        for a in areas {
            if let Some(r) = image_pixels(m, w, h, a) {
                fill_black(&mut rgba, w, r);
            }
        }
        unsafe {
            let Some(bmp) = rgba_bitmap(lib, w, h, &rgba) else { return false };
            let ok = lib.is_true(lib.FPDFImageObj_SetBitmap(ptr::null_mut(), 0, obj, bmp));
            lib.FPDFBitmap_Destroy(bmp);
            ok
        }
    }

    /// Removes invisible text objects (an earlier text recognition).
    fn remove_invisible_text(&self) {
        const FPDF_PAGEOBJ_TEXT: c_int = 1;
        const INVISIBLE: FPDF_TEXT_RENDERMODE = 3;
        let lib = self.lib();
        unsafe {
            for i in (0..lib.FPDFPage_CountObjects(self.handle)).rev() {
                let obj = lib.FPDFPage_GetObject(self.handle, i);
                if !obj.is_null()
                    && lib.FPDFPageObj_GetType(obj) == FPDF_PAGEOBJ_TEXT
                    && lib.FPDFTextObj_GetTextRenderMode(obj) == INVISIBLE
                    && lib.is_true(lib.FPDFPage_RemoveObject(self.handle, obj))
                {
                    lib.FPDFPageObj_Destroy(obj);
                }
            }
        }
    }

    fn text_page(&self) -> Option<TextPage<'_>> {
        let handle = unsafe { self.lib().FPDFText_LoadPage(self.handle) };
        (!handle.is_null()).then(|| TextPage { lib: self.lib(), handle, _page: std::marker::PhantomData })
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
                let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
                let ok = unsafe { lib.FPDFText_GetLooseCharBox(tp.handle, i, &mut r) };
                let rect =
                    Some(to_box(&r)).filter(|b| lib.is_true(ok) && b.right - b.left > 0.0 && b.top - b.bottom > 0.0);
                text.chars.push(TextChar { ch, rect });
            }
        }
        let mut pos: c_int = 0;
        let mut link: FPDF_LINK = ptr::null_mut();
        while lib.is_true(unsafe { lib.FPDFLink_Enumerate(self.handle, &mut pos, &mut link) }) {
            let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
            if !lib.is_true(unsafe { lib.FPDFLink_GetAnnotRect(link, &mut r) }) {
                continue;
            }
            let mut target =
                self.doc.dest_page(unsafe { lib.FPDFLink_GetDest(self.doc.handle, link) }).map(LinkTarget::Page);
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
                            let needed =
                                unsafe { lib.FPDFAction_GetURIPath(self.doc.handle, action, ptr::null_mut(), 0) }
                                    as usize;
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
                text.links.push(PageLink { rect: to_box(&r), target });
            }
        }
        text
    }

    /// Search hits (rectangles) with the text around each.
    pub fn search_with_context(&self, query: &SearchQuery, limit: usize) -> Vec<(Vec<PdfBox>, HitContext)> {
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
                        rects.push(PdfBox { left: l as f32, bottom: b as f32, right: r as f32, top: t as f32 });
                    }
                }
                if !rects.is_empty() {
                    const AROUND: c_int = 60;
                    let total = lib.FPDFText_CountChars(tp.handle);
                    let get = |from: c_int, n: c_int| {
                        let n = n.min(total - from).max(0);
                        if n == 0 {
                            return String::new();
                        }
                        let mut buf = vec![0u16; n as usize + 1];
                        let got = lib.FPDFText_GetText(tp.handle, from, n, buf.as_mut_ptr());
                        // Line breaks and runs of spaces read as one space.
                        utf16_buf_to_string(&buf[..(got.max(0) as usize).min(buf.len())])
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    };
                    let from = (start - AROUND).max(0);
                    let mut before = get(from, start - from);
                    let mut after = get(start + count, AROUND);
                    // Whole words only, roughly 40 characters each side.
                    if before.chars().count() > 40 {
                        let cut: String = before.chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
                        before = format!("…{}", cut.split_once(' ').map_or(cut.as_str(), |(_, rest)| rest));
                    }
                    if after.chars().count() > 40 {
                        let cut: String = after.chars().take(40).collect();
                        after = format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head));
                    }
                    let gap = |s: &str| !s.is_empty() && !s.ends_with('…');
                    let context = HitContext {
                        // Keep the space between the context and the match.
                        before: if gap(&before) && lib.FPDFText_GetUnicode(tp.handle, (start - 1).max(0)) == ' ' as u32
                        {
                            before + " "
                        } else {
                            before
                        },
                        text: get(start, count),
                        after: if !after.is_empty() && lib.FPDFText_GetUnicode(tp.handle, start + count) == ' ' as u32 {
                            format!(" {after}")
                        } else {
                            after
                        },
                    };
                    hits.push((rects, context));
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
        (!handle.is_null()).then(|| Annot { lib: self.lib(), handle })
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
        let new = if hidden { flags | FPDF_ANNOT_FLAG_HIDDEN } else { flags & !FPDF_ANNOT_FLAG_HIDDEN };
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
                SUBTYPE_INK => match Shape::parse(&self.annot_string(&a, SHAPE_KEY)) {
                    Some(shape) => AnnotKind::from_shape(shape),
                    None => AnnotKind::Ink,
                },
                _ => AnnotKind::Other,
            };
            let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
            unsafe { lib.FPDFAnnot_GetRect(a.handle, &mut r) };
            let string = |key: &str| {
                read_utf16(|buf, len| unsafe {
                    lib.FPDFAnnot_GetStringValue(a.handle, key, buf as *mut FPDF_WCHAR, len)
                })
            };
            let (mut hr, mut vr, mut w) = (0.0, 0.0, 1.0);
            unsafe { lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut w) };
            let (mut color, mut interior) =
                (self.annot_color(&a, COLORTYPE_COLOR), self.annot_color(&a, COLORTYPE_INTERIOR));
            let box_colors = (kind == AnnotKind::FreeText).then(|| self.box_colors(&a));
            if let Some((border, fill)) = box_colors {
                color = border;
                interior = fill;
            }
            if box_colors.is_none() && (color.is_none() || interior.is_none()) {
                // PDFium hides colours once an annotation has an appearance stream;
                // read them from the drawing instructions instead.
                let ap = read_utf16(|buf, len| unsafe {
                    lib.FPDFAnnot_GetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, buf as *mut FPDF_WCHAR, len)
                });
                let (stroke, fill) = appearance_colors(&ap);
                const FPDF_OBJECT_STRING: FPDF_OBJECT_TYPE = 3;
                // A string /IC is magic-pdf's "no fill" marker.
                let no_fill = unsafe { lib.FPDFAnnot_GetValueType(a.handle, "IC") } == FPDF_OBJECT_STRING;
                if kind.has_interior() {
                    color = color.or(stroke);
                    if !no_fill {
                        interior = interior.or(fill);
                    }
                } else if matches!(kind, AnnotKind::Highlight | AnnotKind::Note) {
                    color = color.or(fill).or(stroke);
                } else {
                    color = color.or(stroke).or(fill);
                }
            }
            out.push(AnnotInfo {
                index,
                kind,
                rect: to_box(&r),
                color,
                interior,
                width: w,
                contents: string("Contents"),
                author: string("T"),
                modified: string("M"),
                font_size: None,
                text_color: None,
                rich: None,
                vertices: if kind.shape().is_some() { shapes::decode_points(&string(POINTS_KEY)) } else { Vec::new() },
                replies: serde_json::from_str(&string(REPLIES_KEY)).unwrap_or_default(),
                status: string(STATUS_KEY),
                other_replies: Vec::new(),
                in_reply_to: self.reply_parent(&a),
            });
            if kind == AnnotKind::FreeText
                && let Some(last) = out.last_mut()
            {
                let rich = self.read_rich(&a);
                let first = rich.first_style(CharStyle::default());
                last.font_size = Some(first.size);
                last.text_color = Some(first.color);
                last.rich = Some(rich);
            }
        }
        // Replies and review states written by other applications join their parent's
        // thread.
        // Replies to replies (how Acrobat stores a conversation) join the thread of the
        // annotation at the start of the chain.
        let root = |mut i: usize| {
            for _ in 0..out.len() {
                match out.iter().position(|p| Some(p.index) == out[i].in_reply_to) {
                    Some(parent) => i = parent,
                    None => break,
                }
            }
            i
        };
        let threads: Vec<(usize, usize)> = out
            .iter()
            .enumerate()
            .filter(|(_, a)| a.in_reply_to.is_some())
            .map(|(i, _)| (i, root(i)))
            .filter(|&(i, r)| i != r)
            .collect();
        for (child, parent) in threads {
            let Some(a) = self.annot(out[child].index) else { continue };
            // magic-pdf's own copies of its replies: already in the parent's list.
            if !self.annot_string(&a, super::lopdf_ops::REPLY_COPY_KEY).is_empty() {
                continue;
            }
            let state = self.annot_string(&a, "State");
            let c = out[child].clone();
            let p = &mut out[parent];
            if !state.is_empty() && self.annot_string(&a, "StateModel") != "Marked" {
                if p.status.is_empty() || p.status == "None" {
                    p.status = state;
                }
            } else {
                p.other_replies.push(Reply { author: c.author, date: c.modified, text: c.contents });
            }
        }
        out
    }

    fn annot_string(&self, a: &Annot, key: &str) -> String {
        let lib = self.lib();
        read_utf16(|buf, len| unsafe { lib.FPDFAnnot_GetStringValue(a.handle, key, buf as *mut FPDF_WCHAR, len) })
    }

    /// Index of the annotation `a` replies to (/IRT), if any.
    fn reply_parent(&self, a: &Annot) -> Option<usize> {
        let lib = self.lib();
        if !lib.is_true(unsafe { lib.FPDFAnnot_HasKey(a.handle, "IRT") }) {
            return None;
        }
        let handle = unsafe { lib.FPDFAnnot_GetLinkedAnnot(a.handle, "IRT") };
        if handle.is_null() {
            return None;
        }
        let parent = Annot { lib, handle };
        let index = unsafe { lib.FPDFPage_GetAnnotIndex(self.handle, parent.handle) };
        usize::try_from(index).ok()
    }

    fn annot_rect(&self, a: &Annot) -> PdfBox {
        let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
        unsafe { self.lib().FPDFAnnot_GetRect(a.handle, &mut r) };
        to_box(&r)
    }

    fn set_rect(&self, a: &Annot, b: PdfBox) {
        let r = FS_RECTF { left: b.left, top: b.top, right: b.right, bottom: b.bottom };
        unsafe { self.lib().FPDFAnnot_SetRect(a.handle, &r) };
    }

    /// Adds an image filling the annotation rectangle (stamps).
    fn append_image(&self, a: &Annot, rect: PdfBox, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
        let lib = self.lib();
        if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
            return Err("invalid image".into());
        }
        unsafe {
            let bmp = lib.FPDFBitmap_Create(width as c_int, height as c_int, 1);
            if bmp.is_null() {
                return Err("out of memory for the image".into());
            }
            let stride = lib.FPDFBitmap_GetStride(bmp) as usize;
            let buf = lib.FPDFBitmap_GetBuffer(bmp) as *mut u8;
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let s = &rgba[(y * width as usize + x) * 4..][..4];
                    let d = buf.add(y * stride + x * 4);
                    *d = s[2];
                    *d.add(1) = s[1];
                    *d.add(2) = s[0];
                    *d.add(3) = s[3];
                }
            }
            let img = lib.FPDFPageObj_NewImageObj(self.doc.handle);
            let ok = lib.is_true(lib.FPDFImageObj_SetBitmap(ptr::null_mut(), 0, img, bmp));
            lib.FPDFBitmap_Destroy(bmp);
            if !ok {
                lib.FPDFPageObj_Destroy(img);
                return Err("PDFium could not store the image".into());
            }
            let (w, h) = ((rect.right - rect.left) as f64, (rect.top - rect.bottom) as f64);
            let m = FS_MATRIX { a: w as f32, b: 0.0, c: 0.0, d: h as f32, e: rect.left, f: rect.bottom };
            lib.FPDFPageObj_SetMatrix(img, &m);
            if !lib.is_true(lib.FPDFAnnot_AppendObject(a.handle, img)) {
                lib.FPDFPageObj_Destroy(img);
                return Err("PDFium could not add the image to the stamp".into());
            }
        }
        Ok(())
    }

    /// The first image of a stamp as RGBA pixels.
    fn stamp_image(&self, a: &Annot) -> Option<(u32, u32, Vec<u8>)> {
        const FPDF_PAGEOBJ_IMAGE: c_int = 3;
        let lib = self.lib();
        unsafe {
            let n = lib.FPDFAnnot_GetObjectCount(a.handle);
            let obj = (0..n)
                .map(|i| lib.FPDFAnnot_GetObject(a.handle, i))
                .find(|o| !o.is_null() && lib.FPDFPageObj_GetType(*o) == FPDF_PAGEOBJ_IMAGE)?;
            // The rendered image includes its transparency mask.
            self.object_image(obj)
        }
    }

    /// Everything needed to recreate an annotation elsewhere (copy and paste).
    pub fn annot_clip(&self, index: usize) -> Result<NewAnnot, String> {
        let lib = self.lib();
        let info = self.annots(|i| i != index).into_iter().next().ok_or("this annotation cannot be copied")?;
        let a = self.annot(index).ok_or("annotation not found")?;
        if matches!(info.kind, AnnotKind::Other | AnnotKind::Line) {
            return Err(format!("{} annotations cannot be copied", info.kind.label()));
        }
        let mut n = NewAnnot::new(info.kind, info.rect, info.color.unwrap_or([0, 0, 0]));
        n.interior = info.interior;
        n.width = info.width;
        n.contents = info.contents.clone();
        // The copy (or the annotation moved to another page) keeps its author.
        n.author = info.author.clone();
        n.vertices = info.vertices.clone();
        n.status = info.status.clone();
        n.replies = serde_json::from_str(&self.annot_string(&a, REPLIES_KEY)).unwrap_or_default();
        let measure = self.annot_string(&a, MEASURE_KEY);
        n.measure = (!measure.is_empty()).then_some(measure);
        if let Some(rich) = info.rich {
            n.font_size = info.font_size.unwrap_or(12.0);
            n.text_color = info.text_color.unwrap_or([0, 0, 0]);
            n.rich = Some(rich);
        }
        unsafe {
            for i in 0..lib.FPDFAnnot_CountAttachmentPoints(a.handle) {
                let mut q = FS_QUADPOINTSF { x1: 0.0, y1: 0.0, x2: 0.0, y2: 0.0, x3: 0.0, y3: 0.0, x4: 0.0, y4: 0.0 };
                if lib.is_true(lib.FPDFAnnot_GetAttachmentPoints(a.handle, i, &mut q)) {
                    n.quads.push([q.x1, q.y1, q.x2, q.y2, q.x3, q.y3, q.x4, q.y4]);
                }
            }
            if info.kind == AnnotKind::Ink {
                for i in 0..lib.FPDFAnnot_GetInkListCount(a.handle) {
                    let len = lib.FPDFAnnot_GetInkListPath(a.handle, i, ptr::null_mut(), 0);
                    let mut pts = vec![FS_POINTF { x: 0.0, y: 0.0 }; len as usize];
                    lib.FPDFAnnot_GetInkListPath(a.handle, i, pts.as_mut_ptr(), len);
                    n.ink.push(pts.iter().map(|p| (p.x, p.y)).collect());
                }
            }
        }
        if info.kind == AnnotKind::Stamp {
            let text = self.annot_string(&a, STAMP_KEY);
            n.stamp = if !text.is_empty() {
                let (title, subtitle) = text.split_once('\n').unwrap_or((&text, ""));
                Some(StampSpec::Text { title: title.to_string(), subtitle: subtitle.to_string() })
            } else {
                let (width, height, rgba) = self.stamp_image(&a).ok_or("this stamp cannot be copied")?;
                Some(StampSpec::Image { width, height, rgba })
            };
        }
        Ok(n)
    }

    fn annot_color(&self, a: &Annot, kind: FPDFANNOT_COLORTYPE) -> Option<Rgb> {
        // "No fill" is stored as a non-array /IC (there is no API to remove the key);
        // only a real colour array counts.
        const FPDF_OBJECT_ARRAY: FPDF_OBJECT_TYPE = 5;
        let key = if kind == COLORTYPE_INTERIOR { "IC" } else { "C" };
        if unsafe { self.lib().FPDFAnnot_GetValueType(a.handle, key) } != FPDF_OBJECT_ARRAY {
            return None;
        }
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
            AnnotKind::FreeText => SUBTYPE_FREETEXT,
            AnnotKind::Stamp => SUBTYPE_STAMP,
            k if k.shape().is_some() => SUBTYPE_INK,
            other => return Err(format!("creating {} annotations is not supported", other.label())),
        };
        let handle = unsafe { lib.FPDFPage_CreateAnnot(self.handle, subtype) };
        if handle.is_null() {
            return Err("PDFium could not create the annotation".into());
        }
        let a = Annot { lib, handle };
        let index = unsafe { lib.FPDFPage_GetAnnotIndex(self.handle, a.handle) };
        unsafe {
            let rect = FS_RECTF { left: n.rect.left, top: n.rect.top, right: n.rect.right, bottom: n.rect.bottom };
            lib.FPDFAnnot_SetRect(a.handle, &rect);
            lib.FPDFAnnot_SetFlags(a.handle, FPDF_ANNOT_FLAG_PRINT);
            if n.kind != AnnotKind::FreeText {
                let [r, g, b] = n.color;
                lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_COLOR, r.into(), g.into(), b.into(), 255);
                if let Some([r, g, b]) = n.interior {
                    lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_INTERIOR, r.into(), g.into(), b.into(), 255);
                }
            }
            if n.kind.has_border_width() {
                lib.FPDFAnnot_SetBorder(a.handle, 0.0, 0.0, n.width);
            }
            for q in &n.quads {
                let qp =
                    FS_QUADPOINTSF { x1: q[0], y1: q[1], x2: q[2], y2: q[3], x3: q[4], y3: q[5], x4: q[6], y4: q[7] };
                lib.FPDFAnnot_AppendAttachmentPoints(a.handle, &qp);
            }
            let shape = n.kind.shape();
            let shape_ink = shape.map(|s| shapes::ink_strokes(s, &n.vertices, n.width));
            for stroke in shape_ink.as_ref().unwrap_or(&n.ink) {
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
            if n.kind == AnnotKind::FreeText {
                let style = CharStyle { size: n.font_size, color: n.text_color, ..Default::default() };
                let rich = n.rich.clone().unwrap_or_else(|| RichText::plain(&n.contents, style));
                self.write_rich(&a, &rich);
                self.write_box_colors(&a, n.color, n.interior, rich.first_style(style).size);
            }
            if let Some(shape) = shape {
                lib.FPDFAnnot_SetStringValue_str(a.handle, SHAPE_KEY, shape.name());
                lib.FPDFAnnot_SetStringValue_str(a.handle, POINTS_KEY, &shapes::encode_points(&n.vertices));
                if let Some(m) = &n.measure {
                    lib.FPDFAnnot_SetStringValue_str(a.handle, MEASURE_KEY, m);
                }
            }
            if !n.replies.is_empty() {
                let json = serde_json::to_string(&n.replies).unwrap_or_default();
                lib.FPDFAnnot_SetStringValue_str(a.handle, REPLIES_KEY, &json);
            }
            if !n.status.is_empty() {
                lib.FPDFAnnot_SetStringValue_str(a.handle, STATUS_KEY, &n.status);
            }
            match &n.stamp {
                Some(StampSpec::Text { title, subtitle }) => {
                    lib.FPDFAnnot_SetStringValue_str(a.handle, STAMP_KEY, &format!("{title}\n{subtitle}"));
                    lib.FPDFAnnot_SetStringValue_str(a.handle, "Name", "magic-pdf");
                    if n.contents.is_empty() {
                        lib.FPDFAnnot_SetStringValue_str(a.handle, "Contents", title);
                    }
                }
                Some(StampSpec::Image { width, height, rgba }) => {
                    lib.FPDFAnnot_SetStringValue_str(a.handle, "Name", "magic-pdf-image");
                    if let Err(e) = self.append_image(&a, n.rect, *width, *height, rgba) {
                        lib.FPDFPage_RemoveAnnot(self.handle, index);
                        return Err(e);
                    }
                }
                None => {}
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
        let restyle = change.color.is_some()
            || change.rect.is_some()
            || change.transform.is_some()
            || change.interior.is_some()
            || change.width.is_some()
            || change.font_size.is_some()
            || change.text_color.is_some()
            || change.rich.is_some();
        let subtype = unsafe { lib.FPDFAnnot_GetSubtype(a.handle) };
        // Stamps other than magic-pdf's text stamps keep their appearance: it is scaled
        // to the new rectangle when moved or resized (images cannot be redrawn).
        let keep_ap = subtype == SUBTYPE_STAMP && self.annot_string(&a, STAMP_KEY).is_empty();
        let shape = Shape::parse(&self.annot_string(&a, SHAPE_KEY)).filter(|_| subtype == SUBTYPE_INK);
        unsafe {
            if restyle && !keep_ap {
                // PDFium refuses colour access while an appearance stream exists. Drop it;
                // PDFium regenerates it from the new style when the page is rendered.
                lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null());
            }
            let is_box = lib.FPDFAnnot_GetSubtype(a.handle) == SUBTYPE_FREETEXT;
            if is_box && (change.color.is_some() || change.interior.is_some()) {
                let (border, fill) = self.box_colors(&a);
                let border = border.unwrap_or([0, 0, 0]);
                if change.color.is_some() {
                    undo.color = Some(border);
                }
                if change.interior.is_some() {
                    undo.interior = Some(fill);
                }
                let size = self.read_rich(&a).first_style(CharStyle::default()).size;
                self.write_box_colors(&a, change.color.unwrap_or(border), change.interior.unwrap_or(fill), size);
            }
            if let Some([r, g, b]) = change.color.filter(|_| !is_box) {
                undo.color = Some(self.annot_color(&a, COLORTYPE_COLOR).unwrap_or([0, 0, 0]));
                lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_COLOR, r.into(), g.into(), b.into(), 255);
            }
            if let Some(interior) = change.interior.filter(|_| !is_box) {
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
            let text_change = change.rich.is_some()
                || change.font_size.is_some()
                || change.text_color.is_some()
                || (is_box && change.contents.is_some());
            if is_box && text_change {
                // A text box's text and styles change together (and undo together).
                let old = self.read_rich(&a);
                let mut new = change.rich.clone().unwrap_or_else(|| match &change.contents {
                    Some(text) => RichText::plain(text, old.first_style(CharStyle::default())),
                    None => old.clone(),
                });
                textbox::restyle_whole_box(&mut new, change.font_size, change.text_color);
                self.write_rich(&a, &new);
                undo.rich = Some(old);
            }
            if change.rect.is_some() || change.transform.is_some() {
                let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
                lib.FPDFAnnot_GetRect(a.handle, &mut r);
                let current = to_box(&r);
                // A freehand drawing's frame is its strokes plus half the line width:
                // fit the strokes into the new frame minus that margin, so the frame
                // ends up exactly where it was dragged.
                let margin = if subtype == SUBTYPE_INK && shape.is_none() {
                    let (mut hr, mut vr, mut w) = (0.0, 0.0, 1.0);
                    lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut w);
                    (w / 2.0).max(0.5)
                } else {
                    0.0
                };
                let inner = |b: PdfBox| b.expand(-margin);
                let usable = |b: &PdfBox| b.right - b.left > 0.01 && b.top - b.bottom > 0.01;
                let t = match (change.transform, change.rect) {
                    (Some(t), _) => t,
                    (None, Some(new)) if margin > 0.0 && usable(&inner(current)) && usable(&inner(new)) => {
                        rect_transform(inner(current), inner(new))
                    }
                    (None, Some(new)) => rect_transform(current, new),
                    (None, None) => unreachable!(),
                };
                undo.transform = Some(invert_transform(t));
                if subtype == SUBTYPE_INK {
                    self.transform_ink(&a, t);
                }
                if shape.is_some() {
                    let pts: Vec<(f32, f32)> = shapes::decode_points(&string(POINTS_KEY))
                        .into_iter()
                        .map(|(x, y)| (t[0] * x + t[2], t[1] * y + t[3]))
                        .collect();
                    lib.FPDFAnnot_SetStringValue_str(a.handle, POINTS_KEY, &shapes::encode_points(&pts));
                }
                let new = apply_transform_box(t, current);
                let nr = FS_RECTF { left: new.left, top: new.top, right: new.right, bottom: new.bottom };
                lib.FPDFAnnot_SetRect(a.handle, &nr);
            }
            if let Some(replies) = &change.replies {
                undo.replies = Some(serde_json::from_str(&string(REPLIES_KEY)).unwrap_or_default());
                let json =
                    if replies.is_empty() { String::new() } else { serde_json::to_string(replies).unwrap_or_default() };
                lib.FPDFAnnot_SetStringValue_str(a.handle, REPLIES_KEY, &json);
            }
            if let Some(status) = &change.status {
                undo.status = Some(string(STATUS_KEY));
                lib.FPDFAnnot_SetStringValue_str(a.handle, STATUS_KEY, status);
            }
            if let Some(text) = change.contents.as_ref().filter(|_| !is_box) {
                undo.contents = Some(string("Contents"));
                lib.FPDFAnnot_SetStringValue_str(a.handle, "Contents", text);
            }
            lib.FPDFAnnot_SetStringValue_str(a.handle, "M", &pdf_date_now());
        }
        Ok(undo)
    }

    /// An ink annotation's strokes.
    fn ink_paths(&self, a: &Annot) -> Vec<Vec<(f32, f32)>> {
        let lib = self.lib();
        unsafe {
            (0..lib.FPDFAnnot_GetInkListCount(a.handle))
                .map(|i| {
                    let len = lib.FPDFAnnot_GetInkListPath(a.handle, i, ptr::null_mut(), 0);
                    let mut pts = vec![FS_POINTF { x: 0.0, y: 0.0 }; len as usize];
                    lib.FPDFAnnot_GetInkListPath(a.handle, i, pts.as_mut_ptr(), len);
                    pts.into_iter().map(|p| (p.x, p.y)).collect()
                })
                .collect()
        }
    }

    /// Applies a transform to an ink annotation's strokes.
    fn transform_ink(&self, a: &Annot, t: [f32; 4]) {
        let lib = self.lib();
        unsafe {
            let n = lib.FPDFAnnot_GetInkListCount(a.handle);
            let mut strokes = Vec::new();
            for i in 0..n {
                let len = lib.FPDFAnnot_GetInkListPath(a.handle, i, ptr::null_mut(), 0);
                let mut pts = vec![FS_POINTF { x: 0.0, y: 0.0 }; len as usize];
                lib.FPDFAnnot_GetInkListPath(a.handle, i, pts.as_mut_ptr(), len);
                for p in &mut pts {
                    p.x = t[0] * p.x + t[2];
                    p.y = t[1] * p.y + t[3];
                }
                strokes.push(pts);
            }
            lib.FPDFAnnot_RemoveInkList(a.handle);
            for pts in strokes {
                lib.FPDFAnnot_AddInkStroke(a.handle, pts.as_ptr(), pts.len() as _);
            }
        }
    }

    /// Draws the appearance of annotations magic-pdf renders itself (text boxes,
    /// shapes, text stamps) after they were created or changed.
    pub fn refresh_appearance(&self, index: usize, fonts: &dyn FontSet) -> Result<(), String> {
        let lib = self.lib();
        let a = self.annot(index).ok_or("annotation not found")?;
        let subtype = unsafe { lib.FPDFAnnot_GetSubtype(a.handle) };
        let set_ap = |content: &str| -> Result<(), String> {
            let content = wide(content);
            let ok = unsafe { lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, content.as_ptr()) };
            if lib.is_true(ok) { Ok(()) } else { Err("PDFium could not set the appearance".into()) }
        };
        match subtype {
            SUBTYPE_FREETEXT => {
                drop(a);
                self.refresh_text_box(index, fonts)
            }
            SUBTYPE_INK => {
                let Some(shape) = Shape::parse(&self.annot_string(&a, SHAPE_KEY)) else {
                    // A freehand drawing.
                    unsafe { lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null()) };
                    let (mut hr, mut vr, mut width) = (0.0, 0.0, 1.0);
                    unsafe { lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut width) };
                    let color = self.annot_color(&a, COLORTYPE_COLOR).unwrap_or([0, 0, 0]);
                    let Some((rect, content)) = shapes::freehand_appearance(&self.ink_paths(&a), color, width) else {
                        return Ok(());
                    };
                    self.set_rect(&a, rect);
                    return set_ap(&content);
                };
                unsafe { lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null()) };
                let (mut hr, mut vr, mut width) = (0.0, 0.0, 1.0);
                unsafe { lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut width) };
                let color = self.annot_color(&a, COLORTYPE_COLOR).unwrap_or([0, 0, 0]);
                let fill = if shape.closed() { self.annot_color(&a, COLORTYPE_INTERIOR) } else { None };
                let pts = shapes::decode_points(&self.annot_string(&a, POINTS_KEY));
                let (rect, content) = match shapes::Scale::decode(&self.annot_string(&a, MEASURE_KEY)) {
                    Some(scale) if matches!(shape, Shape::Distance | Shape::Area) => {
                        let text = shapes::measure_label(shape, &pts, &scale);
                        // The measurement is also the annotation's comment.
                        unsafe { lib.FPDFAnnot_SetStringValue_str(a.handle, "Contents", &text) };
                        shapes::measure_appearance(shape, &pts, color, width, fill, &text, fonts)
                    }
                    _ => shapes::appearance(shape, &pts, color, width, fill),
                };
                self.set_rect(&a, rect);
                set_ap(&content)
            }
            SUBTYPE_TEXT => {
                // PDFium's own note icon is always yellow; draw it in the note's colour.
                unsafe { lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null()) };
                let color = self.annot_color(&a, COLORTYPE_COLOR).unwrap_or([255, 210, 0]);
                set_ap(&shapes::note_appearance(self.annot_rect(&a), color))
            }
            SUBTYPE_STAMP => {
                let text = self.annot_string(&a, STAMP_KEY);
                if text.is_empty() {
                    return Ok(());
                }
                unsafe { lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null()) };
                let color = self.annot_color(&a, COLORTYPE_COLOR).unwrap_or([200, 0, 0]);
                let (title, subtitle) = text.split_once('\n').unwrap_or((&text, ""));
                set_ap(&shapes::stamp_appearance(self.annot_rect(&a), title, subtitle, color, fonts))
            }
            _ => Ok(()),
        }
    }

    /// Re-lays out a text box from its stored text and style and sets its appearance
    /// (the box grows or shrinks in height to fit the text).
    pub fn refresh_text_box(&self, index: usize, fonts: &dyn FontSet) -> Result<(), String> {
        let lib = self.lib();
        let a = self.annot(index).ok_or("annotation not found")?;
        unsafe {
            // Without an appearance PDFium lets us read the colours.
            lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, ptr::null());
            let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
            lib.FPDFAnnot_GetRect(a.handle, &mut r);
            let (mut hr, mut vr, mut width) = (0.0, 0.0, 1.0);
            lib.FPDFAnnot_GetBorder(a.handle, &mut hr, &mut vr, &mut width);
            let (border, fill) = self.box_colors(&a);
            let style = BoxStyle { border: (border.unwrap_or([0, 0, 0]), width), fill };
            let rich = self.read_rich(&a);
            // Older magic-pdf boxes switch to the convention other applications use.
            self.write_box_colors(&a, style.border.0, fill, rich.first_style(CharStyle::default()).size);
            let (rect, content) = textbox::appearance(to_box(&r), &rich, &style, fonts);
            let new = FS_RECTF { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom };
            lib.FPDFAnnot_SetRect(a.handle, &new);
            let content = wide(&content);
            if !lib.is_true(lib.FPDFAnnot_SetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, content.as_ptr())) {
                return Err("PDFium could not set the text box appearance".into());
            }
        }
        Ok(())
    }

    /// A text box's styled text: from /RC if present, else /Contents with the /DA style.
    fn read_rich(&self, a: &Annot) -> RichText {
        let lib = self.lib();
        let string = |key: &str| {
            read_utf16(|buf, len| unsafe { lib.FPDFAnnot_GetStringValue(a.handle, key, buf as *mut FPDF_WCHAR, len) })
        };
        let (size, da_color) = textbox::parse_default_appearance(&string("DA"));
        // The text colour is in /DS; /DA holds it only for boxes without /DS (in the
        // Acrobat convention its colour is the border's).
        let color = textbox::parse_default_style_color(&string("DS")).or(da_color);
        let default = CharStyle { size: size.unwrap_or(12.0), color: color.unwrap_or([0, 0, 0]), ..Default::default() };
        // Rich text without any characters (a new, still empty box) carries no style;
        // the box's default style (/DA) then decides, so typing starts in the size and
        // colour chosen for the box.
        let rc = string("RC");
        RichText::parse_xhtml(&rc, default)
            .filter(|r| !r.spans.is_empty())
            .unwrap_or_else(|| RichText::plain(&string("Contents"), default))
    }

    /// Stores styled text: plain text, rich text and default styles together.
    fn write_rich(&self, a: &Annot, rich: &RichText) {
        let lib = self.lib();
        let first = rich.first_style(CharStyle::default());
        unsafe {
            lib.FPDFAnnot_SetStringValue_str(a.handle, "Contents", &rich.text());
            lib.FPDFAnnot_SetStringValue_str(a.handle, "RC", &rich.to_xhtml());
            lib.FPDFAnnot_SetStringValue_str(a.handle, "DS", &rich.default_style_string());
            // /DA's colour is the border's; keep it, update the size.
            let (_, border) = textbox::parse_default_appearance(&self.annot_string(a, "DA"));
            lib.FPDFAnnot_SetStringValue_str(
                a.handle,
                "DA",
                &textbox::default_appearance(first.size, border.unwrap_or([0, 0, 0])),
            );
        }
    }

    /// A text box's border and fill colour, from either colour convention: Acrobat's
    /// (/C fill, /DA border; what other applications write and magic-pdf now writes) or
    /// the one older magic-pdf versions wrote (/C border, /IC fill).
    fn box_colors(&self, a: &Annot) -> (Option<Rgb>, Option<Rgb>) {
        const FPDF_OBJECT_ARRAY: FPDF_OBJECT_TYPE = 5;
        let lib = self.lib();
        let legacy = self.annot_string(a, BOX_STYLE_KEY).is_empty()
            && textbox::is_legacy_magic_style(&self.annot_string(a, "DS"));
        let ap = || {
            read_utf16(|buf, len| unsafe {
                lib.FPDFAnnot_GetAP(a.handle, FPDF_ANNOT_APPEARANCEMODE_NORMAL, buf as *mut FPDF_WCHAR, len)
            })
        };
        if legacy {
            let (stroke, fill) = appearance_colors(&ap());
            let fill = if unsafe { lib.FPDFAnnot_GetValueType(a.handle, "IC") } == FPDF_OBJECT_ARRAY {
                self.annot_color(a, COLORTYPE_INTERIOR).or(fill)
            } else {
                None
            };
            return (self.annot_color(a, COLORTYPE_COLOR).or(stroke), fill);
        }
        let (_, border) = textbox::parse_default_appearance(&self.annot_string(a, "DA"));
        // Only a /C array is a fill. PDFium does not hand out colours while an
        // appearance exists; the background is then the first colour it fills with.
        let fill = if unsafe { lib.FPDFAnnot_GetValueType(a.handle, "C") } == FPDF_OBJECT_ARRAY {
            self.annot_color(a, COLORTYPE_COLOR).or_else(|| textbox::rectangle_fill(&ap()))
        } else {
            None
        };
        (border.or(Some([0, 0, 0])), fill)
    }

    /// Stores a text box's colours the way Acrobat and other applications read them:
    /// /C the fill (none: not an array), /DA the border colour (and default font size).
    fn write_box_colors(&self, a: &Annot, border: Rgb, fill: Option<Rgb>, size: f32) {
        let lib = self.lib();
        unsafe {
            match fill {
                Some([r, g, b]) => {
                    lib.FPDFAnnot_SetColor(a.handle, COLORTYPE_COLOR, r.into(), g.into(), b.into(), 255);
                }
                // There is no API to remove a key; a non-array /C is no colour.
                None => {
                    lib.FPDFAnnot_SetStringValue_str(a.handle, "C", "");
                }
            }
            // An old magic-pdf fill must not be read as one any more.
            if lib.FPDFAnnot_GetValueType(a.handle, "IC") != 0 {
                lib.FPDFAnnot_SetStringValue_str(a.handle, "IC", "");
            }
            lib.FPDFAnnot_SetStringValue_str(a.handle, "DA", &textbox::default_appearance(size, border));
            lib.FPDFAnnot_SetStringValue_str(a.handle, BOX_STYLE_KEY, "1");
        }
    }

    /// Forwards a mouse event to the form-fill module. Returns true if a field handled it.
    pub fn form_mouse(&self, event: FormMouse, x: f32, y: f32, modifiers: i32) -> bool {
        let (lib, form) = (self.lib(), self.doc.form);
        if form.is_null() {
            return false;
        }
        let (x, y) = (x as f64, y as f64);
        let ok = unsafe {
            match event {
                FormMouse::Move => lib.FORM_OnMouseMove(form, self.handle, modifiers, x, y),
                FormMouse::Down => lib.FORM_OnLButtonDown(form, self.handle, modifiers, x, y),
                FormMouse::Up => lib.FORM_OnLButtonUp(form, self.handle, modifiers, x, y),
                FormMouse::DoubleClick => lib.FORM_OnLButtonDoubleClick(form, self.handle, modifiers, x, y),
            }
        };
        lib.is_true(ok)
    }

    pub fn form_key(&self, key: FormKey, modifiers: i32) {
        let (lib, form) = (self.lib(), self.doc.form);
        if form.is_null() {
            return;
        }
        unsafe {
            match key {
                FormKey::Down(code) => {
                    lib.FORM_OnKeyDown(form, self.handle, code as c_int, modifiers);
                }
                FormKey::Char(c) => {
                    let mut buf = [0u16; 2];
                    if let Some(ch) = char::from_u32(c) {
                        for unit in ch.encode_utf16(&mut buf) {
                            lib.FORM_OnChar(form, self.handle, *unit as c_int, modifiers);
                        }
                    }
                }
            }
        }
    }

    pub fn form_selected_text(&self) -> String {
        let (lib, form) = (self.lib(), self.doc.form);
        if form.is_null() {
            return String::new();
        }
        read_utf16(|buf, len| unsafe { lib.FORM_GetSelectedText(form, self.handle, buf, len) })
    }

    pub fn form_replace_selection(&self, text: &str) {
        let (lib, form) = (self.lib(), self.doc.form);
        if !form.is_null() {
            let w = wide(text);
            unsafe { lib.FORM_ReplaceSelection(form, self.handle, w.as_ptr()) };
        }
    }

    pub fn form_select_all(&self) {
        let (lib, form) = (self.lib(), self.doc.form);
        if !form.is_null() {
            unsafe { lib.FORM_SelectAllText(form, self.handle) };
        }
    }

    pub fn form_undo(&self, redo: bool) {
        let (lib, form) = (self.lib(), self.doc.form);
        if !form.is_null() {
            unsafe {
                if redo {
                    lib.FORM_Redo(form, self.handle);
                } else {
                    lib.FORM_Undo(form, self.handle);
                }
            }
        }
    }

    /// Form field widgets on this page.
    pub fn widgets(&self) -> Vec<WidgetInfo> {
        let (lib, form) = (self.lib(), self.doc.form);
        let mut out = Vec::new();
        if form.is_null() {
            return out;
        }
        for index in 0..self.annot_count() {
            let Some(a) = self.annot(index) else { continue };
            if unsafe { lib.FPDFAnnot_GetSubtype(a.handle) } != SUBTYPE_WIDGET {
                continue;
            }
            let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
            unsafe { lib.FPDFAnnot_GetRect(a.handle, &mut r) };
            let field_type = unsafe { lib.FPDFAnnot_GetFormFieldType(form, a.handle) };
            let name = read_utf16(|buf, len| unsafe {
                lib.FPDFAnnot_GetFormFieldName(form, a.handle, buf as *mut FPDF_WCHAR, len)
            });
            let value = read_utf16(|buf, len| unsafe {
                lib.FPDFAnnot_GetFormFieldValue(form, a.handle, buf as *mut FPDF_WCHAR, len)
            });
            let checked = lib.is_true(unsafe { lib.FPDFAnnot_IsChecked(form, a.handle) });
            out.push(WidgetInfo { rect: to_box(&r), field_type, name, value, checked });
        }
        out
    }

    /// Permanently removes annotations (and their popups) by index.
    pub fn remove_annots(&self, indices: &[usize]) {
        let lib = self.lib();
        // Replies (/IRT) to removed annotations, and replies to those, go too: left
        // behind they would point at nothing and show as stray notes elsewhere.
        let mut targets: Vec<usize> = indices.to_vec();
        let count = unsafe { lib.FPDFPage_GetAnnotCount(self.handle) }.max(0) as usize;
        loop {
            let more: Vec<usize> = (0..count)
                .filter(|i| !targets.contains(i))
                .filter(|&i| self.annot(i).and_then(|a| self.reply_parent(&a)).is_some_and(|p| targets.contains(&p)))
                .collect();
            if more.is_empty() {
                break;
            }
            targets.extend(more);
        }
        let mut all: Vec<usize> = Vec::new();
        for &i in &targets {
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

/// The four faces of the text box font family.
pub struct FontFamily {
    regular: PdfiumFont,
    bold: PdfiumFont,
    italic: PdfiumFont,
    bold_italic: PdfiumFont,
}

impl FontFamily {
    pub fn load_noto_sans(lib: Lib) -> Result<FontFamily, String> {
        Ok(FontFamily {
            regular: PdfiumFont::load(lib, crate::fonts::REGULAR)?,
            bold: PdfiumFont::load(lib, crate::fonts::BOLD)?,
            italic: PdfiumFont::load(lib, crate::fonts::ITALIC)?,
            bold_italic: PdfiumFont::load(lib, crate::fonts::BOLD_ITALIC)?,
        })
    }
}

impl FontSet for FontFamily {
    fn font(&self, bold: bool, italic: bool) -> &dyn GlyphSource {
        match (bold, italic) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        }
    }
}

/// Glyph metrics and outlines from a TrueType font loaded into a scratch document (so
/// nothing is added to the user's documents).
pub struct PdfiumFont {
    lib: Lib,
    _doc: Doc,
    font: FPDF_FONT,
    advances: RefCell<std::collections::HashMap<char, f32>>,
    ascent: f32,
}

impl PdfiumFont {
    pub fn load(lib: Lib, ttf: &[u8]) -> Result<PdfiumFont, String> {
        const FPDF_FONT_TRUETYPE: c_int = 2;
        let doc = Doc::create(lib)?;
        let font = unsafe { lib.FPDFText_LoadFont(doc.handle, ttf.as_ptr(), ttf.len() as _, FPDF_FONT_TRUETYPE, 1) };
        if font.is_null() {
            return Err("PDFium could not load the text box font".into());
        }
        let mut ascent = 0.8;
        unsafe { lib.FPDFFont_GetAscent(font, 1.0, &mut ascent) };
        Ok(PdfiumFont { lib, _doc: doc, font, advances: RefCell::new(std::collections::HashMap::new()), ascent })
    }
}

impl Drop for PdfiumFont {
    fn drop(&mut self) {
        unsafe { self.lib.FPDFFont_Close(self.font) };
    }
}

impl GlyphSource for PdfiumFont {
    fn advance(&self, ch: char) -> f32 {
        *self.advances.borrow_mut().entry(ch).or_insert_with(|| {
            let mut w = 0.0;
            let ok = unsafe { self.lib.FPDFFont_GetGlyphWidth(self.font, ch as u32, 1.0, &mut w) };
            if self.lib.is_true(ok) { w } else { 0.5 }
        })
    }

    fn ascent(&self) -> f32 {
        self.ascent
    }

    fn outline(&self, ch: char, size: f32, x: f32, y: f32, out: &mut String) {
        use std::fmt::Write as _;
        const SEGMENT_LINETO: c_int = 0;
        const SEGMENT_BEZIERTO: c_int = 1;
        const SEGMENT_MOVETO: c_int = 2;
        let lib = self.lib;
        unsafe {
            // PDFium returns outlines in em units (whatever size is asked for), so
            // request size 1 and scale ourselves.
            let path = lib.FPDFFont_GetGlyphPath(self.font, ch as u32, 1.0);
            if path.is_null() {
                return;
            }
            let mut bezier: Vec<(f32, f32)> = Vec::new();
            for i in 0..lib.FPDFGlyphPath_CountGlyphSegments(path) {
                let seg = lib.FPDFGlyphPath_GetGlyphPathSegment(path, i);
                let (mut px, mut py) = (0.0, 0.0);
                lib.FPDFPathSegment_GetPoint(seg, &mut px, &mut py);
                let (px, py) = (x + px * size, y + py * size);
                match lib.FPDFPathSegment_GetType(seg) {
                    SEGMENT_MOVETO => {
                        let _ = writeln!(out, "{px:.2} {py:.2} m");
                    }
                    SEGMENT_LINETO => {
                        let _ = writeln!(out, "{px:.2} {py:.2} l");
                    }
                    SEGMENT_BEZIERTO => {
                        bezier.push((px, py));
                        if bezier.len() == 3 {
                            let [(x1, y1), (x2, y2), (x3, y3)] = [bezier[0], bezier[1], bezier[2]];
                            let _ = writeln!(out, "{x1:.2} {y1:.2} {x2:.2} {y2:.2} {x3:.2} {y3:.2} c");
                            bezier.clear();
                        }
                    }
                    _ => {}
                }
                if lib.is_true(lib.FPDFPathSegment_GetClose(seg)) {
                    out.push_str("h\n");
                }
            }
        }
    }
}

/// The transform `[sx, sy, tx, ty]` mapping rectangle `from` onto `to`.
pub fn rect_transform(from: PdfBox, to: PdfBox) -> [f32; 4] {
    let scale = |f0: f32, f1: f32, t0: f32, t1: f32| {
        if (f1 - f0).abs() < 1e-3 { 1.0 } else { (t1 - t0) / (f1 - f0) }
    };
    let sx = scale(from.left, from.right, to.left, to.right);
    let sy = scale(from.bottom, from.top, to.bottom, to.top);
    [sx, sy, to.left - sx * from.left, to.bottom - sy * from.bottom]
}

pub fn invert_transform([sx, sy, tx, ty]: [f32; 4]) -> [f32; 4] {
    [1.0 / sx, 1.0 / sy, -tx / sx, -ty / sy]
}

pub fn apply_transform_box([sx, sy, tx, ty]: [f32; 4], b: PdfBox) -> PdfBox {
    let (x0, x1) = (sx * b.left + tx, sx * b.right + tx);
    let (y0, y1) = (sy * b.bottom + ty, sy * b.top + ty);
    PdfBox { left: x0.min(x1), bottom: y0.min(y1), right: x0.max(x1), top: y0.max(y1) }
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    #[test]
    fn transforms_map_rectangles_and_invert_exactly() {
        let a = PdfBox { left: 10.0, bottom: 20.0, right: 110.0, top: 70.0 };
        let b = PdfBox { left: 300.0, bottom: 400.0, right: 500.0, top: 425.0 };
        let t = rect_transform(a, b);
        assert_eq!(apply_transform_box(t, a), b);
        let back = apply_transform_box(invert_transform(t), b);
        for (x, y) in [(back.left, a.left), (back.bottom, a.bottom), (back.right, a.right), (back.top, a.top)] {
            assert!((x - y).abs() < 1e-3);
        }
    }
}

/// Stroke and fill colours used for drawing in a content stream (the first of each
/// that is actually painted with). Understands RG/rg, G/g and K/k colour operators.
pub fn appearance_colors(content: &str) -> (Option<Rgb>, Option<Rgb>) {
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut nums: Vec<f32> = Vec::new();
    let (mut stroke, mut fill): (Option<Rgb>, Option<Rgb>) = (Some([0, 0, 0]), Some([0, 0, 0]));
    let (mut used_stroke, mut used_fill) = (None, None);
    for tok in content.split_ascii_whitespace() {
        if let Ok(n) = tok.parse::<f32>() {
            nums.push(n);
            continue;
        }
        let last = |k: usize| (nums.len() >= k).then(|| nums[nums.len() - k..].to_vec());
        let cmyk = |v: &[f32]| [0, 1, 2].map(|i| to_u8((1.0 - v[i]) * (1.0 - v[3])));
        match tok {
            "RG" => stroke = last(3).map(|v| [to_u8(v[0]), to_u8(v[1]), to_u8(v[2])]),
            "rg" => fill = last(3).map(|v| [to_u8(v[0]), to_u8(v[1]), to_u8(v[2])]),
            "G" => stroke = last(1).map(|v| [to_u8(v[0]); 3]),
            "g" => fill = last(1).map(|v| [to_u8(v[0]); 3]),
            "K" => stroke = last(4).map(|v| cmyk(&v)),
            "k" => fill = last(4).map(|v| cmyk(&v)),
            "S" | "s" => used_stroke = used_stroke.or(stroke),
            "f" | "F" | "f*" => used_fill = used_fill.or(fill),
            "B" | "B*" | "b" | "b*" => {
                used_stroke = used_stroke.or(stroke);
                used_fill = used_fill.or(fill);
            }
            _ => {}
        }
        nums.clear();
    }
    (used_stroke, used_fill)
}

#[cfg(test)]
mod appearance_tests {
    use super::appearance_colors;

    #[test]
    fn reads_painted_colours() {
        let ap = "q 1 1 1 rg 0 0.431 0.902 RG 10 w 105 605 90 90 re B Q";
        assert_eq!(appearance_colors(ap), (Some([0, 110, 230]), Some([255, 255, 255])));
        // Set but never used for painting: not reported.
        assert_eq!(appearance_colors("1 0 0 rg 0 0 1 RG 0 0 m 10 10 l S"), (Some([0, 0, 255]), None));
        assert_eq!(appearance_colors("0.5 g 0 0 5 5 re f"), (None, Some([128, 128, 128])));
        assert_eq!(appearance_colors("0 1 1 0 K 0 0 m 1 1 l S"), (Some([255, 0, 0]), None));
        assert_eq!(appearance_colors(""), (None, None));
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    #[test]
    fn noto_sans_faces_cover_the_characters_people_type() {
        let lib = super::super::worker::shared_pdfium().unwrap().bindings();
        let fonts = FontFamily::load_noto_sans(lib).unwrap();
        let text = "AZaz09 äöüß ÄÖÜ áéíóú ąęłśźż ÆØÅ Привет ЁЖЩ € – — „“ ‚‘ … ©";
        for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
            let font = fonts.font(bold, italic);
            for ch in text.chars().filter(|c| !c.is_whitespace()) {
                let mut out = String::new();
                font.outline(ch, 12.0, 0.0, 0.0, &mut out);
                assert!(!out.is_empty(), "{ch:?} missing (bold {bold}, italic {italic})");
                assert!(font.advance(ch) > 0.0);
            }
        }
        // Bold really is wider than regular.
        assert!(fonts.font(true, false).advance('m') > fonts.font(false, false).advance('m'));
    }
}
