//! Per-tab document state.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use eframe::egui::{Pos2, Rect, Vec2};

use crate::engine::{
    Align, AnnotInfo, AnnotKind, CharStyle, DocId, DocInfo, PageGeom, PageText, PdfBox, RenderKey, Request, RichText,
    Rotation, WidgetInfo,
};
use crate::settings::FileState;
use crate::view::layout::{self, Layout, LayoutMode, LayoutParams};
use crate::view::viewport::{FitMode, Viewport};

/// Screen points per PDF point at 100 % zoom (96 dpi screen vs 72 dpi PDF).
pub const ACTUAL_SIZE: f32 = 96.0 / 72.0;

pub const ZOOM_PRESETS: [f32; 18] = [
    10.0, 25.0, 33.0, 50.0, 67.0, 75.0, 100.0, 125.0, 150.0, 200.0, 300.0, 400.0, 600.0, 800.0, 1200.0, 1600.0, 2400.0,
    3200.0,
];

// One per open tab, so the size of the Ready variant does not matter.
#[allow(clippy::large_enum_variant)]
pub enum LoadState {
    Loading,
    NeedsPassword { input: String, wrong: bool },
    Failed(String),
    Ready(DocInfo),
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub page: usize,
    pub rects: Vec<PdfBox>,
    /// The hit with the text around it.
    pub context: crate::engine::HitContext,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub match_case: bool,
    pub whole_word: bool,
    /// The query the current results belong to.
    pub submitted: Option<String>,
    pub generation: u64,
    pub hits: Vec<SearchHit>,
    pub running: bool,
    pub current: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub page: usize,
    /// Cursor positions between characters (0 = before the first): where the
    /// selection started and where it ends now. Equal means nothing is selected.
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    /// Indices of the selected characters.
    pub fn range(&self) -> std::ops::Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

/// An annotation being drawn with the mouse (PDF page coordinates).
/// Which annotations the Comments sidebar lists.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommentFilter {
    pub text: String,
    pub kind: Option<AnnotKind>,
    pub author: Option<String>,
    /// "" lists comments without a status.
    pub status: Option<String>,
    /// Only annotations someone wrote a comment on (or replied to).
    pub with_comment: bool,
}

impl CommentFilter {
    pub fn active(&self) -> bool {
        *self != CommentFilter::default()
    }

    pub fn matches(&self, a: &AnnotInfo) -> bool {
        let text = self.text.trim().to_lowercase();
        let status = if a.status == "None" { "" } else { a.status.as_str() };
        (text.is_empty()
            || a.contents.to_lowercase().contains(&text)
            || a.author.to_lowercase().contains(&text)
            || a.replies.iter().chain(&a.other_replies).any(|r| r.text.to_lowercase().contains(&text)))
            && self.kind.is_none_or(|k| k == a.kind)
            && self.author.as_ref().is_none_or(|w| *w == a.author)
            && self.status.as_ref().is_none_or(|s| s == status)
            && (!self.with_comment || has_comment(a))
    }
}

/// Whether someone wrote a comment on the annotation. A text box's text and a
/// measurement's value are what the annotation shows, not a comment on it.
pub fn has_comment(a: &AnnotInfo) -> bool {
    let generated = matches!(a.kind, AnnotKind::FreeText | AnnotKind::Distance | AnnotKind::Area);
    (!generated && !a.contents.trim().is_empty()) || !a.replies.is_empty() || !a.other_replies.is_empty()
}

#[derive(Clone, Debug)]
pub enum Draft {
    Ink {
        page: usize,
        points: Vec<Pos2>,
    },
    Shape {
        page: usize,
        start: Pos2,
        end: Pos2,
    },
    /// A polygon or cloud being clicked together; `hover` is where the next corner would go.
    Poly {
        page: usize,
        points: Vec<Pos2>,
        hover: Option<Pos2>,
    },
}

/// Which part of an annotation is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragMode {
    Move,
    /// Resize handle: 0 top-left, 1 top, 2 top-right, 3 right, 4 bottom-right,
    /// 5 bottom, 6 bottom-left, 7 left (as seen on screen).
    Resize(u8),
}

/// An annotation being moved or resized (page-local coordinates: points, origin at
/// the top-left of the page as displayed).
#[derive(Clone, Debug)]
pub struct AnnotDrag {
    pub page: usize,
    pub index: usize,
    pub mode: DragMode,
    pub start: Rect,
    pub start_pointer: Pos2,
    pub current: Rect,
}

/// The currently selected annotation and its comment being edited.
#[derive(Clone, Debug)]
pub struct AnnotSelection {
    pub page: usize,
    pub index: usize,
    pub contents: String,
    /// The comment as last saved (or loaded), to avoid saving the same text twice.
    pub committed: String,
    pub focus_contents: bool,
    /// A text box's text is being edited on the page.
    pub editing: bool,
    /// Text boxes: styles of the text being edited.
    pub rich: Option<RichEdit>,
}

