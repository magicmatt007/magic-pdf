//! Messages exchanged between the UI thread and the PDFium worker thread.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type DocId = u64;

/// Clockwise rotation (view rotation, or a page's intrinsic /Rotate).
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

    pub fn quarter_turns(self) -> i32 {
        match self {
            Self::R0 => 0,
            Self::R90 => 1,
            Self::R180 => 2,
            Self::R270 => 3,
        }
    }

    pub fn from_quarter_turns(n: i32) -> Self {
        match n.rem_euclid(4) {
            0 => Self::R0,
            1 => Self::R90,
            2 => Self::R180,
            _ => Self::R270,
        }
    }

    /// `self` followed by `other`.
    pub fn then(self, other: Rotation) -> Rotation {
        Self::from_quarter_turns(self.quarter_turns() + other.quarter_turns())
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

impl PdfBox {
    pub fn union(self, o: PdfBox) -> PdfBox {
        PdfBox {
            left: self.left.min(o.left),
            bottom: self.bottom.min(o.bottom),
            right: self.right.max(o.right),
            top: self.top.max(o.top),
        }
    }

    pub fn expand(self, d: f32) -> PdfBox {
        PdfBox {
            left: self.left - d,
            bottom: self.bottom - d,
            right: self.right + d,
            top: self.top + d,
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }
}

/// How a page's PDF coordinates map onto its displayed (unrotated-by-view) area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageGeom {
    /// Crop box size before the page's own rotation.
    pub size: (f32, f32),
    /// Lower-left corner of the crop box.
    pub origin: (f32, f32),
    /// The page's intrinsic /Rotate.
    pub rotation: Rotation,
}

/// Identifies one rendered bitmap of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderKey {
    pub doc: DocId,
    pub page: usize,
    /// Output width in physical pixels (after rotation).
    pub width_px: u32,
    pub rotation: Rotation,
    /// Bumped whenever the page content changes (annotation edits).
    pub version: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub page: Option<usize>,
    pub children: Vec<OutlineItem>,
}

#[derive(Clone, Debug)]
pub struct DocInfo {
    /// Displayed page sizes in points (width, height), including each page's own rotation.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnnotKind {
    Highlight,
    Underline,
    StrikeOut,
    Squiggly,
    Note,
    Ink,
    Square,
    Circle,
    FreeText,
    /// A line annotation from another application (not editable here).
    Line,
    Stamp,
    /// Shapes drawn with magic-pdf (stored as ink with a private shape key).
    DrawnLine,
    Arrow,
    Polygon,
    Cloud,
    Other,
}

impl AnnotKind {
    pub fn label(self) -> &'static str {
        match self {
            AnnotKind::Highlight => "Highlight",
            AnnotKind::Underline => "Underline",
            AnnotKind::StrikeOut => "Strikeout",
            AnnotKind::Squiggly => "Squiggly",
            AnnotKind::Note => "Note",
            AnnotKind::Ink => "Ink",
            AnnotKind::Square => "Rectangle",
            AnnotKind::Circle => "Ellipse",
            AnnotKind::FreeText => "Text box",
            AnnotKind::Line => "Line",
            AnnotKind::Stamp => "Stamp",
            AnnotKind::DrawnLine => "Line",
            AnnotKind::Arrow => "Arrow",
            AnnotKind::Polygon => "Polygon",
            AnnotKind::Cloud => "Cloud",
            AnnotKind::Other => "Annotation",
        }
    }

    /// Kinds whose appearance PDFium can regenerate after a style change.
    pub fn style_editable(self) -> bool {
        matches!(
            self,
            AnnotKind::Highlight
                | AnnotKind::Underline
                | AnnotKind::StrikeOut
                | AnnotKind::Squiggly
                | AnnotKind::Note
                | AnnotKind::Ink
                | AnnotKind::Square
                | AnnotKind::Circle
                | AnnotKind::FreeText
        ) || self.shape().is_some()
    }

    /// The magic-pdf shape behind a kind.
    pub fn shape(self) -> Option<super::shapes::Shape> {
        use super::shapes::Shape;
        match self {
            AnnotKind::DrawnLine => Some(Shape::Line),
            AnnotKind::Arrow => Some(Shape::Arrow),
            AnnotKind::Polygon => Some(Shape::Polygon),
            AnnotKind::Cloud => Some(Shape::Cloud),
            _ => None,
        }
    }

    pub fn from_shape(shape: super::shapes::Shape) -> AnnotKind {
        use super::shapes::Shape;
        match shape {
            Shape::Line => AnnotKind::DrawnLine,
            Shape::Arrow => AnnotKind::Arrow,
            Shape::Polygon => AnnotKind::Polygon,
            Shape::Cloud => AnnotKind::Cloud,
        }
    }

    /// Kinds that can be moved by dragging (text markup stays on its text).
    pub fn movable(self) -> bool {
        matches!(
            self,
            AnnotKind::Square | AnnotKind::Circle | AnnotKind::Ink | AnnotKind::Note | AnnotKind::FreeText | AnnotKind::Stamp
        ) || self.shape().is_some()
    }

    /// How the kind can be resized.
    pub fn resize(self) -> Resize {
        match self {
            // A text box can be made larger than its text, never smaller.
            AnnotKind::Square | AnnotKind::Circle | AnnotKind::Ink | AnnotKind::FreeText | AnnotKind::Stamp => Resize::Free,
            k if k.shape().is_some() => Resize::Free,
            _ => Resize::None,
        }
    }

    pub fn has_border_width(self) -> bool {
        matches!(self, AnnotKind::Ink | AnnotKind::Square | AnnotKind::Circle | AnnotKind::FreeText) || self.shape().is_some()
    }

    pub fn has_interior(self) -> bool {
        matches!(
            self,
            AnnotKind::Square | AnnotKind::Circle | AnnotKind::FreeText | AnnotKind::Polygon | AnnotKind::Cloud
        )
    }
}

