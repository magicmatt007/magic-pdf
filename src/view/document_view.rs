//! The central document widget: draws pages and handles scrolling, zooming, text
//! selection and links.

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Pos2, Rect, Sense, Shadow, Stroke, StrokeKind, Ui, Vec2,
    pos2, vec2,
};

use crate::document::{DocTab, LoadState, Selection};
use crate::engine::{LinkTarget, PdfBox, RenderKey};
use crate::view::cache::{TextureCache, width_bucket};
use crate::view::layout::{GAP, LayoutMode, local_point_to_pdf, pdf_box_to_local};
use crate::view::viewport::{FitMode, Viewport};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Hand,
    SelectText,
}

pub struct ViewContext<'a> {
    pub cache: &'a mut TextureCache,
    pub tool: Tool,
    pub night: bool,
    /// Pages whose text the view wants loaded.
    pub text_requests: &'a mut Vec<usize>,
}

const SEARCH_COLOR: Color32 = Color32::from_rgba_premultiplied(120, 100, 0, 90);
const SEARCH_CURRENT_COLOR: Color32 = Color32::from_rgba_premultiplied(160, 60, 0, 130);
const SELECTION_COLOR: Color32 = Color32::from_rgba_premultiplied(30, 90, 200, 90);

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

    paint_pages(ui, tab, view, cx);
}

fn centered_text(ui: &Ui, view: Rect, text: &str) {
    ui.painter().text(
        view.center(),
        egui::Align2::CENTER_CENTER,
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

    // Dragging: middle button always pans; primary pans with the hand tool.
    let (middle_down, primary_down) = ui.input(|i| {
        (
            i.pointer.button_down(egui::PointerButton::Middle),
            i.pointer.primary_down(),
        )
    });
    let panning = response.dragged() && (middle_down || cx.tool == Tool::Hand);
    if panning {
        tab.viewport.offset -= response.drag_delta() / tab.viewport.zoom;
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }

    // Pointer interaction with page content.
    let Some(pointer) = pointer.filter(|p| view.contains(*p)) else { return };
    let doc_pt = tab.viewport.to_doc(pointer, view);
    let Some(page) = tab.layout.page_at(doc_pt).or_else(|| {
        // While selecting, keep tracking the page the selection started on.
        tab.selection.filter(|_| response.dragged()).map(|s| s.page)
    }) else {
        if cx.tool == Tool::Hand && !panning {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if response.clicked() {
            tab.selection = None;
        }
        return;
    };
    let page_rect = tab.layout.page_rect(page).unwrap_or(Rect::NOTHING);
    let local = (doc_pt - page_rect.min).to_pos2();
    let pdf_pt = local_point_to_pdf(local, tab.page_size(page), tab.rotation);

    let text = match tab.texts.get(&page) {
        Some(Some(t)) => Some(t),
        Some(None) => None,
        None => {
            cx.text_requests.push(page);
            None
        }
    };

    // Links.
    let link = text.and_then(|t| {
        t.links
            .iter()
            .find(|l| contains(&l.rect, pdf_pt, 0.0))
            .map(|l| l.target.clone())
    });
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

    if cx.tool == Tool::Hand {
        if link.is_none() && !panning {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if response.clicked() {
            tab.selection = None;
        }
        return;
    }

    // Text selection.
    let Some(text) = text else { return };
    let over_text = text
        .chars
        .iter()
        .any(|c| c.rect.is_some_and(|r| contains(&r, pdf_pt, 1.0)));
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
        }
    } else if response.dragged_by(egui::PointerButton::Primary) && primary_down {
        if let Some(sel) = &mut tab.selection
            && sel.page == page
            && let Some(i) = nearest_char(text, pdf_pt)
        {
            sel.head = i;
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
        }
    } else if response.clicked() {
        tab.selection = None;
    }
}

fn contains(b: &PdfBox, p: Pos2, slack: f32) -> bool {
    p.x >= b.left - slack && p.x <= b.right + slack && p.y >= b.bottom - slack && p.y <= b.top + slack
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
            if horizontal {
                // Keep the vertical position; the clamp centres pages that fit.
            } else if tab.mode == LayoutMode::SinglePage {
                tab.viewport.offset.x = rect.min.x - margin;
            }
        }
    }
    if let Some((page, pdf_rect)) = tab.pending_reveal.take() {
        tab.relayout(view.size());
        if let Some(page_rect) = tab.layout.page_rect(page) {
            let local = pdf_box_to_local(&pdf_rect, tab.page_size(page), tab.rotation);
            let target = local.translate(page_rect.min.to_vec2()).expand(20.0);
            tab.viewport.reveal(target, view);
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

fn paint_pages(ui: &Ui, tab: &mut DocTab, view: Rect, cx: ViewContext<'_>) {
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
        let key = RenderKey {
            doc: tab.id,
            page,
            width_px: want_w,
            rotation: tab.rotation,
        };
        let is_visible = rect.intersects(visible);
        let distance = (rect.center() - center).length() / visible.size().length().max(1.0);
        tab.wanted
            .push((key, if is_visible { distance } else { 2.0 + distance }));

        if !is_visible {
            continue;
        }
        if !tab.texts.contains_key(&page) && cx.tool == Tool::SelectText {
            cx.text_requests.push(page);
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

        match cx.cache.best(tab.id, page, tab.rotation, want_w) {
            Some(tex) => {
                if tex.size()[0] as u32 != want_w {
                    // A placeholder is shown; make sure a cheap preview arrives quickly too.
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

        paint_overlays(&painter, tab, page, rect, view);
    }
}

fn paint_overlays(painter: &egui::Painter, tab: &DocTab, page: usize, page_rect: Rect, view: Rect) {
    let page_size = tab.page_size(page);
    let to_screen = |b: &PdfBox| {
        let local = pdf_box_to_local(b, page_size, tab.rotation);
        tab.viewport
            .rect_to_screen(local.translate(page_rect.min.to_vec2()), view)
    };

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
        && let Some(Some(text)) = tab.texts.get(&page)
    {
        // Merge consecutive character boxes on the same line into one rectangle.
        let mut lines: Vec<Rect> = Vec::new();
        for c in text.chars.get(sel.range()).unwrap_or(&[]) {
            let Some(r) = &c.rect else { continue };
            let s = to_screen(r);
            match lines.last_mut() {
                Some(last)
                    if (last.center().y - s.center().y).abs() < last.height().max(s.height()) * 0.5
                        && s.left() >= last.left() - 2.0 =>
                {
                    *last = last.union(s);
                }
                _ => lines.push(s),
            }
        }
        for r in lines {
            painter.rect_filled(r, CornerRadius::ZERO, SELECTION_COLOR);
        }
    }
}
