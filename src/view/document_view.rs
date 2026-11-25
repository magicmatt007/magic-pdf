//! The central document widget: draws pages and handles scrolling, zooming, text
//! selection, links and annotation tools.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, Sense, Shadow, Shape, Stroke, StrokeKind,
    Ui, Vec2, pos2, vec2,
};

use crate::i18n::{t, tf};
use crate::icons;
use crate::document::{AnnotDrag, AnnotSelection, DocTab, DragMode, Draft, LoadState, Selection, quad};
use crate::engine::{
    Align, AnnotChange, CharStyle, AnnotInfo, AnnotKind, FormMouse, LinkTarget, NewAnnot, PdfBox, RenderKey, Request, Resize, Rgb,
    form_mods,
};
use crate::engine::StampSpec;
use crate::settings::{AnnotStyle, Signature};
use crate::keys::kb;
use crate::view::cache::{TextureCache, exact_width, snap_to_pixels, width_bucket};
use crate::view::color_picker;
use crate::view::layout::{GAP, LayoutMode, geom_box_to_local, geom_local_to_pdf, geom_pdf_to_local};
use crate::view::viewport::{FitMode, Viewport};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Hand,
    SelectText,
    Highlight,
    Underline,
    StrikeOut,
    Note,
    Ink,
    Rect,
    Ellipse,
    TextBox,
    Line,
    Arrow,
    Polygon,
    Cloud,
    Stamp,
    Signature,
    Distance,
    Area,
    Redact,
}

impl Tool {
    pub const ANNOTATION_TOOLS: [Tool; 16] = [
        Tool::Highlight,
        Tool::Underline,
        Tool::StrikeOut,
        Tool::Note,
        Tool::TextBox,
        Tool::Ink,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Line,
        Tool::Arrow,
        Tool::Polygon,
        Tool::Cloud,
        Tool::Distance,
        Tool::Area,
        Tool::Stamp,
        Tool::Signature,
    ];

    /// The annotation kind a shape tool draws.
    pub fn shape_kind(self) -> Option<AnnotKind> {
        match self {
            Tool::Line => Some(AnnotKind::DrawnLine),
            Tool::Arrow => Some(AnnotKind::Arrow),
            Tool::Polygon => Some(AnnotKind::Polygon),
            Tool::Cloud => Some(AnnotKind::Cloud),
            Tool::Distance => Some(AnnotKind::Distance),
            Tool::Area => Some(AnnotKind::Area),
            _ => None,
        }
    }

    /// Tools that draw by dragging on the page.
    fn draws(self) -> bool {
        matches!(self, Tool::Ink | Tool::Rect | Tool::Ellipse | Tool::TextBox) || self.shape_kind().is_some()
    }

    pub fn markup_kind(self) -> Option<AnnotKind> {
        match self {
            Tool::Highlight => Some(AnnotKind::Highlight),
            Tool::Underline => Some(AnnotKind::Underline),
            Tool::StrikeOut => Some(AnnotKind::StrikeOut),
            _ => None,
        }
    }

    pub fn icon(self) -> egui::RichText {
        icons::text(match self {
            Tool::Hand => icons::HAND,
            Tool::SelectText => icons::CURSOR_TEXT,
            Tool::Highlight => icons::HIGHLIGHTER,
            Tool::Underline => icons::TEXT_UNDERLINE,
            Tool::StrikeOut => icons::TEXT_STRIKETHROUGH,
            Tool::Note => icons::NOTE_PENCIL,
            Tool::Ink => icons::SCRIBBLE,
            Tool::Rect => icons::RECTANGLE,
            Tool::Ellipse => icons::CIRCLE,
            Tool::TextBox => icons::TEXTBOX,
            Tool::Line => icons::LINE,
            Tool::Arrow => icons::ARROW,
            Tool::Polygon => icons::POLYGON,
            Tool::Cloud => icons::CLOUD,
            Tool::Stamp => icons::STAMP,
            Tool::Signature => icons::SIGNATURE,
            Tool::Distance => icons::RULER,
            Tool::Area => icons::AREA,
            Tool::Redact => icons::EYE_SLASH,
        })
    }

    pub fn label(self) -> &'static str {
        t(match self {
            Tool::Hand => "Hand (H)",
            Tool::SelectText => "Select text (S)",
            Tool::Highlight => "Highlight text (U)",
            Tool::Underline => "Underline text",
            Tool::StrikeOut => "Strike out text",
            Tool::Note => "Sticky note (N)",
            Tool::Ink => "Freehand drawing (D)",
            Tool::Rect => "Rectangle (R)",
            Tool::Ellipse => "Ellipse (E)",
            Tool::TextBox => "Text box (T): click or drag, then type",
            Tool::Line => "Line (L), Shift: 45° steps",
            Tool::Arrow => "Arrow (A), Shift: 45° steps",
            Tool::Polygon => "Polygon (P): click the corners, double-click or Enter to finish; or drag a rectangle",
            Tool::Cloud => "Cloud (C): click the corners, double-click or Enter to finish; or drag a rectangle",
            Tool::Stamp => "Stamp (K): click to place",
            Tool::Signature => "Signature (G): click to place",
            Tool::Distance => "Measure distance (M), Shift: 45° steps",
            Tool::Area => "Measure area: click the corners, double-click or Enter to finish",
            Tool::Redact => "Redact (X): drag over an area or click a word to mark it; click a mark to unmark it",
        })
    }

    /// Colour slot this tool uses for new annotations.
    pub fn color(self, style: &mut AnnotStyle) -> Option<&mut Rgb> {
        match self {
            Tool::Highlight => Some(&mut style.highlight),
            Tool::Underline => Some(&mut style.underline),
            Tool::StrikeOut => Some(&mut style.strikeout),
            Tool::Note => Some(&mut style.note),
            Tool::Ink => Some(&mut style.ink),
            Tool::Rect | Tool::Ellipse | Tool::Line | Tool::Arrow | Tool::Polygon | Tool::Cloud | Tool::Distance | Tool::Area => {
                Some(&mut style.shape)
            }
            Tool::TextBox => Some(&mut style.textbox_border),
            Tool::Stamp if style.stamp_image.is_none() => Some(&mut style.stamp_color),
            Tool::Signature => Some(&mut style.signature_color),
            Tool::Stamp => None,
            Tool::Hand | Tool::SelectText | Tool::Redact => None,
        }
    }
}

pub struct ViewContext<'a> {
    pub cache: &'a mut TextureCache,
    /// Recently picked colours (shared by all colour pickers).
    pub recent_colors: &'a mut Vec<Rgb>,
    pub tool: Tool,
    pub night: bool,
    pub style: &'a AnnotStyle,
    pub author: &'a str,
    /// What the stamp tool places (None: nothing chosen or the image is missing).
    pub stamp: Option<&'a StampSpec>,
    /// What the signature tool places.
    pub signature: Option<&'a Signature>,
    /// The picture of an image signature.
    pub signature_image: Option<&'a StampSpec>,
    /// Presentation mode: pages on black.
    pub presenting: bool,
}

const SEARCH_COLOR: Color32 = Color32::from_rgba_premultiplied(120, 100, 0, 90);
const SEARCH_CURRENT_COLOR: Color32 = Color32::from_rgba_premultiplied(160, 60, 0, 130);
const REDACTION_FILL: Color32 = Color32::from_rgba_premultiplied(60, 0, 0, 70);
const REDACTION_LINE: Color32 = Color32::from_rgb(220, 30, 30);
const SELECTION_COLOR: Color32 = Color32::from_rgba_premultiplied(30, 90, 200, 90);
const NOTE_SIZE: f32 = 20.0;

pub fn rgb(c: Rgb) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

pub fn show(ui: &mut Ui, tab: &mut DocTab, mut cx: ViewContext<'_>) {
    let (view, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let bg = if cx.presenting {
        Color32::BLACK
    } else if cx.night {
        Color32::from_gray(24)
    } else {
        crate::theme::canvas(ui.visuals().dark_mode)
    };
    ui.painter().rect_filled(view, CornerRadius::ZERO, bg);

    match &tab.state {
        LoadState::Ready(_) => {}
        LoadState::Loading => {
            centered_text(ui, view, "Loading…");
            return;
        }
        LoadState::Failed(e) => {
            centered_text(ui, view, &tf("Could not open {file}:\n{e}", &[("file", &tab.path.display()), ("e", e)]));
            return;
        }
        LoadState::NeedsPassword { .. } => {
            centered_text(ui, view, "This document is password protected.");
            return;
        }
    }
    if tab.page_count() == 0 {
        centered_text(ui, view, "The document has no pages.");
        return;
    }
    tab.view_rect = view;

    tab.relayout(view.size());
    apply_fit(tab, view);
    tab.relayout(view.size());

    handle_input(ui, tab, &response, view, &mut cx);

    apply_nudge(tab);
    apply_pending_navigation(tab, view);
    tab.viewport.clamp(tab.layout.size, view.size());
    update_current_page(tab, view);

    paint_pages(ui, tab, view, &mut cx);
    if tab.mode == LayoutMode::Grid
        && let Some(rect) = tab.grid_focus.and_then(|p| tab.layout.page_rect(p))
    {
        let s = tab.viewport.rect_to_screen(rect, view).expand(4.0);
        ui.painter_at(view).rect_stroke(s, CornerRadius::same(3), Stroke::new(3.0, Color32::from_rgb(30, 120, 255)), StrokeKind::Outside);
    }
    selection_toolbar(ui, tab, view, &cx);
    annotation_popup(ui, tab, view, cx.recent_colors, cx.author);
}

fn centered_text(ui: &Ui, view: Rect, text: &str) {
    ui.painter().text(
        view.center(),
        Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(16.0),
        Color32::from_gray(220),
    );
}

/// Recomputes zoom for sticky fit modes (window resize, layout change).
fn apply_fit(tab: &mut DocTab, view: Rect) {
    if tab.viewport.fit == FitMode::None {
        return;
    }
    let page = tab.current_page;
    let target = if tab.mode == LayoutMode::Facing {
        tab.layout.row_rect(page)
    } else {
        tab.layout.page_rect(page)
    };
    let Some(target) = target else { return };
    if let Some(z) = Viewport::fit_zoom(tab.viewport.fit, target.size(), view.size(), GAP * tab.viewport.zoom.min(1.0))
        && (z - tab.viewport.zoom).abs() > 1e-4
    {
        // Keep the current page anchored while the zoom changes.
        let anchor_doc = target.min;
        let anchor_screen = tab.viewport.to_screen(anchor_doc, view);
        tab.viewport.zoom = z;
        tab.viewport.offset = anchor_doc.to_vec2() - (anchor_screen - view.min) / z;
        if tab.pending_goto.is_none() {
            tab.pending_goto = Some(page);
        }
    }
}

/// Screen position -> (page, PDF point), if the page geometry is known.
fn screen_to_pdf(tab: &DocTab, screen: Pos2, view: Rect, page_hint: Option<usize>) -> Option<(usize, Pos2)> {
    let doc_pt = tab.viewport.to_doc(screen, view);
    let page = tab.layout.page_at(doc_pt).or(page_hint)?;
    let page_rect = tab.layout.page_rect(page)?;
    let geom = tab.geoms.get(&page)?;
    let local = (doc_pt - page_rect.min).to_pos2();
    Some((page, geom_local_to_pdf(local, geom, tab.rotation)))
}

/// PDF point on `page` -> screen position.
fn pdf_to_screen(tab: &DocTab, page: usize, p: Pos2, view: Rect) -> Option<Pos2> {
    let page_rect = tab.layout.page_rect(page)?;
    let geom = tab.geoms.get(&page)?;
    let local = geom_pdf_to_local(p, geom, tab.rotation);
    Some(tab.viewport.to_screen(page_rect.min + local.to_vec2(), view))
}

fn pdf_box_to_screen(tab: &DocTab, page: usize, b: &PdfBox, view: Rect) -> Option<Rect> {
    let page_rect = tab.layout.page_rect(page)?;
    let geom = tab.geoms.get(&page)?;
    let local = geom_box_to_local(b, geom, tab.rotation);
    Some(tab.viewport.rect_to_screen(local.translate(page_rect.min.to_vec2()), view))
}

fn annot_at(tab: &DocTab, page: usize, p: Pos2) -> Option<&AnnotInfo> {
    tab.annots
        .get(&page)?
        .iter()
        .rev()
        .filter(|a| a.in_reply_to.is_none())
        .find(|a| a.rect.expand(2.0).contains(p.x, p.y))
}

/// Screen position -> PDF point on a given page (also outside the page).
fn screen_to_pdf_on(tab: &DocTab, screen: Pos2, view: Rect, page: usize) -> Option<Pos2> {
    let page_rect = tab.layout.page_rect(page)?;
    let geom = tab.geoms.get(&page)?;
    let local = (tab.viewport.to_doc(screen, view) - page_rect.min).to_pos2();
    Some(geom_local_to_pdf(local, geom, tab.rotation))
}

/// The redaction tool: drag to mark an area, click a word to mark it, click a mark
/// to unmark it.
fn redact_input(ui: &Ui, tab: &mut DocTab, response: &egui::Response, view: Rect, pointer: Option<Pos2>) {
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    }
    let on_page = pointer.filter(|p| view.contains(*p)).and_then(|pt| screen_to_pdf(tab, pt, view, None));
    if let Some((page, _)) = on_page
        && !tab.texts.contains_key(&page)
    {
        tab.request_text(page);
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        tab.redact_drag = on_page.map(|(page, p)| (page, p, p));
    } else if response.dragged_by(egui::PointerButton::Primary) {
        if let Some(page) = tab.redact_drag.map(|d| d.0)
            && let Some(p) = pointer.and_then(|pt| screen_to_pdf_on(tab, pt, view, page))
            && let Some(d) = &mut tab.redact_drag
        {
            d.2 = p;
        }
    } else if response.drag_stopped() {
        if let Some((page, a, b)) = tab.redact_drag.take() {
            let r = PdfBox {
                left: a.x.min(b.x),
                bottom: a.y.min(b.y),
                right: a.x.max(b.x),
                top: a.y.max(b.y),
            };
            tab.mark_redaction(page, r);
        }
    } else if response.clicked()
        && let Some((page, p)) = on_page
    {
        if let Some(i) = tab.redactions.iter().rposition(|(q, b)| *q == page && b.contains(p.x, p.y)) {
            tab.redactions.remove(i);
        } else if let Some(word) = word_at(tab, page, p) {
            tab.mark_redaction(page, word.expand(1.0));
        }
    }
}