pub type Rgb = [u8; 3];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resize {
    None,
    Free,
}

/// A new annotation to create. Coordinates are in PDF page space.
#[derive(Clone, Debug)]
pub struct NewAnnot {
    pub kind: AnnotKind,
    pub rect: PdfBox,
    pub color: Rgb,
    pub interior: Option<Rgb>,
    pub width: f32,
    pub contents: String,
    pub author: String,
    /// Text markup quads: x1,y1 (top-left), x2,y2 (top-right), x3,y3 (bottom-left), x4,y4.
    pub quads: Vec<[f32; 8]>,
    pub ink: Vec<Vec<(f32, f32)>>,
    /// Text boxes: font size and text colour.
    pub font_size: f32,
    pub text_color: Rgb,
    /// Text boxes: styled text (instead of plain `contents` in `font_size`/`text_color`).
    pub rich: Option<super::rich::RichText>,
    /// Lines, arrows, polygons and clouds: their points.
    pub vertices: Vec<(f32, f32)>,
    /// Stamps: what they show.
    pub stamp: Option<StampSpec>,
    /// Comment thread and review status.
    pub replies: Vec<Reply>,
    pub status: String,
}

/// What a new stamp shows.
#[derive(Clone, Debug, PartialEq)]
pub enum StampSpec {
    /// A framed title such as APPROVED with an optional smaller line (author, date).
    Text { title: String, subtitle: String },
    /// An image (e.g. a scanned signature), RGBA pixels.
    Image { width: u32, height: u32, rgba: Vec<u8> },
}

/// A reply in a comment thread.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Reply {
    pub author: String,
    /// PDF date string.
    pub date: String,
    pub text: String,
}

/// Review status of a comment (as in Acrobat's review state model).
pub const STATUSES: [&str; 5] = ["None", "Accepted", "Rejected", "Cancelled", "Completed"];

impl NewAnnot {
    /// A new annotation of `kind` with defaults for everything else.
    pub fn new(kind: AnnotKind, rect: PdfBox, color: Rgb) -> NewAnnot {
        NewAnnot {
            kind,
            rect,
            color,
            interior: None,
            width: 1.0,
            contents: String::new(),
            author: String::new(),
            quads: Vec::new(),
            ink: Vec::new(),
            font_size: 12.0,
            text_color: [0, 0, 0],
            rich: None,
            vertices: Vec::new(),
            stamp: None,
            replies: Vec::new(),
            status: String::new(),
        }
    }

