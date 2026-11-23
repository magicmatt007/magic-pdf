//! PDFium engine: a dedicated worker thread that owns the PDFium library and all open
//! documents. PDFium is not thread-safe, so the UI only talks to it through channels.

pub mod commands;
pub mod compare;
pub mod pages;
pub mod redact;
pub mod ocr;
mod lopdf_ops;
pub mod rich;
pub mod shapes;
pub mod textbox;
mod pdf;
#[cfg(windows)]
mod print_windows;
mod worker;

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender};

pub use commands::*;
pub use pages::{HeaderFooter, Margins, PageMapping, PageOp, move_target};
pub use rich::{Align, CharStyle, RichText};
pub use pdf::{FIELD_TEXT, pdf_date_now};

pub struct Engine {
    tx: Sender<Request>,
    rx: Receiver<Response>,
}

impl Engine {
    /// Spawns the worker thread and binds PDFium. `wake` is called whenever a response
    /// is available (used to request a UI repaint).
    pub fn start(wake: impl Fn() + Send + 'static) -> Result<Self, String> {
        let (req_tx, req_rx) = crossbeam_channel::unbounded();
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("pdfium".into())
            .spawn(move || worker::run(req_rx, resp_tx, ready_tx, Box::new(wake)))
            .map_err(|e| e.to_string())?;
        ready_rx
            .recv()
            .map_err(|_| "PDFium worker exited during start-up".to_string())??;
        Ok(Self {
            tx: req_tx,
            rx: resp_rx,
        })
    }

    pub fn send(&self, req: Request) {
        // The worker only stops when the UI drops the engine, so a failed send is harmless.
        let _ = self.tx.send(req);
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }
}