/// The box of the word under `p`, if any.
fn word_at(tab: &DocTab, page: usize, p: Pos2) -> Option<PdfBox> {
    let text = tab.texts.get(&page)?.as_ref()?;
    let i = text.chars.iter().position(|c| c.rect.is_some_and(|r| r.expand(1.0).contains(p.x, p.y)))?;
    let is_word = |c: char| c.is_alphanumeric() || "_-@.".contains(c);
    if !is_word(text.chars[i].ch) {
        return text.chars[i].rect;
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && is_word(text.chars[a - 1].ch) {
        a -= 1;
    }
    while b + 1 < text.chars.len() && is_word(text.chars[b + 1].ch) {
        b += 1;
    }
    // Trailing full stops belong to the sentence, not to the word.
    while b > a && text.chars[b].ch == '.' {
        b -= 1;
    }
    text.chars[a..=b].iter().filter_map(|c| c.rect).reduce(PdfBox::union)
}

fn handle_input(ui: &Ui, tab: &mut DocTab, response: &egui::Response, view: Rect, cx: &mut ViewContext<'_>) {
    let (scroll, zoom_delta, shift, pointer) = ui.input(|i| {
        (
            i.smooth_scroll_delta,
            i.zoom_delta(),
            i.modifiers.shift,
            i.pointer.hover_pos(),
        )
    });
    let hovered = response.hovered() || response.contains_pointer();
    if response.clicked() || response.drag_started() {
        // Delete now refers to annotations again, not to selected thumbnails.
        tab.pages_focused = false;
    }
    if !ui.input(|i| i.pointer.primary_down()) {
        tab.selecting = false;
    }

    // Zoom (Ctrl+wheel, pinch).
    if hovered && zoom_delta != 1.0 {
        let anchor = pointer.unwrap_or(view.center());
        tab.viewport.fit = FitMode::None;
        let z = tab.viewport.zoom * zoom_delta;
        tab.viewport.zoom_at(z, anchor, view);
        tab.relayout(view.size());
    }

    // Wheel scrolling.
    let mut delta = Vec2::ZERO;
    if hovered && scroll != Vec2::ZERO {
        delta = scroll;
        if tab.mode.is_horizontal() {
            if shift {
                // Shift+wheel scrolls vertically in horizontal mode (egui turned it into x).
                delta = vec2(0.0, scroll.x + scroll.y);
            } else if scroll.x == 0.0 {
                // A plain mouse wheel moves through the page row.
                delta = vec2(scroll.y, 0.0);
            }
        }
    }
    delta += std::mem::take(&mut tab.pending_scroll);

    if tab.mode == LayoutMode::SinglePage && delta.y != 0.0 {
        let content_h = tab.layout.size.y * tab.viewport.zoom;
        let off_y = tab.viewport.offset.y * tab.viewport.zoom;
        let at_top = off_y <= 0.5;
        let at_bottom = off_y + view.height() >= content_h - 0.5;
        if (delta.y > 0.0 && at_top) || (delta.y < 0.0 && at_bottom) {
            tab.single_page_scroll_acc += delta.y;
            if tab.single_page_scroll_acc.abs() > 120.0 {
                let forward = tab.single_page_scroll_acc < 0.0;
                tab.single_page_scroll_acc = 0.0;
                tab.step_page(forward);
                if !forward {
                    tab.pending_scroll = vec2(0.0, -1e6);
                }
                return;
            }
        } else {
            tab.single_page_scroll_acc = 0.0;
        }
    }
    tab.viewport.offset -= delta / tab.viewport.zoom;

    let (middle_down, primary_down) = ui.input(|i| {
        (
            i.pointer.button_down(egui::PointerButton::Middle),
            i.pointer.primary_down(),
        )
    });
    let tool = cx.tool;

    // Only use the pointer for page interaction when the page is what is under it,
    // not a popup or menu drawn on top (e.g. the annotation popup over a form field).
    // A drag that started on the page keeps going even if it passes under a popup.
    let pointer = pointer.filter(|p| {
        response.dragged() || response.drag_stopped() || ui.ctx().layer_id_at(*p) == Some(ui.layer_id())
    });

    if tool == Tool::Redact && !middle_down {
        redact_input(ui, tab, response, view, pointer);
        return;
    }

    // Drawing tools.
    if let Some((page, p)) = pointer.filter(|p| view.contains(*p)).and_then(|pt| screen_to_pdf(tab, pt, view, None)) {
        tab.hover_pdf = Some((page, (p.x, p.y)));
    } else if hovered {
        tab.hover_pdf = None;
    }
    if tool.draws() && !middle_down {
        draw_tool_input(ui, tab, response, view, cx, pointer);
        if hovered {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }
        // A click (no drag) with the text box tool places a box of default width.
        if tool == Tool::TextBox
            && response.clicked()
            && let Some((page, p)) = pointer.and_then(|pt| screen_to_pdf(tab, pt, view, None))
        {
            let rect = PdfBox {
                left: p.x,
                bottom: p.y - 20.0,
                right: p.x + crate::engine::textbox::DEFAULT_WIDTH,
                top: p.y,
            };
            create(tab, page, cx, AnnotKind::FreeText, rect, cx.style.textbox_border, Vec::new(), Vec::new());
            return;
        }
        if response.dragged() || tab.draft.is_some() {
            return;
        }
    }

    // Moving and resizing annotations with the hand and select tools.
    if matches!(tool, Tool::Hand | Tool::SelectText) && !middle_down && annot_drag_input(ui, tab, response, view, pointer) {
        return;
    }

    // Interactive form fields take mouse input with the hand and select tools.
    if matches!(tool, Tool::Hand | Tool::SelectText) && !middle_down && form_input(ui, tab, view, pointer) {
        return;
    }

    // Panning: middle button always; primary with the hand tool.
    let panning = response.dragged() && (middle_down || tool == Tool::Hand);
    if panning {
        tab.viewport.offset -= response.drag_delta() / tab.viewport.zoom;
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }

    let Some(pointer) = pointer.filter(|p| view.contains(*p)) else { return };
    let drag_page = tab.selection.filter(|_| response.dragged()).map(|s| s.page);
    let doc_pt = tab.viewport.to_doc(pointer, view);
    let Some(page) = tab.layout.page_at(doc_pt).or(drag_page) else {
        if tool == Tool::Hand && !panning {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if response.clicked() {
            tab.selection = None;
            tab.deselect_annot();
        }
        return;
    };
    if !tab.texts.contains_key(&page) {
        tab.request_text(page);
    }
    let Some((_, pdf_pt)) = screen_to_pdf(tab, pointer, view, Some(page)) else { return };

    // Grid overview: double-click a page to open it in the previous layout.
    if tab.mode == LayoutMode::Grid && matches!(tool, Tool::Hand | Tool::SelectText) {
        if response.double_clicked() {
            tab.open_from_grid(page);
            return;
        }
        if response.clicked() {
            tab.grid_focus = Some(page);
        }
    }

    // Stamps and signatures: a click places them.
    if matches!(tool, Tool::Stamp | Tool::Signature) {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        if response.clicked() {
            place_stamp(tab, page, pdf_pt, cx);
        }
        return;
    }

    // Sticky notes.
    if tool == Tool::Note {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        if response.clicked() {
            let rect = PdfBox {
                left: pdf_pt.x,
                top: pdf_pt.y,
                right: pdf_pt.x + NOTE_SIZE,
                bottom: pdf_pt.y - NOTE_SIZE,
            };
            create(tab, page, cx, AnnotKind::Note, rect, cx.style.note, Vec::new(), Vec::new());
        }
        return;
    }

    let text = tab.texts.get(&page).and_then(|t| t.as_ref());

    // Links (hand and select tools).
    let link = text
        .filter(|_| matches!(tool, Tool::Hand | Tool::SelectText))
        .and_then(|t| t.links.iter().find(|l| l.rect.contains(pdf_pt.x, pdf_pt.y)))
        .map(|l| l.target.clone());

    // Annotations under the pointer.
    let hovered_annot = annot_at(tab, page, pdf_pt).cloned();
    if let Some(a) = &hovered_annot
        && !panning
        && !response.dragged()
        && (!a.contents.is_empty() || !a.author.is_empty())
    {
        let mut tip = if a.author.is_empty() {
            a.contents.clone()
        } else if a.contents.is_empty() {
            a.author.clone()
        } else {
            format!("{}\n— {}", a.contents, a.author)
        };
        for r in a.other_replies.iter().chain(&a.replies) {
            let who = if r.author.is_empty() { "?" } else { r.author.as_str() };
            tip += &format!("\n↳ {who}: {}", r.text);
        }
        response.clone().on_hover_text_at_pointer(tip);
    }

    if let Some(target) = &link {
        if !panning {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if let LinkTarget::Uri(uri) = target {
            response.clone().on_hover_text_at_pointer(uri.as_str());
        }
        if response.clicked() {
            match target {
                LinkTarget::Page(p) => tab.goto_page(*p),
                LinkTarget::Uri(uri) => {
                    if let Err(e) = open::that_detached(uri) {
                        log::warn!("failed to open {uri}: {e}");
                    }
                }
            }
            return;
        }
    }

    if matches!(tool, Tool::Hand | Tool::SelectText) && hovered_annot.is_some() && link.is_none() && !panning {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if response.double_clicked()
        && let Some(a) = hovered_annot.as_ref().filter(|a| a.kind == AnnotKind::FreeText)
        && matches!(tool, Tool::Hand | Tool::SelectText)
    {
        // Double-click a text box to edit its text.
        let index = a.index;
        focus_annot(tab, page, index, true);
        return;
    }
    if response.clicked() && link.is_none() {
        match &hovered_annot {
            Some(a) if matches!(tool, Tool::Hand | Tool::SelectText) => {
                let index = a.index;
                tab.select_annot(page, index);
                return;
            }
            _ => tab.deselect_annot(),
        }
    }

    if tool == Tool::Hand {
        if link.is_none() && hovered_annot.is_none() && !panning {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if response.clicked() {
            tab.selection = None;
        }
        return;
    }

    // Text selection (select tool and text markup tools).
    let Some(text) = tab.texts.get(&page).and_then(|t| t.as_ref()) else { return };
    let over_text = text
        .chars
        .iter()
        .any(|c| c.rect.is_some_and(|r| r.expand(1.0).contains(pdf_pt.x, pdf_pt.y)));
    if over_text && link.is_none() {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        // A drag is noticed only after a few pixels of movement: start where the
        // button was pressed.
        let origin = ui
            .input(|i| i.pointer.press_origin())
            .and_then(|o| screen_to_pdf_on(tab, o, view, page))
            .unwrap_or(pdf_pt);
        if let Some(i) = caret_at(text, origin) {
            // Nothing is selected until the mouse moves away from where it started.
            tab.selection = Some(Selection {
                page,
                anchor: i,
                head: i,
            });
            tab.selecting = true;
            tab.deselect_annot();
        }
    } else if response.dragged_by(egui::PointerButton::Primary) && primary_down {
        if let Some(sel) = &mut tab.selection
            && sel.page == page
            && let Some(i) = caret_at(text, pdf_pt)
        {
            sel.head = i;
        }
    } else if response.drag_stopped() {
        if let Some(kind) = tool.markup_kind() {
            create_markup(tab, cx, kind);
        }
    } else if response.double_clicked() {
        if let Some(i) = nearest_char(text, pdf_pt) {
            let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
            let mut a = i;
            while a > 0 && is_word(text.chars[a - 1].ch) {
                a -= 1;
            }
            let mut b = i;
            while b + 1 < text.chars.len() && is_word(text.chars[b + 1].ch) {
                b += 1;
            }
            tab.selection = Some(Selection {
                page,
                anchor: a,
                head: b + 1,
            });
            if let Some(kind) = tool.markup_kind() {
                create_markup(tab, cx, kind);
            }
        }
    } else if response.clicked() {
        tab.selection = None;
    }
}

/// Routes mouse input to PDFium's form-fill module when it concerns a form field.
/// Returns true if the input was consumed.
fn form_input(ui: &Ui, tab: &mut DocTab, view: Rect, pointer: Option<Pos2>) -> bool {
    let Some(pointer) = pointer.filter(|p| view.contains(*p)) else { return false };
    let doc_pt = tab.viewport.to_doc(pointer, view);
    let Some(page) = tab.layout.page_at(doc_pt) else {
        if tab.form_focus.is_some() && ui.input(|i| i.pointer.primary_pressed()) {
            tab.outbox.push(Request::FormKillFocus { doc: tab.id });
            tab.form_focus = None;
        }
        return false;
    };
    let Some((_, p)) = screen_to_pdf(tab, pointer, view, Some(page)) else { return false };
    let widget = tab
        .widgets
        .get(&page)
        .and_then(|ws| ws.iter().find(|w| w.rect.contains(p.x, p.y)))
        .cloned();
    let focused_here = tab.form_focus == Some(page);
    let (pressed, released, double, moved, mods) = ui.input(|i| {
        let m = &i.modifiers;
        let mods = (if m.shift { form_mods::SHIFT } else { 0 })
            | (if m.command || m.ctrl { form_mods::CONTROL } else { 0 })
            | (if m.alt { form_mods::ALT } else { 0 });
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_released(),
            i.pointer.button_double_clicked(egui::PointerButton::Primary),
            i.pointer.delta() != Vec2::ZERO,
            mods,
        )
    });
    if widget.is_none() && !focused_here && !tab.form_pressed {
        if pressed && tab.form_focus.is_some() {
            // Clicking outside any field ends editing.
            tab.outbox.push(Request::FormKillFocus { doc: tab.id });
            tab.form_focus = None;
        }
        return false;
    }
    if let Some(w) = &widget {
        ui.ctx().set_cursor_icon(if w.field_type == crate::engine::FIELD_TEXT {
            CursorIcon::Text
        } else {
            CursorIcon::PointingHand
        });
    }
    let mut send = |event| {
        tab.outbox.push(Request::FormMouse {
            doc: tab.id,
            page,
            event,
            x: p.x,
            y: p.y,
            modifiers: mods,
        });
    };
    if moved {
        send(FormMouse::Move);
    }
    if double {
        send(FormMouse::DoubleClick);
    } else if pressed {
        send(FormMouse::Down);
        tab.form_pressed = true;
    }
    if released && tab.form_pressed {
        send(FormMouse::Up);
        tab.form_pressed = false;
    }
    if pressed {
        tab.selection = None;
        tab.deselect_annot();
    }
    // Hovering a field without focus only changes the cursor; other tools still work
    // for drags that start elsewhere.
    widget.is_some() || focused_here || tab.form_pressed || released
}

fn draw_tool_input(
    ui: &Ui,
    tab: &mut DocTab,
    response: &egui::Response,
    view: Rect,
    cx: &mut ViewContext<'_>,
    pointer: Option<Pos2>,
) {
    let Some(pointer) = pointer else { return };
    let poly_tool = matches!(cx.tool, Tool::Polygon | Tool::Cloud | Tool::Area);
    if poly_tool {
        // Clicks add corners; double-click, Enter or a click on the first corner finish.
        let at = screen_to_pdf(tab, pointer, view, match &tab.draft {
            Some(Draft::Poly { page, .. }) => Some(*page),
            _ => None,
        });
        if let (Some(Draft::Poly { hover, .. }), Some((_, p))) = (&mut tab.draft, at) {
            *hover = Some(p);
        }
        let finish = ui.input(|i| i.key_pressed(egui::Key::Enter)) || response.double_clicked();
        if response.clicked() && !response.double_clicked() {
            let current = match &tab.draft {
                Some(Draft::Poly { page, points, .. }) => Some((*page, points[0], points.len())),
                _ => None,
            };
            match (current, at) {
                (Some((page, first, count)), Some((pg, p))) if page == pg => {
                    let first = pdf_to_screen(tab, pg, first, view);
                    let points = vec![(); count];
                    let closes = points.len() >= 3 && first.is_some_and(|f| (f - pointer).length() < 8.0);
                    if closes {
                        finish_poly(tab, cx);
                        return;
                    }
                    if let Some(Draft::Poly { points, .. }) = &mut tab.draft
                        && points.last().is_none_or(|l| (*l - p).length() > 1.0)
                    {
                        points.push(p);
                    }
                }
                (_, Some((page, p))) => {
                    tab.deselect_annot();
                    tab.draft = Some(Draft::Poly {
                        page,
                        points: vec![p],
                        hover: None,
                    });
                }
                _ => {}
            }
        }
        if finish {
            finish_poly(tab, cx);
            return;
        }
        if matches!(tab.draft, Some(Draft::Poly { .. })) {
            return;
        }
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        let press = ui.input(|i| i.pointer.press_origin()).unwrap_or(pointer);
        let doc_pt = tab.viewport.to_doc(press, view);
        if let Some(page) = tab.layout.page_at(doc_pt) {
            if !tab.geoms.contains_key(&page) {
                tab.request_text(page);
            }
            if let Some((page, p)) = screen_to_pdf(tab, press, view, Some(page)) {
                tab.deselect_annot();
                tab.draft = Some(match cx.tool {
                    Tool::Ink => Draft::Ink {
                        page,
                        points: vec![p],
                    },
                    _ => Draft::Shape { page, start: p, end: p },
                });
            }
        }
    } else if response.dragged_by(egui::PointerButton::Primary) {
        let page = match &tab.draft {
            Some(Draft::Ink { page, .. } | Draft::Shape { page, .. } | Draft::Poly { page, .. }) => *page,
            None => return,
        };
        let shift = ui.input(|i| i.modifiers.shift);
        if let Some((_, p)) = screen_to_pdf(tab, pointer, view, Some(page)) {
            match &mut tab.draft {
                Some(Draft::Ink { points, .. }) => {
                    if points.last().is_none_or(|l| (*l - p).length() > 0.5) {
                        points.push(p);
                    }
                }
                Some(Draft::Shape { start, end, .. }) => {
                    *end = if shift && matches!(cx.tool, Tool::Line | Tool::Arrow | Tool::Distance) { snap_45(*start, p) } else { p };
                }
                Some(Draft::Poly { .. }) | None => {}
            }
        }
    } else if response.drag_stopped() || (tab.draft.is_some() && !response.dragged()) {
        let Some(draft) = tab.draft.take() else { return };
        let style = cx.style;
        match draft {
            Draft::Ink { page, points } if points.len() > 1 => {
                let w = style.ink_width;
                let rect = points
                    .iter()
                    .map(|p| PdfBox {
                        left: p.x,
                        bottom: p.y,
                        right: p.x,
                        top: p.y,
                    })
                    .reduce(PdfBox::union)
                    .unwrap()
                    .expand(w);
                let stroke = points.iter().map(|p| (p.x, p.y)).collect();
                create(tab, page, cx, AnnotKind::Ink, rect, style.ink, Vec::new(), vec![stroke]);
            }
            Draft::Shape { page, start, end } if matches!(cx.tool, Tool::Line | Tool::Arrow | Tool::Distance) => {
                if (end - start).length() >= 3.0 {
                    create_shape(tab, page, cx, vec![(start.x, start.y), (end.x, end.y)]);
                }
            }
            Draft::Shape { page, start, end } if matches!(cx.tool, Tool::Polygon | Tool::Cloud | Tool::Area) => {
                // A dragged rectangle.
                if (end.x - start.x).abs() >= 3.0 && (end.y - start.y).abs() >= 3.0 {
                    let pts = vec![(start.x, start.y), (end.x, start.y), (end.x, end.y), (start.x, end.y)];
                    create_shape(tab, page, cx, pts);
                }
            }
            Draft::Shape { page, start, end } if cx.tool == Tool::TextBox => {
                // The drag sets position and width; the height follows the text.
                let left = start.x.min(end.x);
                let mut right = start.x.max(end.x);
                if right - left < 30.0 {
                    right = left + crate::engine::textbox::DEFAULT_WIDTH;
                }
                let top = start.y.max(end.y);
                let rect = PdfBox {
                    left,
                    bottom: top - 20.0,
                    right,
                    top,
                };
                create(tab, page, cx, AnnotKind::FreeText, rect, style.textbox_border, Vec::new(), Vec::new());
            }
            Draft::Shape { page, start, end } => {
                let rect = PdfBox {
                    left: start.x.min(end.x),
                    bottom: start.y.min(end.y),
                    right: start.x.max(end.x),
                    top: start.y.max(end.y),
                };
                if rect.right - rect.left >= 3.0 && rect.top - rect.bottom >= 3.0 {
                    let kind = if cx.tool == Tool::Ellipse {
                        AnnotKind::Circle
                    } else {
                        AnnotKind::Square
                    };
                    create(tab, page, cx, kind, rect, style.shape, Vec::new(), Vec::new());
                }
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create(
    tab: &mut DocTab,
    page: usize,
    cx: &ViewContext<'_>,
    kind: AnnotKind,
    rect: PdfBox,
    color: Rgb,
    quads: Vec<[f32; 8]>,
    ink: Vec<Vec<(f32, f32)>>,
) {
    let style = cx.style;
    let (width, interior) = match kind {
        AnnotKind::Ink => (style.ink_width, None),
        AnnotKind::Square | AnnotKind::Circle => (style.shape_width, style.shape_fill),
        AnnotKind::FreeText => (style.textbox_border_width, style.textbox_fill),
        _ => (1.0, None),
    };
    let mut annot = NewAnnot::new(kind, rect, color);
    annot.interior = interior;
    annot.width = width;
    annot.author = cx.author.to_string();
    annot.quads = quads;
    annot.ink = ink;
    annot.font_size = style.textbox_font_size;
    annot.text_color = style.textbox_text;
    tab.outbox.push(Request::AddAnnot { doc: tab.id, page, annot });
}

/// Rounds the direction from `a` to `b` to a multiple of 45°.
fn snap_45(a: Pos2, b: Pos2) -> Pos2 {
    let d = b - a;
    let angle = (d.y.atan2(d.x) / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
    a + vec2(angle.cos(), angle.sin()) * d.length()
}

/// Creates the line, arrow, polygon or cloud of the current tool.
fn create_shape(tab: &mut DocTab, page: usize, cx: &ViewContext<'_>, vertices: Vec<(f32, f32)>) {
    let Some(kind) = cx.tool.shape_kind() else { return };
    let style = cx.style;
    let mut annot = NewAnnot::new(kind, PdfBox { left: 0.0, bottom: 0.0, right: 0.0, top: 0.0 }, style.shape);
    annot.width = style.shape_width;
    annot.interior = if kind.has_interior() { style.shape_fill } else { None };
    annot.author = cx.author.to_string();
    annot.vertices = vertices;
    if matches!(kind, AnnotKind::Distance | AnnotKind::Area) {
        annot.measure = Some(measure_scale(style).encode());
    }
    tab.outbox.push(Request::AddAnnot { doc: tab.id, page, annot });
}

/// The measuring scale chosen in the tool options.
pub fn measure_scale(style: &AnnotStyle) -> crate::engine::shapes::Scale {
    crate::engine::shapes::Scale::new(style.measure_ratio as f64, &style.measure_unit)
}

/// What a measurement being drawn reads.
fn draft_measure(tool: Tool, pts: &[Pos2], style: &AnnotStyle) -> Option<String> {
    use crate::engine::shapes::{Shape, measure_label};
    let shape = match tool {
        Tool::Distance => Shape::Distance,
        Tool::Area => Shape::Area,
        _ => return None,
    };
    let pts: Vec<(f32, f32)> = pts.iter().map(|p| (p.x, p.y)).collect();
    Some(measure_label(shape, &pts, &measure_scale(style))).filter(|s| !s.is_empty())
}

/// Finishes a polygon or cloud being clicked together.
fn finish_poly(tab: &mut DocTab, cx: &ViewContext<'_>) {
    if let Some(Draft::Poly { page, points, .. }) = tab.draft.take()
        && points.len() >= 3
    {
        create_shape(tab, page, cx, points.iter().map(|p| (p.x, p.y)).collect());
    }
}

/// Places the chosen stamp or signature centred on `at` (PDF space).
fn place_stamp(tab: &mut DocTab, page: usize, at: Pos2, cx: &ViewContext<'_>) {
    let point = PdfBox {
        left: at.x,
        bottom: at.y,
        right: at.x,
        top: at.y,
    };
    let centred = |w: f32, h: f32| PdfBox {
        left: at.x - w / 2.0,
        bottom: at.y - h / 2.0,
        right: at.x + w / 2.0,
        top: at.y + h / 2.0,
    };
    let style = cx.style;
    let annot = match cx.tool {
        Tool::Stamp => {
            let Some(spec) = cx.stamp else {
                tab.error_message = Some("Choose a stamp in the tool options bar first.".into());
                return;
            };
            let rect = match spec {
                // The engine sizes text stamps to their text.
                StampSpec::Text { .. } => point,
                StampSpec::Image { width, height, .. } => {
                    let w = 150.0_f32;
                    centred(w, w * *height as f32 / (*width).max(1) as f32)
                }
            };
            let mut a = NewAnnot::new(AnnotKind::Stamp, rect, style.stamp_color);
            a.stamp = Some(spec.clone());
            a
        }
        _ => match (cx.signature, cx.signature_image) {
            (Some(Signature::Drawn(strokes)), _) => {
                // 150 pt wide, keeping the drawing's proportions.
                let w = 150.0_f32;
                let h = strokes.iter().flatten().map(|p| p[1]).fold(0.0_f32, f32::max).max(0.05) * w;
                let r = centred(w, h);
                let ink: Vec<Vec<(f32, f32)>> = strokes
                    .iter()
                    .map(|s| s.iter().map(|p| (r.left + p[0] * w, r.top - p[1] * w)).collect())
                    .collect();
                let mut a = NewAnnot::new(AnnotKind::Ink, r.expand(2.0), style.signature_color);
                a.width = 1.5;
                a.ink = ink;
                a
            }
            (Some(Signature::Image(_)), Some(spec @ StampSpec::Image { width, height, .. })) => {
                let w = 150.0_f32;
                let mut a = NewAnnot::new(AnnotKind::Stamp, centred(w, w * *height as f32 / (*width).max(1) as f32), [0, 0, 0]);
                a.stamp = Some(spec.clone());
                a
            }
            _ => {
                tab.error_message = Some("Create a signature first (tool options bar: Signatures…).".into());
                return;
            }
        },
    };
    let mut annot = annot;
    annot.author = cx.author.to_string();
    tab.outbox.push(Request::AddAnnot { doc: tab.id, page, annot });
}

/// Turns the current text selection into a text markup annotation.
pub fn create_markup(tab: &mut DocTab, cx: &ViewContext<'_>, kind: AnnotKind) {
    let Some((page, lines)) = tab.selection_line_boxes() else { return };
    let color = match kind {
        AnnotKind::Highlight => cx.style.highlight,
        AnnotKind::Underline => cx.style.underline,
        _ => cx.style.strikeout,
    };
    let rect = lines.iter().copied().reduce(PdfBox::union).unwrap();
    let quads = lines.iter().map(quad).collect();
    create(tab, page, cx, kind, rect, color, quads, Vec::new());
    tab.selection = None;
}

/// The cursor position between characters nearest to `p`: before the nearest
/// character, or after it when `p` is on its right half.
fn caret_at(text: &crate::engine::PageText, p: Pos2) -> Option<usize> {
    let i = nearest_char(text, p)?;
    let r = text.chars[i].rect?;
    Some(if p.x > (r.left + r.right) / 2.0 { i + 1 } else { i })
}

fn nearest_char(text: &crate::engine::PageText, p: Pos2) -> Option<usize> {
    text.chars
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let r = c.rect?;
            let dx = (r.left - p.x).max(0.0).max(p.x - r.right);
            // Vertical distance counts more so that we prefer the line under the cursor.
            let dy = (r.bottom - p.y).max(0.0).max(p.y - r.top) * 3.0;
            Some((i, dx * dx + dy * dy))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

fn apply_pending_navigation(tab: &mut DocTab, view: Rect) {
    if let Some(page) = tab.pending_show.take() {
        tab.relayout(view.size());
        if let Some(rect) = tab.layout.page_rect(page) {
            tab.viewport.reveal(rect.expand(GAP / 2.0), view);
        }
    }
    if let Some(page) = tab.pending_goto.take() {
        let page = page.min(tab.page_count().saturating_sub(1));
        tab.set_current_page(page);
        tab.relayout(view.size());
        if let Some(rect) = tab.layout.page_rect(page) {
            let horizontal = tab.mode.is_horizontal();
            let margin = GAP / 2.0;
            tab.viewport.scroll_to(rect.min, horizontal, margin);
            if tab.mode == LayoutMode::SinglePage {
                tab.viewport.offset.x = rect.min.x - margin;
            }
        }
    }
    if let Some((page, pdf_rect)) = tab.pending_reveal.take() {
        tab.relayout(view.size());
        match (tab.layout.page_rect(page), tab.geoms.get(&page)) {
            (Some(page_rect), Some(geom)) => {
                let local = geom_box_to_local(&pdf_rect, geom, tab.rotation);
                let target = local.translate(page_rect.min.to_vec2()).expand(20.0);
                tab.viewport.reveal(target, view);
            }
            (Some(_), None) => {
                // Geometry not loaded yet: go to the page and retry once it is.
                tab.request_text(page);
                tab.pending_goto = Some(page);
                tab.pending_reveal = Some((page, pdf_rect));
            }
            _ => {}
        }
    }
}

fn update_current_page(tab: &mut DocTab, view: Rect) {
    let visible = tab.viewport.visible_doc_rect(view);
    tab.visible_pages = tab.layout.pages_in(visible);
    if tab.mode == LayoutMode::SinglePage {
        tab.visible_pages = vec![tab.current_page];
        return;
    }
    let best = tab
        .layout
        .pages_in(visible)
        .into_iter()
        .filter_map(|p| {
            let r = tab.layout.page_rect(p)?.intersect(visible);
            Some((p, r.area()))
        })
        // Prefer the earliest page among those (nearly) as visible as the best one.
        .fold(None::<(usize, f32)>, |best, (p, a)| match best {
            Some((_, ba)) if a <= ba * 1.05 => best,
            _ => Some((p, a)),
        });
    if let Some((page, _)) = best {
        tab.set_current_page(page);
    }
}

fn paint_pages(ui: &Ui, tab: &mut DocTab, view: Rect, cx: &mut ViewContext<'_>) {
    let painter = ui.painter_at(view);
    let ppp = ui.ctx().pixels_per_point();
    let visible = tab.viewport.visible_doc_rect(view);
    let center = visible.center();

    // Prefetch one screen ahead/behind along the scroll axis.
    let prefetch = if tab.mode.is_horizontal() {
        visible.expand2(vec2(visible.width(), 0.0))
    } else {
        visible.expand2(vec2(0.0, visible.height()))
    };

    let page_bg = if cx.night {
        Color32::from_gray(30)
    } else {
        Color32::WHITE
    };

    for page in tab.layout.pages_in(prefetch) {
        let Some(rect) = tab.layout.page_rect(page) else { continue };
        let size = rect.size();
        // Rendered at exactly the pixel size it is shown at, on whole pixels: sharp.
        let screen = snap_to_pixels(tab.viewport.rect_to_screen(rect, view), ppp);
        let want_w = exact_width(screen.width() * ppp, size.y / size.x.max(1.0));
        let version = tab.page_version(page);
        let key = RenderKey {
            doc: tab.id,
            page,
            width_px: want_w,
            rotation: tab.rotation,
            version,
            tile: 0,
        };
        let is_visible = rect.intersects(visible);
        let distance = (rect.center() - center).length() / visible.size().length().max(1.0);
        tab.wanted
            .push((key, if is_visible { distance } else { 2.0 + distance }));

        if !is_visible {
            continue;
        }
        if cx.tool != Tool::Hand && !tab.texts.contains_key(&page) {
            tab.request_text(page);
        }

        painter.add(
            Shadow {
                offset: [2, 3],
                blur: 8,
                spread: 0,
                color: Color32::from_black_alpha(90),
            }
            .as_shape(screen, CornerRadius::ZERO),
        );
        painter.rect_filled(screen, CornerRadius::ZERO, page_bg);

        match cx.cache.best(tab.id, page, tab.rotation, want_w, version) {
            Some((tex, shown)) => {
                if shown != key {
                    // A placeholder is shown; make sure the crisp one arrives quickly.
                    tab.wanted.retain(|(k, _)| *k != key);
                    tab.wanted.push((key, distance * 0.5));
                }
                painter.image(
                    tex.id(),
                    screen,
                    Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                // Nothing yet: ask for a fast low-resolution render first.
                let preview = RenderKey {
                    width_px: width_bucket(160.0, size.y / size.x.max(1.0)).min(want_w),
                    ..key
                };
                tab.wanted.push((preview, -1.0 + distance));
            }
        }

        // Above the size limit of whole-page renders, sharp tiles cover what is on screen.
        let full_w = (screen.width() * ppp).round() as u32;
        if full_w > want_w {
            paint_tiles(&painter, tab, cx, page, screen, view, full_w, version, distance);
        }

        paint_overlays(&painter, tab, page, view);
    }
    paint_drag_target(&painter, tab, view);
    paint_draft(&painter, tab, view, cx);
}

/// Draws the visible tiles of `page` rendered `full_w` pixels wide, and asks for the
/// missing ones.
#[allow(clippy::too_many_arguments)]
fn paint_tiles(
    painter: &egui::Painter,
    tab: &mut DocTab,
    cx: &mut ViewContext<'_>,
    page: usize,
    screen: Rect,
    view: Rect,
    full_w: u32,
    version: u32,
    distance: f32,
) {
    use crate::engine::TILE;
    let scale = screen.width() / full_w as f32;
    let full_h = (screen.height() / scale).round() as u32;
    let vis = screen.intersect(view);
    if !vis.is_positive() {
        return;
    }
    let tile = TILE as f32;
    let to_px = |p: Pos2| ((p - screen.min) / scale).to_pos2();
    let (a, b) = (to_px(vis.min), to_px(vis.max));
    let cols = (a.x / tile).floor().max(0.0) as u32..=((b.x / tile).ceil() as u32).min(full_w.div_ceil(TILE)).saturating_sub(1);
    let rows = (a.y / tile).floor().max(0.0) as u32..=((b.y / tile).ceil() as u32).min(full_h.div_ceil(TILE)).saturating_sub(1);
    for row in rows {
        for col in cols.clone() {
            let key = RenderKey {
                doc: tab.id,
                page,
                width_px: full_w,
                rotation: tab.rotation,
                version,
                tile: RenderKey::tile_at(col, row),
            };
            let (x, y) = (col * TILE, row * TILE);
            let (w, h) = (TILE.min(full_w - x), TILE.min(full_h.saturating_sub(y)).max(1));
            let r = Rect::from_min_size(
                screen.min + vec2(x as f32, y as f32) * scale,
                vec2(w as f32, h as f32) * scale,
            );
            match cx.cache.get(&key) {
                Some(tex) => {
                    painter.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                }
                None => tab.wanted.push((key, distance * 0.5 + 0.01)),
            }
        }
    }
}

/// Where a dragged annotation will land, over all pages (it can be dropped on another).
fn paint_drag_target(painter: &egui::Painter, tab: &DocTab, view: Rect) {
    let Some(d) = tab.annot_drag.as_ref() else { return };
    let Some(page_rect) = tab.layout.page_rect(d.page) else { return };
    let s = tab.viewport.rect_to_screen(d.current.translate(page_rect.min.to_vec2()), view);
    let blue = Color32::from_rgb(30, 120, 255);
    painter.rect(s, CornerRadius::same(2), Color32::from_rgba_unmultiplied(30, 120, 255, 30), Stroke::new(1.5, blue), StrokeKind::Outside);
}

fn paint_overlays(painter: &egui::Painter, tab: &DocTab, page: usize, view: Rect) {
    if !tab.geoms.contains_key(&page) {
        return;
    }
    let to_screen = |b: &PdfBox| pdf_box_to_screen(tab, page, b, view).unwrap_or(Rect::NOTHING);

    // Comparison: removed words red, added words green.
    for (r, added) in tab.diff_marks.get(&page).into_iter().flatten() {
        let (fill, line) = if *added {
            (Color32::from_rgba_unmultiplied(40, 200, 80, 70), Color32::from_rgb(20, 150, 60))
        } else {
            (Color32::from_rgba_unmultiplied(230, 40, 40, 60), Color32::from_rgb(200, 30, 30))
        };
        let s = to_screen(r).expand(1.0);
        painter.rect(s, CornerRadius::same(2), fill, Stroke::new(1.0, line), StrokeKind::Outside);
    }

    // Areas marked for redaction, and the one being dragged out.
    let drag = tab.redact_drag.filter(|d| d.0 == page).map(|(_, a, b)| PdfBox {
        left: a.x.min(b.x),
        bottom: a.y.min(b.y),
        right: a.x.max(b.x),
        top: a.y.max(b.y),
    });
    for b in tab.redactions.iter().filter(|(p, _)| *p == page).map(|(_, b)| b).chain(drag.as_ref()) {
        painter.rect(
            to_screen(b),
            CornerRadius::ZERO,
            REDACTION_FILL,
            Stroke::new(1.5, REDACTION_LINE),
            StrokeKind::Outside,
        );
    }

    for (i, hit) in tab.search.hits.iter().enumerate() {
        if hit.page != page {
            continue;
        }
        let current = tab.search.current == Some(i);
        for r in &hit.rects {
            let s = to_screen(r);
            if current {
                painter.rect(
                    s.expand(1.0),
                    CornerRadius::same(2),
                    SEARCH_CURRENT_COLOR,
                    Stroke::new(1.5, Color32::from_rgb(230, 90, 0)),
                    StrokeKind::Outside,
                );
            } else {
                painter.rect_filled(s, CornerRadius::same(2), SEARCH_COLOR);
            }
        }
    }

    if let Some(sel) = tab.selection.filter(|s| s.page == page)
        && let Some((_, lines)) = tab.selection_line_boxes()
    {
        let _ = sel;
        for r in lines {
            painter.rect_filled(to_screen(&r), CornerRadius::ZERO, SELECTION_COLOR);
        }
    }

    let blue = Color32::from_rgb(30, 120, 255);
    if tab.annot_drag.as_ref().is_some_and(|d| d.page == page) {
        // Drawn by paint_drag_target, over all pages.
    } else if let Some(sel) = tab.selected_annot.as_ref().filter(|s| s.page == page)
        && let Some(a) = tab.annot(page, sel.index)
    {
        let s = to_screen(&a.rect);
        painter.rect_stroke(s.expand(3.0), CornerRadius::same(2), Stroke::new(1.5, blue), StrokeKind::Outside);
        for (_, h) in handles(s, a.kind.resize()) {
            painter.rect(h, CornerRadius::same(1), Color32::WHITE, Stroke::new(1.5, blue), StrokeKind::Middle);
        }
    }
}

fn paint_draft(painter: &egui::Painter, tab: &DocTab, view: Rect, cx: &ViewContext<'_>) {
    let zoom = tab.viewport.zoom;
    match &tab.draft {
        Some(Draft::Ink { page, points }) => {
            let pts: Vec<Pos2> = points
                .iter()
                .filter_map(|p| pdf_to_screen(tab, *page, *p, view))
                .collect();
            let stroke = Stroke::new(cx.style.ink_width * zoom, rgb(cx.style.ink));
            painter.add(Shape::line(pts, stroke));
        }
        Some(Draft::Poly { page, points, hover }) => {
            let mut pts: Vec<Pos2> = points.iter().filter_map(|p| pdf_to_screen(tab, *page, *p, view)).collect();
            if let Some(h) = hover.and_then(|h| pdf_to_screen(tab, *page, h, view)) {
                pts.push(h);
            }
            let stroke = Stroke::new(cx.style.shape_width * zoom, rgb(cx.style.shape));
            for p in &pts[..points.len().min(pts.len())] {
                painter.circle_filled(*p, 3.0, rgb(cx.style.shape));
            }
            let label_at = pts.last().copied();
            painter.add(Shape::line(pts, stroke));
            let mut corners = points.clone();
            corners.extend(*hover);
            if let (Some(at), Some(text)) = (label_at, draft_measure(cx.tool, &corners, cx.style)) {
                measure_tag(painter, at, &text);
            }
        }
        Some(Draft::Shape { page, start, end }) if matches!(cx.tool, Tool::Line | Tool::Arrow | Tool::Distance) => {
            let (Some(a), Some(b)) = (pdf_to_screen(tab, *page, *start, view), pdf_to_screen(tab, *page, *end, view))
            else {
                return;
            };
            let stroke = Stroke::new(cx.style.shape_width * zoom, rgb(cx.style.shape));
            if cx.tool == Tool::Arrow {
                painter.arrow(a, b - a, stroke);
            } else {
                painter.line_segment([a, b], stroke);
            }
            if let Some(text) = draft_measure(cx.tool, &[*start, *end], cx.style) {
                measure_tag(painter, b, &text);
            }
        }
        Some(Draft::Shape { page, start, end }) => {
            let (Some(a), Some(b)) = (pdf_to_screen(tab, *page, *start, view), pdf_to_screen(tab, *page, *end, view))
            else {
                return;
            };
            let r = Rect::from_two_pos(a, b);
            let (stroke, fill) = if cx.tool == Tool::TextBox {
                (
                    Stroke::new(cx.style.textbox_border_width.max(1.0) * zoom, rgb(cx.style.textbox_border)),
                    Color32::TRANSPARENT,
                )
            } else {
                (
                    Stroke::new(cx.style.shape_width * zoom, rgb(cx.style.shape)),
                    cx.style.shape_fill.map(rgb).unwrap_or(Color32::TRANSPARENT),
                )
            };
            if cx.tool == Tool::Ellipse {
                painter.add(Shape::ellipse_filled(r.center(), r.size() / 2.0, fill));
                painter.add(Shape::ellipse_stroke(r.center(), r.size() / 2.0, stroke));
            } else {
                painter.rect(r, CornerRadius::ZERO, fill, stroke, StrokeKind::Middle);
            }
        }
        None => {}
    }
}

/// A measurement read-out next to the pointer while drawing.
fn measure_tag(painter: &egui::Painter, at: Pos2, text: &str) {
    let galley = painter.layout_no_wrap(text.to_string(), egui::FontId::proportional(13.0), Color32::WHITE);
    let r = Rect::from_min_size(at + vec2(12.0, 8.0), galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(r, CornerRadius::same(4), Color32::from_black_alpha(200));
    painter.galley(r.min + vec2(5.0, 2.0), galley, Color32::WHITE);
}

const HANDLE: f32 = 8.0;

/// Resize handles (number, screen rectangle) for a selected annotation.
fn handles(screen: Rect, resize: Resize) -> Vec<(u8, Rect)> {
    if resize == Resize::None {
        return Vec::new();
    }
    [
        screen.left_top(),
        screen.center_top(),
        screen.right_top(),
        screen.right_center(),
        screen.right_bottom(),
        screen.center_bottom(),
        screen.left_bottom(),
        screen.left_center(),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, p)| (i as u8, Rect::from_center_size(p, vec2(HANDLE, HANDLE))))
    .collect()
}

fn handle_cursor(h: u8) -> CursorIcon {
    match h {
        0 | 4 => CursorIcon::ResizeNwSe,
        2 | 6 => CursorIcon::ResizeNeSw,
        1 | 5 => CursorIcon::ResizeVertical,
        _ => CursorIcon::ResizeHorizontal,
    }
}

/// The rectangle after dragging a handle by `d` (page-local points). With
/// `keep_aspect`, corner handles keep the width/height ratio.
pub fn resize_rect(r: Rect, handle: u8, d: Vec2, keep_aspect: bool) -> Rect {
    const MIN: f32 = 4.0;
    let (mut l, mut t, mut rr, mut b) = (r.left(), r.top(), r.right(), r.bottom());
    match handle {
        0 => (l, t) = (l + d.x, t + d.y),
        1 => t += d.y,
        2 => (rr, t) = (rr + d.x, t + d.y),
        3 => rr += d.x,
        4 => (rr, b) = (rr + d.x, b + d.y),
        5 => b += d.y,
        6 => (l, b) = (l + d.x, b + d.y),
        _ => l += d.x,
    }
    let mut out = Rect::from_min_max(pos2(l.min(rr), t.min(b)), pos2(l.max(rr), t.max(b)));
    if keep_aspect && matches!(handle, 0 | 2 | 4 | 6) && r.height() > 0.0 {
        let aspect = r.width() / r.height();
        let w = out.width().max(out.height() * aspect);
        let size = vec2(w, w / aspect);
        // Keep the corner opposite the dragged one in place.
        let anchor = match handle {
            0 => r.right_bottom(),
            2 => r.left_bottom(),
            4 => r.left_top(),
            _ => r.right_top(),
        };
        let min = pos2(
            if matches!(handle, 0 | 6) { anchor.x - size.x } else { anchor.x },
            if matches!(handle, 0 | 2) { anchor.y - size.y } else { anchor.y },
        );
        out = Rect::from_min_size(min, size);
    }
    if out.width() < MIN {
        out.max.x = out.min.x + MIN;
    }
    if out.height() < MIN {
        out.max.y = out.min.y + MIN;
    }
    out
}

/// Page-local rectangle of an annotation.
fn annot_local(tab: &DocTab, page: usize, b: &PdfBox) -> Option<Rect> {
    Some(geom_box_to_local(b, tab.geoms.get(&page)?, tab.rotation))
}

/// Page-local rectangle back to PDF page space.
fn local_to_pdf_box(tab: &DocTab, page: usize, r: Rect) -> Option<PdfBox> {
    let geom = tab.geoms.get(&page)?;
    let a = geom_local_to_pdf(r.min, geom, tab.rotation);
    let b = geom_local_to_pdf(r.max, geom, tab.rotation);
    Some(PdfBox {
        left: a.x.min(b.x),
        bottom: a.y.min(b.y),
        right: a.x.max(b.x),
        top: a.y.max(b.y),
    })
}

fn send_rect(tab: &mut DocTab, page: usize, index: usize, rect: PdfBox) {
    // Show the result right away; the engine's answer confirms it.
    if let Some(list) = tab.annots.get_mut(&page)
        && let Some(a) = list.iter_mut().find(|a| a.index == index)
    {
        a.rect = rect;
    }
    tab.outbox.push(Request::UpdateAnnot {
        doc: tab.id,
        page,
        index,
        change: AnnotChange {
            rect: Some(rect),
            ..Default::default()
        },
    });
}

/// Starts, updates and finishes dragging an annotation. Returns true if the input
/// was used for that.
fn annot_drag_input(
    ui: &Ui,
    tab: &mut DocTab,
    response: &egui::Response,
    view: Rect,
    pointer: Option<Pos2>,
) -> bool {
    let (primary_down, shift, press) =
        ui.input(|i| (i.pointer.primary_down(), i.modifiers.shift, i.pointer.press_origin()));
    let local_of = |tab: &DocTab, screen: Pos2, page: usize| -> Option<Pos2> {
        let pr = tab.layout.page_rect(page)?;
        Some((tab.viewport.to_doc(screen, view) - pr.min).to_pos2())
    };

    // A drag in progress.
    if let Some(mut d) = tab.annot_drag.clone() {
        if primary_down {
            if let Some(p) = pointer.or(ui.input(|i| i.pointer.latest_pos()))
                && let Some(local) = local_of(tab, p, d.page)
            {
                let delta = local - d.start_pointer;
                d.current = match d.mode {
                    DragMode::Move => d.start.translate(delta),
                    DragMode::Resize(h) => resize_rect(d.start, h, delta, shift),
                };
                ui.ctx().set_cursor_icon(match d.mode {
                    DragMode::Move => CursorIcon::Grabbing,
                    DragMode::Resize(h) => handle_cursor(h),
                });
                tab.annot_drag = Some(d);
            }
            return true;
        }
        tab.annot_drag = None;
        let moved = (d.current.min - d.start.min).length() > 0.1 || (d.current.max - d.start.max).length() > 0.1;
        // Dropped on another page: the annotation moves there.
        let target = pointer
            .or(ui.input(|i| i.pointer.latest_pos()))
            .and_then(|p| tab.layout.page_at(tab.viewport.to_doc(p, view)))
            .filter(|&t| t != d.page && d.mode == DragMode::Move);
        if moved
            && let Some(to_page) = target
            && let (Some(from), Some(to)) = (tab.layout.page_rect(d.page), tab.layout.page_rect(to_page))
            && let Some(rect) = local_to_pdf_box(tab, to_page, d.current.translate(from.min - to.min))
        {
            tab.deselect_annot();
            tab.outbox.push(Request::MoveAnnotToPage {
                doc: tab.id,
                page: d.page,
                index: d.index,
                to_page,
                rect,
            });
        } else if moved && let Some(rect) = local_to_pdf_box(tab, d.page, d.current) {
            send_rect(tab, d.page, d.index, rect);
        }
        return true;
    }

    let Some(p) = pointer else { return false };
    let Some(page) = tab.layout.page_at(tab.viewport.to_doc(p, view)) else { return false };

    // Hovering the selected annotation's handles.
    let selected = tab
        .selected_annot
        .as_ref()
        .filter(|s| s.page == page && !s.editing)
        .and_then(|s| tab.annot(page, s.index))
        .cloned();
    let handle_under = |at: Pos2| {
        selected.as_ref().and_then(|a| {
            let screen = pdf_box_to_screen(tab, page, &a.rect, view)?;
            handles(screen, a.kind.resize())
                .into_iter()
                .find(|(_, r)| r.expand(2.0).contains(at))
                .map(|(h, _)| (a.clone(), h))
        })
    };
    if let Some((_, h)) = handle_under(p) {
        ui.ctx().set_cursor_icon(handle_cursor(h));
    }

    if !response.drag_started_by(egui::PointerButton::Primary) {
        // Movable annotations show a move cursor.
        if let Some((_, pdf)) = screen_to_pdf(tab, p, view, Some(page))
            && annot_at(tab, page, pdf).is_some_and(|a| a.kind.movable())
            && handle_under(p).is_none()
        {
            ui.ctx().set_cursor_icon(CursorIcon::Move);
        }
        return false;
    }

    let start = press.unwrap_or(p);
    let (annot, mode) = if let Some((a, h)) = handle_under(start) {
        (a, DragMode::Resize(h))
    } else {
        let Some((_, pdf)) = screen_to_pdf(tab, start, view, Some(page)) else { return false };
        match annot_at(tab, page, pdf).filter(|a| a.kind.movable()) {
            Some(a) => (a.clone(), DragMode::Move),
            None => return false,
        }
    };
    let (Some(rect), Some(start_pointer)) = (annot_local(tab, page, &annot.rect), local_of(tab, start, page)) else {
        return false;
    };
    tab.select_annot(page, annot.index);
    tab.selection = None;
    tab.annot_drag = Some(AnnotDrag {
        page,
        index: annot.index,
        mode,
        start: rect,
        start_pointer,
        current: rect,
    });
    true
}

/// Moves the selected annotation by the arrow-key nudge collected this frame.
fn apply_nudge(tab: &mut DocTab) {
    let nudge = std::mem::take(&mut tab.nudge);
    if nudge == Vec2::ZERO {
        return;
    }
    let Some(sel) = tab.selected_annot.as_ref().filter(|s| !s.editing) else { return };
    let (page, index) = (sel.page, sel.index);
    let Some(a) = tab.annot(page, index).filter(|a| a.kind.movable()).cloned() else { return };
    if let Some(local) = annot_local(tab, page, &a.rect)
        && let Some(rect) = local_to_pdf_box(tab, page, local.translate(nudge))
    {
        send_rect(tab, page, index, rect);
    }
}

/// Small floating toolbar next to a text selection made with the select tool.
fn selection_toolbar(ui: &Ui, tab: &mut DocTab, view: Rect, cx: &ViewContext<'_>) {
    // Hidden while the user is still dragging out the selection. (Not merely while the
    // mouse button is down: pressing one of the toolbar's own buttons must work.)
    if cx.tool != Tool::SelectText || tab.selecting {
        return;
    }
    let Some((page, lines)) = tab.selection_line_boxes() else { return };
    let Some(last) = lines.last().and_then(|b| pdf_box_to_screen(tab, page, b, view)) else { return };
    if !view.contains(last.right_bottom()) {
        return;
    }
    let mut action = None;
    let mut redact = false;
    egui::Area::new(Id::unique(("selection_toolbar", tab.id)))
        .fixed_pos(last.right_bottom() + vec2(4.0, 4.0))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(icons::text(icons::COPY)).on_hover_text(kb(t("Copy (Ctrl+C)"))).clicked() {
                        action = Some(None);
                    }
                    for (kind, tool) in [
                        (AnnotKind::Highlight, Tool::Highlight),
                        (AnnotKind::Underline, Tool::Underline),
                        (AnnotKind::StrikeOut, Tool::StrikeOut),
                    ] {
                        let resp = ui.button(tool.icon()).on_hover_text(kind.label());
                        #[cfg(test)]
                        ui.ctx().data_mut(|d| d.insert_temp(Id::unique(("test_button", kind.label())), resp.rect));
                        if resp.clicked() {
                            action = Some(Some(kind));
                        }
                    }
                    if ui.button(Tool::Redact.icon()).on_hover_text(t("Mark for redaction")).clicked() {
                        redact = true;
                    }
                });
            });
        });
    match action {
        Some(None) => {
            if let Some(text) = tab.selected_text() {
                ui.ctx().copy_text(text);
            }
        }
        Some(Some(kind)) => create_markup(tab, cx, kind),
        None => {}
    }
    if redact {
        tab.mark_selection();
    }
}

/// Properties popup for the selected annotation.
fn annotation_popup(ui: &Ui, tab: &mut DocTab, view: Rect, recent: &mut Vec<Rgb>, author: &str) {
    if tab.annot_drag.is_some() {
        return;
    }
    let Some(sel) = tab.selected_annot.clone() else { return };
    let Some(annot) = tab.annot(sel.page, sel.index).cloned() else {
        // Deleted (or not loaded yet).
        if tab.annots.contains_key(&sel.page) {
            tab.selected_annot = None;
        }
        return;
    };
    let Some(screen) = pdf_box_to_screen(tab, sel.page, &annot.rect, view) else { return };
    if !view.intersects(screen) {
        return;
    }
    if sel.editing {
        text_box_editor(ui, tab, &annot, screen, view, recent);
        return;
    }
    let width = 260.0;
    let mut pos = pos2(screen.right() + 10.0, screen.top());
    if pos.x + width > view.right() {
        pos.x = (screen.left() - width - 10.0).max(view.left());
    }
    pos.y = pos.y.clamp(view.top(), (view.bottom() - 220.0).max(view.top()));

    let mut change = AnnotChange::default();
    let mut delete = false;
    let mut contents = sel.contents.clone();
    let mut focus = sel.focus_contents;
    let mut start_editing = false;
    egui::Area::new(Id::unique(("annot_popup", tab.id)))
        .fixed_pos(pos)
        .constrain(true)
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.strong(annot.kind.label());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button(icons::sized(icons::TRASH, 14.0))
                            .on_hover_text(t("Delete (Del)"))
                            .clicked()
                        {
                            delete = true;
                        }
                    });
                });
                let date = format_pdf_date(&annot.modified);
                let meta = match (annot.author.is_empty(), date.is_empty()) {
                    (false, false) => format!("{} · {date}", annot.author),
                    (false, true) => annot.author.clone(),
                    (true, false) => date,
                    (true, true) => String::new(),
                };
                if !meta.is_empty() {
                    ui.weak(meta);
                }
                if annot.kind.style_editable() {
                    let is_box = annot.kind == AnnotKind::FreeText;
                    egui::Grid::new(("annot_style", tab.id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label(if is_box { "Border" } else { "Colour" });
                        if let Some(Some(c)) = color_picker::color_button(ui, "annot_color", annot.color, false, recent) {
                            change.color = Some(c);
                        }
                        ui.end_row();
                        if annot.kind.has_interior() {
                            ui.label(t("Fill"));
                            if let Some(c) = color_picker::color_button(ui, "annot_fill", annot.interior, true, recent) {
                                change.interior = Some(c);
                            }
                            ui.end_row();
                        }
                        if is_box {
                            ui.label(t("Text"));
                            if let Some(Some(c)) =
                                color_picker::color_button(ui, "annot_text", annot.text_color, false, recent)
                            {
                                change.text_color = Some(c);
                            }
                            ui.end_row();
                        }
                    });
                    if annot.kind.has_border_width()
                        && let Some(w) = slider_field(ui, "Width", annot.width, 0.0..=12.0, 0.5)
                    {
                        change.width = Some(w);
                    }
                }
                if annot.kind == AnnotKind::FreeText {
                    // A text box's "comment" is its visible text, edited on the page.
                    // Scales all text proportionally (individual sizes keep their ratio).
                    if let Some(size) = slider_field(ui, "Size", annot.font_size.unwrap_or(12.0), 4.0..=96.0, 1.0) {
                        change.font_size = Some(size);
                    }
                    if ui.button(t("Edit text (double-click the box)")).clicked() {
                        start_editing = true;
                    }
                } else {
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut contents)
                            .hint_text(t("Add a comment…"))
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    #[cfg(test)]
                    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_comment_box"), edit.rect));
                    if focus {
                        edit.request_focus();
                        focus = false;
                    }
                    if edit.lost_focus() && contents != sel.committed {
                        change.contents = Some(contents.clone());
                    }
                }
                // Replies (also those other applications wrote), and a field to reply.
                let replies: Vec<&crate::engine::Reply> = annot.other_replies.iter().chain(&annot.replies).collect();
                if !replies.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(150.0).id_salt(("popup_replies", tab.id)).show(ui, |ui| {
                        for r in replies {
                            ui.horizontal_wrapped(|ui| {
                                ui.weak("↳");
                                ui.strong(if r.author.is_empty() { "?" } else { r.author.as_str() });
                                ui.weak(format_pdf_date(&r.date));
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.add_space(12.0);
                                ui.label(&r.text);
                            });
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut tab.reply_draft)
                            .hint_text(t("Reply…"))
                            .desired_rows(1)
                            .desired_width(ui.available_width() - 60.0),
                    );
                    #[cfg(test)]
                    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_reply_box"), edit.rect));
                    let enter = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                    let text = tab.reply_draft.trim().to_string();
                    let button = ui.add_enabled(!text.is_empty(), egui::Button::new(t("Reply")));
                    #[cfg(test)]
                    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_reply_button"), button.rect));
                    if (button.clicked() || enter) && !text.is_empty() {
                        let mut all = annot.replies.clone();
                        all.push(crate::engine::Reply {
                            author: author.to_string(),
                            date: crate::engine::pdf_date_now(),
                            text,
                        });
                        change.replies = Some(all);
                        tab.reply_draft.clear();
                    }
                });
            });
        });
    if let Some(s) = &mut tab.selected_annot {
        if change.contents.is_some() {
            s.committed = contents.clone();
        }
        s.contents = contents;
        s.focus_contents = focus;
        if start_editing {
            s.editing = true;
            s.focus_contents = true;
        }
    }
    if delete {
        tab.outbox.push(Request::DeleteAnnot {
            doc: tab.id,
            page: sel.page,
            index: sel.index,
        });
        tab.selected_annot = None;
    } else if change != AnnotChange::default() {
        tab.outbox.push(Request::UpdateAnnot {
            doc: tab.id,
            page: sel.page,
            index: sel.index,
            change,
        });
    }
}