/// Styles of a text box's text while it is edited (one per character of
/// `AnnotSelection::contents`).
#[derive(Clone, Debug, PartialEq)]
pub struct RichEdit {
    pub styles: Vec<CharStyle>,
    /// Style for newly typed text.
    pub typing: CharStyle,
    pub align: Align,
    /// The text as last saved, to avoid saving unchanged text.
    pub committed: RichText,
    /// Cursor position (characters) in the last frame, to follow cursor moves.
    pub last_cursor: Option<usize>,
    /// The selection (start, end) as of the last frame the text had focus; kept while
    /// the formatting bar is used, as egui forgets it when the text loses focus.
    pub selection: Option<(usize, usize)>,
}

impl RichEdit {
    pub fn new(rich: &RichText) -> RichEdit {
        let chars = rich.chars();
        RichEdit {
            styles: chars.iter().map(|c| c.1).collect(),
            typing: rich.first_style(CharStyle::default()),
            align: rich.align,
            committed: rich.clone(),
            last_cursor: None,
            selection: None,
        }
    }

    pub fn to_rich(&self, text: &str) -> RichText {
        let chars: Vec<(char, CharStyle)> =
            text.chars().zip(self.styles.iter().copied().chain(std::iter::repeat(self.typing))).collect();
        RichText::from_chars(&chars, self.align)
    }
}

/// Keeps per-character styles in step with an edit from `old` to `new`: characters
/// outside the changed middle keep their style, inserted ones get `typing`.
pub fn sync_styles(old: &str, new: &str, styles: &[CharStyle], typing: CharStyle) -> Vec<CharStyle> {
    let (a, b): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    let style = |i: usize| styles.get(i).copied().unwrap_or(typing);
    let mut out: Vec<CharStyle> = (0..prefix).map(style).collect();
    out.extend(std::iter::repeat_n(typing, b.len() - prefix - suffix));
    out.extend((a.len() - suffix..a.len()).map(style));
    out
}

/// Text being changed with the edit-text tool: one line or a paragraph.
pub struct TextEditing {
    pub page: usize,
    /// Its text objects, top to bottom.
    pub objects: Vec<usize>,
    pub original: String,
    pub text: String,
    /// Where it was (PDF page space), its size and colour as found.
    pub rect: PdfBox,
    pub size: f32,
    pub color: crate::engine::Rgb,
    /// The changes besides the text.
    pub offset: (f32, f32),
    pub new_size: f32,
    pub new_color: crate::engine::Rgb,
    /// The editor still has to take the keyboard focus.
    pub focus: bool,
    /// The change last sent for a preview, and whether its answer is still out.
    pub asked: Option<crate::engine::TextChange>,
    pub waiting: bool,
    /// The newest preview: its change, the page region it shows and the picture.
    pub preview: Option<(crate::engine::TextChange, PdfBox, eframe::egui::TextureHandle)>,
    /// Where the text is in the newest preview.
    pub preview_text: Option<PdfBox>,
    /// Why the change cannot be made (from the last preview).
    pub error: Option<String>,
    /// A drag moving the text: where it started (PDF space) and the offset then.
    pub drag: Option<(Pos2, (f32, f32))>,
    /// Started by dragging the text: the move is kept when the drag ends.
    pub move_only: bool,
}

impl TextEditing {
    pub fn new(page: usize, objects: &[&crate::engine::TextObject]) -> TextEditing {
        let first = objects[0];
        let text = objects.iter().map(|o| o.text.trim()).collect::<Vec<_>>().join(" ");
        let rect = objects.iter().skip(1).fold(first.rect, |r, o| PdfBox {
            left: r.left.min(o.rect.left),
            bottom: r.bottom.min(o.rect.bottom),
            right: r.right.max(o.rect.right),
            top: r.top.max(o.rect.top),
        });
        TextEditing {
            page,
            objects: objects.iter().map(|o| o.index).collect(),
            original: text.clone(),
            text,
            rect,
            size: first.size,
            color: first.color,
            offset: (0.0, 0.0),
            new_size: first.size,
            new_color: first.color,
            focus: true,
            asked: None,
            waiting: false,
            preview: None,
            preview_text: None,
            error: None,
            drag: None,
            move_only: false,
        }
    }

    /// The change as it stands.
    pub fn change(&self) -> crate::engine::TextChange {
        crate::engine::TextChange {
            objects: self.objects.clone(),
            text: (self.text != self.original).then(|| self.text.clone()),
            offset: self.offset,
            size: ((self.new_size - self.size).abs() > 0.01).then_some(self.new_size),
            color: (self.new_color != self.color).then_some(self.new_color),
            delete: false,
        }
    }

    /// Where the text is now: as the newest preview shows it, or estimated (moved,
    /// and resized from its top left corner).
    pub fn frame(&self) -> PdfBox {
        if let (Some((change, _, _)), Some(text)) = (&self.preview, self.preview_text)
            && *change == self.change()
        {
            return text;
        }
        let k = self.new_size / self.size.max(0.01);
        let r = self.rect;
        let (dx, dy) = self.offset;
        PdfBox {
            left: r.left + dx,
            top: r.top + dy,
            right: r.left + (r.right - r.left) * k + dx,
            bottom: r.top - (r.top - r.bottom) * k + dy,
        }
    }
}

