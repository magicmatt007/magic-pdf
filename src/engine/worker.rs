//! The PDFium worker loop. Everything in here runs on the `pdfium` thread.

use std::collections::HashMap;

use crossbeam_channel::{Receiver, RecvError, Sender, TryRecvError};
use pdfium_render::prelude::*;

use super::commands::*;
use super::library_search_dirs;

struct SearchJob {
    doc: DocId,
    generation: u64,
    query: SearchQuery,
    next_page: usize,
}

struct Worker {
    pdfium: &'static Pdfium,
    docs: HashMap<DocId, PdfDocument<'static>>,
    renders: HashMap<RenderKey, f32>,
    text_requests: Vec<(DocId, usize)>,
    searches: Vec<SearchJob>,
    tx: Sender<Response>,
    wake: Box<dyn Fn() + Send>,
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
    let pdfium = match bind_pdfium() {
        Ok(p) => p,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    // Documents borrow the library, and both live for the whole process.
    let pdfium: &'static Pdfium = Box::leak(Box::new(pdfium));

    let mut w = Worker {
        pdfium,
        docs: HashMap::new(),
        renders: HashMap::new(),
        text_requests: Vec::new(),
        searches: Vec::new(),
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
        self.renders.is_empty() && self.text_requests.is_empty() && self.searches.is_empty()
    }

    fn send(&self, resp: Response) {
        let _ = self.tx.send(resp);
        (self.wake)();
    }

    fn handle(&mut self, req: Request) {
        match req {
            Request::Open {
                doc,
                path,
                password,
            } => self.open(doc, &path, password.as_deref()),
            Request::Close { doc } => {
                self.docs.remove(&doc);
                self.renders.retain(|k, _| k.doc != doc);
                self.text_requests.retain(|(d, _)| *d != doc);
                self.searches.retain(|s| s.doc != doc);
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
            Request::Search {
                doc,
                generation,
                query,
            } => {
                self.searches.retain(|s| s.doc != doc);
                self.searches.push(SearchJob {
                    doc,
                    generation,
                    query,
                    next_page: 0,
                });
            }
            Request::CancelSearch { doc } => self.searches.retain(|s| s.doc != doc),
        }
    }

    /// Performs one unit of work, so new requests are picked up quickly.
    fn step(&mut self) {
        if let Some((doc, page)) = (!self.text_requests.is_empty()).then(|| self.text_requests.remove(0)) {
            self.page_text(doc, page);
            return;
        }
        let next = self
            .renders
            .iter()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .map(|(k, _)| *k);
        if let Some(key) = next {
            self.renders.remove(&key);
            self.render(key);
            return;
        }
        if !self.searches.is_empty() {
            self.search_step();
        }
    }

    fn open(&mut self, doc: DocId, path: &std::path::Path, password: Option<&str>) {
        match self.pdfium.load_pdf_from_file(path, password) {
            Ok(document) => {
                let page_sizes = document
                    .pages()
                    .page_sizes()
                    .map(|sizes| {
                        sizes
                            .iter()
                            .map(|r| (r.width().value.abs(), r.height().value.abs()))
                            .collect()
                    })
                    .unwrap_or_default();
                let outline = document
                    .bookmarks()
                    .root()
                    .map(|root| read_outline_siblings(root, 0, &mut 0))
                    .unwrap_or_default();
                let title = document
                    .metadata()
                    .get(PdfDocumentMetadataTagType::Title)
                    .map(|t| t.value().trim().to_string())
                    .filter(|t| !t.is_empty());
                self.docs.insert(doc, document);
                self.send(Response::Opened {
                    doc,
                    info: DocInfo {
                        page_sizes,
                        outline,
                        title,
                    },
                });
            }
            Err(e) => {
                let password_required = matches!(
                    e,
                    PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
                );
                self.send(Response::OpenFailed {
                    doc,
                    error: e.to_string(),
                    password_required,
                });
            }
        }
    }

    fn render(&mut self, key: RenderKey) {
        let result = (|| -> Result<([usize; 2], Vec<u8>), PdfiumError> {
            let document = self.docs.get(&key.doc).ok_or(PdfiumError::PageIndexOutOfBounds)?;
            let page = document.pages().get(key.page as PdfPageIndex)?;
            let out_w = if key.rotation.swaps_axes() {
                page.height().value
            } else {
                page.width().value
            };
            let scale = key.width_px as f32 / out_w.max(1.0);
            let rotation = match key.rotation {
                Rotation::R0 => PdfPageRenderRotation::None,
                Rotation::R90 => PdfPageRenderRotation::Degrees90,
                Rotation::R180 => PdfPageRenderRotation::Degrees180,
                Rotation::R270 => PdfPageRenderRotation::Degrees270,
            };
            let config = PdfRenderConfig::new()
                .scale_page_by_factor(scale)
                .rotate(rotation, true)
                .render_form_data(true)
                .render_annotations(true)
                .set_clear_color(PdfColor::WHITE);
            let bitmap = page.render_with_config(&config)?;
            let size = [bitmap.width() as usize, bitmap.height() as usize];
            Ok((size, bitmap.as_rgba_bytes()))
        })();
        match result {
            Ok((size, rgba)) => self.send(Response::Rendered { key, size, rgba }),
            Err(e) => {
                log::warn!("render {key:?} failed: {e}");
                self.send(Response::RenderFailed { key });
            }
        }
    }

    fn page_text(&mut self, doc: DocId, page_index: usize) {
        let Some(document) = self.docs.get(&doc) else {
            return;
        };
        let mut text = PageText::default();
        if let Ok(page) = document.pages().get(page_index as PdfPageIndex) {
            if let Ok(page_text) = page.text() {
                for ch in page_text.chars().iter() {
                    let Some(c) = ch.unicode_char() else { continue };
                    let rect = ch
                        .loose_bounds()
                        .ok()
                        .filter(|r| r.width().value > 0.0 && r.height().value > 0.0)
                        .map(to_box);
                    text.chars.push(TextChar { ch: c, rect });
                }
            }
            for link in page.links().iter() {
                let Ok(rect) = link.rect() else { continue };
                let target = link_target(&link);
                if let Some(target) = target {
                    text.links.push(PageLink {
                        rect: to_box(rect),
                        target,
                    });
                }
            }
        }
        self.send(Response::PageText {
            doc,
            page: page_index,
            text,
        });
    }

    fn search_step(&mut self) {
        let job = &mut self.searches[0];
        let Some(document) = self.docs.get(&job.doc) else {
            self.searches.remove(0);
            return;
        };
        let page_count = document.pages().len() as usize;
        if job.next_page >= page_count {
            let (doc, generation) = (job.doc, job.generation);
            self.searches.remove(0);
            self.send(Response::SearchDone { doc, generation });
            return;
        }
        let page_index = job.next_page;
        job.next_page += 1;
        let options = PdfSearchOptions::new()
            .match_case(job.query.match_case)
            .match_whole_word(job.query.whole_word);
        let mut hits = Vec::new();
        if let Ok(page) = document.pages().get(page_index as PdfPageIndex)
            && let Ok(text) = page.text()
            && let Ok(search) = text.search(&job.query.text, &options)
        {
            while let Some(segments) = search.find_next() {
                let rects: Vec<PdfBox> = segments.iter().map(|s| to_box(s.bounds())).collect();
                if !rects.is_empty() {
                    hits.push(rects);
                }
                if hits.len() > 10_000 {
                    break;
                }
            }
        }
        let (doc, generation) = (job.doc, job.generation);
        if !hits.is_empty() {
            self.send(Response::SearchResults {
                doc,
                generation,
                page: page_index,
                hits,
            });
        }
    }
}

fn to_box(r: PdfRect) -> PdfBox {
    PdfBox {
        left: r.left().value,
        bottom: r.bottom().value,
        right: r.right().value,
        top: r.top().value,
    }
}

fn link_target(link: &PdfLink) -> Option<LinkTarget> {
    if let Some(action) = link.action() {
        if let Some(uri) = action.as_uri_action() {
            return uri.uri().ok().map(LinkTarget::Uri);
        }
        if let Some(local) = action.as_local_destination_action() {
            return local
                .destination()
                .ok()
                .and_then(|d| d.page_index().ok())
                .map(|p| LinkTarget::Page(p as usize));
        }
    }
    link.destination()
        .and_then(|d| d.page_index().ok())
        .map(|p| LinkTarget::Page(p as usize))
}

const MAX_OUTLINE_ITEMS: usize = 20_000;
const MAX_OUTLINE_DEPTH: usize = 64;

/// Walks a bookmark level via first_child/next_sibling with guards against malformed
/// (cyclic or huge) outlines.
fn read_outline_siblings(first: PdfBookmark, depth: usize, count: &mut usize) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    let mut current = Some(first);
    while let Some(bookmark) = current {
        *count += 1;
        if *count > MAX_OUTLINE_ITEMS {
            break;
        }
        let page = bookmark
            .destination()
            .and_then(|d| d.page_index().ok())
            .or_else(|| {
                bookmark.action().and_then(|a| {
                    a.as_local_destination_action()
                        .and_then(|l| l.destination().ok())
                        .and_then(|d| d.page_index().ok())
                })
            })
            .map(|p| p as usize);
        let children = match bookmark.first_child() {
            Some(child) if depth < MAX_OUTLINE_DEPTH => read_outline_siblings(child, depth + 1, count),
            _ => Vec::new(),
        };
        items.push(OutlineItem {
            title: bookmark.title().unwrap_or_default(),
            page,
            children,
        });
        current = bookmark.next_sibling();
    }
    items
}
