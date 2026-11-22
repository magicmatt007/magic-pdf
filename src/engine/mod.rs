//! PDFium engine: a dedicated worker thread that owns the PDFium library and all open
//! documents. PDFium is not thread-safe, so the UI only talks to it through channels.

pub mod commands;
pub mod pages;
pub mod textbox;
mod pdf;
#[cfg(windows)]
mod print_windows;
mod worker;

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender};

pub use commands::*;
pub use pages::{PageMapping, PageOp, move_target};
pub use pdf::FIELD_TEXT;

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
    }
    dirs.push(PathBuf::from("third_party/pdfium"));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