pub struct DocTab {
    pub id: DocId,
    pub path: PathBuf,
    pub name: String,
    pub state: LoadState,

    pub mode: LayoutMode,
    /// Layout to return to when a page is opened from the grid overview.
    pub mode_before_grid: LayoutMode,
    pub rotation: Rotation,
    pub viewport: Viewport,
    pub layout: Layout,
    layout_params: Option<LayoutParams>,
    /// Page indicated as "current" in the UI.
    pub current_page: usize,
    pub page_input: String,

    pub pending_goto: Option<usize>,
    /// Page to scroll into view without moving it to the top (grid keyboard focus).
    pub pending_show: Option<usize>,
    /// Page the arrow keys have moved to in the grid overview (Enter opens it).
    pub grid_focus: Option<usize>,
    pub pending_reveal: Option<(usize, PdfBox)>,
    /// Scroll request in screen points, applied by the view.
    pub pending_scroll: Vec2,
    pub single_page_scroll_acc: f32,
    /// View rectangle from the last frame.
    pub view_rect: Rect,

    pub search: SearchState,
    pub texts: HashMap<usize, Option<PageText>>,
    pub selection: Option<Selection>,
    /// A text-selection drag is in progress.
    pub selecting: bool,

    /// Page geometry, known once text or annotations of a page were loaded.
    pub geoms: HashMap<usize, PageGeom>,
    /// Annotations per page (filled by a background scan after opening).
    pub annots: HashMap<usize, Vec<AnnotInfo>>,
    pub annots_scanned: bool,
    pub selected_annot: Option<AnnotSelection>,
    /// Mark the document unsaved once opened (a tab moved here with unsaved changes).
    pub mark_dirty_on_open: bool,
    /// A just-created annotation to open for typing once it is listed.
    pub pending_edit: Option<(usize, usize)>,
    pub draft: Option<Draft>,
    /// A box dragged with the digital signature tool, waiting for the signing dialog.
    pub sign_box: Option<(usize, PdfBox)>,
    /// Areas marked for redaction (PDF page space), applied together.
    pub redactions: Vec<(usize, PdfBox)>,
    /// Redaction area being dragged out: page, start and end (PDF page space).
    pub redact_drag: Option<(usize, Pos2, Pos2)>,
    /// Text objects per page for the edit-text tool (fetched when the tool is used),
    /// the pages asked for, and the text being edited.
    pub text_objects: HashMap<usize, Vec<crate::engine::TextObject>>,
    pub text_objects_requested: HashSet<usize>,
    /// Text objects grouped into paragraphs, per page (see `Response::TextObjects`).
    pub text_paragraphs: HashMap<usize, Vec<Vec<usize>>>,
    /// Outlines the edit-text tool draws over the pages this frame: page, box, and
    /// whether it is the text under the pointer.
    pub text_outlines: Vec<(usize, PdfBox, bool)>,
    pub text_editing: Option<TextEditing>,
    pub annot_drag: Option<AnnotDrag>,
    /// Arrow-key nudge of the selected annotation, in displayed page points.
    pub nudge: Vec2,
    /// Bumped per page when its content changes, so stale renders are replaced.
    pub page_versions: HashMap<usize, u32>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub dirty: bool,
    /// Pages selected in the thumbnail sidebar (for page operations).
    pub page_selection: BTreeSet<usize>,
    /// Anchor for Shift+click range selection of pages.
    pub page_anchor: Option<usize>,
    /// Gap (insert-before page) under the pointer while files are dragged over the
    /// thumbnails.
    pub thumb_drop_gap: Option<usize>,
    /// The thumbnails were clicked last (so Delete removes pages, not annotations).
    pub pages_focused: bool,
    /// Form field widgets per page.
    pub widgets: HashMap<usize, Vec<WidgetInfo>>,
    /// Page whose form field has keyboard focus.
    pub form_focus: Option<usize>,
    /// A mouse press was forwarded to a form field (so the release goes there too).
    pub form_pressed: bool,
    /// Requests produced by the UI this frame; sent by the app.
    pub outbox: Vec<Request>,

    /// Renders requested this frame with their priority (rebuilt every frame).
    pub wanted: Vec<(RenderKey, f32)>,
    pub requested: HashSet<RenderKey>,
    pub last_wanted: HashSet<RenderKey>,
    pub thumbs_follow_page: Option<usize>,
    /// Pages (partly) on screen in the document view, for the thumbnails.
    pub visible_pages: Vec<usize>,
    /// The page and PDF position under the mouse (where Ctrl+V pastes).
    pub hover_pdf: Option<(usize, (f32, f32))>,
    /// A message for the user raised by the document view.
    pub error_message: Option<String>,
    /// Comments sidebar: filters and the reply being written.
    pub comment_filter: CommentFilter,
    pub reply_draft: String,
    /// Comparison marks per page: rectangles and whether they are additions.
    pub diff_marks: HashMap<usize, Vec<(PdfBox, bool)>>,
    /// Outline sidebar: the bookmark being renamed and its new title.
    pub outline_rename: Option<(Vec<usize>, String)>,
}

