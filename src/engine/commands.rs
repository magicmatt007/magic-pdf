//! Messages exchanged between the UI thread and the PDFium worker thread.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type DocId = u64;

/// Clockwise view rotation applied to every page of a document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn cw(self) -> Self {
        match self {
            Self::R0 => Self::R90,
            Self::R90 => Self::R180,
            Self::R180 => Self::R270,
            Self::R270 => Self::R0,
        }
    }

    pub fn ccw(self) -> Self {
        self.cw().cw().cw()
    }

    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::R90 | Self::R270)
    }
}

/// Rectangle in PDF page space (points, origin bottom-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PdfBox {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

/// Identifies one rendered bitmap of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderKey {
    pub doc: DocId,
    pub page: usize,
    /// Output width in physical pixels (after rotation).
    pub width_px: u32,
    pub rotation: Rotation,
}

#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub page: Option<usize>,
    pub children: Vec<OutlineItem>,
}

#[derive(Clone, Debug)]
pub struct DocInfo {
    /// Unrotated page sizes in points (width, height).
    pub page_sizes: Vec<(f32, f32)>,
    pub outline: Vec<OutlineItem>,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TextChar {
    pub ch: char,
    /// None for generated characters (line breaks) without geometry.
    pub rect: Option<PdfBox>,
}

#[derive(Clone, Debug)]
pub enum LinkTarget {
    Page(usize),
    Uri(String),
}

#[derive(Clone, Debug)]
pub struct PageLink {
    pub rect: PdfBox,
    pub target: LinkTarget,
}

#[derive(Clone, Debug, Default)]
pub struct PageText {
    pub chars: Vec<TextChar>,
    pub links: Vec<PageLink>,
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    pub text: String,
    pub match_case: bool,
    pub whole_word: bool,
}

pub enum Request {
    Open {
        doc: DocId,
        path: PathBuf,
        password: Option<String>,
    },
    Close {
        doc: DocId,
    },
    /// Queue a page render. Lower priority values are rendered first.
    Render {
        key: RenderKey,
        priority: f32,
    },
    /// Drop queued renders of `doc` that are not in `keep`.
    Retain {
        doc: DocId,
        keep: HashSet<RenderKey>,
    },
    PageText {
        doc: DocId,
        page: usize,
    },
    /// Start (or restart) a document-wide search. `generation` tags results.
    Search {
        doc: DocId,
        generation: u64,
        query: SearchQuery,
    },
    CancelSearch {
        doc: DocId,
    },
}

pub enum Response {
    Opened {
        doc: DocId,
        info: DocInfo,
    },
    OpenFailed {
        doc: DocId,
        error: String,
        password_required: bool,
    },
    Rendered {
        key: RenderKey,
        size: [usize; 2],
        rgba: Vec<u8>,
    },
    RenderFailed {
        key: RenderKey,
    },
    PageText {
        doc: DocId,
        page: usize,
        text: PageText,
    },
    SearchResults {
        doc: DocId,
        generation: u64,
        page: usize,
        /// Each hit may span several rectangles (e.g. across line breaks).
        hits: Vec<Vec<PdfBox>>,
    },
    SearchDone {
        doc: DocId,
        generation: u64,
    },
}