/// A slider with a number field next to it. Returns the new value once it is final:
/// when a drag ends, or when a typed value is confirmed (Enter or leaving the field).
/// Typed values are not applied per keystroke, so "14" does not go through "1".
fn slider_field(ui: &mut Ui, label: &str, value: f32, range: std::ops::RangeInclusive<f32>, step: f64) -> Option<f32> {
    let mut v = value;
    let mut done = false;
    ui.horizontal(|ui| {
        let slider = ui.add(egui::Slider::new(&mut v, range.clone()).step_by(step).show_value(false));
        done |= slider.drag_stopped() || (slider.changed() && !slider.dragged());
        let field = ui.add(
            egui::DragValue::new(&mut v)
                .range(range)
                .speed(0.1)
                .suffix(" pt")
                .update_while_editing(false),
        );
        #[cfg(test)]
        ui.ctx().data_mut(|d| d.insert_temp(Id::unique(("test_number_field", label)), field.rect));
        done |= field.drag_stopped() || (field.changed() && !field.dragged());
        ui.label(label);
    });
    (done && (v - value).abs() > f32::EPSILON).then_some(v)
}

/// Edits a text box's text in place: an editor drawn over the box with the same fonts,
/// sizes and colours, and a formatting bar above it. Formatting applies to the selected
/// text, or to what is typed next. The text is saved when editing ends (click
/// elsewhere, Esc).
fn text_box_editor(ui: &Ui, tab: &mut DocTab, annot: &AnnotInfo, screen: Rect, view: Rect, recent: &mut Vec<Rgb>) {
    let zoom = tab.viewport.zoom;
    let pad = crate::engine::textbox::PADDING * zoom;
    let border = annot.width * zoom;
    let fill = annot.interior.map(rgb).unwrap_or(Color32::WHITE);
    let ctx = ui.ctx().clone();
    let editor_id = Id::unique(("text_box_editor", tab.id));
    let edit_id = editor_id.with("text");
    let Some(sel) = tab.selected_annot.as_mut() else { return };
    let Some(re) = sel.rich.as_mut() else { return };

    // The selection as of the last frame (characters).
    let range = egui::TextEdit::load_state(&ctx, edit_id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| {
            let (a, b) = (r.primary.index.0, r.secondary.index.0);
            (a.min(b), a.max(b))
        });
    let text_focused = ctx.memory(|m| m.has_focus(edit_id));
    if let Some((a, b)) = range.filter(|_| text_focused) {
        // Moving the cursor picks up the style of the text before it.
        if a == b && re.last_cursor != Some(a) {
            let at = if a > 0 { re.styles.get(a - 1) } else { re.styles.first() };
            re.typing = at.copied().unwrap_or(re.typing);
        }
        re.last_cursor = Some(a);
        re.selection = Some((a, b));
    }
    // While the formatting bar is used, the text's selection is the one it last had.
    let selected = re.selection.filter(|(a, b)| a < b && *b <= re.styles.len());
    let current = selected.and_then(|(a, _)| re.styles.get(a).copied()).unwrap_or(re.typing);
    let apply = |re: &mut crate::document::RichEdit, f: &dyn Fn(&mut CharStyle)| match selected {
        Some((a, b)) => {
            for s in re.styles.iter_mut().take(b).skip(a) {
                f(s);
            }
        }
        None => f(&mut re.typing),
    };

    // Keyboard shortcuts for the selection.
    let (bold_key, italic_key, underline_key) = ctx.input_mut(|i| {
        let mut key = |k| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, k));
        (key(egui::Key::B), key(egui::Key::I), key(egui::Key::U))
    });
    if bold_key {
        apply(re, &|s| s.bold = !current.bold);
    }
    if italic_key {
        apply(re, &|s| s.italic = !current.italic);
    }
    if underline_key {
        apply(re, &|s| s.underline = !current.underline);
    }

    // Formatting bar above the box (below it if there is no room).
    let bar_y = if screen.top() - 40.0 > view.top() { screen.top() - 40.0 } else { screen.bottom() + 6.0 };
    let bar = egui::Area::new(editor_id.with("bar"))
        .fixed_pos(pos2(screen.left(), bar_y))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let toggle = |ui: &mut Ui, on: bool, icon: &str, tip: &str| {
                        ui.add(egui::Button::selectable(on, icons::text(icon))).on_hover_text(tip).clicked()
                    };
                    if toggle(ui, current.bold, icons::TEXT_B, &kb(t("Bold (Ctrl+B)"))) {
                        apply(re, &|s| s.bold = !current.bold);
                    }
                    if toggle(ui, current.italic, icons::TEXT_ITALIC, &kb(t("Italic (Ctrl+I)"))) {
                        apply(re, &|s| s.italic = !current.italic);
                    }
                    if toggle(ui, current.underline, icons::TEXT_UNDERLINE, &kb(t("Underline (Ctrl+U)"))) {
                        apply(re, &|s| s.underline = !current.underline);
                    }
                    if toggle(ui, current.strike, icons::TEXT_STRIKETHROUGH, "Strikethrough") {
                        apply(re, &|s| s.strike = !current.strike);
                    }
                    ui.separator();
                    if let Some(Some(c)) = color_picker::color_button(ui, "box_text", Some(current.color), false, recent) {
                        apply(re, &|s| s.color = c);
                    }
                    let mut size = current.size;
                    let size_field = ui.add(
                        egui::DragValue::new(&mut size)
                                .range(4.0..=96.0)
                                .speed(0.2)
                                .suffix(" pt")
                                // Typed values apply on Enter, not per keystroke.
                                .update_while_editing(false),
                    )
                    .on_hover_text(t("Font size (drag, or click and type)"));
                    #[cfg(test)]
                    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_bar_size"), size_field.rect));
                    if size_field.changed() {
                        apply(re, &|s| s.size = size);
                    }
                    ui.separator();
                    for (align, icon, tip) in [
                        (Align::Left, icons::ALIGN_LEFT, "Align left"),
                        (Align::Center, icons::ALIGN_CENTER, "Centre"),
                        (Align::Right, icons::ALIGN_RIGHT, "Align right"),
                    ] {
                        if toggle(ui, re.align == align, icon, tip) {
                            re.align = align;
                        }
                    }
                });
            });
        });
    #[cfg(test)]
    ctx.data_mut(|d| d.insert_temp(Id::unique("test_format_bar"), bar.response.rect));

    // The editor itself.
    let styles = re.styles.clone();
    let (typing, align) = (re.typing, re.align);
    let mut layouter = move |ui: &Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = egui::text::LayoutJob::default();
        for (i, ch) in text.as_str().chars().enumerate() {
            let st = styles.get(i).copied().unwrap_or(typing);
            let stroke = |on: bool| {
                if on {
                    Stroke::new((st.size * 0.06 * zoom).max(1.0), rgb(st.color))
                } else {
                    Stroke::NONE
                }
            };
            job.append(
                ch.encode_utf8(&mut [0; 4]),
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::new(st.size * zoom, crate::fonts::family(st.bold, st.italic)),
                    color: rgb(st.color),
                    underline: stroke(st.underline),
                    strikethrough: stroke(st.strike),
                    line_height: Some(st.size * zoom * crate::engine::textbox::LINE_HEIGHT),
                    ..Default::default()
                },
            );
        }
        job.wrap.max_width = wrap_width;
        job.halign = match align {
            Align::Left => egui::Align::LEFT,
            Align::Center => egui::Align::Center,
            Align::Right => egui::Align::RIGHT,
        };
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let mut contents = sel.contents.clone();
    let mut lost = false;
    let width = (screen.width() - 2.0 * pad - 2.0 * border).max(20.0);
    egui::Area::new(editor_id)
        .fixed_pos(screen.min)
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            egui::Frame::new()
                .fill(fill)
                .stroke(Stroke::new(border, rgb(annot.color.unwrap_or([0, 0, 0]))))
                .inner_margin(pad)
                .show(ui, |ui| {
                    ui.set_width(width);
                    // A box made taller than its text keeps its height while editing.
                    ui.set_min_height((screen.height() - 2.0 * pad - 2.0 * border).max(0.0));
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut contents)
                            .id(edit_id)
                            .layouter(&mut layouter)
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::ZERO)
                            .desired_rows(1)
                            .desired_width(width),
                    );
                    #[cfg(test)]
                    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_text_box_editor"), edit.rect));
                    // Return focus to the text only when nothing else has it (a typed
                    // font size in the bar, the colour picker's hex field, ...).
                    let others_focused = ui.ctx().memory(|m| m.focused()).is_some_and(|f| f != edit_id);
                    if sel.focus_contents && !egui::Popup::is_any_open(ui.ctx()) && !others_focused {
                        edit.request_focus();
                        sel.focus_contents = false;
                    }
                    lost = edit.lost_focus();
                });
        });
    if contents != sel.contents {
        re.styles = crate::document::sync_styles(&sel.contents, &contents, &re.styles, re.typing);
        sel.contents = contents;
    }
    if lost {
        let pointer_on_bar = ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| bar.response.rect.contains(p));
        if pointer_on_bar || egui::Popup::is_any_open(&ctx) {
            // Using the formatting bar: keep editing.
            sel.focus_contents = true;
        } else {
            // Done editing: save, keep the box selected.
            sel.editing = false;
            let rich = re.to_rich(&sel.contents);
            if rich != re.committed {
                re.committed = rich.clone();
                sel.committed = sel.contents.clone();
                let req = Request::UpdateAnnot {
                    doc: tab.id,
                    page: sel.page,
                    index: sel.index,
                    change: AnnotChange {
                        rich: Some(rich),
                        ..Default::default()
                    },
                };
                tab.outbox.push(req);
            }
        }
    }
}

