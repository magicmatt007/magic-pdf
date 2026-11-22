//! The PDFium worker loop. Everything in here runs on the `pdfium` thread.

use std::collections::HashMap;
use std::path::Path;

use crossbeam_channel::{Receiver, RecvError, Sender, TryRecvError};
use pdfium_render::prelude::{Pdfium, PdfiumLibraryBindingsAccessor};

use super::commands::*;
use super::library_search_dirs;
use super::pdf::{Doc, Lib, OpenError};

const MAX_SEARCH_HITS_PER_PAGE: usize = 10_000;

/// One undoable edit. Annotation indices are stable because deletions only hide
/// annotations until the document is saved.
#[derive(Clone, Debug)]
enum Edit {
    Add { page: usize, index: usize },
    Delete { page: usize, index: usize },
    Update { page: usize, index: usize, undo: AnnotChange, redo: AnnotChange },
}

impl Edit {
    fn page(&self) -> usize {
        match self {
            Edit::Add { page, .. } | Edit::Delete { page, .. } | Edit::Update { page, .. } => *page,
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

struct Worker {
    lib: Lib,
    docs: HashMap<DocId, OpenDoc>,
    renders: HashMap<RenderKey, f32>,
    text_requests: Vec<(DocId, usize)>,
    searches: Vec<SearchJob>,
    scans: Vec<ScanJob>,
    tx: Sender<Response>,
    wake: Box<dyn Fn() + Send>,
}

/// Binds and initialises PDFium once per process; it stays loaded until exit.
fn shared_pdfium() -> Result<&'static Pdfium, String> {
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

    let mut w = Worker {
        lib,
        docs: HashMap::new(),
        renders: HashMap::new(),
        text_requests: Vec::new(),
        searches: Vec::new(),
        scans: Vec::new(),
        tx,
        wake,
    };

    loop {
        if w.is_idle() {
            match rx.recv() {
                Ok(req) => w.handle(req),
                Err(RecvError) => return,
            }
        }
        loop {
            match rx.try_recv() {
                Ok(req) => w.handle(req),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        w.step();
    }
}

impl Worker {
    fn is_idle(&self) -> bool {
        self.renders.is_empty() && self.text_requests.is_empty() && self.searches.is_empty() && self.scans.is_empty()
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
            Request::PrintNative {
                doc,
                title,
                current_page,
            } => self.print_native(doc, &title, current_page),
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
            page.render(width, height, key.rotation).map(|rgba| ([width, height], rgba))
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
        let hits = page.search(&job.query, MAX_SEARCH_HITS_PER_PAGE);
        let geom = page.geom();
        if !hits.is_empty() {
            self.send(Response::SearchResults {
                doc,
                generation,
                page: page_index,
                geom,
                hits,
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
                        undo,
                        redo: change,
                    },
                );
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
        let page_index = edit.page();
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
            }
        };
        if let Edit::Update { index, undo: u, redo: r, .. } = &edit {
            remember_colors(od, page_index, *index, if undo { u } else { r });
        }
        if !ok {
            log::warn!("could not {} {edit:?}", if undo { "undo" } else { "redo" });
        }
        if undo {
            od.redo.push(edit);
        } else {
            od.undo.push(edit);
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
