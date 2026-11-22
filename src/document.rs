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
    10.0, 25.0, 33.0, 50.0, 67.0, 75.0, 100.0, 125.0, 150.0, 200.0, 300.0, 400.0, 600.0, 800.0,
    1200.0, 1600.0, 2400.0, 3200.0,
];

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
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn range(&self) -> std::ops::RangeInclusive<usize> {
        self.anchor.min(self.head)..=self.anchor.max(self.head)
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
    }
}

#[derive(Clone, Debug)]
pub enum Draft {
    Ink { page: usize, points: Vec<Pos2> },
    Shape { page: usize, start: Pos2, end: Pos2 },
    /// A polygon or cloud being clicked together; `hover` is where the next corner would go.
    Poly { page: usize, points: Vec<Pos2>, hover: Option<Pos2> },
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
        let chars: Vec<(char, CharStyle)> = text
            .chars()
            .zip(self.styles.iter().copied().chain(std::iter::repeat(self.typing)))
            .collect();
        RichText::from_chars(&chars, self.align)
    }
}

/// Keeps per-character styles in step with an edit from `old` to `new`: characters
/// outside the changed middle keep their style, inserted ones get `typing`.
pub fn sync_styles(old: &str, new: &str, styles: &[CharStyle], typing: CharStyle) -> Vec<CharStyle> {
    let (a, b): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let style = |i: usize| styles.get(i).copied().unwrap_or(typing);
    let mut out: Vec<CharStyle> = (0..prefix).map(style).collect();
    out.extend(std::iter::repeat_n(typing, b.len() - prefix - suffix));
    out.extend((a.len() - suffix..a.len()).map(style));
    out
}

pub struct DocTab {
    pub id: DocId,
    pub path: PathBuf,
    pub name: String,
    pub state: LoadState,

    pub mode: LayoutMode,
    pub rotation: Rotation,
    pub viewport: Viewport,
    pub layout: Layout,
    layout_params: Option<LayoutParams>,
    /// Page indicated as "current" in the UI.
    pub current_page: usize,
    pub page_input: String,

    pub pending_goto: Option<usize>,
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
}

impl DocTab {
    pub fn new(id: DocId, path: PathBuf, default_mode: LayoutMode, saved: Option<&FileState>) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut tab = Self {
            id,
            path,
            name,
            state: LoadState::Loading,
            mode: default_mode,
            rotation: Rotation::R0,
            viewport: Viewport {
                fit: default_fit(default_mode),
                ..Default::default()
            },
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
        let Some(sel) = self.selected_annot.take() else { return };
        if let Some(edit) = &sel.rich {
            let rich = edit.to_rich(&sel.contents);
            if rich != edit.committed && self.annot(sel.page, sel.index).is_some() {
                self.outbox.push(Request::UpdateAnnot {
                    doc: self.id,
                    page: sel.page,
                    index: sel.index,
                    change: crate::engine::AnnotChange {
                        rich: Some(rich),
                        ..Default::default()
                    },
                });
            }
            return;
        }
        let changed = self.annot(sel.page, sel.index).is_some() && sel.contents != sel.committed;
        if changed {
            self.outbox.push(Request::UpdateAnnot {
                doc: self.id,
                page: sel.page,
                index: sel.index,
                change: crate::engine::AnnotChange {
                    contents: Some(sel.contents),
                    ..Default::default()
                },
            });
        }
    }

    pub fn select_annot(&mut self, page: usize, index: usize) {
        if self
            .selected_annot
            .as_ref()
            .is_some_and(|s| s.page == page && s.index == index)
        {
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
        self.deselect_annot();
        self.page_selection.clear();
        self.page_anchor = None;
        self.pages_focused = false;
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
        if self.dirty {
            format!("● {}", self.name)
        } else {
            self.name.clone()
        }
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
            self.mode = mode;
            self.viewport.fit = default_fit(mode);
            if mode == LayoutMode::Grid {
                self.viewport.zoom = 0.3 * ACTUAL_SIZE;
            }
            self.goto_page(self.current_page);
        }
    }

    pub fn rotate(&mut self, clockwise: bool) {
        self.rotation = if clockwise {
            self.rotation.cw()
        } else {
            self.rotation.ccw()
        };
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
        let page = if matches!(self.mode, LayoutMode::SinglePage | LayoutMode::Horizontal)
            || rows.is_empty()
        {
            if forward {
                self.current_page + 1
            } else {
                self.current_page.saturating_sub(1)
            }
        } else {
            let row = rows
                .iter()
                .position(|r| r.contains(&self.current_page))
                .unwrap_or(0);
            let target = if forward {
                (row + 1).min(rows.len() - 1)
            } else {
                row.saturating_sub(1)
            };
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
            .get(*range.start()..=(*range.end()).min(text.chars.len().saturating_sub(1)))?
            .iter()
            .map(|c| c.ch)
            .collect();
        Some(s.replace("\r\n", "\n").replace('\r', "\n"))
    }

    /// Line boxes of the text selection in PDF space, merged per line.
    pub fn selection_line_boxes(&self) -> Option<(usize, Vec<PdfBox>)> {
        let sel = self.selection?;
        let text = self.texts.get(&sel.page)?.as_ref()?;
        let mut lines: Vec<PdfBox> = Vec::new();
        for c in text.chars.get(sel.range())? {
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
            self.selection = Some(Selection {
                page,
                anchor: 0,
                head: text.chars.len() - 1,
            });
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
        CharStyle {
            bold,
            ..Default::default()
        }
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
            rect: PdfBox {
                left: 0.0,
                bottom: 0.0,
                right: 1.0,
                top: 1.0,
            },
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
    }
}