impl DocTab {
    pub fn new(id: DocId, path: PathBuf, default_mode: LayoutMode, saved: Option<&FileState>) -> Self {
        let name =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        let mut tab = Self {
            id,
            path,
            name,
            state: LoadState::Loading,
            mode: default_mode,
            mode_before_grid: reading_mode(default_mode),
            rotation: Rotation::R0,
            viewport: Viewport { fit: default_fit(default_mode), ..Default::default() },
            layout: Layout::default(),
            layout_params: None,
            current_page: 0,
            page_input: "1".into(),
            pending_goto: None,
            pending_reveal: None,
            pending_scroll: Vec2::ZERO,
            single_page_scroll_acc: 0.0,
            view_rect: Rect::NOTHING,
            search: SearchState::default(),
            texts: HashMap::new(),
            selection: None,
            selecting: false,
            geoms: HashMap::new(),
            annots: HashMap::new(),
            annots_scanned: false,
            selected_annot: None,
            pending_edit: None,
            mark_dirty_on_open: false,
            draft: None,
            sign_box: None,
            redactions: Vec::new(),
            pending_show: None,
            grid_focus: None,
            redact_drag: None,
            text_objects: HashMap::new(),
            text_objects_requested: HashSet::new(),
            text_paragraphs: HashMap::new(),
            text_outlines: Vec::new(),
            text_editing: None,
            annot_drag: None,
            nudge: Vec2::ZERO,
            page_versions: HashMap::new(),
            can_undo: false,
            can_redo: false,
            dirty: false,
            page_selection: BTreeSet::new(),
            page_anchor: None,
            pages_focused: false,
            thumb_drop_gap: None,
            widgets: HashMap::new(),
            form_focus: None,
            form_pressed: false,
            outbox: Vec::new(),
            wanted: Vec::new(),
            requested: HashSet::new(),
            last_wanted: HashSet::new(),
            thumbs_follow_page: None,
            visible_pages: Vec::new(),
            hover_pdf: None,
            error_message: None,
            comment_filter: CommentFilter::default(),
            reply_draft: String::new(),
            outline_rename: None,
            diff_marks: HashMap::new(),
        };
        if let Some(s) = saved {
            tab.mode = s.mode;
            tab.rotation = s.rotation;
            tab.viewport.fit = s.fit;
            tab.viewport.zoom = s.zoom;
            tab.pending_goto = Some(s.page);
        }
        tab
    }

    pub fn info_mut(&mut self) -> Option<&mut DocInfo> {
        match &mut self.state {
            LoadState::Ready(info) => Some(info),
            _ => None,
        }
    }

    pub fn info(&self) -> Option<&DocInfo> {
        match &self.state {
            LoadState::Ready(info) => Some(info),
            _ => None,
        }
    }

    pub fn page_count(&self) -> usize {
        self.info().map_or(0, |i| i.page_sizes.len())
    }

    pub fn page_version(&self, page: usize) -> u32 {
        self.page_versions.get(&page).copied().unwrap_or(0)
    }

    pub fn request_text(&mut self, page: usize) {
        if let std::collections::hash_map::Entry::Vacant(e) = self.texts.entry(page) {
            e.insert(None);
            self.outbox.push(Request::PageText { doc: self.id, page });
        }
    }

    pub fn annot(&self, page: usize, index: usize) -> Option<&AnnotInfo> {
        self.annots.get(&page)?.iter().find(|a| a.index == index)
    }

    /// Ends editing the selected annotation, saving its comment if it was changed.
    pub fn deselect_annot(&mut self) {
        self.commit_annot_edit();
        self.selected_annot = None;
    }

    /// Sends the comment or text box text still being typed (it is otherwise sent when
    /// the field loses focus). Called before saving, printing and closing so the typing
    /// is not missing. Returns true if there was something to send.
    pub fn commit_annot_edit(&mut self) -> bool {
        let id = self.id;
        let exists = self.selected_annot.as_ref().is_some_and(|s| self.annot(s.page, s.index).is_some());
        let Some(sel) = self.selected_annot.as_mut().filter(|_| exists) else { return false };
        let change = match &mut sel.rich {
            Some(edit) => {
                let rich = edit.to_rich(&sel.contents);
                if rich == edit.committed {
                    return false;
                }
                edit.committed = rich.clone();
                crate::engine::AnnotChange { rich: Some(rich), ..Default::default() }
            }
            None if sel.contents != sel.committed => {
                crate::engine::AnnotChange { contents: Some(sel.contents.clone()), ..Default::default() }
            }
            None => return false,
        };
        sel.committed = sel.contents.clone();
        let (page, index) = (sel.page, sel.index);
        self.outbox.push(Request::UpdateAnnot { doc: id, page, index, change });
        true
    }

