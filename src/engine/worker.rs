//! The PDFium worker loop. Everything in here runs on the `pdfium` thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, RecvError, Sender, TryRecvError};
use pdfium_render::prelude::{Pdfium, PdfiumLibraryBindingsAccessor};

use std::sync::Arc;

use super::commands::*;
use super::pages::{PageMapping, PageOp};
use super::library_search_dirs;
use super::ocr::{self, OcrWord};
use super::pdf::{Doc, FontFamily, Lib, OpenError};

const MAX_SEARCH_HITS_PER_PAGE: usize = 10_000;

/// One undoable edit. Annotation indices are stable because deletions only hide
/// annotations until the document is saved.
#[derive(Clone, Debug)]
enum Edit {
    Add { page: usize, index: usize },
    Delete { page: usize, index: usize },
    Update { page: usize, index: usize, undo: Box<AnnotChange>, redo: Box<AnnotChange> },
    /// A page operation. Undo restores the document from `snapshot` (taken before the
    /// operation, with the annotation bookkeeping of that moment); redo re-applies `op`.
    Pages(Box<PagesEdit>),
}

#[derive(Clone, Debug)]
struct PagesEdit {
    op: PageOp,
    snapshot: Arc<Vec<u8>>,
    deleted: HashMap<(usize, usize), i32>,
    colors: HashMap<(usize, usize), (Option<Rgb>, Option<Rgb>)>,
}

/// Memory allowed for page-operation undo snapshots per document.
const SNAPSHOT_BUDGET: usize = 512 * 1024 * 1024;

impl Edit {
    fn snapshot_bytes(&self) -> usize {
        match self {
            Edit::Pages(p) => p.snapshot.len(),
            _ => 0,
        }
    }
}

struct OpenDoc {
    doc: Doc,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Undo depth at the last save (None: the saved state can no longer be reached).
    saved_at: Option<usize>,
    /// Soft-deleted annotations: (page, index) -> flags before deletion.
    deleted: HashMap<(usize, usize), i32>,
    /// Known colours (stroke, fill) per annotation. PDFium cannot read colours of
    /// annotations that have an appearance stream, which ours get once rendered.
    colors: HashMap<(usize, usize), (Option<Rgb>, Option<Rgb>)>,
    /// Form field values changed since the last save (not part of undo history).
    form_dirty: bool,
}

impl OpenDoc {
    fn dirty(&self) -> bool {
        self.form_dirty || self.saved_at != Some(self.undo.len())
    }

    /// The document as PDF bytes with all edits applied. Deleted annotations are only
    /// hidden in the live document (so they can be restored); they are stripped for
    /// real from a temporary copy.
    fn export(&self, lib: Lib) -> Result<Vec<u8>, String> {
        let bytes = self.doc.save_to_bytes()?;
        if self.deleted.is_empty() {
            return Ok(bytes);
        }
        let copy = Doc::open_bytes(lib, bytes, None).map_err(|_| "could not re-open saved copy")?;
        let mut by_page: HashMap<usize, Vec<usize>> = HashMap::new();
        for &(p, i) in self.deleted.keys() {
            by_page.entry(p).or_default().push(i);
        }
        for (p, indices) in by_page {
            if let Some(page) = copy.page(p) {
                page.remove_annots(&indices);
            }
        }
        copy.save_to_bytes()
    }
}

struct SearchJob {
    doc: DocId,
    generation: u64,
    query: SearchQuery,
    next_page: usize,
}

struct ScanJob {
    doc: DocId,
    next_page: usize,
}

/// A text recognition run. Pages are rendered here; Tesseract runs on a helper
/// thread (one page at a time) so the worker stays responsive meanwhile.
struct OcrJob {
    id: u64,
    doc: DocId,
    pages: Vec<usize>,
    next: usize,
    exe: PathBuf,
    languages: String,
    force: bool,
    /// Tesseract is working on `pages[next]`.
    running: bool,
    results: Vec<(usize, Vec<OcrWord>)>,
    skipped: usize,
}

/// Tesseract's answer for one page, sent back from the helper thread.
struct OcrPageResult {
    job: u64,
    page: usize,
    /// Size of the rendering Tesseract read.
    size: (usize, usize),
    hocr: Result<String, String>,
}

struct Worker {
    lib: Lib,
    docs: HashMap<DocId, OpenDoc>,
    renders: HashMap<RenderKey, f32>,
    text_requests: Vec<(DocId, usize)>,
    searches: Vec<SearchJob>,
    scans: Vec<ScanJob>,
    tx: Sender<Response>,
    wake: Box<dyn Fn() + Send>,
    /// Fonts for text box appearances, loaded on first use.
    text_font: Option<FontFamily>,
    /// The annotation copied last (document, page, annotation).
    clip: Option<(DocId, usize, NewAnnot)>,
    ocr: Option<OcrJob>,
    ocr_runs: u64,
    ocr_tx: Sender<OcrPageResult>,
}

/// Binds and initialises PDFium once per process; it stays loaded until exit.
pub(super) fn shared_pdfium() -> Result<&'static Pdfium, String> {
    static PDFIUM: std::sync::Mutex<Option<&'static Pdfium>> = std::sync::Mutex::new(None);
    let mut slot = PDFIUM.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = *slot {
        return Ok(p);
    }
    let p: &'static Pdfium = Box::leak(Box::new(bind_pdfium()?));
    *slot = Some(p);
    Ok(p)
}

