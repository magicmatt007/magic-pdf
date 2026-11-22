//! PDFium engine: a dedicated worker thread that owns the PDFium library and all open
//! documents. PDFium is not thread-safe, so the UI only talks to it through channels.

pub mod commands;
mod pdf;
#[cfg(windows)]
mod print_windows;
mod worker;

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender};

pub use commands::*;
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

    fn start() -> Engine {
        Engine::start(|| {}).expect("PDFium available (run scripts/fetch-pdfium.sh)")
    }

    fn open(engine: &Engine, doc: DocId, path: PathBuf) -> DocInfo {
        engine.send(Request::Open {
            doc,
            path,
            password: None,
        });
        match recv(engine) {
            Response::Opened { info, .. } => info,
            Response::OpenFailed { error, .. } => panic!("open failed: {error}"),
            _ => panic!("unexpected response"),
        }
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
        let engine = start();
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

        let engine = start();
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

        let engine = start();
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
}