    pub fn select_annot(&mut self, page: usize, index: usize) {
        if self.selected_annot.as_ref().is_some_and(|s| s.page == page && s.index == index) {
            return;
        }
        self.deselect_annot();
        let annot = self.annot(page, index);
        let contents = annot.map(|a| a.contents.clone()).unwrap_or_default();
        let rich = annot.filter(|a| a.kind == AnnotKind::FreeText).map(|a| {
            RichEdit::new(&a.rich.clone().unwrap_or_else(|| {
                RichText::plain(
                    &a.contents,
                    CharStyle {
                        size: a.font_size.unwrap_or(12.0),
                        color: a.text_color.unwrap_or([0, 0, 0]),
                        ..Default::default()
                    },
                )
            }))
        });
        let contents = rich.as_ref().map(|r| r.committed.text()).unwrap_or(contents);
        self.selected_annot = Some(AnnotSelection {
            page,
            index,
            committed: contents.clone(),
            contents,
            focus_contents: false,
            editing: false,
            rich,
        });
        self.selection = None;
    }

    /// Esc: drops the text selection, an annotation being drawn, the annotation
    /// popup (saving its comment) and the thumbnail selection.
    pub fn cancel_interaction(&mut self) {
        self.selection = None;
        self.selecting = false;
        self.draft = None;
        self.annot_drag = None;
        self.redact_drag = None;
        self.deselect_annot();
        self.page_selection.clear();
        self.page_anchor = None;
        self.pages_focused = false;
    }

    /// Marks an area for redaction (ignored if tiny).
    pub fn mark_redaction(&mut self, page: usize, b: PdfBox) {
        if b.right - b.left >= 1.0 && b.top - b.bottom >= 1.0 && !self.redactions.contains(&(page, b)) {
            self.redactions.push((page, b));
        }
    }

    /// Marks every search hit for redaction; returns how many hits.
    pub fn mark_search_hits(&mut self) -> usize {
        let hits: Vec<(usize, PdfBox)> =
            self.search.hits.iter().flat_map(|h| h.rects.iter().map(move |r| (h.page, r.expand(1.0)))).collect();
        let n = self.search.hits.len();
        for (p, b) in hits {
            self.mark_redaction(p, b);
        }
        n
    }

    /// Marks the selected text for redaction (line by line).
    pub fn mark_selection(&mut self) {
        if let Some((page, lines)) = self.selection_line_boxes() {
            for b in lines {
                self.mark_redaction(page, b.expand(1.0));
            }
            self.selection = None;
        }
    }

    /// The marks as one page operation (and clears them).
    pub fn take_redactions(&mut self) -> Option<crate::engine::PageOp> {
        let mut areas: Vec<(usize, Vec<PdfBox>)> = Vec::new();
        for (p, b) in std::mem::take(&mut self.redactions) {
            match areas.iter_mut().find(|(q, _)| *q == p) {
                Some((_, v)) => v.push(b),
                None => areas.push((p, vec![b])),
            }
        }
        areas.sort_by_key(|(p, _)| *p);
        (!areas.is_empty()).then_some(crate::engine::PageOp::Redact { areas })
    }

    /// Pages that page operations act on: the thumbnail selection, else the current page.
    pub fn target_pages(&self) -> Vec<usize> {
        if self.page_selection.is_empty() {
            vec![self.current_page]
        } else {
            self.page_selection.iter().copied().collect()
        }
    }

    /// Updates everything keyed by page number after the page structure changed.
    pub fn apply_page_mapping(&mut self, info: DocInfo, m: &crate::engine::PageMapping) {
        // Text objects are listed anew after any page change.
        self.text_objects.clear();
        self.text_objects_requested.clear();
        self.text_paragraphs.clear();
        self.text_editing = None;
        let map = |p: usize| m.map.get(p).copied().flatten();
        let keep = |p: usize| map(p).filter(|n| !m.changed.contains(n));
        fn remap<T>(old: HashMap<usize, T>, f: impl Fn(usize) -> Option<usize>) -> HashMap<usize, T> {
            old.into_iter().filter_map(|(p, v)| f(p).map(|n| (n, v))).collect()
        }
        self.texts = remap(std::mem::take(&mut self.texts), keep);
        self.geoms = remap(std::mem::take(&mut self.geoms), keep);
        self.annots = remap(std::mem::take(&mut self.annots), map);
        self.widgets = remap(std::mem::take(&mut self.widgets), map);
        self.page_versions = remap(std::mem::take(&mut self.page_versions), map);
        for p in &m.changed {
            *self.page_versions.entry(*p).or_insert(0) += 1;
        }
        self.page_selection = self.page_selection.iter().filter_map(|p| map(*p)).collect();
        self.redactions =
            std::mem::take(&mut self.redactions).into_iter().filter_map(|(p, b)| map(p).map(|n| (n, b))).collect();
        self.redact_drag = None;
        self.page_anchor = self.page_anchor.and_then(map);
        let count = info.page_sizes.len();
        // Stay on the same page if it still exists, else on the page now at its place.
        let current = map(self.current_page).unwrap_or(self.current_page.min(count.saturating_sub(1)));
        self.state = LoadState::Ready(info);
        self.layout_params = None;
        self.selection = None;
        self.selected_annot = None;
        self.draft = None;
        self.form_focus = None;
        self.form_pressed = false;
        self.search.hits.clear();
        self.search.current = None;
        self.search.submitted = None;
        self.requested.clear();
        self.last_wanted.clear();
        self.page_input.clear();
        self.goto_page(current);
    }

