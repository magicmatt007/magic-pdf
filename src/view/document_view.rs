//! The central document widget: draws pages and handles scrolling, zooming, text
//! selection, links and annotation tools.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, Sense, Shadow, Shape, Stroke, StrokeKind,
    Ui, Vec2, pos2, vec2,
};

use crate::icons;
use crate::document::{AnnotSelection, DocTab, Draft, LoadState, Selection, quad};
use crate::engine::{
    AnnotChange, AnnotInfo, AnnotKind, FormMouse, LinkTarget, NewAnnot, PdfBox, RenderKey, Request, Rgb, form_mods,
};
use crate::settings::AnnotStyle;
use crate::view::cache::{TextureCache, width_bucket};
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
}

impl Tool {
    pub const ANNOTATION_TOOLS: [Tool; 8] = [
        Tool::Highlight,
        Tool::Underline,
        Tool::StrikeOut,
        Tool::Note,
        Tool::TextBox,
        Tool::Ink,
        Tool::Rect,
        Tool::Ellipse,
    ];

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
        })
    }

    pub fn label(self) -> &'static str {
        match self {
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
        }
    }

    /// Colour slot this tool uses for new annotations.
    pub fn color(self, style: &mut AnnotStyle) -> Option<&mut Rgb> {
        match self {
            Tool::Highlight => Some(&mut style.highlight),
            Tool::Underline => Some(&mut style.underline),
            Tool::StrikeOut => Some(&mut style.strikeout),
            Tool::Note => Some(&mut style.note),
            Tool::Ink => Some(&mut style.ink),
            Tool::Rect | Tool::Ellipse => Some(&mut style.shape),
            Tool::TextBox => Some(&mut style.textbox_border),
            Tool::Hand | Tool::SelectText => None,
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
}

const SEARCH_COLOR: Color32 = Color32::from_rgba_premultiplied(120, 100, 0, 90);
const SEARCH_CURRENT_COLOR: Color32 = Color32::from_rgba_premultiplied(160, 60, 0, 130);
const SELECTION_COLOR: Color32 = Color32::from_rgba_premultiplied(30, 90, 200, 90);
const NOTE_SIZE: f32 = 20.0;

pub fn rgb(c: Rgb) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

pub fn show(ui: &mut Ui, tab: &mut DocTab, mut cx: ViewContext<'_>) {
    let (view, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let bg = if cx.night {
        Color32::from_gray(24)
    } else {
        Color32::from_gray(96)
    };
    ui.painter().rect_filled(view, CornerRadius::ZERO, bg);

    match &tab.state {
        LoadState::Ready(_) => {}
        LoadState::Loading => {
            centered_text(ui, view, "Loading…");
            return;
        }
        LoadState::Failed(e) => {
            centered_text(ui, view, &format!("Could not open {}:\n{e}", tab.path.display()));
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

    apply_pending_navigation(tab, view);
    tab.viewport.clamp(tab.layout.size, view.size());
    update_current_page(tab, view);

    paint_pages(ui, tab, view, &mut cx);
    selection_toolbar(ui, tab, view, &cx);
    annotation_popup(ui, tab, view, cx.recent_colors);
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
        .find(|a| a.rect.expand(2.0).contains(p.x, p.y))
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

    // Drawing tools.
    if matches!(tool, Tool::Ink | Tool::Rect | Tool::Ellipse | Tool::TextBox) && !middle_down {
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
        let tip = if a.author.is_empty() {
            a.contents.clone()
        } else if a.contents.is_empty() {
            a.author.clone()
        } else {
            format!("{}\n— {}", a.contents, a.author)
        };
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
        if let Some(i) = nearest_char(text, pdf_pt) {
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
            && let Some(i) = nearest_char(text, pdf_pt)
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
                head: b,
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
    let _ = ui;
    let Some(pointer) = pointer else { return };
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
            Some(Draft::Ink { page, .. } | Draft::Shape { page, .. }) => *page,
            None => return,
        };
        if let Some((_, p)) = screen_to_pdf(tab, pointer, view, Some(page)) {
            match &mut tab.draft {
                Some(Draft::Ink { points, .. }) => {
                    if points.last().is_none_or(|l| (*l - p).length() > 0.5) {
                        points.push(p);
                    }
                }
                Some(Draft::Shape { end, .. }) => *end = p,
                None => {}
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
    tab.outbox.push(Request::AddAnnot {
        doc: tab.id,
        page,
        annot: NewAnnot {
            kind,
            rect,
            color,
            interior,
            width,
            contents: String::new(),
            author: cx.author.to_string(),
            quads,
            ink,
            font_size: style.textbox_font_size,
            text_color: style.textbox_text,
        },
    });
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
    if tab.mode == LayoutMode::SinglePage {
        return;
    }
    let visible = tab.viewport.visible_doc_rect(view);
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
        let screen = tab.viewport.rect_to_screen(rect, view);
        let want_w = width_bucket(screen.width() * ppp, size.y / size.x.max(1.0));
        let version = tab.page_version(page);
        let key = RenderKey {
            doc: tab.id,
            page,
            width_px: want_w,
            rotation: tab.rotation,
            version,
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

        paint_overlays(&painter, tab, page, view);
    }
    paint_draft(&painter, tab, view, cx);
}

fn paint_overlays(painter: &egui::Painter, tab: &DocTab, page: usize, view: Rect) {
    if !tab.geoms.contains_key(&page) {
        return;
    }
    let to_screen = |b: &PdfBox| pdf_box_to_screen(tab, page, b, view).unwrap_or(Rect::NOTHING);

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

    if let Some(sel) = tab.selected_annot.as_ref().filter(|s| s.page == page)
        && let Some(a) = tab.annot(page, sel.index)
    {
        let s = to_screen(&a.rect).expand(3.0);
        painter.rect_stroke(
            s,
            CornerRadius::same(2),
            Stroke::new(1.5, Color32::from_rgb(30, 120, 255)),
            StrokeKind::Outside,
        );
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
    egui::Area::new(Id::unique(("selection_toolbar", tab.id)))
        .fixed_pos(last.right_bottom() + vec2(4.0, 4.0))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(icons::text(icons::COPY)).on_hover_text("Copy (Ctrl+C)").clicked() {
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
}

/// Properties popup for the selected annotation.
fn annotation_popup(ui: &Ui, tab: &mut DocTab, view: Rect, recent: &mut Vec<Rgb>) {
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
        text_box_editor(ui, tab, &annot, screen);
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
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.strong(annot.kind.label());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button(icons::sized(icons::TRASH, 14.0))
                            .on_hover_text("Delete (Del)")
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
                            ui.label("Fill");
                            if let Some(c) = color_picker::color_button(ui, "annot_fill", annot.interior, true, recent) {
                                change.interior = Some(c);
                            }
                            ui.end_row();
                        }
                        if is_box {
                            ui.label("Text");
                            if let Some(Some(c)) =
                                color_picker::color_button(ui, "annot_text", annot.text_color, false, recent)
                            {
                                change.text_color = Some(c);
                            }
                            ui.end_row();
                        }
                    });
                    if annot.kind.has_border_width() {
                        let mut w = annot.width;
                        let r = ui.add(egui::Slider::new(&mut w, 0.0..=12.0).text("Width"));
                        if r.drag_stopped() || (r.changed() && !r.dragged()) {
                            change.width = Some(w);
                        }
                    }
                }
                if annot.kind == AnnotKind::FreeText {
                    // A text box's "comment" is its visible text, edited on the page.
                    let mut size = annot.font_size.unwrap_or(12.0);
                    let r = ui.add(egui::Slider::new(&mut size, 6.0..=48.0).step_by(1.0).text("Size"));
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        change.font_size = Some(size);
                    }
                    if ui.button("Edit text (double-click the box)").clicked() {
                        start_editing = true;
                    }
                } else {
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut contents)
                            .hint_text("Add a comment…")
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

/// Edits a text box's text in place: an editor drawn over the box, using the same
/// font (egui's proportional font is the one the box is drawn with), size and colours.
/// The text is saved when the editor loses focus (click elsewhere, Esc).
fn text_box_editor(ui: &Ui, tab: &mut DocTab, annot: &AnnotInfo, screen: Rect) {
    let Some(sel) = tab.selected_annot.as_mut() else { return };
    let zoom = tab.viewport.zoom;
    let size = annot.font_size.unwrap_or(12.0) * zoom;
    let pad = crate::engine::textbox::PADDING * zoom;
    let border = annot.width * zoom;
    let fill = annot.interior.map(rgb).unwrap_or(Color32::WHITE);
    let text_color = rgb(annot.text_color.unwrap_or([0, 0, 0]));
    let mut contents = sel.contents.clone();
    let mut lost = false;
    egui::Area::new(Id::unique(("text_box_editor", tab.id)))
        .fixed_pos(screen.min)
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(fill)
                .stroke(Stroke::new(border, rgb(annot.color.unwrap_or([0, 0, 0]))))
                .inner_margin(pad)
                .show(ui, |ui| {
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut contents)
                            .font(egui::FontId::proportional(size))
                            .text_color(text_color)
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::ZERO)
                            .desired_rows(1)
                            .desired_width((screen.width() - 2.0 * pad - 2.0 * border).max(20.0)),
                    );
                    #[cfg(test)]
                    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_text_box_editor"), edit.rect));
                    if sel.focus_contents {
                        edit.request_focus();
                        sel.focus_contents = false;
                    }
                    lost = edit.lost_focus();
                });
        });
    sel.contents = contents;
    if lost {
        // Done editing: save the text, keep the box selected.
        sel.editing = false;
        if sel.contents != sel.committed {
            sel.committed = sel.contents.clone();
            let req = Request::UpdateAnnot {
                doc: tab.id,
                page: sel.page,
                index: sel.index,
                change: AnnotChange {
                    contents: Some(sel.contents.clone()),
                    ..Default::default()
                },
            };
            tab.outbox.push(req);
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
    tab.select_annot(page, index);
    let is_text_box = tab.annot(page, index).is_some_and(|a| a.kind == AnnotKind::FreeText);
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
            head: 3,
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
            matches!(r, Request::UpdateAnnot { index: 7, change, .. } if change.contents.as_deref() == Some("Hallo"))
        });
        assert!(saved, "typed text was not saved");
        let saves = tab.outbox.iter().filter(|r| matches!(r, Request::UpdateAnnot { .. })).count();
        assert_eq!(saves, 1);
    }
}