fn bind_pdfium() -> Result<Pdfium, String> {
    let mut errors = Vec::new();
    for dir in library_search_dirs() {
        let path = Pdfium::pdfium_platform_library_name_at_path(&dir);
        if !path.exists() {
            continue;
        }
        match Pdfium::bind_to_library(&path) {
            Ok(bindings) => {
                log::info!("Loaded PDFium from {}", path.display());
                return Ok(Pdfium::new(bindings));
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(bindings) => Ok(Pdfium::new(bindings)),
        Err(e) => {
            errors.push(format!("system library: {e}"));
            Err(format!(
                "Could not load the PDFium library. Run scripts/fetch-pdfium.sh (or .ps1 on \
                 Windows) or place the library next to the executable.\n{}",
                errors.join("\n")
            ))
        }
    }
}

pub(super) fn run(
    rx: Receiver<Request>,
    tx: Sender<Response>,
    ready: Sender<Result<(), String>>,
    wake: Box<dyn Fn() + Send>,
) {
    let pdfium = match shared_pdfium() {
        Ok(p) => p,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    let lib: Lib = pdfium.bindings();
    let (ocr_tx, ocr_rx) = crossbeam_channel::unbounded();

    let mut w = Worker {
        lib,
        docs: HashMap::new(),
        renders: HashMap::new(),
        text_requests: Vec::new(),
        searches: Vec::new(),
        scans: Vec::new(),
        tx,
        wake,
        text_font: None,
        clip: None,
        ocr: None,
        ocr_runs: 0,
        ocr_tx,
    };

    loop {
        if w.is_idle() {
            crossbeam_channel::select! {
                recv(rx) -> req => match req {
                    Ok(req) => w.handle(req),
                    Err(RecvError) => return,
                },
                recv(ocr_rx) -> res => {
                    if let Ok(res) = res {
                        w.ocr_page_done(res);
                    }
                }
            }
        }
        loop {
            match rx.try_recv() {
                Ok(req) => w.handle(req),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        while let Ok(res) = ocr_rx.try_recv() {
            w.ocr_page_done(res);
        }
        w.step();
    }
}

impl Worker {
    fn is_idle(&self) -> bool {
        self.renders.is_empty()
            && self.text_requests.is_empty()
            && self.searches.is_empty()
            && self.scans.is_empty()
            && self.ocr.as_ref().is_none_or(|j| j.running)
    }

    fn send(&self, resp: Response) {
        let _ = self.tx.send(resp);
        (self.wake)();
    }

    fn handle(&mut self, req: Request) {
        match req {
            Request::Open { doc, path, password } => self.open(doc, &path, password.as_deref()),
            Request::Close { doc } => {
                self.docs.remove(&doc);
                self.renders.retain(|k, _| k.doc != doc);
                self.text_requests.retain(|(d, _)| *d != doc);
                self.searches.retain(|s| s.doc != doc);
                self.scans.retain(|s| s.doc != doc);
                if self.ocr.as_ref().is_some_and(|j| j.doc == doc) {
                    self.ocr = None;
                }
            }
            Request::Render { key, priority } => {
                self.renders.insert(key, priority);
            }
            Request::Retain { doc, keep } => {
                self.renders.retain(|k, _| k.doc != doc || keep.contains(k));
            }
            Request::PageText { doc, page } => {
                if !self.text_requests.contains(&(doc, page)) {
                    self.text_requests.push((doc, page));
                }
            }
            Request::Search { doc, generation, query } => {
                self.searches.retain(|s| s.doc != doc);
                self.searches.push(SearchJob {
                    doc,
                    generation,
                    query,
                    next_page: 0,
                });
            }
            Request::CancelSearch { doc } => self.searches.retain(|s| s.doc != doc),
            Request::ScanAnnots { doc } => {
                self.scans.retain(|s| s.doc != doc);
                self.scans.push(ScanJob { doc, next_page: 0 });
            }
            Request::AddAnnot { doc, page, annot } => self.add_annot(doc, page, &annot),
            Request::UpdateAnnot { doc, page, index, change } => self.update_annot(doc, page, index, change),
            Request::DeleteAnnot { doc, page, index } => self.delete_annot(doc, page, index),
            Request::CopyAnnot { doc, page, index } => {
                let clip = self
                    .docs
                    .get(&doc)
                    .and_then(|od| od.doc.page(page))
                    .ok_or_else(|| "page not found".to_string())
                    .and_then(|p| p.annot_clip(index));
                match clip {
                    Ok(n) => self.clip = Some((doc, page, n)),
                    Err(e) => self.error(doc, e),
                }
            }
            Request::PasteAnnot { doc, page, at } => self.paste_annot(doc, page, at),
            Request::Undo { doc } => self.undo_redo(doc, true),
            Request::Redo { doc } => self.undo_redo(doc, false),
            Request::Save { doc, path } => self.save(doc, &path),
            Request::FormMouse {
                doc,
                page,
                event,
                x,
                y,
                modifiers,
            } => {
                let mut toggled = false;
                if let Some(od) = self.docs.get(&doc)
                    && let Some(p) = od.doc.page(page)
                {
                    p.form_mouse(event, x, y, modifiers);
                    // Releasing on a checkbox, radio button or list changes its value.
                    toggled = event == FormMouse::Up
                        && od.doc.focused_field().is_some_and(|(_, t)| (2..=5).contains(&t));
                }
                self.after_form(doc, event != FormMouse::Move, toggled);
            }
            Request::FormKey { doc, key, modifiers } => {
                self.with_focused_page(doc, |p| p.form_key(key, modifiers));
                // Characters (incl. backspace/enter), Delete and Space (toggles checkboxes)
                // edit the field; navigation keys do not.
                let edits = matches!(key, FormKey::Char(_) | FormKey::Down(0x2E) | FormKey::Down(0x20));
                self.after_form(doc, true, edits);
            }
            Request::FormCopy { doc, cut } => {
                let mut text = String::new();
                self.with_focused_page(doc, |p| {
                    text = p.form_selected_text();
                    if cut {
                        p.form_replace_selection("");
                    }
                });
                let edited = cut && !text.is_empty();
                if !text.is_empty() {
                    self.send(Response::Clipboard { text });
                }
                self.after_form(doc, true, edited);
            }
            Request::FormPaste { doc, text } => {
                self.with_focused_page(doc, |p| p.form_replace_selection(&text));
                self.after_form(doc, true, true);
            }
            Request::FormSelectAll { doc } => {
                self.with_focused_page(doc, |p| p.form_select_all());
                self.after_form(doc, true, false);
            }
            Request::FormUndo { doc, redo } => {
                self.with_focused_page(doc, |p| p.form_undo(redo));
                self.after_form(doc, true, true);
            }
            Request::FormKillFocus { doc } => {
                if let Some(od) = self.docs.get(&doc) {
                    od.doc.kill_form_focus();
                }
                self.after_form(doc, true, false);
            }
            Request::ExportForPrint { doc, job } => {
                let Some(od) = self.docs.get(&doc) else { return };
                match od.export(self.lib) {
                    Ok(bytes) => self.send(Response::PrintReady { job, bytes }),
                    Err(e) => self.error(doc, format!("Preparing the print job failed: {e}")),
                }
            }
            Request::EditPages { doc, op } => {
                self.edit_pages(doc, op);
            }
            Request::InsertFile { doc, path, at } => match std::fs::read(&path) {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let op = PageOp::InsertFile {
                        at,
                        source: Arc::new(bytes),
                        name,
                        pages: None,
                        count: 0,
                    };
                    self.edit_pages(doc, op);
                }
                Err(e) => self.error(doc, format!("Could not read {}: {e}", path.display())),
            },
            Request::Compare { a, b } => {
                let (Some(da), Some(db)) = (self.docs.get(&a), self.docs.get(&b)) else { return };
                let words = |d: &OpenDoc, i: usize| {
                    d.doc.page(i).map(|p| super::compare::words(&p.text())).unwrap_or_default()
                };
                let n = da.doc.page_count().max(db.doc.page_count());
                let diffs = (0..n)
                    .map(|i| super::compare::diff_page(i, &words(da, i), &words(db, i)))
                    .filter(|d| !d.is_empty())
                    .collect();
                self.send(Response::Compared { a, b, diffs });
            }
            Request::SaveOptimized { doc, path, image_dpi } => match self.save_optimized(doc, &path, image_dpi) {
                Ok(message) => self.send(Response::Exported { message }),
                Err(e) => self.error(doc, format!("Saving {} failed: {e}", path.display())),
            },
            Request::ExtractPages { doc, pages, path } => {
                let result = self.extract(doc, &[(pages, path.clone())]);
                match result {
                    Ok(()) => self.send(Response::Exported {
                        message: format!("Saved pages to {}", path.display()),
                    }),
                    Err(e) => self.error(doc, format!("Extracting pages failed: {e}")),
                }
            }
            Request::SplitDocument { doc, every, dir, stem } => {
                let count = self.docs.get(&doc).map_or(0, |od| od.doc.page_count());
                let every = every.max(1);
                let parts: Vec<(Vec<usize>, std::path::PathBuf)> = (0..count)
                    .step_by(every)
                    .enumerate()
                    .map(|(n, start)| {
                        (
                            (start..(start + every).min(count)).collect(),
                            dir.join(format!("{stem}-{:02}.pdf", n + 1)),
                        )
                    })
                    .collect();
                let n = parts.len();
                match self.extract(doc, &parts) {
                    Ok(()) => self.send(Response::Exported {
                        message: format!("Split into {n} files in {}", dir.display()),
                    }),
                    Err(e) => self.error(doc, format!("Splitting failed: {e}")),
                }
            }
            Request::ExportToFile { doc, path } => {
                let Some(od) = self.docs.get(&doc) else { return };
                let result = od
                    .export(self.lib)
                    .and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| e.to_string()));
                match result {
                    Ok(()) => self.send(Response::ExportedTo { doc, path }),
                    Err(e) => self.error(doc, format!("Moving the document failed: {e}")),
                }
            }
            Request::MarkDirty { doc } => {
                if let Some(od) = self.docs.get_mut(&doc) {
                    od.form_dirty = true;
                }
                self.send_edit_state(doc);
            }
            Request::PrintNative {
                doc,
                title,
                current_page,
            } => self.print_native(doc, &title, current_page),
            Request::Ocr {
                doc,
                pages,
                exe,
                languages,
                force,
            } => {
                if self.ocr.is_some() {
                    self.send(Response::OcrFinished {
                        doc,
                        result: OcrOutcome::Failed("Text recognition is already running.".into()),
                    });
                    return;
                }
                self.ocr_runs += 1;
                self.ocr = Some(OcrJob {
                    id: self.ocr_runs,
                    doc,
                    pages,
                    next: 0,
                    exe,
                    languages,
                    force,
                    running: false,
                    results: Vec::new(),
                    skipped: 0,
                });
            }
            Request::CancelOcr { doc } => self.end_ocr(doc, OcrOutcome::Cancelled),
        }
    }

    /// Stops the recognition of `doc` (if running) and reports `outcome`.
    fn end_ocr(&mut self, doc: DocId, outcome: OcrOutcome) {
        if self.ocr.as_ref().is_some_and(|j| j.doc == doc) {
            // A page still in Tesseract is ignored when it arrives (the job id is gone).
            self.ocr = None;
            self.send(Response::OcrFinished { doc, result: outcome });
        }
    }

    /// Renders the next page of the recognition job and hands it to Tesseract, or
    /// finishes the job.
    fn ocr_step(&mut self) {
        let Some(job) = self.ocr.as_mut() else { return };
        let doc = job.doc;
        let Some(od) = self.docs.get(&doc) else {
            self.ocr = None;
            return;
        };
        let Some(&page_index) = job.pages.get(job.next) else {
            let job = self.ocr.take().expect("job");
            let recognised = job.results.iter().filter(|(_, w)| !w.is_empty()).count();
            let words = job.results.iter().map(|(_, w)| w.len()).sum();
            let pages: Vec<_> = job.results.into_iter().filter(|(_, w)| !w.is_empty()).collect();
            let ok = pages.is_empty()
                || self.edit_pages(doc, PageOp::TextLayer {
                    pages,
                    replace: job.force,
                });
            let result = if ok {
                OcrOutcome::Done {
                    recognised,
                    skipped: job.skipped,
                    words,
                }
            } else {
                OcrOutcome::Failed("The recognised text could not be added to the document.".into())
            };
            self.send(Response::OcrFinished { doc, result });
            return;
        };
        let (done, total) = (job.next, job.pages.len());
        let page = od.doc.page(page_index);
        if page.as_ref().is_none_or(|p| !job.force && p.text_char_count() >= ocr::HAS_TEXT_CHARS) {
            job.skipped += 1;
            job.next += 1;
            return;
        }
        let page = page.expect("checked");
        let (w, h) = od.doc.page_size(page_index);
        let scale = (ocr::DPI / 72.0).min(ocr::MAX_SIDE / w.max(h));
        let size = ((w * scale).round().max(1.0) as usize, (h * scale).round().max(1.0) as usize);
        let image = std::env::temp_dir().join(format!("magic-pdf-ocr-{}-{}-{page_index}.pgm", std::process::id(), job.id));
        let written = page
            .render_gray(size.0, size.1)
            .ok_or_else(|| "could not render the page".to_string())
            .and_then(|gray| ocr::write_pgm(&image, size.0, size.1, &gray).map_err(|e| e.to_string()));
        if let Err(e) = written {
            self.end_ocr(doc, OcrOutcome::Failed(format!("Page {}: {e}", page_index + 1)));
            return;
        }
        job.running = true;
        let (id, exe, languages, tx) = (job.id, job.exe.clone(), job.languages.clone(), self.ocr_tx.clone());
        self.send(Response::OcrProgress { doc, done, total });
        let spawned = std::thread::Builder::new().name("ocr".into()).spawn(move || {
            let hocr = ocr::recognise(&exe, &image, &languages);
            let _ = std::fs::remove_file(&image);
            let _ = tx.send(OcrPageResult {
                job: id,
                page: page_index,
                size,
                hocr,
            });
        });
        if let Err(e) = spawned {
            self.end_ocr(doc, OcrOutcome::Failed(e.to_string()));
        }
    }

    fn ocr_page_done(&mut self, res: OcrPageResult) {
        let Some(job) = self.ocr.as_mut().filter(|j| j.id == res.job) else { return };
        let doc = job.doc;
        match res.hocr {
            Ok(hocr) => {
                let words = ocr::parse_hocr(&hocr);
                let placed = self
                    .docs
                    .get(&doc)
                    .and_then(|od| od.doc.page(res.page))
                    .map(|p| p.place_ocr_words(res.size.0, res.size.1, words))
                    .unwrap_or_default();
                job.results.push((res.page, placed));
                job.running = false;
                job.next += 1;
            }
            Err(e) => self.end_ocr(doc, OcrOutcome::Failed(e)),
        }
    }

    /// Performs one unit of work, so new requests are picked up quickly.
    fn step(&mut self) {
        if !self.text_requests.is_empty() {
            let (doc, page) = self.text_requests.remove(0);
            self.page_text(doc, page);
            return;
        }
        let next = self.renders.iter().min_by(|a, b| a.1.total_cmp(b.1)).map(|(k, _)| *k);
        if let Some(key) = next {
            self.renders.remove(&key);
            self.render(key);
            return;
        }
        if !self.searches.is_empty() {
            self.search_step();
            return;
        }
        if !self.scans.is_empty() {
            self.scan_step();
            return;
        }
        if self.ocr.as_ref().is_some_and(|j| !j.running) {
            self.ocr_step();
        }
    }

    fn open(&mut self, id: DocId, path: &Path, password: Option<&str>) {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                self.send(Response::OpenFailed {
                    doc: id,
                    error: e.to_string(),
                    password_required: false,
                });
                return;
            }
        };
        match Doc::open_bytes(self.lib, data, password) {
            Ok(doc) => {
                let page_sizes = (0..doc.page_count()).map(|i| doc.page_size(i)).collect();
                let info = DocInfo {
                    page_sizes,
                    outline: doc.outline(),
                    title: doc.title(),
                };
                self.docs.insert(
                    id,
                    OpenDoc {
                        doc,
                        undo: Vec::new(),
                        redo: Vec::new(),
                        saved_at: Some(0),
                        deleted: HashMap::new(),
                        colors: HashMap::new(),
                        form_dirty: false,
                    },
                );
                self.send(Response::Opened { doc: id, info });
            }
            Err(e) => self.send(Response::OpenFailed {
                doc: id,
                error: match &e {
                    OpenError::Password => "password required".into(),
                    OpenError::Other(m) => m.clone(),
                },
                password_required: matches!(e, OpenError::Password),
            }),
        }
    }

    fn render(&mut self, key: RenderKey) {
        let result = self.docs.get(&key.doc).and_then(|od| {
            let page = od.doc.page(key.page)?;
            let (w, h) = od.doc.page_size(key.page);
            let (out_w, out_h) = if key.rotation.swaps_axes() { (h, w) } else { (w, h) };
            let width = key.width_px.max(1) as usize;
            let height = ((out_h / out_w) * width as f32).round().max(1.0) as usize;
            match key.tile_pos() {
                None => page.render(width, height, key.rotation).map(|rgba| ([width, height], rgba)),
                Some((col, row)) => {
                    let tile = TILE as usize;
                    let (x, y) = (col as usize * tile, row as usize * tile);
                    if x >= width || y >= height {
                        return None;
                    }
                    let (w, h) = (tile.min(width - x), tile.min(height - y));
                    page.render_region((width, height), (x, y, w, h), key.rotation).map(|rgba| ([w, h], rgba))
                }
            }
        });
        match result {
            Some((size, rgba)) => self.send(Response::Rendered { key, size, rgba }),
            None => self.send(Response::RenderFailed { key }),
        }
    }

    fn page_text(&mut self, doc: DocId, page_index: usize) {
        let Some(od) = self.docs.get(&doc) else { return };
        let Some(page) = od.doc.page(page_index) else { return };
        let resp = Response::PageText {
            doc,
            page: page_index,
            geom: page.geom(),
            text: page.text(),
        };
        self.send(resp);
    }

    fn search_step(&mut self) {
        let job = &mut self.searches[0];
        let Some(od) = self.docs.get(&job.doc) else {
            self.searches.remove(0);
            return;
        };
        if job.next_page >= od.doc.page_count() {
            let (doc, generation) = (job.doc, job.generation);
            self.searches.remove(0);
            self.send(Response::SearchDone { doc, generation });
            return;
        }
        let page_index = job.next_page;
        job.next_page += 1;
        let (doc, generation) = (job.doc, job.generation);
        let Some(page) = od.doc.page(page_index) else { return };
        let (hits, context): (Vec<_>, Vec<_>) = page.search_with_context(&job.query, MAX_SEARCH_HITS_PER_PAGE).into_iter().unzip();
        let geom = page.geom();
        if !hits.is_empty() {
            self.send(Response::SearchResults {
                doc,
                generation,
                page: page_index,
                geom,
                hits,
                context,
            });
        }
    }

    fn scan_step(&mut self) {
        let job = &mut self.scans[0];
        let doc = job.doc;
        let Some(od) = self.docs.get(&doc) else {
            self.scans.remove(0);
            return;
        };
        if job.next_page >= od.doc.page_count() {
            self.scans.remove(0);
            self.send(Response::AnnotScanDone { doc });
            return;
        }
        let page = job.next_page;
        job.next_page += 1;
        self.send_annots(doc, page);
    }

    fn send_annots(&self, doc: DocId, page_index: usize) {
        let Some(od) = self.docs.get(&doc) else { return };
        let Some(page) = od.doc.page(page_index) else { return };
        let mut annots = page.annots(|i| od.deleted.contains_key(&(page_index, i)));
        for a in &mut annots {
            if let Some((c, ic)) = od.colors.get(&(page_index, a.index)) {
                a.color = a.color.or(*c);
                if a.interior.is_none() {
                    a.interior = *ic;
                }
            }
        }
        let geom = page.geom();
        self.send(Response::PageAnnots {
            doc,
            page: page_index,
            geom,
            annots,
        });
        self.send_widgets(doc, page_index);
    }

    fn doc_info(od: &OpenDoc) -> DocInfo {
        DocInfo {
            page_sizes: (0..od.doc.page_count()).map(|i| od.doc.page_size(i)).collect(),
            outline: od.doc.outline(),
            title: od.doc.title(),
        }
    }

    fn pages_changed(&mut self, doc: DocId, mapping: PageMapping) {
        // Recognised pages would land on the wrong pages now.
        self.end_ocr(doc, OcrOutcome::Failed("The pages changed during text recognition.".into()));
        let Some(od) = self.docs.get(&doc) else { return };
        let info = Self::doc_info(od);
        self.send(Response::PagesChanged { doc, info, mapping });
        self.send(Response::FormFocus { doc, page: None });
        self.send_edit_state(doc);
    }

    /// Applies a page operation as an undoable edit; false if it failed (reported).
    fn edit_pages(&mut self, doc: DocId, mut op: PageOp) -> bool {
        let Some(od) = self.docs.get_mut(&doc) else { return false };
        let count = od.doc.page_count();
        if let Err(e) = op.validate(count) {
            self.error(doc, format!("{}: {e}", op.label()));
            return false;
        }
        let snapshot = match od.doc.save_to_bytes() {
            Ok(b) => Arc::new(b),
            Err(e) => {
                self.error(doc, e);
                return false;
            }
        };
        match apply_op(self.lib, od, &op) {
            Ok(inserted) => {
                if let PageOp::InsertFile { count: n, .. } = &mut op {
                    *n = inserted;
                }
                let mapping = op.mapping(count);
                let edit = PagesEdit {
                    op,
                    snapshot,
                    deleted: od.deleted.clone(),
                    colors: od.colors.clone(),
                };
                remap_bookkeeping(od, &mapping);
                Self::push_edit(od, Edit::Pages(Box::new(edit)));
                trim_snapshots(od);
                self.pages_changed(doc, mapping);
                true
            }
            Err(e) => {
                // A failed operation may have half-applied; restore the snapshot.
                if let Ok(d) = Doc::open_bytes(self.lib, snapshot.as_ref().clone(), None) {
                    od.doc = d;
                }
                self.error(doc, format!("{} failed: {e}", op.label()));
                self.pages_changed(doc, PageMapping {
                    map: (0..count).map(Some).collect(),
                    new_count: count,
                    changed: (0..count).collect(),
                });
                false
            }
        }
    }

    fn undo_redo_pages(&mut self, doc: DocId, edit: PagesEdit, undo: bool) {
        let lib = self.lib;
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let mapping = if undo {
            let current = od.doc.page_count();
            let doc_now = match Doc::open_bytes(lib, edit.snapshot.as_ref().clone(), None) {
                Ok(d) => d,
                Err(_) => {
                    self.error(doc, "could not restore the document");
                    return;
                }
            };
            let before = doc_now.page_count();
            od.doc = doc_now;
            od.deleted = edit.deleted.clone();
            od.colors = edit.colors.clone();
            let m = edit.op.mapping(before);
            debug_assert_eq!(m.new_count, current);
            od.redo.push(Edit::Pages(Box::new(edit)));
            m.inverse()
        } else {
            let count = od.doc.page_count();
            if let Err(e) = apply_op(lib, od, &edit.op) {
                self.error(doc, format!("Redo failed: {e}"));
                return;
            }
            let m = edit.op.mapping(count);
            remap_bookkeeping(od, &m);
            od.undo.push(Edit::Pages(Box::new(edit)));
            m
        };
        self.pages_changed(doc, mapping);
    }

    /// Writes a smaller copy of the document; returns a summary for the user.
    fn save_optimized(&self, doc: DocId, path: &Path, image_dpi: Option<u32>) -> Result<String, String> {
        let od = self.docs.get(&doc).ok_or("document not open")?;
        let original = od.export(self.lib)?;
        let mut bytes = original.clone();
        let mut images = 0;
        if let Some(dpi) = image_dpi {
            let copy = Doc::open_bytes(self.lib, bytes.clone(), None).map_err(|_| "could not prepare the copy")?;
            for i in 0..copy.page_count() {
                if let Some(page) = copy.page(i) {
                    images += page.downsample_images(dpi as f32);
                }
            }
            bytes = copy.save_to_bytes()?;
        }
        let compressed = match super::lopdf_ops::compress(&bytes) {
            Ok(c) if c.len() < bytes.len() => c,
            Ok(_) => bytes,
            Err(e) => {
                log::warn!("compression skipped: {e}");
                bytes
            }
        };
        write_atomically(path, &compressed).map_err(|e| e.to_string())?;
        let mb = |n: usize| format!("{:.1} MB", n as f64 / 1_048_576.0);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut msg = format!("Saved {name}: {} → {}", mb(original.len()), mb(compressed.len()));
        if images > 0 {
            msg += &format!(" ({images} image{} scaled down)", if images == 1 { "" } else { "s" });
        }
        Ok(msg)
    }

    /// Writes each (pages, path) to a new PDF, taken from the document with all edits.
    fn extract(&self, doc: DocId, parts: &[(Vec<usize>, std::path::PathBuf)]) -> Result<(), String> {
        let od = self.docs.get(&doc).ok_or("document not open")?;
        if let Some((_, p)) = parts.iter().find(|(_, p)| p.exists() && parts.len() > 1) {
            return Err(format!("{} already exists", p.display()));
        }
        let source = Doc::open_bytes(self.lib, od.export(self.lib)?, None).map_err(|_| "could not prepare the pages")?;
        for (pages, path) in parts {
            let out = Doc::create(self.lib)?;
            out.import_pages(&source, Some(pages), 0)?;
            write_atomically(path, &out.save_to_bytes()?).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn with_focused_page(&self, doc: DocId, f: impl FnOnce(&super::pdf::Page)) {
        let Some(od) = self.docs.get(&doc) else { return };
        if let Some((page, _)) = od.doc.focused_field()
            && let Some(p) = od.doc.page(page)
        {
            f(&p);
        }
    }

    /// Reports repaints, value changes and focus after a form interaction. `edited`
    /// marks interactions known to change a field value (PDFium's own change
    /// notification only fires when an edit is committed).
    fn after_form(&mut self, doc: DocId, report_focus: bool, edited: bool) {
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let mut events = od.doc.take_form_events();
        events.changed |= edited && !events.invalidated_pages.is_empty();
        let focus = od.doc.focused_field();
        let was_dirty = od.dirty();
        if events.changed {
            od.form_dirty = true;
        }
        let now_dirty = od.dirty();
        for page in &events.invalidated_pages {
            self.send(Response::PageChanged { doc, page: *page });
        }
        if events.changed {
            for page in &events.invalidated_pages {
                self.send_widgets(doc, *page);
            }
        }
        if was_dirty != now_dirty {
            self.send_edit_state(doc);
        }
        if report_focus {
            self.send(Response::FormFocus {
                doc,
                page: focus.map(|f| f.0),
            });
        }
    }

    fn send_widgets(&self, doc: DocId, page_index: usize) {
        let Some(od) = self.docs.get(&doc) else { return };
        let Some(page) = od.doc.page(page_index) else { return };
        let widgets = page.widgets();
        self.send(Response::PageWidgets {
            doc,
            page: page_index,
            widgets,
        });
    }

    #[cfg(windows)]
    fn print_native(&mut self, doc: DocId, title: &str, current_page: usize) {
        let Some(od) = self.docs.get(&doc) else { return };
        match super::print_windows::print(&od.doc, title, current_page) {
            Ok(pages) => self.send(Response::Printed { pages }),
            Err(e) => self.error(doc, format!("Printing failed: {e}")),
        }
    }

    #[cfg(not(windows))]
    fn print_native(&mut self, doc: DocId, _title: &str, _current_page: usize) {
        self.error(doc, "Native printing is only available on Windows");
    }

    fn paste_annot(&mut self, doc: DocId, page: usize, at: Option<(f32, f32)>) {
        let Some((from_doc, from_page, mut n)) = self.clip.clone() else { return };
        let (dx, dy) = match at {
            Some((x, y)) => (
                x - (n.rect.left + n.rect.right) / 2.0,
                y - (n.rect.bottom + n.rect.top) / 2.0,
            ),
            // Next to the original, or at the same place on another page.
            None if from_doc == doc && from_page == page => (12.0, -12.0),
            None => (0.0, 0.0),
        };
        n.translate(dx, dy);
        self.add_annot(doc, page, &n);
    }

    fn fonts(&mut self, doc: DocId) -> Option<&FontFamily> {
        if self.text_font.is_none() {
            match FontFamily::load_noto_sans(self.lib) {
                Ok(f) => self.text_font = Some(f),
                Err(e) => {
                    self.error(doc, e);
                    return None;
                }
            }
        }
        self.text_font.as_ref()
    }

    /// Redraws annotations magic-pdf draws itself (after creating or changing them).
    fn refresh_text_box(&mut self, doc: DocId, page_index: usize, index: usize) {
        if self.fonts(doc).is_none() {
            return;
        }
        let (Some(od), Some(font)) = (self.docs.get(&doc), self.text_font.as_ref()) else { return };
        let Some(page) = od.doc.page(page_index) else { return };
        if let Err(e) = page.refresh_appearance(index, font) {
            log::warn!("appearance: {e}");
        }
    }

    fn after_edit(&self, doc: DocId, page: usize) {
        self.send(Response::PageChanged { doc, page });
        self.send_annots(doc, page);
        self.send_edit_state(doc);
    }

    fn send_edit_state(&self, doc: DocId) {
        if let Some(od) = self.docs.get(&doc) {
            self.send(Response::EditState {
                doc,
                can_undo: !od.undo.is_empty(),
                can_redo: !od.redo.is_empty(),
                dirty: od.dirty(),
            });
        }
    }

    fn error(&self, doc: DocId, message: impl Into<String>) {
        self.send(Response::Error {
            doc,
            message: message.into(),
        });
    }

    fn push_edit(od: &mut OpenDoc, edit: Edit) {
        if od.saved_at.is_some_and(|s| s > od.undo.len()) {
            od.saved_at = None;
        }
        od.undo.push(edit);
        od.redo.clear();
    }

    fn add_annot(&mut self, doc: DocId, page_index: usize, annot: &NewAnnot) {
        // A text stamp placed with a click gets its natural size around that point.
        let sized;
        let annot = match &annot.stamp {
            Some(StampSpec::Text { title, subtitle }) if annot.rect.right - annot.rect.left < 1.0 => {
                let Some(fonts) = self.fonts(doc) else { return };
                let (w, h) = super::shapes::stamp_size(title, subtitle, 20.0, fonts);
                let (cx, cy) = (annot.rect.left, annot.rect.top);
                let mut n = annot.clone();
                n.rect = PdfBox {
                    left: cx - w / 2.0,
                    bottom: cy - h / 2.0,
                    right: cx + w / 2.0,
                    top: cy + h / 2.0,
                };
                sized = n;
                &sized
            }
            _ => annot,
        };
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let result = od
            .doc
            .page(page_index)
            .ok_or_else(|| "page not found".to_string())
            .and_then(|p| p.add_annot(annot));
        match result {
            Ok(index) => {
                od.colors.insert((page_index, index), (Some(annot.color), annot.interior));
                Self::push_edit(od, Edit::Add { page: page_index, index });
                self.refresh_text_box(doc, page_index, index);
                self.send(Response::AnnotAdded {
                    doc,
                    page: page_index,
                    index,
                });
                self.after_edit(doc, page_index);
            }
            Err(e) => self.error(doc, e),
        }
    }

    fn update_annot(&mut self, doc: DocId, page_index: usize, index: usize, change: AnnotChange) {
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let result = od
            .doc
            .page(page_index)
            .ok_or_else(|| "page not found".to_string())
            .and_then(|p| p.update_annot(index, &change));
        match result {
            Ok(undo) => {
                remember_colors(od, page_index, index, &change);
                Self::push_edit(
                    od,
                    Edit::Update {
                        page: page_index,
                        index,
                        undo: Box::new(undo),
                        redo: Box::new(change),
                    },
                );
                self.refresh_text_box(doc, page_index, index);
                self.after_edit(doc, page_index);
            }
            Err(e) => self.error(doc, e),
        }
    }

    fn delete_annot(&mut self, doc: DocId, page_index: usize, index: usize) {
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let flags = od.doc.page(page_index).and_then(|p| p.set_annot_hidden(index, true));
        match flags {
            Some(flags) => {
                od.deleted.insert((page_index, index), flags);
                Self::push_edit(od, Edit::Delete { page: page_index, index });
                self.after_edit(doc, page_index);
            }
            None => self.error(doc, "annotation not found"),
        }
    }

    fn undo_redo(&mut self, doc: DocId, undo: bool) {
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let Some(edit) = (if undo { od.undo.pop() } else { od.redo.pop() }) else { return };
        let page_index = match &edit {
            Edit::Add { page, .. } | Edit::Delete { page, .. } | Edit::Update { page, .. } => *page,
            Edit::Pages(p) => {
                let p = p.clone();
                self.undo_redo_pages(doc, *p, undo);
                return;
            }
        };
        let ok = {
            let Some(page) = od.doc.page(page_index) else { return };
            // Adding and un-deleting are both "show", removing and deleting both "hide".
            match (&edit, undo) {
                (Edit::Add { index, .. }, true) | (Edit::Delete { index, .. }, false) => {
                    let flags = page.set_annot_hidden(*index, true);
                    if let Some(f) = flags {
                        od.deleted.insert((page_index, *index), f);
                    }
                    flags.is_some()
                }
                (Edit::Add { index, .. }, false) | (Edit::Delete { index, .. }, true) => od
                    .deleted
                    .remove(&(page_index, *index))
                    .is_some_and(|f| page.set_annot_flags(*index, f)),
                (Edit::Update { index, undo: u, redo: r, .. }, is_undo) => {
                    page.update_annot(*index, if is_undo { u } else { r }).is_ok()
                }
                (Edit::Pages(_), _) => unreachable!("handled above"),
            }
        };
        let updated = if let Edit::Update { index, undo: u, redo: r, .. } = &edit {
            remember_colors(od, page_index, *index, if undo { u } else { r });
            Some(*index)
        } else {
            None
        };
        if !ok {
            log::warn!("could not {} {edit:?}", if undo { "undo" } else { "redo" });
        }
        if undo {
            od.redo.push(edit);
        } else {
            od.undo.push(edit);
        }
        if let Some(index) = updated {
            self.refresh_text_box(doc, page_index, index);
        }
        self.after_edit(doc, page_index);
    }

    fn save(&mut self, doc: DocId, path: &Path) {
        let Some(od) = self.docs.get_mut(&doc) else { return };
        let result = od
            .export(self.lib)
            .and_then(|bytes| write_atomically(path, &bytes).map_err(|e| e.to_string()));
        match result {
            Ok(()) => {
                od.saved_at = Some(od.undo.len());
                od.form_dirty = false;
                self.send(Response::Saved {
                    doc,
                    path: path.to_path_buf(),
                });
                self.send_edit_state(doc);
            }
            Err(e) => self.error(doc, format!("Saving {} failed: {e}", path.display())),
        }
    }
}

/// Applies a page operation; bookmarks are rewritten in the saved bytes (PDFium
/// cannot write them) and the document reopened from those.
fn apply_op(lib: Lib, od: &mut OpenDoc, op: &PageOp) -> Result<usize, String> {
    match op {
        PageOp::SetOutline { outline } => {
            od.doc.kill_form_focus();
            let bytes = super::lopdf_ops::write_outline(&od.doc.save_to_bytes()?, outline)?;
            od.doc = Doc::open_bytes(lib, bytes, None).map_err(|_| "could not reopen the document")?;
            Ok(0)
        }
        _ => od.doc.apply_page_op(op),
    }
}

/// Renumbers page-keyed annotation bookkeeping after a page operation.
fn remap_bookkeeping(od: &mut OpenDoc, m: &PageMapping) {
    let remap = |key: &(usize, usize)| m.map.get(key.0).copied().flatten().map(|p| (p, key.1));
    od.deleted = od.deleted.iter().filter_map(|(k, v)| remap(k).map(|k| (k, *v))).collect();
    od.colors = od.colors.iter().filter_map(|(k, v)| remap(k).map(|k| (k, *v))).collect();
}

/// Drops the oldest history when page snapshots use too much memory.
fn trim_snapshots(od: &mut OpenDoc) {
    let total = |v: &[Edit]| v.iter().map(Edit::snapshot_bytes).sum::<usize>();
    while total(&od.undo) > SNAPSHOT_BUDGET {
        // Remove everything up to and including the oldest page operation.
        let Some(first) = od.undo.iter().position(|e| matches!(e, Edit::Pages(_))) else { break };
        od.undo.drain(..=first);
        od.saved_at = od.saved_at.and_then(|s| s.checked_sub(first + 1));
    }
}

fn remember_colors(od: &mut OpenDoc, page: usize, index: usize, change: &AnnotChange) {
    let entry = od.colors.entry((page, index)).or_insert((None, None));
    if let Some(c) = change.color {
        entry.0 = Some(c);
    }
    if let Some(ic) = change.interior {
        entry.1 = ic;
    }
}

/// Writes to a temporary file next to `path`, then renames it into place.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.magic-pdf-{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