    /// Display name with an unsaved-changes marker.
    pub fn title(&self) -> String {
        if self.dirty { format!("● {}", self.name) } else { self.name.clone() }
    }

    pub fn file_state(&self) -> FileState {
        FileState {
            page: self.current_page,
            zoom: self.viewport.zoom,
            fit: self.viewport.fit,
            mode: self.mode,
            rotation: self.rotation,
        }
    }

    /// Recomputes the layout if any input changed.
    pub fn relayout(&mut self, view_size: Vec2) {
        let Some(info) = self.info() else { return };
        let params = LayoutParams {
            mode: self.mode,
            rotation: self.rotation,
            // Grid rows are re-flowed only in coarse steps to avoid jitter while zooming.
            available_width: ((view_size.x / self.viewport.zoom) / 8.0).floor() * 8.0,
            single_page: self.current_page,
        };
        if self.layout_params.as_ref() != Some(&params) {
            self.layout = layout::compute(&info.page_sizes, &params);
            self.layout_params = Some(params);
        }
    }

    pub fn set_mode(&mut self, mode: LayoutMode) {
        if self.mode != mode {
            self.grid_focus = None;
            if mode == LayoutMode::Grid {
                self.mode_before_grid = self.mode;
            }
            self.mode = mode;
            self.viewport.fit = default_fit(mode);
            if mode == LayoutMode::Grid {
                self.viewport.zoom = 0.3 * ACTUAL_SIZE;
            }
            self.goto_page(self.current_page);
        }
    }

    /// Moves the grid focus by `dx` pages or `dy` rows.
    pub fn grid_move(&mut self, dx: i64, dy: i64) {
        let rows = &self.layout.rows;
        let n = self.page_count();
        if rows.is_empty() || n == 0 {
            return;
        }
        let cur = self.grid_focus.unwrap_or(self.current_page).min(n - 1);
        let page = if dy == 0 {
            (cur as i64 + dx).clamp(0, n as i64 - 1) as usize
        } else {
            let r = rows.iter().position(|r| r.contains(&cur)).unwrap_or(0);
            let c = rows[r].iter().position(|&p| p == cur).unwrap_or(0);
            let target = &rows[(r as i64 + dy).clamp(0, rows.len() as i64 - 1) as usize];
            target[c.min(target.len() - 1)]
        };
        self.grid_focus = Some(page);
        self.pending_show = Some(page);
    }

    /// Leaves the grid overview and shows `page` in the layout used before it.
    pub fn open_from_grid(&mut self, page: usize) {
        self.grid_focus = None;
        self.set_current_page(page);
        self.set_mode(reading_mode(self.mode_before_grid));
    }

    pub fn rotate(&mut self, clockwise: bool) {
        self.rotation = if clockwise { self.rotation.cw() } else { self.rotation.ccw() };
        self.goto_page(self.current_page);
    }

    pub fn goto_page(&mut self, page: usize) {
        let n = self.page_count();
        if n == 0 {
            self.pending_goto = Some(page);
            return;
        }
        let page = page.min(n - 1);
        self.pending_goto = Some(page);
        self.set_current_page(page);
    }

    pub fn set_current_page(&mut self, page: usize) {
        if self.current_page != page || self.page_input.is_empty() {
            self.current_page = page;
            self.page_input = (page + 1).to_string();
            self.thumbs_follow_page = Some(page);
        }
    }

    /// Moves by whole rows (so facing mode steps two pages at a time).
    pub fn step_page(&mut self, forward: bool) {
        let rows = &self.layout.rows;
        let page = if matches!(self.mode, LayoutMode::SinglePage | LayoutMode::Horizontal) || rows.is_empty() {
            if forward { self.current_page + 1 } else { self.current_page.saturating_sub(1) }
        } else {
            let row = rows.iter().position(|r| r.contains(&self.current_page)).unwrap_or(0);
            let target = if forward { (row + 1).min(rows.len() - 1) } else { row.saturating_sub(1) };
            rows[target][0]
        };
        self.goto_page(page.min(self.page_count().saturating_sub(1)));
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.viewport.fit = FitMode::None;
        let view = self.view_rect;
        if view.is_positive() {
            self.viewport.zoom_at(zoom, view.center(), view);
        } else {
            self.viewport.zoom = zoom;
        }
    }

    pub fn zoom_percent(&self) -> f32 {
        self.viewport.zoom / ACTUAL_SIZE * 100.0
    }