/// `D:20261122101500Z` -> `2026-11-22 10:15`.
pub fn format_pdf_date(d: &str) -> String {
    let d = d.strip_prefix("D:").unwrap_or(d);
    let digits: String = d.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return String::new();
    }
    let mut s = format!("{}-{}-{}", &digits[0..4], &digits[4..6], &digits[6..8]);
    if digits.len() >= 12 {
        s += &format!(" {}:{}", &digits[8..10], &digits[10..12]);
    }
    s
}

/// Applies an annotation selection change coming from outside the view (e.g. sidebar).
pub fn focus_annot(tab: &mut DocTab, page: usize, index: usize, edit: bool) {
    let is_text_box = tab.annot(page, index).is_some_and(|a| a.kind == AnnotKind::FreeText);
    if edit && is_text_box {
        // Start from the box's current text and styles.
        tab.deselect_annot();
    }
    tab.select_annot(page, index);
    if let Some(AnnotSelection {
        focus_contents, editing, ..
    }) = &mut tab.selected_annot
    {
        *focus_contents = edit;
        *editing = edit && is_text_box;
    }
    if let Some(rect) = tab.annot(page, index).map(|a| a.rect) {
        tab.reveal(page, rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DocInfo, PageGeom, PageText, Rotation, TextChar};
    use std::path::PathBuf;

    /// A one-page document with the word "test" selected, shown with the select tool.
    fn tab_with_selection() -> DocTab {
        let mut tab = DocTab::new(1, PathBuf::from("t.pdf"), LayoutMode::SinglePage, None);
        tab.state = LoadState::Ready(DocInfo {
            page_sizes: vec![(595.0, 842.0)],
            outline: Vec::new(),
            title: None,
            ..Default::default()
        });
        let chars = "test"
            .chars()
            .enumerate()
            .map(|(i, ch)| TextChar {
                ch,
                rect: Some(PdfBox {
                    left: 100.0 + i as f32 * 10.0,
                    bottom: 700.0,
                    right: 110.0 + i as f32 * 10.0,
                    top: 715.0,
                }),
            })
            .collect();
        tab.texts.insert(0, Some(PageText { chars, links: Vec::new() }));
        tab.geoms.insert(
            0,
            PageGeom {
                size: (595.0, 842.0),
                origin: (0.0, 0.0),
                rotation: Rotation::R0,
            },
        );
        tab.selection = Some(Selection {
            page: 0,
            anchor: 0,
            head: 4,
        });
        tab
    }

    fn frame(ctx: &egui::Context, tab: &mut DocTab, cache: &mut TextureCache, events: Vec<egui::Event>) {
        frame_with(ctx, tab, cache, events, Tool::SelectText);
    }

    fn frame_with(
        ctx: &egui::Context,
        tab: &mut DocTab,
        cache: &mut TextureCache,
        events: Vec<egui::Event>,
        tool: Tool,
    ) {
        TEST_STAMP.with(|s| frame_full(ctx, tab, cache, events, tool, s.borrow().as_ref()));
    }

    thread_local! {
        /// What the stamp tool places in tests.
        static TEST_STAMP: std::cell::RefCell<Option<StampSpec>> = const { std::cell::RefCell::new(None) };
    }

    fn frame_full(
        ctx: &egui::Context,
        tab: &mut DocTab,
        cache: &mut TextureCache,
        events: Vec<egui::Event>,
        tool: Tool,
        stamp: Option<&StampSpec>,
    ) {
        let style = AnnotStyle::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 900.0))),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::no_frame().show(ui, |ui| {
                show(
                    ui,
                    tab,
                    ViewContext {
                        cache: &mut *cache,
                        recent_colors: &mut Vec::new(),
                        tool,
                        night: false,
                        style: &style,
                        author: "me",
                        stamp,
                        signature: None,
                        signature_image: None,
                        presenting: false,
                    },
                );
            });
        });
        out.textures_delta.clear();
    }

    fn button(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn selection_toolbar_highlight_button_creates_annotation() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame(&ctx, &mut tab, &mut cache, Vec::new());
        }
        let rect: Rect = ctx
            .data(|d| d.get_temp(Id::unique(("test_button", "Highlight"))))
            .expect("selection toolbar is shown");
        let at = rect.center();
        tab.outbox.clear();
        frame(&ctx, &mut tab, &mut cache, vec![egui::Event::PointerMoved(at)]);
        frame(&ctx, &mut tab, &mut cache, vec![button(at, true)]);
        frame(&ctx, &mut tab, &mut cache, vec![button(at, false)]);
        let created = tab.outbox.iter().any(|r| {
            matches!(
                r,
                Request::AddAnnot {
                    annot: NewAnnot {
                        kind: AnnotKind::Highlight,
                        ..
                    },
                    ..
                }
            )
        });
        assert!(created, "clicking Highlight did not create an annotation");
        assert!(tab.selection.is_none(), "selection is consumed by the highlight");
    }

    fn click(ctx: &egui::Context, tab: &mut DocTab, cache: &mut TextureCache, at: Pos2, tool: Tool) {
        frame_with(ctx, tab, cache, vec![egui::Event::PointerMoved(at)], tool);
        frame_with(ctx, tab, cache, vec![button(at, true)], tool);
        frame_with(ctx, tab, cache, vec![button(at, false)], tool);
    }

    #[test]
    fn comment_is_saved_when_clicking_elsewhere() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        let rect = PdfBox {
            left: 100.0,
            bottom: 700.0,
            right: 140.0,
            top: 715.0,
        };
        tab.annots.insert(
            0,
            vec![AnnotInfo {
                index: 3,
                kind: AnnotKind::Underline,
                rect,
                color: Some([0, 110, 230]),
                interior: None,
                width: 1.0,
                contents: String::new(),
                author: "me".into(),
                modified: String::new(),
                font_size: None,
                text_color: None,
                rich: None,
                vertices: Vec::new(),
                replies: Vec::new(),
                status: String::new(),
                other_replies: Vec::new(),
                in_reply_to: None,
            }],
        );
        tab.select_annot(0, 3);
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        let comment: Rect = ctx
            .data(|d| d.get_temp(Id::unique("test_comment_box")))
            .expect("annotation popup is shown");
        click(&ctx, &mut tab, &mut cache, comment.center(), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("Check this".into())], Tool::Hand);
        assert!(tab.outbox.is_empty(), "nothing is saved while still typing");

        // Click on an empty part of the page to finish.
        let page = tab.layout.page_rect(0).unwrap();
        let empty = tab.viewport.rect_to_screen(page, tab.view_rect).center_bottom() - vec2(0.0, 40.0);
        click(&ctx, &mut tab, &mut cache, empty, Tool::Hand);

        let saved = tab.outbox.iter().any(|r| {
            matches!(
                r,
                Request::UpdateAnnot { page: 0, index: 3, change, .. }
                    if change.contents.as_deref() == Some("Check this")
            )
        });
        assert!(saved, "comment was lost: {:?}", tab.outbox.len());
        let saves = tab
            .outbox
            .iter()
            .filter(|r| matches!(r, Request::UpdateAnnot { .. }))
            .count();
        assert_eq!(saves, 1, "comment saved more than once");
        assert!(tab.selected_annot.is_none());
    }

    #[test]
    fn clicking_the_comment_box_over_a_form_field_keeps_the_popup() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        let rect = PdfBox {
            left: 100.0,
            bottom: 700.0,
            right: 140.0,
            top: 715.0,
        };
        tab.annots.insert(
            0,
            vec![AnnotInfo {
                index: 3,
                kind: AnnotKind::Underline,
                rect,
                color: Some([0, 110, 230]),
                interior: None,
                width: 1.0,
                contents: String::new(),
                author: "me".into(),
                modified: String::new(),
                font_size: None,
                text_color: None,
                rich: None,
                vertices: Vec::new(),
                replies: Vec::new(),
                status: String::new(),
                other_replies: Vec::new(),
                in_reply_to: None,
            }],
        );
        // A big text field under where the popup appears.
        tab.widgets.insert(
            0,
            vec![crate::engine::WidgetInfo {
                rect: PdfBox {
                    left: 0.0,
                    bottom: 0.0,
                    right: 595.0,
                    top: 690.0,
                },
                field_type: crate::engine::FIELD_TEXT,
                name: "name".into(),
                value: String::new(),
                checked: false,
            }],
        );
        tab.select_annot(0, 3);
        let mut cache = TextureCache::new(usize::MAX);
        for tool in [Tool::Hand, Tool::SelectText] {
            for _ in 0..3 {
                frame_with(&ctx, &mut tab, &mut cache, Vec::new(), tool);
            }
            let comment: Rect = ctx
                .data(|d| d.get_temp(Id::unique("test_comment_box")))
                .expect("annotation popup is shown");
            tab.outbox.clear();
            click(&ctx, &mut tab, &mut cache, comment.center(), tool);
            assert!(tab.selected_annot.is_some(), "{tool:?}: popup closed on click");
            assert!(
                !tab.outbox.iter().any(|r| matches!(r, Request::FormMouse { .. })),
                "{tool:?}: click went through the popup to the form field"
            );
            frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("ok".into())], tool);
            assert!(tab.selected_annot.as_ref().unwrap().contents.ends_with("ok"));
        }
    }

    #[test]
    fn replies_show_in_the_popup_and_can_be_added_there() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        let mut a = square(5, AnnotKind::Square);
        a.contents = "Wrong figure".into();
        a.replies = vec![crate::engine::Reply { author: "Bob".into(), date: String::new(), text: "fixed in v2".into() }];
        tab.annots.insert(0, vec![a]);
        tab.select_annot(0, 5);
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame(&ctx, &mut tab, &mut cache, Vec::new());
        }
        let reply_box: Rect = ctx.data(|d| d.get_temp(Id::unique("test_reply_box"))).expect("reply field in the popup");
        tab.outbox.clear();
        tab.reply_draft = "thanks".into();
        frame(&ctx, &mut tab, &mut cache, Vec::new());
        let reply_button: Rect = ctx.data(|d| d.get_temp(Id::unique("test_reply_button"))).unwrap();
        let at = reply_button.center();
        assert!((reply_button.center().y - reply_box.center().y).abs() < 20.0, "next to the field");
        frame(&ctx, &mut tab, &mut cache, vec![egui::Event::PointerMoved(at)]);
        frame(&ctx, &mut tab, &mut cache, vec![button(at, true)]);
        frame(&ctx, &mut tab, &mut cache, vec![button(at, false)]);
        let replies = tab.outbox.iter().find_map(|r| match r {
            Request::UpdateAnnot { change, .. } => change.replies.clone(),
            _ => None,
        });
        let replies = replies.expect("the reply is sent");
        assert_eq!(replies.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["fixed in v2", "thanks"]);
        assert_eq!(replies[1].author, "me");
        assert!(tab.reply_draft.is_empty());
    }

    #[test]
    fn fill_checkbox_on_a_selected_shape_sends_an_update() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        tab.annots.insert(
            0,
            vec![AnnotInfo {
                index: 5,
                kind: AnnotKind::Square,
                rect: PdfBox {
                    left: 100.0,
                    bottom: 600.0,
                    right: 200.0,
                    top: 700.0,
                },
                color: Some([0, 0, 255]),
                interior: None,
                width: 2.0,
                contents: String::new(),
                author: String::new(),
                modified: String::new(),
                font_size: None,
                text_color: None,
                rich: None,
                vertices: Vec::new(),
                replies: Vec::new(),
                status: String::new(),
                other_replies: Vec::new(),
                in_reply_to: None,
            }],
        );
        tab.select_annot(0, 5);
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        // The fill swatch is the last colour button in a shape's popup.
        let fill: Rect = ctx.data(|d| d.get_temp(Id::unique("test_color_button"))).expect("fill button shown");
        click(&ctx, &mut tab, &mut cache, fill.center(), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        let cell: Rect = ctx
            .data(|d| d.get_temp(Id::unique(("test_palette_cell", 1usize, 2usize))))
            .expect("colour picker open");
        tab.outbox.clear();
        click(&ctx, &mut tab, &mut cache, cell.center(), Tool::Hand);
        let interior = tab.outbox.iter().find_map(|r| match r {
            Request::UpdateAnnot { change, .. } => change.interior,
            _ => None,
        });
        assert_eq!(interior, Some(Some(color_picker::palette()[1][2])), "picking a fill colour");
        assert!(tab.selected_annot.is_some(), "popup stays open");
    }

    #[test]
    fn text_box_tool_creates_and_edits_in_place() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::TextBox);
        }
        // Click on the page with the text box tool.
        let page = tab.viewport.rect_to_screen(tab.layout.page_rect(0).unwrap(), tab.view_rect);
        let at = page.center();
        tab.outbox.clear();
        click(&ctx, &mut tab, &mut cache, at, Tool::TextBox);
        let created = tab.outbox.iter().find_map(|r| match r {
            Request::AddAnnot { annot, .. } if annot.kind == AnnotKind::FreeText => Some(annot.clone()),
            _ => None,
        });
        let created = created.expect("a text box is created");
        assert!((created.rect.right - created.rect.left - 200.0).abs() < 1e-3);

        // The engine lists the new box; it opens for typing.
        tab.annots.insert(
            0,
            vec![AnnotInfo {
                index: 7,
                kind: AnnotKind::FreeText,
                rect: PdfBox {
                    bottom: created.rect.top - 23.0,
                    ..created.rect
                },
                color: Some([220, 30, 30]),
                interior: Some([255, 255, 255]),
                width: 1.0,
                contents: String::new(),
                author: String::new(),
                modified: String::new(),
                font_size: Some(12.0),
                text_color: Some([0, 0, 0]),
                rich: None,
                vertices: Vec::new(),
                replies: Vec::new(),
                status: String::new(),
                other_replies: Vec::new(),
                in_reply_to: None,
            }],
        );
        focus_annot(&mut tab, 0, 7, true);
        for _ in 0..2 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::TextBox);
        }
        assert!(ctx.data(|d| d.get_temp::<Rect>(Id::unique("test_text_box_editor"))).is_some());
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("Hallo".into())], Tool::TextBox);
        tab.outbox.clear();

        // Click elsewhere on the page (with the hand tool) to finish.
        click(&ctx, &mut tab, &mut cache, page.center_bottom() - vec2(0.0, 30.0), Tool::Hand);
        let saved = tab.outbox.iter().any(|r| {
            matches!(r, Request::UpdateAnnot { index: 7, change, .. }
                if change.rich.as_ref().map(|r| r.text()).as_deref() == Some("Hallo"))
        });
        assert!(saved, "typed text was not saved");
        let saves = tab.outbox.iter().filter(|r| matches!(r, Request::UpdateAnnot { .. })).count();
        assert_eq!(saves, 1);
    }

    fn square(index: usize, kind: AnnotKind) -> AnnotInfo {
        AnnotInfo {
            index,
            kind,
            rect: PdfBox {
                left: 100.0,
                bottom: 600.0,
                right: 200.0,
                top: 700.0,
            },
            color: Some([255, 0, 0]),
            interior: None,
            width: 2.0,
            contents: String::new(),
            author: String::new(),
            modified: String::new(),
            font_size: Some(12.0),
            text_color: Some([0, 0, 0]),
            rich: None,
            vertices: Vec::new(),
            replies: Vec::new(),
            status: String::new(),
            other_replies: Vec::new(),
            in_reply_to: None,
        }
    }

    fn drag(ctx: &egui::Context, tab: &mut DocTab, cache: &mut TextureCache, from: Pos2, to: Pos2, tool: Tool) {
        frame_with(ctx, tab, cache, vec![egui::Event::PointerMoved(from)], tool);
        frame_with(ctx, tab, cache, vec![button(from, true)], tool);
        for i in 1..=8 {
            let p = from + (to - from) * (i as f32 / 8.0);
            frame_with(ctx, tab, cache, vec![egui::Event::PointerMoved(p)], tool);
        }
        frame_with(ctx, tab, cache, vec![button(to, false)], tool);
        frame_with(ctx, tab, cache, Vec::new(), tool);
    }

    fn sent_rect(tab: &DocTab) -> Option<PdfBox> {
        tab.outbox.iter().find_map(|r| match r {
            Request::UpdateAnnot { change, .. } => change.rect,
            _ => None,
        })
    }

    fn setup(kind: AnnotKind) -> (egui::Context, DocTab, TextureCache) {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        tab.annots.insert(0, vec![square(4, kind)]);
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        tab.outbox.clear();
        (ctx, tab, cache)
    }

    #[test]
    fn highlighting_back_to_the_start_marks_nothing() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Highlight);
        }
        // "test": one character per 10 pt from x = 100, baseline 700..715.
        let [start, far, t_left, e_right] = [101.0, 128.0, 101.0, 118.0].map(|x| pdf_to_screen(&tab, 0, pos2(x, 707.0), tab.view_rect).unwrap());
        tab.outbox.clear();
        // Out to the third character and back to where it started.
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::PointerMoved(start)], Tool::Highlight);
        frame_with(&ctx, &mut tab, &mut cache, vec![button(start, true)], Tool::Highlight);
        for p in [start + vec2(6.0, 0.0), far, start + vec2(6.0, 0.0), start] {
            frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::PointerMoved(p)], Tool::Highlight);
        }
        frame_with(&ctx, &mut tab, &mut cache, vec![button(start, false)], Tool::Highlight);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Highlight);
        assert!(added(&tab).is_empty(), "nothing selected, nothing highlighted");
        assert_eq!(tab.selected_text(), None);

        // From the left edge of "t" to the right half of "e": exactly "te".
        drag(&ctx, &mut tab, &mut cache, t_left, e_right, Tool::Highlight);
        let a = added(&tab);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].rect.left, a[0].rect.right), (100.0, 120.0), "two characters");
    }

    #[test]
    fn dropping_an_annotation_on_another_page_moves_it_there() {
        let ctx = egui::Context::default();
        let mut tab = DocTab::new(1, PathBuf::from("t.pdf"), LayoutMode::Horizontal, None);
        tab.state = LoadState::Ready(DocInfo {
            page_sizes: vec![(595.0, 842.0); 2],
            ..Default::default()
        });
        for p in 0..2 {
            tab.geoms.insert(p, PageGeom { size: (595.0, 842.0), origin: (0.0, 0.0), rotation: Rotation::R0 });
        }
        tab.viewport.fit = FitMode::None;
        tab.viewport.zoom = 0.4;
        tab.annots.insert(0, vec![square(0, AnnotKind::Note)]);
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        tab.outbox.clear();
        let a = tab.annots[&0][0].rect;
        let from = pdf_to_screen(&tab, 0, pos2((a.left + a.right) / 2.0, (a.bottom + a.top) / 2.0), tab.view_rect).unwrap();
        let to = pdf_to_screen(&tab, 1, pos2(300.0, 400.0), tab.view_rect).unwrap();
        drag(&ctx, &mut tab, &mut cache, from, to, Tool::Hand);
        let moved = tab.outbox.iter().find_map(|r| match r {
            Request::MoveAnnotToPage { page, index, to_page, rect, .. } => Some((*page, *index, *to_page, *rect)),
            _ => None,
        });
        let (page, index, to_page, rect) = moved.expect("a move to the other page");
        assert_eq!((page, index, to_page), (0, 0, 1));
        let (cx, cy) = ((rect.left + rect.right) / 2.0, (rect.bottom + rect.top) / 2.0);
        assert!((cx - 300.0).abs() < 4.0 && (cy - 400.0).abs() < 4.0, "dropped under the mouse: {rect:?}");
        assert!(sent_rect(&tab).is_none(), "not also moved on its own page");
    }

    #[test]
    fn resize_math() {
        let r = Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 50.0));
        assert_eq!(resize_rect(r, 4, vec2(20.0, 10.0), false), Rect::from_min_max(pos2(10.0, 10.0), pos2(130.0, 70.0)));
        assert_eq!(resize_rect(r, 7, vec2(-5.0, 99.0), false), Rect::from_min_max(pos2(5.0, 10.0), pos2(110.0, 60.0)));
        // Shift keeps the 2:1 ratio, anchored at the opposite corner.
        let k = resize_rect(r, 4, vec2(100.0, 0.0), true);
        assert_eq!(k.min, r.min);
        assert!((k.width() / k.height() - 2.0).abs() < 1e-4);
        // Never smaller than 4 pt.
        let tiny = resize_rect(r, 3, vec2(-500.0, 0.0), false);
        assert!(tiny.width() >= 4.0 && tiny.height() >= 4.0);
    }

    #[test]
    fn dragging_a_shape_moves_it() {
        let (ctx, mut tab, mut cache) = setup(AnnotKind::Square);
        let screen = pdf_box_to_screen(&tab, 0, &tab.annots[&0][0].rect, tab.view_rect).unwrap();
        let z = tab.viewport.zoom;
        drag(&ctx, &mut tab, &mut cache, screen.center(), screen.center() + vec2(60.0, 30.0), Tool::Hand);
        let r = sent_rect(&tab).expect("move sent");
        assert!((r.left - (100.0 + 60.0 / z)).abs() < 0.5, "{r:?}");
        assert!((r.top - (700.0 - 30.0 / z)).abs() < 0.5, "{r:?}");
        assert!((r.right - r.left - 100.0).abs() < 0.01, "size kept");
        assert_eq!(tab.selected_annot.as_ref().map(|s| s.index), Some(4));
        assert_eq!(tab.annots[&0][0].rect, r, "shown at the new place right away");
    }

    #[test]
    fn dragging_a_corner_handle_resizes() {
        let (ctx, mut tab, mut cache) = setup(AnnotKind::Square);
        tab.select_annot(0, 4);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        let screen = pdf_box_to_screen(&tab, 0, &tab.annots[&0][0].rect, tab.view_rect).unwrap();
        let z = tab.viewport.zoom;
        let corner = screen.right_bottom();
        drag(&ctx, &mut tab, &mut cache, corner, corner + vec2(40.0, 20.0), Tool::Hand);
        let r = sent_rect(&tab).expect("resize sent");
        assert!((r.left - 100.0).abs() < 0.01 && (r.top - 700.0).abs() < 0.01, "top-left stays: {r:?}");
        assert!((r.right - (200.0 + 40.0 / z)).abs() < 0.5, "{r:?}");
        assert!((r.bottom - (600.0 - 20.0 / z)).abs() < 0.5, "{r:?}");
    }

    #[test]
    fn arrow_nudge_and_text_box_handles() {
        let (ctx, mut tab, mut cache) = setup(AnnotKind::Square);
        tab.select_annot(0, 4);
        tab.nudge = vec2(10.0, 0.0);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        let r = sent_rect(&tab).expect("nudge sent");
        assert!((r.left - 110.0).abs() < 1e-3 && (r.bottom - 600.0).abs() < 1e-3, "{r:?}");

        // Text boxes only resize in width; notes not at all.
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 40.0));
        assert_eq!(handles(screen, AnnotKind::FreeText.resize()).len(), 8);
        assert!(handles(screen, AnnotKind::Note.resize()).is_empty());
        assert_eq!(handles(screen, AnnotKind::Ink.resize()).len(), 8);
        assert!(!AnnotKind::Highlight.movable());
    }

    fn key(k: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
        let ev = |pressed| egui::Event::Key {
            key: k,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        };
        vec![ev(true), ev(false)]
    }

    /// A text box being edited, with focus in the editor.
    fn editing_box() -> (egui::Context, DocTab, TextureCache) {
        let (ctx, mut tab, mut cache) = setup(AnnotKind::FreeText);
        focus_annot(&mut tab, 0, 4, true);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        (ctx, tab, cache)
    }

    fn finish(ctx: &egui::Context, tab: &mut DocTab, cache: &mut TextureCache) -> crate::engine::RichText {
        tab.outbox.clear();
        let page = tab.viewport.rect_to_screen(tab.layout.page_rect(0).unwrap(), tab.view_rect);
        click(ctx, tab, cache, page.center_bottom() - vec2(0.0, 30.0), Tool::Hand);
        tab.outbox
            .iter()
            .find_map(|r| match r {
                Request::UpdateAnnot { change, .. } => change.rich.clone(),
                _ => None,
            })
            .expect("text saved")
    }

    #[test]
    fn select_all_then_ctrl_b_makes_the_text_bold() {
        let (ctx, mut tab, mut cache) = editing_box();
        let ctrl = egui::Modifiers::COMMAND;
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("Hello".into())], Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::A, ctrl), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::B, ctrl), Tool::Hand);
        let rich = finish(&ctx, &mut tab, &mut cache);
        assert_eq!(rich.text(), "Hello");
        assert!(rich.spans.iter().all(|s| s.style.bold), "{rich:?}");
    }

    #[test]
    fn ctrl_b_without_selection_styles_what_is_typed_next() {
        let (ctx, mut tab, mut cache) = editing_box();
        let ctrl = egui::Modifiers::COMMAND;
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("ab".into())], Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::B, ctrl), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("CD".into())], Tool::Hand);
        let rich = finish(&ctx, &mut tab, &mut cache);
        assert_eq!(rich.text(), "abCD");
        assert_eq!(rich.spans.len(), 2, "{rich:?}");
        assert!(!rich.spans[0].style.bold && rich.spans[1].style.bold);
        assert_eq!(rich.spans[1].text, "CD");
    }

    #[test]
    fn clicking_the_format_bar_keeps_editing() {
        let (ctx, mut tab, mut cache) = editing_box();
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("x".into())], Tool::Hand);
        let bar: Rect = ctx.data(|d| d.get_temp(Id::unique("test_format_bar"))).expect("format bar shown");
        // The first button (bold).
        click(&ctx, &mut tab, &mut cache, pos2(bar.left() + 20.0, bar.center().y), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        assert!(tab.selected_annot.as_ref().is_some_and(|s| s.editing), "still editing");
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("y".into())], Tool::Hand);
        let rich = finish(&ctx, &mut tab, &mut cache);
        assert_eq!(rich.text(), "xy");
        assert!(rich.spans.last().unwrap().style.bold, "typed after clicking B: {rich:?}");
    }

    #[test]
    fn typing_a_font_size_applies_once_on_enter() {
        let (ctx, mut tab, mut cache) = setup(AnnotKind::FreeText);
        tab.select_annot(0, 4);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        let field: Rect = ctx
            .data(|d| d.get_temp(Id::unique(("test_number_field", "Size"))))
            .expect("size field shown");
        tab.outbox.clear();
        click(&ctx, &mut tab, &mut cache, field.center(), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        let none = egui::Modifiers::NONE;
        // Replace the shown value with "14".
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::A, egui::Modifiers::COMMAND), Tool::Hand);
        for t in ["1", "4"] {
            frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text(t.into())], Tool::Hand);
        }
        let sizes = |tab: &DocTab| -> Vec<f32> {
            tab.outbox
                .iter()
                .filter_map(|r| match r {
                    Request::UpdateAnnot { change, .. } => change.font_size,
                    _ => None,
                })
                .collect()
        };
        assert!(sizes(&tab).is_empty(), "nothing applied while typing: {:?}", sizes(&tab));
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::Enter, none), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        assert_eq!(sizes(&tab), vec![14.0]);
    }

    #[test]
    fn typing_a_size_for_selected_text_in_the_format_bar() {
        let (ctx, mut tab, mut cache) = editing_box();
        frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text("Hello".into())], Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::A, egui::Modifiers::COMMAND), Tool::Hand);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        let field: Rect = ctx.data(|d| d.get_temp(Id::unique("test_bar_size"))).expect("size field");
        click(&ctx, &mut tab, &mut cache, field.center(), Tool::Hand);
        for _ in 0..2 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::A, egui::Modifiers::COMMAND), Tool::Hand);
        for t in ["2", "0"] {
            frame_with(&ctx, &mut tab, &mut cache, vec![egui::Event::Text(t.into())], Tool::Hand);
        }
        frame_with(&ctx, &mut tab, &mut cache, key(egui::Key::Enter, egui::Modifiers::NONE), Tool::Hand);
        for _ in 0..2 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        }
        let rich = finish(&ctx, &mut tab, &mut cache);
        assert_eq!(rich.text(), "Hello", "the typed digits went into the size, not the text");
        assert!(rich.spans.iter().all(|s| s.style.size == 20.0), "{rich:?}");
    }

    #[test]
    fn text_box_height_handle_resizes() {
        let (ctx, mut tab, mut cache) = setup(AnnotKind::FreeText);
        tab.select_annot(0, 4);
        frame_with(&ctx, &mut tab, &mut cache, Vec::new(), Tool::Hand);
        let screen = pdf_box_to_screen(&tab, 0, &tab.annots[&0][0].rect, tab.view_rect).unwrap();
        let z = tab.viewport.zoom;
        let bottom = screen.center_bottom();
        drag(&ctx, &mut tab, &mut cache, bottom, bottom + vec2(0.0, 50.0), Tool::Hand);
        let r = sent_rect(&tab).expect("resize sent");
        assert!((r.bottom - (600.0 - 50.0 / z)).abs() < 0.5, "{r:?}");
        assert_eq!((r.left, r.right, r.top), (100.0, 200.0, 700.0));
    }

    fn added(tab: &DocTab) -> Vec<NewAnnot> {
        tab.outbox
            .iter()
            .filter_map(|r| match r {
                Request::AddAnnot { annot, .. } => Some(annot.clone()),
                _ => None,
            })
            .collect()
    }

    fn ready(tool: Tool) -> (egui::Context, DocTab, TextureCache, Rect) {
        let ctx = egui::Context::default();
        let mut tab = tab_with_selection();
        tab.selection = None;
        let mut cache = TextureCache::new(usize::MAX);
        for _ in 0..3 {
            frame_with(&ctx, &mut tab, &mut cache, Vec::new(), tool);
        }
        tab.outbox.clear();
        let page = tab.viewport.rect_to_screen(tab.layout.page_rect(0).unwrap(), tab.view_rect);
        (ctx, tab, cache, page)
    }

    #[test]
    fn arrow_tool_drags_an_arrow_and_shift_snaps_it() {
        let (ctx, mut tab, mut cache, page) = ready(Tool::Arrow);
        let from = page.center();
        drag(&ctx, &mut tab, &mut cache, from, from + vec2(100.0, 8.0), Tool::Arrow);
        let a = added(&tab);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, AnnotKind::Arrow);
        let v = &a[0].vertices;
        assert_eq!(v.len(), 2);
        assert!(v[1].0 > v[0].0 && (v[1].1 - v[0].1).abs() > 0.5, "not snapped: {v:?}");
    }

    #[test]
    fn polygon_tool_clicks_corners_and_enter_finishes() {
        let (ctx, mut tab, mut cache, page) = ready(Tool::Polygon);
        let c = page.center();
        for p in [c, c + vec2(80.0, 0.0), c + vec2(40.0, 60.0)] {
            click(&ctx, &mut tab, &mut cache, p, Tool::Polygon);
            // Clicks far apart in time (no double-click).
            ctx.input_mut(|i| i.time += 1.0);
        }
        assert!(added(&tab).is_empty(), "still open");
        let enter = vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }];
        frame_with(&ctx, &mut tab, &mut cache, enter, Tool::Polygon);
        let a = added(&tab);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].kind, a[0].vertices.len()), (AnnotKind::Polygon, 3));
        assert!(tab.draft.is_none());
    }

    #[test]
    fn cloud_tool_drag_makes_a_rectangular_cloud() {
        let (ctx, mut tab, mut cache, page) = ready(Tool::Cloud);
        let from = page.center();
        drag(&ctx, &mut tab, &mut cache, from, from + vec2(120.0, 80.0), Tool::Cloud);
        let a = added(&tab);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].kind, a[0].vertices.len()), (AnnotKind::Cloud, 4));
    }

    #[test]
    fn stamp_tool_places_the_chosen_stamp_or_asks_for_one() {
        let (ctx, mut tab, mut cache, page) = ready(Tool::Stamp);
        click(&ctx, &mut tab, &mut cache, page.center(), Tool::Stamp);
        assert!(added(&tab).is_empty());
        assert!(tab.error_message.take().is_some(), "asks to choose a stamp");
        let spec = StampSpec::Text {
            title: "DRAFT".into(),
            subtitle: String::new(),
        };
        TEST_STAMP.with(|s| *s.borrow_mut() = Some(spec.clone()));
        ctx.input_mut(|i| i.time += 1.0);
        click(&ctx, &mut tab, &mut cache, page.center(), Tool::Stamp);
        TEST_STAMP.with(|s| *s.borrow_mut() = None);
        let a = added(&tab);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].kind, a[0].stamp.clone()), (AnnotKind::Stamp, Some(spec)));
        assert_eq!(a[0].rect.left, a[0].rect.right, "the engine sizes text stamps");
    }
}