    /// Moves everything by (dx, dy).
    pub fn translate(&mut self, dx: f32, dy: f32) {
        self.rect = PdfBox {
            left: self.rect.left + dx,
            bottom: self.rect.bottom + dy,
            right: self.rect.right + dx,
            top: self.rect.top + dy,
        };
        for q in &mut self.quads {
            for i in 0..4 {
                q[2 * i] += dx;
                q[2 * i + 1] += dy;
            }
        }
        for p in self.ink.iter_mut().flatten().chain(self.vertices.iter_mut()) {
            p.0 += dx;
            p.1 += dy;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnnotInfo {
    /// Index within the page; stable while the document is open (deletions are soft).
    pub index: usize,
    pub kind: AnnotKind,
    pub rect: PdfBox,
    pub color: Option<Rgb>,
    pub interior: Option<Rgb>,
    pub width: f32,
    pub contents: String,
    pub author: String,
    pub modified: String,
    /// Text boxes: font size and text colour of the first character.
    pub font_size: Option<f32>,
    pub text_color: Option<Rgb>,
    /// Text boxes: the styled text.
    pub rich: Option<super::rich::RichText>,
    /// Shapes: their points.
    pub vertices: Vec<(f32, f32)>,
    /// Comment thread written in magic-pdf.
    pub replies: Vec<Reply>,
    /// Replies written in other applications (read-only here).
    pub other_replies: Vec<Reply>,
    /// Review status ("" or one of `STATUSES`).
    pub status: String,
    /// A reply written in another application (shown in its parent's thread).
    pub in_reply_to: Option<usize>,
}

/// Property changes; `None` leaves a property untouched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnnotChange {
    pub color: Option<Rgb>,
    pub interior: Option<Option<Rgb>>,
    pub width: Option<f32>,
    pub contents: Option<String>,
    pub font_size: Option<f32>,
    pub text_color: Option<Rgb>,
    /// Text boxes: new styled text (replaces the text and its styles).
    pub rich: Option<super::rich::RichText>,
    /// Move or resize: the new rectangle (PDF page space).
    pub rect: Option<PdfBox>,
    /// Move or resize as an exact transform `[sx, sy, tx, ty]` (x' = sx*x + tx); used
    /// for undo/redo so the result does not depend on the current rectangle.
    pub transform: Option<[f32; 4]>,
    /// Comment thread (magic-pdf's own replies) and review status.
    pub replies: Option<Vec<Reply>>,
    pub status: Option<String>,
}

/// A form field widget (interactive AcroForm field).
#[derive(Clone, Debug, PartialEq)]
pub struct WidgetInfo {
    pub rect: PdfBox,
    /// PDFium field type: 1 push button, 2 checkbox, 3 radio, 4 combo box, 5 list box,
    /// 6 text field, 7 signature.
    pub field_type: i32,
    pub name: String,
    pub value: String,
    pub checked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormMouse {
    Move,
    Down,
    Up,
    DoubleClick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormKey {
    /// Windows virtual-key code (PDFium uses these on all platforms).
    Down(u32),
    /// A character (Unicode scalar value); sent after `Down` for text input.
    Char(u32),
}

/// Modifier flags for form events (PDFium's FWL_EVENTFLAG values).
pub mod form_mods {
    pub const SHIFT: i32 = 1 << 0;
    pub const CONTROL: i32 = 1 << 1;
    pub const ALT: i32 = 1 << 2;
}

/// How a text recognition run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum OcrOutcome {
    Done {
        /// Pages that received a text layer.
        recognised: usize,
        /// Pages skipped because they already have text.
        skipped: usize,
        /// Words added.
        words: usize,
    },
    Cancelled,
    Failed(String),
}

/// Print settings for the CUPS (`lp`) path.
#[derive(Clone, Debug, PartialEq)]
pub struct PrintJob {
    pub printer: Option<String>,
    pub copies: u32,
    /// CUPS page ranges like "1-3,5"; None prints all pages.
    pub pages: Option<String>,
    pub two_sided: bool,
    pub fit_to_page: bool,
    pub title: String,
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
    /// List annotations of every page (answered with one `PageAnnots` per page).
    ScanAnnots {
        doc: DocId,
    },
    AddAnnot {
        doc: DocId,
        page: usize,
        annot: NewAnnot,
    },
    UpdateAnnot {
        doc: DocId,
        page: usize,
        index: usize,
        change: AnnotChange,
    },
    DeleteAnnot {
        doc: DocId,
        page: usize,
        index: usize,
    },
    /// Copy an annotation to the annotation clipboard (kept by the engine).
    CopyAnnot {
        doc: DocId,
        page: usize,
        index: usize,
    },
    /// Paste the copied annotation on `page`, centred on `at` (PDF space), or next to
    /// the original when `at` is None.
    PasteAnnot {
        doc: DocId,
        page: usize,
        at: Option<(f32, f32)>,
    },
    Undo {
        doc: DocId,
    },
    Redo {
        doc: DocId,
    },
    Save {
        doc: DocId,
        path: PathBuf,
    },
    FormMouse {
        doc: DocId,
        page: usize,
        event: FormMouse,
        x: f32,
        y: f32,
        modifiers: i32,
    },
    FormKey {
        doc: DocId,
        key: FormKey,
        modifiers: i32,
    },
    /// Copy (or cut) the selection in the focused field; answered with `Clipboard`.
    FormCopy {
        doc: DocId,
        cut: bool,
    },
    FormPaste {
        doc: DocId,
        text: String,
    },
    FormSelectAll {
        doc: DocId,
    },
    FormUndo {
        doc: DocId,
        redo: bool,
    },
    FormKillFocus {
        doc: DocId,
    },
    /// Produce the document as PDF bytes (with edits) for printing via CUPS.
    ExportForPrint {
        doc: DocId,
        job: PrintJob,
    },
    /// Change the page structure (undoable).
    EditPages {
        doc: DocId,
        op: super::pages::PageOp,
    },
    /// Insert the pages of another PDF file at `at` (undoable).
    InsertFile {
        doc: DocId,
        path: PathBuf,
        at: usize,
    },
    /// Write a smaller copy of the document to `path`: losslessly compressed, and with
    /// images above `image_dpi` scaled down (JPEG) when set.
    SaveOptimized {
        doc: DocId,
        path: PathBuf,
        image_dpi: Option<u32>,
    },
    /// Write `pages` to a new PDF file.
    ExtractPages {
        doc: DocId,
        pages: Vec<usize>,
        path: PathBuf,
    },
    /// Write the document as several files of `every` pages each, named
    /// `<stem>-01.pdf`, `<stem>-02.pdf`, … in `dir`.
    SplitDocument {
        doc: DocId,
        every: usize,
        dir: PathBuf,
        stem: String,
    },
    /// Write the document with all edits to `path` (for moving a tab to another
    /// window); answered with `ExportedTo`.
    ExportToFile {
        doc: DocId,
        path: PathBuf,
    },
    /// Treat the document as having unsaved changes (a tab moved here from another
    /// window with unsaved edits).
    MarkDirty {
        doc: DocId,
    },
    /// Recognise text (OCR) on `pages` with Tesseract (`exe`) in `languages` (e.g.
    /// "deu+eng") and add it as an invisible text layer, as one undoable edit. Pages
    /// that already have text are skipped unless `force` (which also replaces an
    /// earlier recognition). Answered with `OcrProgress` and finally `OcrFinished`.
    Ocr {
        doc: DocId,
        pages: Vec<usize>,
        exe: PathBuf,
        languages: String,
        force: bool,
    },
    /// Stop a running recognition; nothing is added to the document.
    CancelOcr {
        doc: DocId,
    },
    /// Show the native print dialog and print (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    PrintNative {
        doc: DocId,
        title: String,
        current_page: usize,
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
        geom: PageGeom,
        text: PageText,
    },
    SearchResults {
        doc: DocId,
        generation: u64,
        page: usize,
        geom: PageGeom,
        /// Each hit may span several rectangles (e.g. across line breaks).
        hits: Vec<Vec<PdfBox>>,
    },
    SearchDone {
        doc: DocId,
        generation: u64,
    },
    PageAnnots {
        doc: DocId,
        page: usize,
        geom: PageGeom,
        annots: Vec<AnnotInfo>,
    },
    AnnotScanDone {
        doc: DocId,
    },
    /// Page content changed; cached renders are stale.
    PageChanged {
        doc: DocId,
        page: usize,
    },
    /// The newly created annotation (so the UI can select it).
    AnnotAdded {
        doc: DocId,
        page: usize,
        index: usize,
    },
    EditState {
        doc: DocId,
        can_undo: bool,
        can_redo: bool,
        dirty: bool,
    },
    Saved {
        doc: DocId,
        path: PathBuf,
    },
    Error {
        doc: DocId,
        message: String,
    },
    /// The page structure changed: new document info and how old page numbers map
    /// to new ones.
    PagesChanged {
        doc: DocId,
        info: DocInfo,
        mapping: super::pages::PageMapping,
    },
    ExportedTo {
        doc: DocId,
        path: PathBuf,
    },
    /// Pages were written to other files.
    Exported {
        message: String,
    },
    /// Form widgets of a page (sent with `PageAnnots` and after field edits).
    PageWidgets {
        doc: DocId,
        page: usize,
        widgets: Vec<WidgetInfo>,
    },
    /// Which page holds the focused form field, if any.
    FormFocus {
        doc: DocId,
        page: Option<usize>,
    },
    Clipboard {
        text: String,
    },
    PrintReady {
        job: PrintJob,
        bytes: Vec<u8>,
    },
    /// Text recognition is working on its `done + 1`th of `total` pages.
    OcrProgress {
        doc: DocId,
        done: usize,
        total: usize,
    },
    /// Text recognition ended (also when cancelled or failed).
    OcrFinished {
        doc: DocId,
        result: OcrOutcome,
    },
    /// Native printing finished (`None`: cancelled).
    #[cfg_attr(not(windows), allow(dead_code))]
    Printed {
        pages: Option<usize>,
    },
}
