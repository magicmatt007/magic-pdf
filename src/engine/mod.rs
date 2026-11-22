//! PDFium engine: a dedicated worker thread that owns the PDFium library and all open
//! documents. PDFium is not thread-safe, so the UI only talks to it through channels.

pub mod commands;
mod worker;

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender};

pub use commands::*;

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
        engine.rx.recv_timeout(Duration::from_secs(20)).expect("engine response")
    }

    fn sample() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.pdf")
    }

    #[test]
    fn open_render_text_search() {
        let engine = Engine::start(|| {}).expect("PDFium available (run scripts/fetch-pdfium.sh)");
        engine.send(Request::Open {
            doc: 1,
            path: sample(),
            password: None,
        });
        let Response::Opened { info, .. } = recv(&engine) else {
            panic!("open failed")
        };
        assert_eq!(info.page_sizes.len(), 40);
        assert_eq!(info.page_sizes[2], (842.0, 595.0));
        assert_eq!(info.title.as_deref(), Some("magic-pdf sample"));
        assert_eq!(info.outline.len(), 4);
        assert_eq!(info.outline[1].page, Some(10));
        assert_eq!(info.outline[1].children[0].page, Some(14));

        let key = RenderKey {
            doc: 1,
            page: 0,
            width_px: 300,
            rotation: Rotation::R90,
        };
        engine.send(Request::Render { key, priority: 0.0 });
        let Response::Rendered { size, rgba, .. } = recv(&engine) else {
            panic!("render failed")
        };
        // Rotated A4 rendered 300 px wide is ~212 px tall.
        assert_eq!(size[0], 300);
        assert!((size[1] as i32 - 212).abs() <= 1, "{size:?}");
        assert_eq!(rgba.len(), size[0] * size[1] * 4);
        assert!(rgba.chunks(4).any(|p| p[0] < 128), "page should not be blank");

        engine.send(Request::PageText { doc: 1, page: 3 });
        let Response::PageText { text, .. } = recv(&engine) else {
            panic!("no text")
        };
        let s: String = text.chars.iter().map(|c| c.ch).collect();
        assert!(s.contains("Page 4"), "{s}");
        assert_eq!(text.links.len(), 2);
        assert!(text.links.iter().any(|l| matches!(l.target, LinkTarget::Page(0))));
        assert!(
            text.links
                .iter()
                .any(|l| matches!(&l.target, LinkTarget::Uri(u) if u.starts_with("https://example.com")))
        );

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
}