    pub fn zoom_step(&mut self, zoom_in: bool) {
        let current = self.zoom_percent();
        let next = if zoom_in {
            ZOOM_PRESETS.iter().copied().find(|&p| p > current * 1.01)
        } else {
            ZOOM_PRESETS.iter().rev().copied().find(|&p| p < current * 0.99)
        };
        if let Some(pct) = next {
            self.set_zoom(pct / 100.0 * ACTUAL_SIZE);
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let text = self.texts.get(&sel.page)?.as_ref()?;
        let range = sel.range();
        let s: String = text
            .chars
            .get(range.start.min(text.chars.len())..range.end.min(text.chars.len()))?
            .iter()
            .map(|c| c.ch)
            .collect();
        (!s.is_empty()).then(|| s.replace("\r\n", "\n").replace('\r', "\n"))
    }

    /// Line boxes of the text selection in PDF space, merged per line.
    pub fn selection_line_boxes(&self) -> Option<(usize, Vec<PdfBox>)> {
        let sel = self.selection?;
        let text = self.texts.get(&sel.page)?.as_ref()?;
        let mut lines: Vec<PdfBox> = Vec::new();
        let range = sel.range();
        for c in text.chars.get(range.start.min(text.chars.len())..range.end.min(text.chars.len()))? {
            let Some(r) = c.rect else { continue };
            match lines.last_mut() {
                Some(last) if same_line(last, &r) => *last = last.union(r),
                _ => lines.push(r),
            }
        }
        (!lines.is_empty()).then_some((sel.page, lines))
    }

    pub fn select_all_on_page(&mut self) {
        let page = self.current_page;
        if let Some(Some(text)) = self.texts.get(&page)
            && !text.chars.is_empty()
        {
            self.selection = Some(Selection { page, anchor: 0, head: text.chars.len() });
        }
    }

    /// Scrolls so that `rect` (PDF space) on `page` is visible.
    pub fn reveal(&mut self, page: usize, rect: PdfBox) {
        if self.mode == LayoutMode::SinglePage {
            self.set_current_page(page);
        }
        self.pending_reveal = Some((page, rect));
    }

    /// Selects search hit `index` and scrolls to it.
    pub fn show_search_hit(&mut self, index: usize) {
        if let Some(hit) = self.search.hits.get(index) {
            self.search.current = Some(index);
            let (page, rect) = (hit.page, hit.rects[0]);
            self.reveal(page, rect);
        }
    }

    pub fn search_step(&mut self, forward: bool) {
        let n = self.search.hits.len();
        if n == 0 {
            return;
        }
        let next = match self.search.current {
            None => 0,
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        };
        self.show_search_hit(next);
    }
}

/// `mode`, or the vertical layout if `mode` is the grid overview itself.
fn reading_mode(mode: LayoutMode) -> LayoutMode {
    if mode == LayoutMode::Grid { LayoutMode::Vertical } else { mode }
}

pub fn default_fit(mode: LayoutMode) -> FitMode {
    match mode {
        LayoutMode::Horizontal => FitMode::Height,
        LayoutMode::Vertical | LayoutMode::Facing => FitMode::Width,
        LayoutMode::SinglePage => FitMode::Page,
        LayoutMode::Grid => FitMode::None,
    }
}

/// Whether two character boxes sit on the same text line (assumes horizontal text).
pub fn same_line(a: &PdfBox, b: &PdfBox) -> bool {
    let overlap = a.top.min(b.top) - a.bottom.max(b.bottom);
    let h = (a.top - a.bottom).min(b.top - b.bottom).max(0.1);
    overlap > h * 0.5 && b.left >= a.left - h
}

/// Quad points (TL, TR, BL, BR) for a line box, as used by text markup annotations.
pub fn quad(b: &PdfBox) -> [f32; 8] {
    [b.left, b.top, b.right, b.top, b.left, b.bottom, b.right, b.bottom]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(bold: bool) -> CharStyle {
        CharStyle { bold, ..Default::default() }
    }

    #[test]
    fn styles_follow_text_edits() {
        let (n, b) = (st(false), st(true));
        // "ab" with b bold; type "X" between them in normal style.
        assert_eq!(sync_styles("ab", "aXb", &[n, b], n), vec![n, n, b]);
        // Delete the bold character.
        assert_eq!(sync_styles("ab", "a", &[n, b], n), vec![n]);
        // Replace a selection ("bc" -> "Z") with bold typing.
        assert_eq!(sync_styles("abcd", "aZd", &[n, n, n, n], b), vec![n, b, n]);
        // Repeated letters: "aa" -> "aaa" inserts one new character.
        assert_eq!(sync_styles("aa", "aaa", &[b, b], n).len(), 3);
        assert_eq!(sync_styles("", "hi", &[], b), vec![b, b]);
    }

    #[test]
    fn rich_edit_round_trip() {
        let rich = RichText::from_chars(&[('a', st(true)), ('b', st(false))], Align::Right);
        let e = RichEdit::new(&rich);
        assert_eq!(e.to_rich("ab"), rich);
    }

    #[test]
    fn comment_filters_combine() {
        let note = |author: &str, text: &str, status: &str| AnnotInfo {
            index: 0,
            kind: AnnotKind::Note,
            rect: PdfBox { left: 0.0, bottom: 0.0, right: 1.0, top: 1.0 },
            color: None,
            interior: None,
            width: 1.0,
            contents: text.into(),
            author: author.into(),
            modified: String::new(),
            font_size: None,
            text_color: None,
            rich: None,
            vertices: Vec::new(),
            replies: vec![crate::engine::Reply {
                author: "Bob".into(),
                date: String::new(),
                text: "fixed in v2".into(),
            }],
            other_replies: Vec::new(),
            status: status.into(),
            in_reply_to: None,
        };
        let a = note("Ann", "Typo here", "Accepted");
        let b = note("Carl", "Wrong figure", "");
        let mut f = CommentFilter::default();
        assert!(!f.active() && f.matches(&a) && f.matches(&b));
        f.text = "typo".into();
        assert!(f.matches(&a) && !f.matches(&b));
        f.text = "V2".into();
        assert!(f.matches(&a) && f.matches(&b), "searches replies too");
        f.text.clear();
        f.status = Some(String::new());
        assert!(!f.matches(&a) && f.matches(&b), "no status");
        f.status = Some("Accepted".into());
        f.author = Some("Ann".into());
        assert!(f.matches(&a) && !f.matches(&b));
        f.kind = Some(AnnotKind::Highlight);
        assert!(!f.matches(&a));

        // Only annotations with a comment: a highlight without one is left out, as is
        // a text box (its text is not a comment) unless someone replied to it.
        let mut f = CommentFilter { with_comment: true, ..Default::default() };
        let mut bare = note("Ann", "  ", "");
        bare.kind = AnnotKind::Highlight;
        bare.replies.clear();
        assert!(f.active() && f.matches(&a) && !f.matches(&bare));
        let mut text_box = bare.clone();
        text_box.kind = AnnotKind::FreeText;
        text_box.contents = "Box text".into();
        assert!(!f.matches(&text_box));
        bare.contents = "check this".into();
        assert!(f.matches(&bare));
        f.with_comment = false;
        assert!(f.matches(&text_box));
    }

    #[test]
    fn a_comment_still_being_typed_is_sent_before_saving() {
        let mut tab = DocTab::new(1, PathBuf::from("c.pdf"), LayoutMode::SinglePage, None);
        tab.annots.insert(
            0,
            vec![AnnotInfo {
                index: 3,
                kind: AnnotKind::Square,
                rect: PdfBox { left: 0.0, bottom: 0.0, right: 10.0, top: 10.0 },
                color: None,
                interior: None,
                width: 1.0,
                contents: "old".into(),
                author: String::new(),
                modified: String::new(),
                font_size: None,
                text_color: None,
                rich: None,
                vertices: Vec::new(),
                replies: Vec::new(),
                other_replies: Vec::new(),
                status: String::new(),
                in_reply_to: None,
            }],
        );
        tab.select_annot(0, 3);
        assert!(!tab.commit_annot_edit(), "nothing typed yet");
        tab.selected_annot.as_mut().unwrap().contents = "typed".into();
        assert!(tab.commit_annot_edit());
        let sent: Vec<_> = tab
            .outbox
            .iter()
            .filter_map(|r| match r {
                Request::UpdateAnnot { index: 3, change, .. } => change.contents.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec!["typed".to_string()]);
        assert!(!tab.commit_annot_edit(), "sent once");
        tab.deselect_annot();
        assert_eq!(tab.outbox.len(), 1, "not sent again when the comment closes");
    }

    #[test]
    fn opening_a_page_from_the_grid_returns_to_the_previous_layout() {
        let mut tab = DocTab::new(1, PathBuf::from("x.pdf"), LayoutMode::Facing, None);
        tab.set_mode(LayoutMode::Grid);
        tab.open_from_grid(7);
        assert_eq!(tab.mode, LayoutMode::Facing);
        assert_eq!(tab.current_page, 7);
        assert_eq!(tab.pending_goto, Some(7));

        // Arrow keys in the grid: by page, by row (same column, clamped), Enter opens.
        let mut tab = DocTab::new(3, PathBuf::from("z.pdf"), LayoutMode::Grid, None);
        tab.layout.rows = vec![vec![0, 1, 2], vec![3, 4, 5], vec![6, 7]];
        tab.state = LoadState::Ready(DocInfo { page_sizes: vec![(595.0, 842.0); 8], ..Default::default() });
        tab.grid_move(1, 0);
        assert_eq!(tab.grid_focus, Some(1));
        tab.grid_move(0, 1);
        assert_eq!(tab.grid_focus, Some(4));
        tab.grid_move(1, 0);
        tab.grid_move(0, 1);
        assert_eq!(tab.grid_focus, Some(7), "last row is shorter");
        tab.grid_move(0, 5);
        assert_eq!(tab.grid_focus, Some(7));
        tab.grid_move(-20, 0);
        assert_eq!(tab.grid_focus, Some(0));
        assert_eq!(tab.pending_show, Some(0));

        // Starting in the grid falls back to the vertical layout.
        let mut tab = DocTab::new(2, PathBuf::from("y.pdf"), LayoutMode::Grid, None);
        tab.open_from_grid(3);
        assert_eq!(tab.mode, LayoutMode::Vertical);
    }
}