/// Directories searched for the PDFium shared library, in order.
pub fn library_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("MAGIC_PDF_PDFIUM_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        dirs.push(exe_dir.to_path_buf());
        // Development layout: target/{debug,release}/magic-pdf -> ../../third_party/pdfium
        dirs.push(exe_dir.join("../../third_party/pdfium"));
        // Installed on Linux: /usr/lib/magic-pdf
        dirs.push(exe_dir.join("../lib/magic-pdf"));
        // macOS app bundle: Contents/MacOS/magic-pdf -> Contents/Frameworks
        dirs.push(exe_dir.join("../Frameworks"));
    }
    dirs.push(PathBuf::from("third_party/pdfium"));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use rich::Span;
    use super::shapes;
    use std::time::Duration;

    fn recv(engine: &Engine) -> Response {
        let r = engine.rx.recv_timeout(Duration::from_secs(5)).expect("engine response");
        if std::env::var_os("MAGIC_PDF_TRACE").is_some() {
            eprintln!("<- {}", describe(&r));
        }
        r
    }

    /// Receives until `f` matches, skipping unrelated responses.
    fn recv_until<T>(engine: &Engine, mut f: impl FnMut(Response) -> Option<T>) -> T {
        loop {
            if let Some(t) = f(recv(engine)) {
                return t;
            }
        }
    }

    fn describe(r: &Response) -> String {
        match r {
            Response::FormFocus { page, .. } => format!("FormFocus {page:?}"),
            Response::PageChanged { page, .. } => format!("PageChanged {page}"),
            Response::EditState { dirty, .. } => format!("EditState dirty={dirty}"),
            Response::PageWidgets { widgets, .. } => format!("PageWidgets {widgets:?}"),
            Response::Error { message, .. } => format!("Error {message}"),
            _ => "other".into(),
        }
    }

    fn sample() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.pdf")
    }

    /// PDFium is not thread-safe, so engine tests run one at a time.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn start() -> (std::sync::MutexGuard<'static, ()>, Engine) {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let engine = Engine::start(|| {}).expect("PDFium available (run scripts/fetch-pdfium.sh)");
        (guard, engine)
    }

    fn open(engine: &Engine, doc: DocId, path: PathBuf) -> DocInfo {
        engine.send(Request::Open {
            doc,
            path,
            password: None,
        });
        recv_until(engine, |r| match r {
            Response::Opened { doc: d, info } if d == doc => Some(info),
            Response::OpenFailed { error, .. } => panic!("open failed: {error}"),
            _ => None,
        })
    }

    fn render(engine: &Engine, doc: DocId, page: usize, rotation: Rotation, version: u32) -> ([usize; 2], Vec<u8>) {
        let key = RenderKey {
            doc,
            page,
            width_px: 300,
            rotation,
            version,
            tile: 0,
        };
        engine.send(Request::Render { key, priority: 0.0 });
        recv_until(engine, |r| match r {
            Response::Rendered { key: k, size, rgba } if k == key => Some((size, rgba)),
            Response::RenderFailed { key: k } if k == key => panic!("render failed"),
            _ => None,
        })
    }

    fn annots(engine: &Engine, doc: DocId) -> Vec<AnnotInfo> {
        recv_until(engine, |r| match r {
            Response::PageAnnots { doc: d, page: 0, annots, .. } if d == doc => Some(annots),
            _ => None,
        })
    }

    fn edit_state(engine: &Engine) -> (bool, bool, bool) {
        recv_until(engine, |r| match r {
            Response::EditState {
                can_undo,
                can_redo,
                dirty,
                ..
            } => Some((can_undo, can_redo, dirty)),
            _ => None,
        })
    }

    #[test]
    fn open_render_text_search() {
        let (_serial, engine) = start();
        let info = open(&engine, 1, sample());
        assert_eq!(info.page_sizes.len(), 40);
        assert_eq!(info.page_sizes[2], (842.0, 595.0));
        // Page 6 is A4 portrait with /Rotate 90, so it is displayed landscape.
        assert_eq!(info.page_sizes[5], (842.0, 595.0));
        assert_eq!(info.title.as_deref(), Some("magic-pdf sample"));
        assert_eq!(info.outline.len(), 4);
        assert_eq!(info.outline[1].page, Some(10));
        assert_eq!(info.outline[1].children[0].page, Some(14));

        let (size, rgba) = render(&engine, 1, 0, Rotation::R90, 0);
        // Rotated A4 rendered 300 px wide is ~212 px tall.
        assert_eq!(size[0], 300);
        assert!((size[1] as i32 - 212).abs() <= 1, "{size:?}");
        assert_eq!(rgba.len(), size[0] * size[1] * 4);
        assert!(rgba.chunks(4).any(|p| p[0] < 128), "page should not be blank");

        engine.send(Request::PageText { doc: 1, page: 3 });
        let Response::PageText { text, geom, .. } = recv(&engine) else {
            panic!("no text")
        };
        assert_eq!(geom.rotation, Rotation::R0);
        let s: String = text.chars.iter().map(|c| c.ch).collect();
        assert!(s.contains("Page 4"), "{s}");
        assert_eq!(text.links.len(), 2);
        assert!(text.links.iter().any(|l| matches!(l.target, LinkTarget::Page(0))));
        assert!(
            text.links
                .iter()
                .any(|l| matches!(&l.target, LinkTarget::Uri(u) if u.starts_with("https://example.com")))
        );

        engine.send(Request::PageText { doc: 1, page: 5 });
        let Response::PageText { geom, .. } = recv(&engine) else {
            panic!("no text")
        };
        assert_eq!(geom.rotation, Rotation::R90);
        assert_eq!(geom.size, (595.0, 842.0));

        engine.send(Request::Search {
            doc: 1,
            generation: 7,
            query: SearchQuery {
                text: "page 12.".into(),
                match_case: false,
                whole_word: false,
            },
        });
        let mut pages = Vec::new();
        loop {
            match recv(&engine) {
                Response::SearchResults {
                    generation, page, hits, ..
                } => {
                    assert_eq!(generation, 7);
                    assert!(!hits.is_empty());
                    pages.push(page);
                }
                Response::SearchDone { generation, .. } => {
                    assert_eq!(generation, 7);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(pages, vec![11]);

        engine.send(Request::Open {
            doc: 2,
            path: PathBuf::from("/nonexistent.pdf"),
            password: None,
        });
        assert!(matches!(recv(&engine), Response::OpenFailed { password_required: false, .. }));
    }

    #[test]
    fn annotations_edit_undo_save_roundtrip() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.pdf");
        let out = dir.join("out.pdf");
        std::fs::copy(sample(), &src).unwrap();

        let (_serial, engine) = start();
        open(&engine, 1, src.clone());
        let (_, before) = render(&engine, 1, 0, Rotation::R0, 0);

        // Highlight the "Page 1" heading, using its character boxes.
        engine.send(Request::PageText { doc: 1, page: 0 });
        let Response::PageText { text, .. } = recv(&engine) else { panic!() };
        let heading = text
            .chars
            .iter()
            .take(6)
            .filter_map(|c| c.rect)
            .reduce(PdfBox::union)
            .unwrap();
        let new = |kind, rect: PdfBox| NewAnnot {
            kind,
            rect,
            color: [255, 230, 0],
            interior: None,
            width: 2.0,
            contents: String::new(),
            author: "tester".into(),
            quads: Vec::new(),
            ink: Vec::new(),
            font_size: 12.0,
            text_color: [0, 0, 0],
            ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
        };
        let mut hl = new(AnnotKind::Highlight, heading);
        hl.quads = vec![[
            heading.left,
            heading.top,
            heading.right,
            heading.top,
            heading.left,
            heading.bottom,
            heading.right,
            heading.bottom,
        ]];
        engine.send(Request::AddAnnot { doc: 1, page: 0, annot: hl });
        let list = annots(&engine, 1);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, AnnotKind::Highlight);
        assert_eq!(list[0].author, "tester");
        assert_eq!(edit_state(&engine), (true, false, true));

        // PDFium must generate an appearance for our annotation: the heading turns yellow.
        let (_, after) = render(&engine, 1, 0, Rotation::R0, 1);
        let yellowish = |px: &[u8]| px[0] > 200 && px[1] > 180 && px[2] < 120;
        let count = |buf: &[u8]| buf.chunks(4).filter(|p| yellowish(p)).count();
        assert!(count(&after) > count(&before) + 50, "highlight not rendered");

        let mut ink = new(
            AnnotKind::Ink,
            PdfBox {
                left: 100.0,
                bottom: 300.0,
                right: 300.0,
                top: 400.0,
            },
        );
        ink.ink = vec![vec![(100.0, 300.0), (200.0, 400.0), (300.0, 300.0)]];
        engine.send(Request::AddAnnot { doc: 1, page: 0, annot: ink });
        let square = new(
            AnnotKind::Square,
            PdfBox {
                left: 50.0,
                bottom: 50.0,
                right: 150.0,
                top: 120.0,
            },
        );
        engine.send(Request::AddAnnot { doc: 1, page: 0, annot: square });
        let _ = annots(&engine, 1);
        let list = annots(&engine, 1);
        assert_eq!(list.len(), 3);
        let square_index = list.iter().find(|a| a.kind == AnnotKind::Square).unwrap().index;

        // Change colour and comment of the square.
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index: square_index,
            change: AnnotChange {
                color: Some([0, 0, 255]),
                contents: Some("check this".into()),
                ..Default::default()
            },
        });
        let list = annots(&engine, 1);
        let sq = list.iter().find(|a| a.index == square_index).unwrap();
        assert_eq!(sq.color, Some([0, 0, 255]));
        assert_eq!(sq.contents, "check this");

        // Undo the change, redo it.
        engine.send(Request::Undo { doc: 1 });
        let list = annots(&engine, 1);
        let sq = list.iter().find(|a| a.index == square_index).unwrap();
        assert_eq!(sq.color, Some([255, 230, 0]));
        assert_eq!(sq.contents, "");
        engine.send(Request::Redo { doc: 1 });
        let _ = annots(&engine, 1);

        // Delete, undo, redo.
        engine.send(Request::DeleteAnnot {
            doc: 1,
            page: 0,
            index: square_index,
        });
        assert_eq!(annots(&engine, 1).len(), 2);
        engine.send(Request::Undo { doc: 1 });
        assert_eq!(annots(&engine, 1).len(), 3);
        engine.send(Request::Redo { doc: 1 });
        assert_eq!(annots(&engine, 1).len(), 2);

        // Save: the deleted square must be physically gone from the file.
        engine.send(Request::Save {
            doc: 1,
            path: out.clone(),
        });
        let saved = recv_until(&engine, |r| match r {
            Response::Saved { path, .. } => Some(path),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        assert_eq!(saved, out);
        assert_eq!(edit_state(&engine), (true, false, false));
        let bytes = std::fs::read(&out).unwrap();
        let has = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(has("/Highlight") && has("/Ink"));
        assert!(!has("/Square"), "deleted annotation leaked into the saved file");

        if let Some(keep) = std::env::var_os("MAGIC_PDF_KEEP_TEST_OUTPUT") {
            std::fs::copy(&out, keep).unwrap();
        }

        // Re-open the saved copy.
        engine.send(Request::ScanAnnots { doc: 2 });
        open(&engine, 2, out.clone());
        engine.send(Request::ScanAnnots { doc: 2 });
        let list = annots(&engine, 2);
        assert_eq!(list.len(), 2);
        let ink = list.iter().find(|a| a.kind == AnnotKind::Ink).unwrap();
        assert!((ink.width - 2.0).abs() < 0.01);

        // Undoing after saving makes the document dirty again.
        engine.send(Request::Undo { doc: 1 });
        assert_eq!(edit_state(&engine), (true, true, true));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn widgets(engine: &Engine, doc: DocId) -> Vec<WidgetInfo> {
        recv_until(engine, |r| match r {
            Response::PageWidgets { doc: d, page: 0, widgets } if d == doc => Some(widgets),
            _ => None,
        })
    }

    #[test]
    fn form_fill_save_and_print_export() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-form-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("filled.pdf");
        let form = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/form.pdf");

        let (_serial, engine) = start();
        open(&engine, 1, form);
        engine.send(Request::ScanAnnots { doc: 1 });
        let ws = widgets(&engine, 1);
        assert_eq!(ws.len(), 2, "{ws:?}");
        let text = ws.iter().find(|w| w.name == "name").unwrap().clone();
        let check = ws.iter().find(|w| w.name == "agree").unwrap().clone();
        assert_eq!(text.field_type, FIELD_TEXT);
        assert!(!check.checked);

        let click = |w: &WidgetInfo| {
            let (x, y) = ((w.rect.left + w.rect.right) / 2.0, (w.rect.bottom + w.rect.top) / 2.0);
            for event in [FormMouse::Move, FormMouse::Down, FormMouse::Up] {
                engine.send(Request::FormMouse {
                    doc: 1,
                    page: 0,
                    event,
                    x,
                    y,
                    modifiers: 0,
                });
            }
        };
        let focus = |engine: &Engine| {
            recv_until(engine, |r| match r {
                Response::FormFocus { page, .. } => Some(page),
                _ => None,
            })
        };

        // Focus the text field and type.
        click(&text);
        let _ = focus(&engine);
        assert_eq!(focus(&engine), Some(0));
        for c in "Hello".chars() {
            engine.send(Request::FormKey {
                doc: 1,
                key: FormKey::Char(c as u32),
                modifiers: 0,
            });
        }
        // Backspace removes the last character.
        engine.send(Request::FormKey {
            doc: 1,
            key: FormKey::Char(8),
            modifiers: 0,
        });
        // Typing marks the document as having unsaved changes and repaints the page.
        recv_until(&engine, |r| match r {
            Response::EditState { dirty: true, .. } => Some(()),
            _ => None,
        });
        recv_until(&engine, |r| match r {
            Response::PageChanged { page: 0, .. } => Some(()),
            _ => None,
        });

        // Toggle the checkbox, which also moves focus.
        click(&check);
        engine.send(Request::FormKillFocus { doc: 1 });
        recv_until(&engine, |r| match r {
            Response::FormFocus { page: None, .. } => Some(()),
            _ => None,
        });

        engine.send(Request::ExportForPrint {
            doc: 1,
            job: PrintJob {
                printer: None,
                copies: 1,
                pages: None,
                two_sided: false,
                fit_to_page: true,
                title: "t".into(),
            },
        });
        let bytes = recv_until(&engine, |r| match r {
            Response::PrintReady { bytes, .. } => Some(bytes),
            _ => None,
        });
        assert!(bytes.starts_with(b"%PDF"));

        engine.send(Request::Save {
            doc: 1,
            path: out.clone(),
        });
        recv_until(&engine, |r| match r {
            Response::Saved { .. } => Some(()),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        assert!(!edit_state(&engine).2, "saving clears the dirty flag");

        open(&engine, 2, out);
        engine.send(Request::ScanAnnots { doc: 2 });
        let ws = widgets(&engine, 2);
        let text = ws.iter().find(|w| w.name == "name").unwrap();
        let check = ws.iter().find(|w| w.name == "agree").unwrap();
        assert_eq!(text.value, "Hell");
        assert!(check.checked, "checkbox state was not saved");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn pages_changed(engine: &Engine) -> (DocInfo, PageMapping) {
        recv_until(engine, |r| match r {
            Response::PagesChanged { info, mapping, .. } => Some((info, mapping)),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        })
    }

    #[test]
    fn page_operations_with_undo_save_extract_split() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-pages-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_serial, engine) = start();
        let info = open(&engine, 1, sample());
        let original = info.page_sizes.clone();
        assert_eq!(original.len(), 40);

        // An annotation on page 4 (index 3).
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 3,
            annot: NewAnnot {
                kind: AnnotKind::Square,
                rect: PdfBox {
                    left: 50.0,
                    bottom: 50.0,
                    right: 150.0,
                    top: 150.0,
                },
                color: [255, 0, 0],
                interior: None,
                width: 2.0,
                contents: "on page 4".into(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 12.0,
                text_color: [0, 0, 0],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        recv_until(&engine, |r| matches!(r, Response::AnnotAdded { .. }).then_some(()));

        let pages_with_note = |engine: &Engine| -> Vec<usize> {
            engine.send(Request::ScanAnnots { doc: 1 });
            let mut pages = Vec::new();
            recv_until(engine, |r| match r {
                Response::PageAnnots { page, annots, .. } => {
                    if annots.iter().any(|a| a.contents == "on page 4") {
                        pages.push(page);
                    }
                    None
                }
                Response::AnnotScanDone { .. } => Some(()),
                _ => None,
            });
            pages
        };

        // Delete page 2: the annotated page becomes page 3 (index 2).
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::Delete { pages: vec![1] },
        });
        let (info, m) = pages_changed(&engine);
        assert_eq!(info.page_sizes.len(), 39);
        assert_eq!(m.map[3], Some(2));
        assert_eq!(pages_with_note(&engine), vec![2]);

        // Move the first page to the end.
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::Move { pages: vec![0], to: 38 },
        });
        let (info, _) = pages_changed(&engine);
        assert_eq!(info.page_sizes[38], original[0]);
        assert_eq!(info.page_sizes[0], original[2]);

        // Rotate the third page: width and height swap.
        let (w, h) = info.page_sizes[2];
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::Rotate {
                pages: vec![2],
                quarter_turns: 1,
            },
        });
        let (info, m) = pages_changed(&engine);
        assert_eq!(info.page_sizes[2], (h, w));
        assert_eq!(m.changed, vec![2]);

        // Insert a blank page at the start and another PDF at the end.
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::InsertBlank {
                at: 0,
                size: (100.0, 200.0),
            },
        });
        let (info, _) = pages_changed(&engine);
        assert_eq!(info.page_sizes[0], (100.0, 200.0));
        let form = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/form.pdf");
        engine.send(Request::InsertFile {
            doc: 1,
            path: form,
            at: 40,
        });
        let (info, _) = pages_changed(&engine);
        assert_eq!(info.page_sizes.len(), 41);
        // Deleted page 2, moved page 1 to the end, inserted a page in front: index 2.
        assert_eq!(pages_with_note(&engine), vec![2]);

        // Invalid operations are refused without changing anything.
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::Delete {
                pages: (0..41).collect(),
            },
        });
        recv_until(&engine, |r| matches!(r, Response::Error { .. }).then_some(()));

        // Undo everything: back to the original document, annotation on page 4.
        let mut sizes_now = Vec::new();
        for _ in 0..5 {
            engine.send(Request::Undo { doc: 1 });
            sizes_now = pages_changed(&engine).0.page_sizes;
        }
        assert_eq!(sizes_now, original);
        assert_eq!(pages_with_note(&engine), vec![3]);

        // Redo two steps (delete page 2, move first page to the end) and save.
        for _ in 0..2 {
            engine.send(Request::Redo { doc: 1 });
            pages_changed(&engine);
        }
        let out = dir.join("edited.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let reopened = open(&engine, 2, out);
        assert_eq!(reopened.page_sizes.len(), 39);
        assert_eq!(reopened.page_sizes[38], original[0]);

        // Extract two pages, then split into files of 15 pages.
        let extracted = dir.join("two.pdf");
        engine.send(Request::ExtractPages {
            doc: 1,
            pages: vec![0, 5],
            path: extracted.clone(),
        });
        recv_until(&engine, |r| matches!(r, Response::Exported { .. }).then_some(()));
        assert_eq!(open(&engine, 3, extracted).page_sizes.len(), 2);
        let parts = dir.join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        engine.send(Request::SplitDocument {
            doc: 1,
            every: 15,
            dir: parts.clone(),
            stem: "doc".into(),
        });
        recv_until(&engine, |r| match r {
            Response::Exported { .. } => Some(()),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        let counts: Vec<usize> = (1..=3)
            .map(|n| open(&engine, 10 + n, parts.join(format!("doc-{n:02}.pdf"))).page_sizes.len())
            .collect();
        assert_eq!(counts, vec![15, 15, 9]);
        // Splitting again would overwrite: refused.
        engine.send(Request::SplitDocument {
            doc: 1,
            every: 15,
            dir: parts,
            stem: "doc".into(),
        });
        recv_until(&engine, |r| matches!(r, Response::Error { .. }).then_some(()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shape_fill_can_be_turned_on_and_off() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let rect = PdfBox {
            left: 100.0,
            bottom: 300.0,
            right: 300.0,
            top: 500.0,
        };
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 0,
            annot: NewAnnot {
                kind: AnnotKind::Square,
                rect,
                color: [0, 0, 255],
                interior: None,
                width: 2.0,
                contents: String::new(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 12.0,
                text_color: [0, 0, 0],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        let index = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            _ => None,
        });
        // Count red pixels inside the shape (page 0 rendered 300 px wide: 595 pt).
        let red_inside = |engine: &Engine, version| {
            let (size, rgba) = render(engine, 1, 0, Rotation::R0, version);
            let s = size[0] as f32 / 595.0;
            let (x0, x1) = ((120.0 * s) as usize, (280.0 * s) as usize);
            let (y0, y1) = (((842.0 - 480.0) * s) as usize, ((842.0 - 320.0) * s) as usize);
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &rgba[(y * size[0] + x) * 4..][..4];
                    if p[0] > 200 && p[1] < 80 && p[2] < 80 {
                        n += 1;
                    }
                }
            }
            n
        };
        assert_eq!(red_inside(&engine, 1), 0);
        let set = |interior| Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index,
            change: AnnotChange {
                interior: Some(interior),
                ..Default::default()
            },
        };
        engine.send(set(Some([255, 0, 0])));
        let a = annots(&engine, 1);
        assert_eq!(a.iter().find(|a| a.index == index).unwrap().interior, Some([255, 0, 0]));
        assert!(red_inside(&engine, 2) > 1000, "fill not drawn");
        engine.send(set(None));
        let a = annots(&engine, 1);
        assert_eq!(a.iter().find(|a| a.index == index).unwrap().interior, None, "fill not removed");
        assert_eq!(red_inside(&engine, 3), 0, "fill still drawn");
        // Again after the appearance was regenerated (PDFium then hides colours).
        engine.send(set(Some([255, 0, 0])));
        let _ = annots(&engine, 1);
        assert!(red_inside(&engine, 4) > 1000);
        engine.send(set(None));
        let a = annots(&engine, 1);
        assert_eq!(a.iter().find(|a| a.index == index).unwrap().interior, None);
        assert_eq!(red_inside(&engine, 5), 0);
        // Undo brings the fill back.
        engine.send(Request::Undo { doc: 1 });
        let a = annots(&engine, 1);
        assert_eq!(a.iter().find(|a| a.index == index).unwrap().interior, Some([255, 0, 0]));
        assert!(red_inside(&engine, 6) > 1000);
    }

    #[test]
    fn text_box_renders_wraps_and_saves() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-textbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_serial, engine) = start();
        let form = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/form.pdf");
        open(&engine, 1, form);
        // An empty area of the page, 150 pt wide.
        let rect = PdfBox {
            left: 300.0,
            bottom: 380.0,
            right: 450.0,
            top: 400.0,
        };
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 0,
            annot: NewAnnot {
                kind: AnnotKind::FreeText,
                rect,
                color: [220, 30, 30],
                interior: Some([255, 255, 255]),
                width: 1.0,
                contents: "Grüße und Привет – this sentence is long enough to wrap".into(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 12.0,
                text_color: [0, 0, 255],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        let index = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        let list = annots(&engine, 1);
        let tb = list.iter().find(|a| a.index == index).unwrap().clone();
        assert_eq!(tb.kind, AnnotKind::FreeText);
        assert_eq!(tb.font_size, Some(12.0));
        assert_eq!(tb.text_color, Some([0, 0, 255]));
        assert_eq!(tb.rect.top, 400.0, "the box keeps its top edge");
        assert!(tb.rect.top - tb.rect.bottom > 40.0, "box grew to fit the wrapped lines: {:?}", tb.rect);

        // Text pixels inside the box (rendered 600 px wide: 595 pt page).
        let blue_in_box = |engine: &Engine, version, r: PdfBox| {
            let key = RenderKey {
                doc: 1,
                page: 0,
                width_px: 600,
                rotation: Rotation::R0,
                version,
                tile: 0,
            };
            engine.send(Request::Render { key, priority: 0.0 });
            let (size, rgba) = recv_until(engine, |m| match m {
                Response::Rendered { key: k, size, rgba } if k == key => Some((size, rgba)),
                _ => None,
            });
            let s = size[0] as f32 / 595.0;
            let mut n = 0;
            for y in ((842.0 - r.top) * s) as usize..((842.0 - r.bottom) * s) as usize {
                for x in (r.left * s) as usize..(r.right * s) as usize {
                    let p = &rgba[(y * size[0] + x) * 4..][..4];
                    if p[2] > 150 && p[0] < 100 && p[1] < 100 {
                        n += 1;
                    }
                }
            }
            n
        };
        assert!(blue_in_box(&engine, 1, tb.rect) > 200, "text not drawn");

        // Recolour the text: the appearance is regenerated.
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index,
            change: AnnotChange {
                text_color: Some([0, 0, 0]),
                ..Default::default()
            },
        });
        let list = annots(&engine, 1);
        assert_eq!(list.iter().find(|a| a.index == index).unwrap().text_color, Some([0, 0, 0]));
        assert_eq!(blue_in_box(&engine, 2, tb.rect), 0, "old colour still drawn");

        let out = dir.join("tb.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let bytes = std::fs::read(&out).unwrap();
        let has = |n: &str| bytes.windows(n.len()).any(|w| w == n.as_bytes());
        assert!(has("/FreeText") && has("/DA"));
        assert!(!has("/FontFile"), "no font must be embedded in the document");
        if let Some(keep) = std::env::var_os("MAGIC_PDF_KEEP_TEXTBOX") {
            std::fs::copy(&out, keep).unwrap();
        }

        open(&engine, 2, out);
        engine.send(Request::ScanAnnots { doc: 2 });
        let list = recv_until(&engine, |r| match r {
            Response::PageAnnots { doc: 2, page: 0, annots, .. } => Some(annots),
            _ => None,
        });
        let tb2 = list.iter().find(|a| a.kind == AnnotKind::FreeText).unwrap();
        assert!(tb2.contents.starts_with("Grüße und Привет"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn moving_and_resizing_annotations() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let add = |kind, rect: PdfBox, ink: Vec<Vec<(f32, f32)>>| {
            engine.send(Request::AddAnnot {
                doc: 1,
                page: 0,
                annot: NewAnnot {
                    kind,
                    rect,
                    color: [255, 0, 0],
                    interior: None,
                    width: 2.0,
                    contents: "x".into(),
                    author: String::new(),
                    quads: Vec::new(),
                    ink,
                    font_size: 12.0,
                    text_color: [0, 0, 0],
                    ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
                },
            });
            recv_until(&engine, |r| match r {
                Response::AnnotAdded { index, .. } => Some(index),
                _ => None,
            })
        };
        let b = |l: f32, bo: f32, r: f32, t: f32| PdfBox {
            left: l,
            bottom: bo,
            right: r,
            top: t,
        };
        let square = add(AnnotKind::Square, b(100.0, 100.0, 200.0, 150.0), Vec::new());
        let ink = add(
            AnnotKind::Ink,
            b(98.0, 298.0, 202.0, 402.0),
            vec![vec![(100.0, 300.0), (200.0, 400.0)]],
        );
        let rect_of = |list: &[AnnotInfo], i| list.iter().find(|a| a.index == i).unwrap().rect;
        let drain = |engine: &Engine| while engine.rx.recv_timeout(Duration::from_millis(100)).is_ok() {};
        let mv = |index, rect| {
            drain(&engine);
            engine.send(Request::UpdateAnnot {
                doc: 1,
                page: 0,
                index,
                change: AnnotChange {
                    rect: Some(rect),
                    ..Default::default()
                },
            });
            annots(&engine, 1)
        };

        // Move the square.
        let list = mv(square, b(300.0, 500.0, 400.0, 550.0));
        assert_eq!(rect_of(&list, square), b(300.0, 500.0, 400.0, 550.0));
        // Red border pixels appear at the new place and not at the old one.
        // Rendered at 1200 px (about 2 px per point) so thin strokes are solid.
        let red_at = |engine: &Engine, version, x: f32, y: f32| {
            let key = RenderKey {
                doc: 1,
                page: 0,
                width_px: 1200,
                rotation: Rotation::R0,
                version,
                tile: 0,
            };
            engine.send(Request::Render { key, priority: 0.0 });
            let (size, rgba) = recv_until(engine, |m| match m {
                Response::Rendered { key: k, size, rgba } if k == key => Some((size, rgba)),
                _ => None,
            });
            let s = size[0] as f32 / 595.0;
            let (px, py) = ((x * s) as usize, ((842.0 - y) * s) as usize);
            (py.saturating_sub(5)..py + 5).any(|yy| {
                (px.saturating_sub(5)..px + 5).any(|xx| {
                    let p = &rgba[(yy * size[0] + xx) * 4..][..4];
                    p[0] > 200 && p[1] < 80 && p[2] < 80
                })
            })
        };
        assert!(red_at(&engine, 1, 300.5, 525.0), "square drawn at new place");
        assert!(!red_at(&engine, 1, 100.5, 125.0), "square gone from old place");

        // Resize the ink drawing to twice the width: its stroke scales along.
        let list = mv(ink, b(98.0, 298.0, 306.0, 402.0));
        assert_eq!(rect_of(&list, ink), b(98.0, 298.0, 306.0, 402.0));
        assert!(red_at(&engine, 2, 302.0, 400.0), "stroke end moved with the resize");

        // Undo the resize: stroke and rectangle return.
        drain(&engine);
        engine.send(Request::Undo { doc: 1 });
        let list = annots(&engine, 1);
        // PDFium keeps a small margin around ink strokes; the frame must be back
        // around the original stroke (100..200), not the resized one.
        let r = rect_of(&list, ink);
        assert!(r.left <= 100.0 && r.right >= 200.0 && r.right < 210.0, "{r:?}");
        assert!(red_at(&engine, 3, 200.0, 400.0));
        assert!(!red_at(&engine, 3, 302.0, 400.0));
    }

    #[test]
    fn colours_survive_saving_and_reopening() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-colours-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 0,
            annot: NewAnnot {
                kind: AnnotKind::Square,
                rect: PdfBox {
                    left: 100.0,
                    bottom: 600.0,
                    right: 200.0,
                    top: 700.0,
                },
                color: [0, 110, 230],
                interior: Some([255, 255, 255]),
                width: 10.0,
                contents: String::new(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 12.0,
                text_color: [0, 0, 0],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        recv_until(&engine, |r| matches!(r, Response::AnnotAdded { .. }).then_some(()));
        // Rendering makes PDFium generate (and store) the appearance.
        render(&engine, 1, 0, Rotation::R0, 1);
        let out = dir.join("c.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));

        open(&engine, 2, out);
        engine.send(Request::ScanAnnots { doc: 2 });
        let list = recv_until(&engine, |r| match r {
            Response::PageAnnots { doc: 2, page: 0, annots, .. } => Some(annots),
            _ => None,
        });
        let sq = list.iter().find(|a| a.kind == AnnotKind::Square).unwrap();
        assert_eq!(sq.color, Some([0, 110, 230]), "border colour after reopening");
        assert_eq!(sq.interior, Some([255, 255, 255]), "fill after reopening");
        assert!((sq.width - 10.0).abs() < 0.01);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rich_text_boxes() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-rich-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_serial, engine) = start();
        let form = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/form.pdf");
        open(&engine, 1, form);
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 0,
            annot: NewAnnot {
                kind: AnnotKind::FreeText,
                rect: PdfBox {
                    left: 300.0,
                    bottom: 380.0,
                    right: 500.0,
                    top: 400.0,
                },
                color: [220, 30, 30],
                interior: None,
                width: 1.0,
                contents: "plain".into(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 12.0,
                text_color: [0, 0, 0],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        let index = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            _ => None,
        });
        let base = CharStyle::default();
        let rich = RichText {
            spans: vec![
                Span {
                    text: "Fett ".into(),
                    style: CharStyle { bold: true, ..base },
                },
                Span {
                    text: "kursiv ".into(),
                    style: CharStyle { italic: true, ..base },
                },
                Span {
                    text: "rot".into(),
                    style: CharStyle {
                        color: [220, 30, 30],
                        underline: true,
                        size: 18.0,
                        ..base
                    },
                },
                Span {
                    text: "\nGrüße, Привет".into(),
                    style: CharStyle {
                        bold: true,
                        italic: true,
                        ..base
                    },
                },
            ],
            align: Align::Center,
        };
        let drain = |engine: &Engine| while engine.rx.recv_timeout(Duration::from_millis(100)).is_ok() {};
        drain(&engine);
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index,
            change: AnnotChange {
                rich: Some(rich.clone()),
                ..Default::default()
            },
        });
        let list = annots(&engine, 1);
        let tb = list.iter().find(|a| a.index == index).unwrap();
        assert_eq!(tb.rich.as_ref(), Some(&rich));
        assert_eq!(tb.contents, "Fett kursiv rot\nGrüße, Привет");
        // Two lines: the 18 pt line and a 12 pt line.
        let h = tb.rect.top - tb.rect.bottom;
        assert!((h - (8.0 + 18.0 * 1.25 + 12.0 * 1.25)).abs() < 0.01, "{h}");

        // Red text pixels exist (the 18 pt "rot").
        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 1);
        let red = rgba.chunks(4).filter(|p| p[0] > 180 && p[1] < 90 && p[2] < 90).count();
        assert!(red > 30, "red text not drawn: {red} px at {size:?}");

        let out = dir.join("rich.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let bytes = std::fs::read(&out).unwrap();
        let has = |n: &str| bytes.windows(n.len()).any(|w| w == n.as_bytes());
        assert!(has("/RC") && has("/DS"));
        assert!(!has("/FontFile"), "no font embedded");
        if let Some(keep) = std::env::var_os("MAGIC_PDF_KEEP_RICH") {
            std::fs::copy(&out, keep).unwrap();
        }

        open(&engine, 2, out);
        engine.send(Request::ScanAnnots { doc: 2 });
        let list = recv_until(&engine, |r| match r {
            Response::PageAnnots { doc: 2, page: 0, annots, .. } => Some(annots),
            _ => None,
        });
        let again = list.iter().find(|a| a.kind == AnnotKind::FreeText).unwrap();
        assert_eq!(again.rich.as_ref(), Some(&rich), "styles survive saving");

        // Undo goes back to the plain text.
        drain(&engine);
        engine.send(Request::Undo { doc: 1 });
        let list = annots(&engine, 1);
        let tb = list.iter().find(|a| a.index == index).unwrap();
        assert_eq!(tb.contents, "plain");
        assert_eq!(tb.rich.as_ref().map(|r| r.spans.len()), Some(1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn text_boxes_resize_but_never_below_their_text() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 0,
            annot: NewAnnot {
                kind: AnnotKind::FreeText,
                rect: PdfBox {
                    left: 100.0,
                    bottom: 680.0,
                    right: 300.0,
                    top: 700.0,
                },
                color: [0, 0, 0],
                interior: None,
                width: 1.0,
                contents: "one line".into(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 12.0,
                text_color: [0, 0, 0],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        let index = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            _ => None,
        });
        let drain = |engine: &Engine| while engine.rx.recv_timeout(Duration::from_millis(100)).is_ok() {};
        let resize = |rect: PdfBox| {
            drain(&engine);
            engine.send(Request::UpdateAnnot {
                doc: 1,
                page: 0,
                index,
                change: AnnotChange {
                    rect: Some(rect),
                    ..Default::default()
                },
            });
            annots(&engine, 1).into_iter().find(|a| a.index == index).unwrap().rect
        };
        // Taller and wider than the text: kept.
        let big = PdfBox {
            left: 100.0,
            bottom: 500.0,
            right: 400.0,
            top: 700.0,
        };
        assert_eq!(resize(big), big);
        // Much smaller than the text: the height grows back to fit one line.
        let r = resize(PdfBox {
            left: 100.0,
            bottom: 695.0,
            right: 400.0,
            top: 700.0,
        });
        assert_eq!(r.top, 700.0);
        assert!((r.top - r.bottom - (8.0 + 15.0)).abs() < 0.01, "{r:?}");
    }

    #[test]
    fn new_text_boxes_keep_the_chosen_text_style() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        engine.send(Request::AddAnnot {
            doc: 1,
            page: 0,
            annot: NewAnnot {
                kind: AnnotKind::FreeText,
                rect: PdfBox {
                    left: 100.0,
                    bottom: 680.0,
                    right: 300.0,
                    top: 700.0,
                },
                color: [0, 0, 0],
                interior: None,
                width: 1.0,
                contents: String::new(),
                author: String::new(),
                quads: Vec::new(),
                ink: Vec::new(),
                font_size: 20.0,
                text_color: [0, 110, 230],
                ..NewAnnot::new(AnnotKind::Other, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, [0, 0, 0])
            },
        });
        let index = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            _ => None,
        });
        let list = annots(&engine, 1);
        let tb = list.iter().find(|a| a.index == index).unwrap();
        assert_eq!(tb.font_size, Some(20.0), "size from the tool options");
        assert_eq!(tb.text_color, Some([0, 110, 230]), "colour from the tool options");
        let first = tb.rich.as_ref().unwrap().first_style(CharStyle::default());
        assert_eq!((first.size, first.color), (20.0, [0, 110, 230]), "typing starts in that style");
    }

    /// Discards responses until the engine has been quiet for a moment.
    fn drain(engine: &Engine) {
        while engine.rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
    }

    fn add(engine: &Engine, doc: DocId, annot: NewAnnot) -> usize {
        engine.send(Request::AddAnnot { doc, page: 0, annot });
        recv_until(engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        })
    }

    fn pdf_box(left: f32, bottom: f32, right: f32, top: f32) -> PdfBox {
        PdfBox {
            left,
            bottom,
            right,
            top,
        }
    }

    /// Fraction of pixels in a region (fractions of the page, top-left origin) that
    /// are mostly `channel` (0 red, 2 blue).
    fn coloured(size: [usize; 2], rgba: &[u8], region: (f32, f32, f32, f32), channel: usize) -> usize {
        let (x0, y0, x1, y1) = region;
        let mut n = 0;
        for y in (y0 * size[1] as f32) as usize..(y1 * size[1] as f32) as usize {
            for x in (x0 * size[0] as f32) as usize..(x1 * size[0] as f32) as usize {
                let p = &rgba[(y * size[0] + x) * 4..][..4];
                if p[channel] > 180 && p[(channel + 1) % 3] < 90 {
                    n += 1;
                }
            }
        }
        n
    }

    fn save_and_reopen(engine: &Engine, doc: DocId, name: &str) -> Vec<AnnotInfo> {
        let out = std::env::temp_dir().join(format!("magic-pdf-{name}-{}.pdf", std::process::id()));
        engine.send(Request::Save { doc, path: out.clone() });
        recv_until(engine, |r| match r {
            Response::Saved { .. } => Some(()),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        if let Some(dir) = std::env::var_os("MAGIC_PDF_KEEP_DIR") {
            std::fs::copy(&out, PathBuf::from(dir).join(format!("{name}.pdf"))).unwrap();
        }
        open(engine, doc + 100, out.clone());
        engine.send(Request::ScanAnnots { doc: doc + 100 });
        let list = annots(engine, doc + 100);
        std::fs::remove_file(out).ok();
        list
    }

    #[test]
    fn arrows_polygons_and_clouds_draw_move_and_survive_saving() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let mut arrow = NewAnnot::new(AnnotKind::Arrow, pdf_box(0.0, 0.0, 0.0, 0.0), [230, 0, 0]);
        arrow.width = 3.0;
        arrow.vertices = vec![(100.0, 400.0), (300.0, 400.0)];
        let a = add(&engine, 1, arrow);
        let mut cloud = NewAnnot::new(AnnotKind::Cloud, pdf_box(0.0, 0.0, 0.0, 0.0), [0, 0, 230]);
        cloud.vertices = vec![(350.0, 100.0), (500.0, 100.0), (500.0, 250.0), (350.0, 250.0)];
        cloud.interior = Some([0, 0, 255]);
        let c = add(&engine, 1, cloud);
        let list = annots(&engine, 1);
        let info = |i: usize| list.iter().find(|x| x.index == i).unwrap().clone();
        assert_eq!(info(a).kind, AnnotKind::Arrow);
        assert_eq!(info(a).vertices, vec![(100.0, 400.0), (300.0, 400.0)]);
        assert_eq!(info(a).color, Some([230, 0, 0]));
        let r = info(a).rect;
        assert!(r.left < 100.0 && r.right > 300.0 && r.top > 405.0, "rect covers the head: {r:?}");
        assert_eq!(info(c).kind, AnnotKind::Cloud);
        assert_eq!(info(c).interior, Some([0, 0, 255]));

        // Page 1 is A4 (595 x 842); the arrow is red around y = 400 (52% from the top).
        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 1);
        assert!(coloured(size, &rgba, (0.17, 0.5, 0.5, 0.55), 0) > 20, "red arrow drawn");
        assert!(coloured(size, &rgba, (0.62, 0.72, 0.82, 0.86), 2) > 100, "filled blue cloud drawn");

        // Moving the arrow moves its points.
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index: a,
            change: AnnotChange {
                transform: Some([1.0, 1.0, 0.0, -100.0]),
                ..Default::default()
            },
        });
        let list = annots(&engine, 1);
        let moved = list.iter().find(|x| x.index == a).unwrap();
        assert_eq!(moved.vertices, vec![(100.0, 300.0), (300.0, 300.0)]);
        assert_eq!(moved.color, Some([230, 0, 0]), "style kept");

        let list = save_and_reopen(&engine, 1, "shapes");
        assert!(list.iter().any(|x| x.kind == AnnotKind::Arrow && x.vertices == vec![(100.0, 300.0), (300.0, 300.0)]));
        assert!(list.iter().any(|x| x.kind == AnnotKind::Cloud));
    }

    #[test]
    fn stamps_size_themselves_scale_and_copy() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        // A text stamp placed with a click (zero-size rect) gets its natural size.
        let mut stamp = NewAnnot::new(AnnotKind::Stamp, pdf_box(200.0, 0.0, 200.0, 600.0), [200, 0, 0]);
        stamp.stamp = Some(StampSpec::Text {
            title: "APPROVED".into(),
            subtitle: "matthias, 2026-11-22".into(),
        });
        let s = add(&engine, 1, stamp);
        // A half-transparent 2x1 image: left pixel blue, right pixel transparent.
        let mut image = NewAnnot::new(AnnotKind::Stamp, pdf_box(100.0, 100.0, 200.0, 150.0), [0, 0, 0]);
        image.stamp = Some(StampSpec::Image {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 255, 255, 0, 0, 0, 0],
        });
        let i = add(&engine, 1, image);
        let list = annots(&engine, 1);
        let info = |list: &[AnnotInfo], i: usize| list.iter().find(|x| x.index == i).unwrap().clone();
        let r = info(&list, s).rect;
        assert!((r.left + r.right) / 2.0 - 200.0 < 0.5 && (r.top + r.bottom) / 2.0 - 600.0 < 0.5, "centred: {r:?}");
        assert!(r.right - r.left > 100.0 && r.top - r.bottom > 20.0, "natural size: {r:?}");
        assert_eq!(info(&list, s).contents, "APPROVED");

        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 1);
        let stamp_region = (r.left / 595.0, 1.0 - r.top / 842.0, r.right / 595.0, 1.0 - r.bottom / 842.0);
        assert!(coloured(size, &rgba, stamp_region, 0) > 20, "red stamp drawn");
        // Image stamp: page y 100..150 is 82%..88% from the top; x 100..150 blue, 150..200 transparent.
        assert!(coloured(size, &rgba, (0.17, 0.83, 0.25, 0.87), 2) > 50, "blue half");
        assert_eq!(coloured(size, &rgba, (0.26, 0.83, 0.33, 0.87), 2), 0, "transparent half");

        // Enlarging the image stamp scales its picture.
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index: i,
            change: AnnotChange {
                rect: Some(pdf_box(100.0, 100.0, 400.0, 150.0)),
                ..Default::default()
            },
        });
        drain(&engine);
        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 2);
        assert!(coloured(size, &rgba, (0.17, 0.83, 0.42, 0.87), 2) > 200, "blue half now 150 pt wide");

        let dark = |size: [usize; 2], rgba: &[u8]| {
            let (x0, x1, y0, y1) = ((0.44 * size[0] as f32) as usize, (0.66 * size[0] as f32) as usize, (0.62 * size[1] as f32) as usize, (0.66 * size[1] as f32) as usize);
            (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).filter(|&(x, y)| rgba[(y * size[0] + x) * 4..][..3].iter().all(|&c| c < 60)).count()
        };
        let dark_before = dark(size, &rgba);
        // Copy the image stamp and paste it elsewhere: the picture comes along.
        engine.send(Request::CopyAnnot { doc: 1, page: 0, index: i });
        engine.send(Request::PasteAnnot {
            doc: 1,
            page: 0,
            at: Some((250.0, 300.0)),
        });
        let pasted = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        let list = annots(&engine, 1);
        let p = info(&list, pasted).rect;
        assert_eq!(((p.left + p.right) / 2.0, (p.top + p.bottom) / 2.0), (250.0, 300.0));
        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 3);
        // y 275..325 is 61%..67% from the top; x 100..250 blue, 250..400 transparent.
        assert!(coloured(size, &rgba, (0.18, 0.62, 0.41, 0.66), 2) > 200, "pasted picture");
        assert_eq!(coloured(size, &rgba, (0.44, 0.62, 0.66, 0.66), 2), 0, "pasted transparency");
        assert_eq!(dark(size, &rgba), dark_before, "the transparent half stays see-through");

        let list = save_and_reopen(&engine, 1, "stamps");
        assert_eq!(list.iter().filter(|x| x.kind == AnnotKind::Stamp).count(), 3);
    }

    #[test]
    fn copied_text_boxes_keep_their_formatting_and_paste_next_to_the_original() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let mut tb = NewAnnot::new(AnnotKind::FreeText, pdf_box(100.0, 600.0, 300.0, 640.0), [0, 0, 0]);
        let mut rich = RichText::plain("Hello world", CharStyle::default());
        rich.spans[0].style.bold = true;
        tb.rich = Some(rich.clone());
        let t = add(&engine, 1, tb);
        engine.send(Request::CopyAnnot { doc: 1, page: 0, index: t });
        engine.send(Request::PasteAnnot { doc: 1, page: 0, at: None });
        let pasted = recv_until(&engine, |r| match r {
            Response::AnnotAdded { index, .. } => Some(index),
            _ => None,
        });
        let list = annots(&engine, 1);
        let (orig, copy) = (
            list.iter().find(|x| x.index == t).unwrap(),
            list.iter().find(|x| x.index == pasted).unwrap(),
        );
        assert_eq!(copy.rich.as_ref().unwrap().text(), "Hello world");
        assert!(copy.rich.as_ref().unwrap().spans[0].style.bold);
        assert_eq!((copy.rect.left - orig.rect.left, copy.rect.top - orig.rect.top), (12.0, -12.0));
        // Undo removes the pasted copy.
        engine.send(Request::Undo { doc: 1 });
        let list = annots(&engine, 1);
        assert!(!list.iter().any(|x| x.index == pasted));
    }

    #[test]
    fn comment_replies_and_status_are_undoable_and_saved() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let mut note = NewAnnot::new(AnnotKind::Note, pdf_box(100.0, 700.0, 120.0, 720.0), [255, 200, 0]);
        note.contents = "Check this".into();
        let n = add(&engine, 1, note);
        drain(&engine);
        let replies = vec![Reply {
            author: "Ann".into(),
            date: "D:20261122120000".into(),
            text: "Done".into(),
        }];
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index: n,
            change: AnnotChange {
                replies: Some(replies.clone()),
                status: Some("Completed".into()),
                ..Default::default()
            },
        });
        let list = annots(&engine, 1);
        let got = list.iter().find(|x| x.index == n).unwrap();
        assert_eq!((got.replies.clone(), got.status.as_str()), (replies.clone(), "Completed"));
        engine.send(Request::Undo { doc: 1 });
        let list = annots(&engine, 1);
        let got = list.iter().find(|x| x.index == n).unwrap();
        assert!(got.replies.is_empty() && got.status.is_empty(), "{got:?}");
        engine.send(Request::Redo { doc: 1 });
        drain(&engine);
        let list = save_and_reopen(&engine, 1, "replies");
        let got = list.iter().find(|x| x.contents == "Check this").unwrap();
        assert_eq!((got.replies.clone(), got.status.as_str()), (replies, "Completed"));
    }

    fn search_pages(engine: &Engine, doc: DocId, text: &str) -> Vec<(usize, Vec<Vec<PdfBox>>)> {
        engine.send(Request::Search {
            doc,
            generation: 1,
            query: SearchQuery {
                text: text.into(),
                match_case: false,
                whole_word: false,
            },
        });
        let mut found = Vec::new();
        loop {
            match recv(engine) {
                Response::SearchResults { doc: d, page, hits, .. } if d == doc => found.push((page, hits)),
                Response::SearchDone { doc: d, .. } if d == doc => return found,
                _ => {}
            }
        }
    }

    fn run_ocr(engine: &Engine, doc: DocId, pages: Vec<usize>, exe: &std::path::Path, force: bool) -> OcrOutcome {
        engine.send(Request::Ocr {
            doc,
            pages,
            exe: exe.to_path_buf(),
            languages: "eng".into(),
            force,
        });
        loop {
            let r = engine.rx.recv_timeout(Duration::from_secs(120)).expect("OCR response");
            if let Response::OcrFinished { doc: d, result } = r
                && d == doc
            {
                return result;
            }
        }
    }

    #[test]
    fn ocr_adds_a_searchable_text_layer() {
        let Some(exe) = ocr::find_tesseract() else {
            eprintln!("SKIPPED ocr_adds_a_searchable_text_layer: Tesseract is not installed");
            return;
        };
        if !ocr::languages(&exe).is_ok_and(|l| l.iter().any(|l| l == "eng")) {
            eprintln!("SKIPPED ocr_adds_a_searchable_text_layer: Tesseract has no English language data");
            return;
        }
        let (_serial, engine) = start();
        let scanned = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scanned.pdf");
        open(&engine, 1, scanned);
        assert!(search_pages(&engine, 1, "fox").is_empty(), "image-only pages have no text");

        let result = run_ocr(&engine, 1, vec![0, 1], &exe, false);
        let OcrOutcome::Done {
            recognised: 2,
            skipped: 0,
            words,
        } = result
        else {
            panic!("unexpected result {result:?}")
        };
        assert!(words > 20, "{words} words");

        let hits = search_pages(&engine, 1, "fox");
        assert_eq!(hits.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec![0, 1]);
        // "fox" sits about 173 pt from the left and 150 pt from the top of page 1.
        let r = hits[0].1[0][0];
        assert!((160.0..190.0).contains(&r.left) && (670.0..710.0).contains(&r.bottom), "{r:?}");
        assert!(r.right - r.left > r.top - r.bottom, "horizontal: {r:?}");
        // Page 2 is stored landscape with /Rotate 90: the text runs up the page.
        let r = hits[1].1[0][0];
        assert!(r.top - r.bottom > r.right - r.left, "rotated with the page: {r:?}");

        // Words are separated, lines broken.
        engine.send(Request::PageText { doc: 1, page: 0 });
        let text = recv_until(&engine, |r| match r {
            Response::PageText { text, .. } => Some(text.chars.iter().map(|c| c.ch).collect::<String>()),
            _ => None,
        });
        assert!(text.contains("Invoice number 4711"), "{text:?}");
        assert!(text.contains("brown fox"), "{text:?}");

        // Pages with text are skipped the second time.
        assert_eq!(run_ocr(&engine, 1, vec![0], &exe, false), OcrOutcome::Done {
            recognised: 0,
            skipped: 1,
            words: 0
        });

        // One undo step removes the whole layer.
        engine.send(Request::Undo { doc: 1 });
        assert!(search_pages(&engine, 1, "fox").is_empty());
        engine.send(Request::Redo { doc: 1 });
        assert_eq!(search_pages(&engine, 1, "fox").len(), 2);

        // Forcing replaces the earlier layer instead of adding a second one.
        run_ocr(&engine, 1, vec![0], &exe, true);
        let hits = search_pages(&engine, 1, "fox");
        assert_eq!(hits[0].1.len(), 1, "one hit on page 1");

        let dir = std::env::temp_dir().join(format!("magic-pdf-ocr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("ocr.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let size = std::fs::metadata(&out).unwrap().len();
        eprintln!("saved with text layer: {size} bytes");
        open(&engine, 2, out);
        assert_eq!(search_pages(&engine, 2, "4711").len(), 2, "text survives save and reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn page_text(engine: &Engine, doc: DocId, page: usize) -> String {
        engine.send(Request::PageText { doc, page });
        recv_until(engine, |r| match r {
            Response::PageText { page: p, text, .. } if p == page => Some(text.chars.iter().map(|c| c.ch).collect()),
            _ => None,
        })
    }

    fn dark_pixels(size: [usize; 2], rgba: &[u8], region: (f32, f32, f32, f32)) -> usize {
        let (x0, y0, x1, y1) = region;
        let mut n = 0;
        for y in (y0 * size[1] as f32) as usize..(y1 * size[1] as f32) as usize {
            for x in (x0 * size[0] as f32) as usize..(x1 * size[0] as f32) as usize {
                if rgba[(y * size[0] + x) * 4..][..3].iter().all(|&c| c < 180) {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn crop_header_footer_and_bookmarks_are_undoable_page_edits() {
        let (_serial, engine) = start();
        let info = open(&engine, 1, sample());
        let edit = |op: PageOp| {
            engine.send(Request::EditPages { doc: 1, op });
            pages_changed(&engine).0
        };
        // Crop page 1 (A4 portrait) and page 6 (stored portrait, shown landscape).
        let margins = Margins {
            top: 100.0,
            right: 0.0,
            bottom: 0.0,
            left: 50.0,
        };
        let cropped = edit(PageOp::Crop {
            pages: vec![0, 5],
            margins,
        });
        assert_eq!(cropped.page_sizes[0], (545.0, 742.0));
        assert_eq!(info.page_sizes[5], (842.0, 595.0));
        assert_eq!(cropped.page_sizes[5], (792.0, 495.0), "margins apply as the page is shown");
        engine.send(Request::Undo { doc: 1 });
        assert_eq!(pages_changed(&engine).0.page_sizes[0], (595.0, 842.0));

        // Footer with page numbers; doing it again replaces it.
        let spec = HeaderFooter {
            header: ["".into(), "".into(), "Confidential".into()],
            footer: ["".into(), "Page {page} of {pages}".into(), "".into()],
            size: 10.0,
            color: [0, 0, 0],
            margin: 20.0,
            first_number: 1,
        };
        edit(PageOp::HeaderFooter {
            pages: (0..40).collect(),
            spec: spec.clone(),
        });
        assert!(page_text(&engine, 1, 0).contains("Page 1 of 40"));
        let text = page_text(&engine, 1, 5);
        assert!(text.contains("Page 6 of 40") && text.contains("Confidential"), "{text}");
        // On the rotated page the footer is at the bottom of the page as shown.
        let (size, rgba) = render(&engine, 1, 5, Rotation::R0, 1);
        assert!(dark_pixels(size, &rgba, (0.4, 0.93, 0.6, 0.99)) > 3, "footer at the bottom centre");
        assert!(dark_pixels(size, &rgba, (0.85, 0.0, 1.0, 0.05)) > 3, "header at the top right");
        let mut again = spec.clone();
        again.footer[1] = "– {page} –".into();
        again.first_number = 3;
        edit(PageOp::HeaderFooter {
            pages: vec![0],
            spec: again,
        });
        let text = page_text(&engine, 1, 0);
        assert!(text.contains("– 3 –") && !text.contains("Page 1 of 40"), "replaced: {text}");
        assert_eq!(text.matches("Confidential").count(), 1);
        edit(PageOp::RemoveHeaderFooter { pages: vec![0] });
        let text = page_text(&engine, 1, 0);
        assert!(!text.contains("Confidential") && text.contains("Page 1"), "removed: {text}");

        // Bookmarks: replace, save, reopen; undo brings the old ones back.
        let outline = vec![
            OutlineItem {
                title: "Einführung".into(),
                page: Some(0),
                children: vec![OutlineItem {
                    title: "Details".into(),
                    page: Some(2),
                    children: Vec::new(),
                }],
            },
            OutlineItem {
                title: "End".into(),
                page: Some(39),
                children: Vec::new(),
            },
        ];
        let info2 = edit(PageOp::SetOutline { outline: outline.clone() });
        assert_eq!(info2.outline, outline);
        assert!(page_text(&engine, 1, 5).contains("Page 6 of 40"), "content kept");
        let path = std::env::temp_dir().join(format!("magic-pdf-outline-{}.pdf", std::process::id()));
        engine.send(Request::Save { doc: 1, path: path.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        assert_eq!(open(&engine, 2, path.clone()).outline, outline);
        std::fs::remove_file(path).ok();
        engine.send(Request::Undo { doc: 1 });
        assert_eq!(pages_changed(&engine).0.outline, info.outline);
    }

    /// A one-page PDF showing a noisy 800×800 RGB picture on 2 × 2 inches (400 dpi).
    fn photo_pdf() -> Vec<u8> {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.7");
        let mut seed = 1u32;
        let pixels: Vec<u8> = (0..800 * 800 * 3)
            .map(|i| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((i / 3 % 800) as u32 / 4 + (seed >> 24) / 4) as u8
            })
            .collect();
        let img = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 800, "Height" => 800,
                "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8 },
            pixels,
        ));
        let content = doc.add_object(Stream::new(dictionary! {}, b"q 144 0 0 144 100 500 cm /Im0 Do Q".to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Contents" => content, "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img } },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn page_chars(engine: &Engine, doc: DocId, page: usize) -> Vec<TextChar> {
        engine.send(Request::PageText { doc, page });
        recv_until(engine, |r| match r {
            Response::PageText { page: p, text, .. } if p == page => Some(text.chars.clone()),
            _ => None,
        })
    }

    /// Every stream of a PDF file, decompressed where possible.
    fn all_streams(bytes: &[u8]) -> Vec<Vec<u8>> {
        let doc = lopdf::Document::load_mem(bytes).unwrap();
        doc.objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .map(|s| s.decompressed_content().unwrap_or_else(|_| s.content.clone()))
            .collect()
    }

    #[test]
    fn redaction_removes_text_for_good_and_is_undoable() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let chars = page_chars(&engine, 1, 0);
        let text: String = chars.iter().map(|c| c.ch).collect();
        let count = |t: &str, w: &str| t.matches(w).count();
        let before = (count(&text, "quick"), count(&text, "brown"));
        // The first "quick" (line 1).
        let start = text.find("quick").unwrap();
        let start = text[..start].chars().count();
        let area = chars[start..start + 5]
            .iter()
            .filter_map(|c| c.rect)
            .reduce(PdfBox::union)
            .unwrap()
            .expand(0.5);
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::Redact { areas: vec![(0, vec![area])] },
        });
        let (_, mapping) = pages_changed(&engine);
        assert_eq!(mapping.changed, vec![0]);
        let after = page_text(&engine, 1, 0);
        assert_eq!(count(&after, "quick"), before.0 - 1, "{after}");
        assert_eq!(count(&after, "brown"), before.1, "the rest of the line stays");
        assert!(after.contains("Page 1"));
        // The rest of the line keeps its place: "brown" is where it was.
        let brown = |cs: &[TextChar]| {
            let t: String = cs.iter().map(|c| c.ch).collect();
            let i = t[..t.find("brown").unwrap()].chars().count();
            cs[i].rect.unwrap()
        };
        let moved = brown(&page_chars(&engine, 1, 0));
        let orig = brown(&chars);
        assert!((moved.left - orig.left).abs() < 0.5 && (moved.bottom - orig.bottom).abs() < 0.5, "{orig:?} -> {moved:?}");
        // Black where the word was.
        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 1);
        let (w, h) = (595.0, 842.0);
        let region = (area.left / w + 0.002, 1.0 - area.top / h + 0.002, area.right / w - 0.002, 1.0 - area.bottom / h - 0.002);
        let px = (((region.2 - region.0) * size[0] as f32) * ((region.3 - region.1) * size[1] as f32)) as usize;
        assert!(dark_pixels(size, &rgba, region) * 10 >= px * 9, "covered black");

        // Saved and reopened: gone, and no old content stream still holds the line.
        let path = std::env::temp_dir().join(format!("magic-pdf-redact-{}.pdf", std::process::id()));
        engine.send(Request::Save { doc: 1, path: path.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let bytes = std::fs::read(&path).unwrap();
        let line = b"The quick brown fox jumps over the lazy dog. Magic PDF horizontal layout line 1, page 1.";
        assert!(
            !all_streams(&bytes).iter().any(|s| s.windows(line.len()).any(|w| w == line)),
            "the original line is still in the file"
        );
        open(&engine, 2, path.clone());
        assert_eq!(count(&page_text(&engine, 2, 0), "quick"), before.0 - 1);
        std::fs::remove_file(path).ok();

        engine.send(Request::Undo { doc: 1 });
        pages_changed(&engine);
        assert_eq!(count(&page_text(&engine, 1, 0), "quick"), before.0);
    }

    #[test]
    fn redaction_blackens_image_pixels_and_drops_the_original_picture() {
        let (_serial, engine) = start();
        let dir = std::env::temp_dir().join(format!("magic-pdf-redact-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.pdf");
        std::fs::write(&src, photo_pdf()).unwrap();
        open(&engine, 1, src);
        // The picture spans 100..244 × 500..644; redact its left half.
        let area = PdfBox { left: 90.0, bottom: 490.0, right: 172.0, top: 650.0 };
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::Redact { areas: vec![(0, vec![area])] },
        });
        pages_changed(&engine);
        let out = dir.join("redacted.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let bytes = std::fs::read(&out).unwrap();
        // One picture left (the original is gone), its left half black, right half not.
        let doc = lopdf::Document::load_mem(&bytes).unwrap();
        let images: Vec<_> = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .filter(|s| s.dict.get(b"Subtype").and_then(|o| o.as_name()).is_ok_and(|n| n == b"Image"))
            .filter(|s| s.dict.get(b"Width").and_then(|o| o.as_i64()).is_ok_and(|w| w == 800))
            .collect();
        assert_eq!(images.len(), 1, "only the redacted picture is in the file");
        let px = images[0].decompressed_content().unwrap_or_else(|_| images[0].content.clone());
        let bpp = px.len() / (800 * 800);
        let at = |x: usize, y: usize| &px[(y * 800 + x) * bpp..][..3];
        assert_eq!(at(100, 400), &[0, 0, 0], "left half blackened");
        assert_ne!(at(700, 400), &[0, 0, 0], "right half kept");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn optimized_copies_are_smaller_and_still_show_the_picture() {
        let (_serial, engine) = start();
        let dir = std::env::temp_dir().join(format!("magic-pdf-opt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.pdf");
        std::fs::write(&src, photo_pdf()).unwrap();
        open(&engine, 1, src.clone());
        let (size, before) = render(&engine, 1, 0, Rotation::R0, 0);
        let out = dir.join("small.pdf");
        engine.send(Request::SaveOptimized {
            doc: 1,
            path: out.clone(),
            image_dpi: Some(150),
        });
        let message = recv_until(&engine, |r| match r {
            Response::Exported { message } => Some(message),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        assert!(message.contains("1 image scaled down"), "{message}");
        let (a, b) = (std::fs::metadata(&src).unwrap().len(), std::fs::metadata(&out).unwrap().len());
        assert!(b * 4 < a, "{a} -> {b} bytes");
        open(&engine, 2, out);
        let (size2, after) = render(&engine, 2, 0, Rotation::R0, 0);
        assert_eq!(size, size2);
        // The picture is still there (page area around it stays white).
        let mean = |px: &[u8], x: usize, y: usize| px[(y * size[0] + x) * 4] as i32;
        let (x, y) = ((170.0 / 595.0 * size[0] as f32) as usize, (270.0 / 842.0 * size[1] as f32) as usize);
        assert!((mean(&before, x, y) - mean(&after, x, y)).abs() < 40, "picture kept");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tiles_match_the_same_area_of_a_whole_page_render() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let full = RenderKey {
            doc: 1,
            page: 0,
            width_px: 1100,
            rotation: Rotation::R90,
            version: 0,
            tile: 0,
        };
        let get = |key: RenderKey| {
            engine.send(Request::Render { key, priority: 0.0 });
            recv_until(&engine, |r| match r {
                Response::Rendered { key: k, size, rgba } if k == key => Some((size, rgba)),
                Response::RenderFailed { key: k } if k == key => panic!("render failed"),
                _ => None,
            })
        };
        let (fsize, frgba) = get(full);
        // Tile (column 1, row 0) and the last, partial column (2).
        for col in [1u32, 2] {
            let (size, rgba) = get(RenderKey {
                tile: RenderKey::tile_at(col, 0),
                ..full
            });
            let x0 = (col * TILE) as usize;
            assert_eq!(size, [(fsize[0] - x0).min(512), 512.min(fsize[1])]);
            let mut differ = 0;
            for y in 0..size[1] {
                for x in 0..size[0] {
                    let a = &rgba[(y * size[0] + x) * 4..][..3];
                    let b = &frgba[(y * fsize[0] + x0 + x) * 4..][..3];
                    if a.iter().zip(b).any(|(p, q)| p.abs_diff(*q) > 8) {
                        differ += 1;
                    }
                }
            }
            assert!(differ * 200 < size[0] * size[1], "tile {col}: {differ} pixels differ");
            assert!(rgba.chunks(4).any(|p| p[0] < 128), "tile {col} shows something");
        }
        assert_eq!(full.tile_pos(), None);
        assert_eq!(RenderKey { tile: RenderKey::tile_at(3, 7), ..full }.tile_pos(), Some((3, 7)));
    }

    #[test]
    fn search_hits_come_with_the_text_around_them() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        engine.send(Request::Search {
            doc: 1,
            generation: 1,
            query: SearchQuery {
                text: "LAYOUT LINE 3,".into(),
                match_case: false,
                whole_word: false,
            },
        });
        let context = recv_until(&engine, |r| match r {
            Response::SearchResults { page: 0, context, hits, .. } => {
                assert_eq!(context.len(), hits.len());
                Some(context)
            }
            _ => None,
        });
        let c = &context[0];
        assert_eq!(c.text, "layout line 3,");
        assert!(c.before.ends_with("Magic PDF horizontal "), "{c:?}");
        assert!(c.after.starts_with(" page 1."), "{c:?}");
        assert!(c.before.chars().count() <= 42 && c.after.chars().count() <= 42, "{c:?}");
    }

    #[test]
    fn comparing_finds_the_words_that_changed() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        // A copy with a footer on page 3 only.
        engine.send(Request::EditPages {
            doc: 1,
            op: PageOp::HeaderFooter {
                pages: vec![2],
                spec: HeaderFooter {
                    header: Default::default(),
                    footer: ["".into(), "Draft copy".into(), "".into()],
                    size: 10.0,
                    color: [0, 0, 0],
                    margin: 20.0,
                    first_number: 1,
                },
            },
        });
        pages_changed(&engine);
        let path = std::env::temp_dir().join(format!("magic-pdf-compare-{}.pdf", std::process::id()));
        engine.send(Request::Save { doc: 1, path: path.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        open(&engine, 2, sample());
        open(&engine, 3, path.clone());
        engine.send(Request::Compare { a: 2, b: 3 });
        let diffs = recv_until(&engine, |r| match r {
            Response::Compared { a: 2, b: 3, diffs } => Some(diffs),
            _ => None,
        });
        if let Some(dir) = std::env::var_os("MAGIC_PDF_KEEP_DIR") {
            std::fs::copy(&path, PathBuf::from(dir).join("compare-b.pdf")).unwrap();
        }
        std::fs::remove_file(path).ok();
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        let d = &diffs[0];
        assert_eq!(d.page, 2);
        assert!(d.removed.is_empty());
        assert_eq!(d.added.len(), 2, "two added words");
        assert_eq!(d.summary, ["+ Draft copy"]);
        // The marks sit near the bottom of the page.
        assert!(d.added.iter().all(|r| r.bottom < 60.0), "{:?}", d.added);
    }

    #[test]
    fn measurements_label_themselves_and_update_when_resized() {
        let (_serial, engine) = start();
        open(&engine, 1, sample());
        let mut d = NewAnnot::new(AnnotKind::Distance, pdf_box(0.0, 0.0, 0.0, 0.0), [0, 0, 200]);
        d.vertices = vec![(100.0, 400.0), (172.0, 400.0)];
        d.measure = Some(shapes::Scale::new(100.0, "m").encode());
        let i = add(&engine, 1, d);
        let mut area = NewAnnot::new(AnnotKind::Area, pdf_box(0.0, 0.0, 0.0, 0.0), [0, 0, 200]);
        area.vertices = vec![(300.0, 300.0), (372.0, 300.0), (372.0, 372.0), (300.0, 372.0)];
        area.measure = Some(shapes::Scale::new(1.0, "cm").encode());
        let j = add(&engine, 1, area);
        let list = annots(&engine, 1);
        let get = |list: &[AnnotInfo], i: usize| list.iter().find(|a| a.index == i).unwrap().clone();
        assert_eq!(get(&list, i).contents, "2.54 m");
        assert_eq!(get(&list, i).kind, AnnotKind::Distance);
        assert_eq!(get(&list, j).contents, "6.45 cm²");
        // The label is drawn above the line (blue pixels there).
        let (size, rgba) = render(&engine, 1, 0, Rotation::R0, 1);
        assert!(coloured(size, &rgba, (0.17, 0.49, 0.29, 0.52), 2) > 5, "label drawn");
        // Stretching the line to twice its length doubles the measurement.
        engine.send(Request::UpdateAnnot {
            doc: 1,
            page: 0,
            index: i,
            change: AnnotChange {
                transform: Some([2.0, 1.0, -100.0, 0.0]),
                ..Default::default()
            },
        });
        let list = annots(&engine, 1);
        assert_eq!(get(&list, i).contents, "5.08 m");
        let list = save_and_reopen(&engine, 1, "measure");
        assert!(list.iter().any(|a| a.kind == AnnotKind::Area && a.contents == "6.45 cm²"));
    }

    /// A one-page PDF with `xml` attached as factur-x.xml and XMP metadata.
    fn einvoice_pdf(xml: &[u8]) -> Vec<u8> {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.7");
        let content = doc.add_object(Stream::new(dictionary! {}, b"".to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Contents" => content,
        });
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }));
        let file = doc.add_object(Stream::new(dictionary! { "Type" => "EmbeddedFile", "Subtype" => "text#2Fxml" }, xml.to_vec()));
        let spec = doc.add_object(dictionary! {
            "Type" => "Filespec", "F" => Object::string_literal("factur-x.xml"), "UF" => Object::string_literal("factur-x.xml"),
            "EF" => dictionary! { "F" => file }, "AFRelationship" => "Data",
        });
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description>\
<dc:title><rdf:Alt><rdf:li xml:lang='x-default'>Invoice</rdf:li></rdf:Alt></dc:title>\
</rdf:Description></rdf:RDF></x:xmpmeta>";
        let metadata = doc.add_object(Stream::new(dictionary! { "Type" => "Metadata", "Subtype" => "XML" }, xmp.to_vec()));
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id, "Metadata" => metadata, "AF" => vec![spec.into()],
            "Names" => dictionary! { "EmbeddedFiles" => dictionary! { "Names" => vec![Object::string_literal("factur-x.xml"), spec.into()] } },
        });
        let info = doc.add_object(dictionary! { "Title" => Object::string_literal("Invoice"), "Producer" => Object::string_literal("Test") });
        doc.trailer.set("Root", catalog);
        doc.trailer.set("Info", info);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn attachments_and_document_properties() {
        let (_serial, engine) = start();
        let dir = std::env::temp_dir().join(format!("magic-pdf-props-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("invoice.pdf");
        let xml = crate::einvoice::SAMPLE.as_bytes();
        std::fs::write(&src, einvoice_pdf(xml)).unwrap();
        let info = open(&engine, 1, src);
        assert_eq!(info.attachments, vec![AttachmentInfo { name: "factur-x.xml".into(), size: xml.len() }]);
        assert_eq!((info.properties.title.as_str(), info.properties.producer.as_str()), ("Invoice", "Test"));
        assert_eq!(info.properties.version, Some(17));
        engine.send(Request::GetAttachment { doc: 1, index: 0 });
        let (name, data) = recv_until(&engine, |r| match r {
            Response::Attachment { name, data, .. } => Some((name, data)),
            Response::Error { message, .. } => panic!("{message}"),
            _ => None,
        });
        assert_eq!((name.as_str(), data.as_slice()), ("factur-x.xml", xml));
        assert!(crate::einvoice::parse(&data).is_ok());

        // Edit the properties: undoable, saved, and the XMP title follows.
        let props = EditableProperties {
            title: "Rechnung Oktober".into(),
            author: "Stadtwerke".into(),
            subject: String::new(),
            keywords: "Wasser, 2026".into(),
        };
        engine.send(Request::EditPages { doc: 1, op: PageOp::SetProperties { properties: props.clone() } });
        let (info, _) = pages_changed(&engine);
        assert_eq!(info.properties.title, "Rechnung Oktober");
        assert_eq!(info.properties.keywords, "Wasser, 2026");
        assert_eq!(info.title.as_deref(), Some("Rechnung Oktober"));
        assert_eq!(info.attachments.len(), 1, "the attachment stays");
        let out = dir.join("saved.pdf");
        engine.send(Request::Save { doc: 1, path: out.clone() });
        recv_until(&engine, |r| matches!(r, Response::Saved { .. }).then_some(()));
        let bytes = std::fs::read(&out).unwrap();
        let saved = String::from_utf8_lossy(&bytes);
        assert!(saved.contains("<rdf:li xml:lang='x-default'>Rechnung Oktober</rdf:li>"), "XMP readable and updated");
        let reopened = open(&engine, 2, out);
        assert_eq!(reopened.properties.author, "Stadtwerke");
        engine.send(Request::Undo { doc: 1 });
        assert_eq!(pages_changed(&engine).0.properties.title, "Invoice");
        std::fs::remove_dir_all(dir).ok();
    }
}

