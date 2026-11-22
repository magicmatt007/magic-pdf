//! Left sidebar: page thumbnails, document outline and search results.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::document::DocTab;
use crate::engine::{AnnotChange, AnnotInfo, AnnotKind, OutlineItem, RenderKey, Reply, Request, STATUSES, pdf_date_now};
use crate::icons;
#[cfg(test)]
use crate::engine::PageOp;
use crate::view::document_view::{focus_annot, format_pdf_date, rgb};
use crate::keys::kb;
use crate::view::cache::{TextureCache, width_bucket};
use crate::view::layout::rotated_size;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SidebarTab {
    #[default]
    Pages,
    Outline,
    Comments,
    Search,
}

/// Narrowest sidebar.
pub const MIN_WIDTH: f32 = 90.0;
/// Range of the preferred thumbnail width.
const THUMB_MIN: f32 = 60.0;
const THUMB_MAX: f32 = 600.0;
/// Horizontal gap between thumbnail columns.
const COL_GAP: f32 = 6.0;
/// Margin around a thumbnail inside its cell (room for the selection background).
const CELL_MARGIN: f32 = 6.0;
/// Height of the page number under a thumbnail.
const LABEL_H: f32 = 24.0;
/// Tallest page shape a thumbnail gets (height / width).
const MAX_ASPECT: f32 = 2.0;

pub fn show(
    ui: &mut Ui,
    tab: &mut DocTab,
    sidebar: &mut SidebarTab,
    cache: &mut TextureCache,
    actions: &mut Vec<PageAction>,
    thumb_width: &mut f32,
    author: &str,
) {
    // Wraps onto several lines in a narrow sidebar instead of keeping it wide.
    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(sidebar, SidebarTab::Pages, "Pages");
        ui.selectable_value(sidebar, SidebarTab::Outline, "Outline");
        let n: usize = tab.annots.values().flatten().filter(|a| a.in_reply_to.is_none()).count();
        let label = if n == 0 { "Comments".to_string() } else { format!("Comments ({n})") };
        ui.selectable_value(sidebar, SidebarTab::Comments, label);
        let label = if tab.search.hits.is_empty() {
            "Search".to_string()
        } else {
            format!("Search ({})", tab.search.hits.len())
        };
        ui.selectable_value(sidebar, SidebarTab::Search, label);
    });
    ui.separator();
    match sidebar {
        SidebarTab::Pages => thumbnails(ui, tab, cache, actions, thumb_width),
        SidebarTab::Outline => outline(ui, tab),
        SidebarTab::Comments => comments(ui, tab, author),
        SidebarTab::Search => search_results(ui, tab),
    }
}

/// Page operations requested from the sidebar or the Pages menu.
#[derive(Clone, Debug, PartialEq)]
pub enum PageAction {
    /// Quarter turns clockwise (negative: counter-clockwise).
    Rotate(i32),
    Delete,
    MoveToStart,
    MoveToEnd,
    /// Move the target pages so they sit before original page `gap` (0..=count).
    MoveTo { pages: Vec<usize>, gap: usize },
    InsertBlankAfter,
    InsertFileAfter,
    /// Insert dropped files before original page `gap`.
    InsertFiles { gap: usize, paths: Vec<std::path::PathBuf> },
    Extract,
    Split,
}

/// Menu entries for page operations (used by the Pages menu and the thumbnail
/// context menu). `n` is the number of pages they would act on.
pub fn page_menu(ui: &mut Ui, n: usize, total: usize) -> Option<PageAction> {
    let pages = if n == 1 { "page".to_string() } else { format!("{n} pages") };
    let mut action = None;
    let mut item = |ui: &mut Ui, enabled: bool, label: String, a: PageAction| {
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            action = Some(a);
        }
    };
    item(ui, true, format!("Rotate {pages} clockwise"), PageAction::Rotate(1));
    item(ui, true, format!("Rotate {pages} counter-clockwise"), PageAction::Rotate(-1));
    ui.separator();
    item(ui, true, format!("Move {pages} to the start"), PageAction::MoveToStart);
    item(ui, true, format!("Move {pages} to the end"), PageAction::MoveToEnd);
    ui.separator();
    item(ui, true, "Insert blank page after".into(), PageAction::InsertBlankAfter);
    item(ui, true, "Insert pages from file…".into(), PageAction::InsertFileAfter);
    ui.separator();
    item(ui, true, format!("Extract {pages} to new file…"), PageAction::Extract);
    item(ui, total > 1, "Split document…".into(), PageAction::Split);
    ui.separator();
    item(ui, n < total, format!("Delete {pages}\tDel"), PageAction::Delete);
    action
}

/// Thumbnail drag state, kept in egui memory between frames.
#[derive(Clone, Default)]
struct ThumbDrag {
    pages: Vec<usize>,
}

/// Grid geometry of the thumbnails: columns, cell width and the top of every row
/// (content coordinates, one extra entry for the total height).
struct Grid {
    cols: usize,
    cell_w: f32,
    row_tops: Vec<f32>,
}

impl Grid {
    fn new(sizes: &[egui::Vec2], width: f32, thumb_width: f32) -> Self {
        let cols = (((width + COL_GAP) / (thumb_width + COL_GAP)).floor() as usize).max(1);
        let cell_w = ((width - COL_GAP * (cols - 1) as f32) / cols as f32).max(1.0);
        let box_w = Self::box_w(cell_w);
        let mut row_tops = vec![0.0];
        let mut y = 0.0;
        for row in sizes.chunks(cols) {
            // Each row is as tall as its tallest page, so landscape pages leave no gaps.
            let h = row.iter().map(|s| Self::fit(*s, box_w).y).fold(0.0, f32::max);
            y += h + 2.0 * CELL_MARGIN + LABEL_H;
            row_tops.push(y);
        }
        Self { cols, cell_w, row_tops }
    }

    fn box_w(cell_w: f32) -> f32 {
        (cell_w - 2.0 * CELL_MARGIN).max(1.0)
    }

    /// Size of a page's thumbnail in a cell of the grid.
    fn fit(page: egui::Vec2, box_w: f32) -> egui::Vec2 {
        page * (box_w / page.x.max(1.0)).min(box_w * MAX_ASPECT / page.y.max(1.0))
    }

    fn cell(&self, origin: egui::Pos2, page: usize) -> Rect {
        let (row, col) = (page / self.cols, page % self.cols);
        let min = origin + vec2(col as f32 * (self.cell_w + COL_GAP), self.row_tops[row]);
        Rect::from_min_size(min, vec2(self.cell_w, self.row_tops[row + 1] - self.row_tops[row]))
    }

    fn rows(&self) -> usize {
        self.row_tops.len() - 1
    }
}

fn thumbnails(
    ui: &mut Ui,
    tab: &mut DocTab,
    cache: &mut TextureCache,
    actions: &mut Vec<PageAction>,
    thumb_width: &mut f32,
) {
    let Some(info) = tab.info() else { return };
    let count = info.page_sizes.len();
    let sizes: Vec<_> = info
        .page_sizes
        .iter()
        .map(|&s| rotated_size(s, tab.rotation))
        .collect();

    // Thumbnail size: slider, or Ctrl+wheel / pinch over the thumbnails.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Size").small().weak());
        ui.spacing_mut().slider_width = (ui.available_width() - 8.0).clamp(40.0, 160.0);
        ui.add(egui::Slider::new(thumb_width, THUMB_MIN..=THUMB_MAX).logarithmic(true).show_value(false))
            .on_hover_text(format!("Thumbnail size ({} + mouse wheel over the thumbnails)", kb("Ctrl")));
    });
    let list_rect = ui.available_rect_before_wrap();
    let zoom = ui.input(|i| {
        let over = i.pointer.hover_pos().is_some_and(|p| list_rect.contains(p));
        if over { i.zoom_delta() } else { 1.0 }
    });
    if zoom != 1.0 {
        *thumb_width = (*thumb_width * zoom).clamp(THUMB_MIN, THUMB_MAX);
    }

    let scrollbar = ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_outer_margin;
    let grid = Grid::new(&sizes, (ui.available_width() - scrollbar).max(1.0), *thumb_width);
    let box_w = Grid::box_w(grid.cell_w);
    let ppp = ui.ctx().pixels_per_point();
    let drag_id = egui::Id::unique(("thumb_drag", tab.id));
    let mut drag: Option<ThumbDrag> = ui.ctx().data(|d| d.get_temp(drag_id));
    let (pointer, primary_down, mods, files_hovering) = ui.input(|i| {
        (
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
            i.modifiers,
            !i.raw.hovered_files.is_empty(),
        )
    });

    let mut area = egui::ScrollArea::vertical()
        .id_salt(("thumbs", tab.id))
        .auto_shrink([false, false]);
    if let Some(page) = tab.thumbs_follow_page.take()
        && page < count
    {
        let top = ui.ctx().data(|d| d.get_temp::<f32>(egui::Id::unique(("thumb_top", tab.id))));
        let visible_h = ui.available_height();
        let row = page / grid.cols;
        let (y, row_h) = (grid.row_tops[row], grid.row_tops[row + 1] - grid.row_tops[row]);
        let out_of_view = top.is_none_or(|t| y < t || y + row_h > t + visible_h);
        if out_of_view {
            area = area.vertical_scroll_offset((y - visible_h / 2.0 + row_h / 2.0).max(0.0));
        }
    }
    let accent = ui.visuals().selection.bg_fill;
    // Stronger shade of the accent for the current page's border and badge.
    let strong = ui.visuals().selection.stroke.color;
    let mut cells: Vec<(usize, Rect)> = Vec::new();
    let mut clip = Rect::NOTHING;
    let output = area.show_viewport(ui, |ui, viewport| {
        clip = ui.clip_rect();
        let total = *grid.row_tops.last().unwrap_or(&0.0);
        let (content, _) = ui.allocate_exact_size(vec2(ui.available_width(), total), Sense::hover());
        let origin = content.min;
        let first_row = grid.row_tops.partition_point(|&t| t <= viewport.min.y).saturating_sub(1);
        let last_row = grid.row_tops.partition_point(|&t| t < viewport.max.y).min(grid.rows());
        let shown = first_row * grid.cols..(last_row * grid.cols).min(count);
        for (page, &s) in sizes.iter().enumerate().take(shown.end).skip(shown.start) {
            let rect = grid.cell(origin, page);
            let response = ui.interact(rect, egui::Id::unique(("thumb", tab.id, page)), Sense::click_and_drag());
            cells.push((page, rect));
            let row_box_h = rect.height() - 2.0 * CELL_MARGIN - LABEL_H;
            let thumb_size = Grid::fit(s, box_w);
            let thumb = Rect::from_center_size(
                pos2(rect.center().x, rect.top() + CELL_MARGIN + row_box_h / 2.0),
                thumb_size,
            );
            let current = page == tab.current_page;
            let selected = tab.page_selection.contains(&page);
            let painter = ui.painter();
            if selected {
                painter.rect_filled(rect, CornerRadius::same(4), accent.gamma_multiply(0.35));
            } else if response.hovered() {
                painter.rect_filled(rect, CornerRadius::same(4), ui.visuals().widgets.hovered.weak_bg_fill);
            }
            painter.rect_filled(thumb, CornerRadius::ZERO, Color32::WHITE);
            let want = width_bucket(thumb.width() * ppp, s.y / s.x.max(1.0));
            let version = tab.page_version(page);
            let key = RenderKey {
                doc: tab.id,
                page,
                width_px: want,
                rotation: tab.rotation,
                version,
            };
            tab.wanted.push((key, 10.0 + (page as f32 - tab.current_page as f32).abs() / 1000.0));
            if let Some((tex, _)) = cache.best(tab.id, page, tab.rotation, want, version) {
                painter.image(
                    tex.id(),
                    thumb,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            let stroke = if current {
                Stroke::new(3.0, strong)
            } else if tab.visible_pages.contains(&page) {
                // Also on screen (e.g. several pages side by side).
                Stroke::new(2.0, accent)
            } else if response.hovered() {
                Stroke::new(1.0, ui.visuals().widgets.hovered.fg_stroke.color)
            } else {
                Stroke::new(1.0, Color32::from_gray(80))
            };
            painter.rect_stroke(thumb, CornerRadius::ZERO, stroke, StrokeKind::Outside);
            // Page number; the current page's is a filled badge.
            let label_center = pos2(rect.center().x, thumb.bottom() + 3.0 + LABEL_H / 2.0);
            let font = egui::FontId::proportional(12.5);
            if current {
                let galley = painter.layout_no_wrap((page + 1).to_string(), font, Color32::WHITE);
                let badge = Rect::from_center_size(label_center, galley.size() + vec2(14.0, 4.0));
                painter.rect_filled(badge, CornerRadius::same(255), strong);
                painter.galley(badge.center() - galley.size() / 2.0, galley, Color32::WHITE);
            } else {
                painter.text(
                    label_center,
                    egui::Align2::CENTER_CENTER,
                    (page + 1).to_string(),
                    font,
                    ui.visuals().text_color(),
                );
            }

            if response.clicked() {
                select_page(tab, page, mods);
            }
            if response.secondary_clicked() && !tab.page_selection.contains(&page) {
                select_page(tab, page, egui::Modifiers::NONE);
            }
            if response.drag_started_by(egui::PointerButton::Primary) {
                if !tab.page_selection.contains(&page) {
                    select_page(tab, page, egui::Modifiers::NONE);
                }
                drag = Some(ThumbDrag {
                    pages: tab.page_selection.iter().copied().collect(),
                });
            }
            let n = tab.target_pages().len();
            response.context_menu(|ui| {
                tab.pages_focused = true;
                if let Some(a) = page_menu(ui, n, count) {
                    actions.push(a);
                    ui.close();
                }
            });
        }
        // Keep scrolling while dragging near the top or bottom edge.
        if ((drag.is_some() && primary_down) || files_hovering)
            && let Some(p) = pointer
        {
            let edge = 32.0;
            if p.y < clip.top() + edge && p.y > clip.top() - edge {
                ui.scroll_with_delta(vec2(0.0, 12.0));
            } else if p.y > clip.bottom() - edge && p.y < clip.bottom() + edge {
                ui.scroll_with_delta(vec2(0.0, -12.0));
            }
            ui.ctx().request_repaint();
        }
    });
    ui.ctx().data_mut(|d| {
        d.insert_temp(egui::Id::unique(("thumb_top", tab.id)), output.state.clamped_offset().y)
    });
    #[cfg(test)]
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("test_thumb_rows"), cells.clone()));

    // Where a drop (dragged pages or files from outside) would land.
    let cols = grid.cols;
    let gap = pointer
        .filter(|p| clip.contains(*p))
        .and_then(|p| drop_gap(&cells, p, cols, count));
    let show_gap = |ui: &Ui, gap: usize| {
        let stroke = Stroke::new(3.0, ui.visuals().selection.stroke.color);
        let painter = ui.painter().with_clip_rect(clip);
        let before = cells.iter().find(|(page, _)| *page == gap).map(|(_, r)| (*r, true));
        let marker = before.or_else(|| cells.iter().find(|(page, _)| *page + 1 == gap).map(|(_, r)| (*r, false)));
        let Some((r, is_before)) = marker else { return };
        if cols == 1 {
            let y = if is_before { r.top() } else { r.bottom() };
            painter.hline(r.x_range().shrink(2.0), y, stroke);
        } else {
            let x = if is_before { r.left() - COL_GAP / 2.0 } else { r.right() + COL_GAP / 2.0 };
            painter.vline(x, r.y_range().shrink(4.0), stroke);
        }
    };

    tab.thumb_drop_gap = if files_hovering { gap } else { None };
    if let Some(g) = tab.thumb_drop_gap {
        show_gap(ui, g);
    }
    match (&drag, primary_down) {
        (Some(d), true) => {
            if let Some(g) = gap
                && crate::engine::move_target(&d.pages, g, count).is_some()
            {
                show_gap(ui, g);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
        }
        (Some(d), false) => {
            if let Some(g) = gap
                && crate::engine::move_target(&d.pages, g, count).is_some()
            {
                actions.push(PageAction::MoveTo {
                    pages: d.pages.clone(),
                    gap: g,
                });
            }
            drag = None;
        }
        _ => {}
    }
    ui.ctx().data_mut(|d| match drag {
        Some(v) => {
            d.insert_temp(drag_id, v);
        }
        None => {
            d.remove::<ThumbDrag>(drag_id);
        }
    });
}

/// Index of the page a drop at `p` lands before (or `count` for the end). In one
/// column the upper or lower half of a thumbnail decides, in a grid the left or
/// right half.
fn drop_gap(cells: &[(usize, Rect)], p: egui::Pos2, cols: usize, count: usize) -> Option<usize> {
    let (last, _) = cells.last()?;
    let before = |r: &Rect| {
        if cols == 1 {
            p.y < r.center().y
        } else {
            p.y < r.top() || (p.y <= r.bottom() && p.x < r.center().x)
        }
    };
    if let Some((page, _)) = cells.iter().find(|(_, r)| before(r)) {
        return Some(*page);
    }
    Some((last + 1).min(count))
}

/// Click on a thumbnail: plain selects one page and shows it, Ctrl toggles, Shift
/// selects a range from the last clicked page.
fn select_page(tab: &mut DocTab, page: usize, mods: egui::Modifiers) {
    tab.pages_focused = true;
    if mods.command || mods.ctrl {
        if !tab.page_selection.remove(&page) {
            tab.page_selection.insert(page);
        }
        tab.page_anchor = Some(page);
    } else if mods.shift {
        let anchor = tab.page_anchor.unwrap_or(tab.current_page);
        let (a, b) = (anchor.min(page), anchor.max(page));
        tab.page_selection = (a..=b).collect();
    } else {
        tab.page_selection = std::iter::once(page).collect();
        tab.page_anchor = Some(page);
        tab.goto_page(page);
    }
}

fn outline(ui: &mut Ui, tab: &mut DocTab) {
    let Some(info) = tab.info() else { return };
    if info.outline.is_empty() {
        ui.weak("This document has no outline.");
        return;
    }
    let items = info.outline.clone();
    let mut goto = None;
    egui::ScrollArea::both()
        .id_salt(("outline", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, item) in items.iter().enumerate() {
                outline_item(ui, item, &mut goto, i, 0);
            }
        });
    if let Some(page) = goto {
        tab.goto_page(page);
    }
}

fn outline_item(ui: &mut Ui, item: &OutlineItem, goto: &mut Option<usize>, index: usize, depth: usize) {
    let title = if item.title.is_empty() { "(untitled)" } else { &item.title };
    if item.children.is_empty() {
        let r = ui.selectable_label(false, title);
        if r.clicked() {
            *goto = item.page;
        }
        if let Some(p) = item.page {
            r.on_hover_text(format!("Page {}", p + 1));
        }
    } else {
        let id = ui.make_persistent_id(("outline_node", depth, index, &item.title));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                if ui.selectable_label(false, title).clicked() {
                    *goto = item.page;
                }
            })
            .body(|ui| {
                for (i, child) in item.children.iter().enumerate() {
                    outline_item(ui, child, goto, i, depth + 1);
                }
            });
    }
}

/// Colour of a review status chip.
fn status_color(status: &str) -> Color32 {
    match status {
        "Accepted" | "Completed" => Color32::from_rgb(30, 140, 70),
        "Rejected" => Color32::from_rgb(200, 40, 40),
        _ => Color32::GRAY,
    }
}

fn comments(ui: &mut Ui, tab: &mut DocTab, author: &str) {
    let mut pages: Vec<usize> = tab.annots.iter().filter(|(_, v)| !v.is_empty()).map(|(p, _)| *p).collect();
    pages.sort_unstable();
    if pages.is_empty() {
        if tab.annots_scanned {
            ui.weak("No annotations yet. Pick a tool from the toolbar to add one.");
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Looking for annotations…");
            });
        }
        return;
    }
    // Filters.
    let all = || tab.annots.values().flatten().filter(|a| a.in_reply_to.is_none());
    let mut kinds: Vec<AnnotKind> = Vec::new();
    let mut authors: Vec<String> = Vec::new();
    for a in all() {
        if !kinds.contains(&a.kind) {
            kinds.push(a.kind);
        }
        if !a.author.is_empty() && !authors.contains(&a.author) {
            authors.push(a.author.clone());
        }
    }
    authors.sort();
    let f = &mut tab.comment_filter;
    ui.horizontal(|ui| {
        ui.label(icons::text(icons::FUNNEL).size(14.0)).on_hover_text("Filter the list");
        ui.add(egui::TextEdit::singleline(&mut f.text).hint_text("Search comments").desired_width(f32::INFINITY));
    });
    ui.horizontal_wrapped(|ui| {
        let any = "All";
        egui::ComboBox::from_id_salt(("filter_kind", tab.id))
            .selected_text(f.kind.map_or(any, |k| k.label()))
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.kind, None, "All types");
                for k in &kinds {
                    ui.selectable_value(&mut f.kind, Some(*k), k.label());
                }
            });
        egui::ComboBox::from_id_salt(("filter_author", tab.id))
            .selected_text(f.author.as_deref().unwrap_or("Anyone"))
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.author, None, "Anyone");
                for a in &authors {
                    ui.selectable_value(&mut f.author, Some(a.clone()), a);
                }
            });
        egui::ComboBox::from_id_salt(("filter_status", tab.id))
            .selected_text(match f.status.as_deref() {
                None => "Any status",
                Some("") => "No status",
                Some(s) => s,
            })
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.status, None, "Any status");
                ui.selectable_value(&mut f.status, Some(String::new()), "No status");
                for s in &STATUSES[1..] {
                    ui.selectable_value(&mut f.status, Some(s.to_string()), *s);
                }
            });
        if f.active() && ui.small_button("Clear").clicked() {
            *f = Default::default();
        }
    });
    ui.separator();

    let filter = tab.comment_filter.clone();
    let mut clicked = None;
    let mut update: Option<(usize, usize, AnnotChange)> = None;
    let selected = tab.selected_annot.as_ref().map(|s| (s.page, s.index));
    let mut shown = 0;
    egui::ScrollArea::vertical()
        .id_salt(("comments", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for page in pages {
                let list: Vec<&AnnotInfo> = tab.annots[&page]
                    .iter()
                    .filter(|a| a.in_reply_to.is_none() && filter.matches(a))
                    .collect();
                if list.is_empty() {
                    continue;
                }
                ui.add_space(4.0);
                ui.weak(format!("Page {}", page + 1));
                for a in list {
                    shown += 1;
                    let is_selected = selected == Some((page, a.index));
                    let frame = egui::Frame::group(ui.style()).fill(if is_selected {
                        ui.visuals().selection.bg_fill.gamma_multiply(0.4)
                    } else {
                        Color32::TRANSPARENT
                    });
                    let resp = frame
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                                ui.painter().rect_filled(
                                    r,
                                    CornerRadius::same(2),
                                    a.color.map(rgb).unwrap_or(Color32::GRAY),
                                );
                                ui.strong(a.kind.label());
                                ui.weak(format_pdf_date(&a.modified));
                                if !a.status.is_empty() && a.status != "None" {
                                    let chip = egui::RichText::new(&a.status).small().color(Color32::WHITE);
                                    egui::Frame::new()
                                        .fill(status_color(&a.status))
                                        .corner_radius(CornerRadius::same(6))
                                        .inner_margin(egui::Margin::symmetric(5, 1))
                                        .show(ui, |ui| ui.label(chip));
                                }
                            });
                            if !a.author.is_empty() {
                                ui.weak(&a.author);
                            }
                            if !a.contents.is_empty() {
                                ui.label(&a.contents);
                            }
                            for r in a.other_replies.iter().chain(&a.replies) {
                                ui.horizontal_wrapped(|ui| {
                                    ui.add_space(10.0);
                                    ui.weak("↳");
                                    ui.strong(if r.author.is_empty() { "?" } else { r.author.as_str() });
                                    ui.weak(format_pdf_date(&r.date));
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.add_space(22.0);
                                    ui.label(&r.text);
                                });
                            }
                        })
                        .response
                        .interact(Sense::click());
                    if resp.clicked() {
                        clicked = Some((page, a.index));
                    }
                    if is_selected {
                        // Reply and status for the selected comment.
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let status = if a.status.is_empty() { "None" } else { a.status.as_str() };
                            egui::ComboBox::from_id_salt(("status", tab.id))
                                .selected_text(format!("Status: {status}"))
                                .show_ui(ui, |ui| {
                                    for s in STATUSES {
                                        if ui.selectable_label(status == s, s).clicked() && status != s {
                                            let change = AnnotChange {
                                                status: Some(if s == "None" { String::new() } else { s.to_string() }),
                                                ..Default::default()
                                            };
                                            update = Some((page, a.index, change));
                                        }
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let edit = ui.add(
                                egui::TextEdit::multiline(&mut tab.reply_draft)
                                    .hint_text("Reply…")
                                    .desired_rows(1)
                                    .desired_width(ui.available_width() - 60.0),
                            );
                            let enter = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                            let text = tab.reply_draft.trim().to_string();
                            if (ui.add_enabled(!text.is_empty(), egui::Button::new("Reply")).clicked() || enter) && !text.is_empty() {
                                let mut replies = a.replies.clone();
                                replies.push(Reply {
                                    author: author.to_string(),
                                    date: pdf_date_now(),
                                    text,
                                });
                                update = Some((
                                    page,
                                    a.index,
                                    AnnotChange {
                                        replies: Some(replies),
                                        ..Default::default()
                                    },
                                ));
                            }
                        });
                    }
                }
            }
            if shown == 0 {
                ui.weak("No comments match the filter.");
            }
        });
    if let Some((page, index, change)) = update {
        if change.replies.is_some() {
            tab.reply_draft.clear();
        }
        tab.outbox.push(Request::UpdateAnnot {
            doc: tab.id,
            page,
            index,
            change,
        });
    }
    if let Some((page, index)) = clicked {
        if selected != Some((page, index)) {
            tab.reply_draft.clear();
        }
        focus_annot(tab, page, index, false);
    }
}

fn search_results(ui: &mut Ui, tab: &mut DocTab) {
    let s = &tab.search;
    match (&s.submitted, s.running) {
        (None, _) => {
            ui.weak(kb("Use the search box (Ctrl+F) to search the document."));
            return;
        }
        (Some(q), true) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Searching for “{q}”… {} found", s.hits.len()));
            });
        }
        (Some(q), false) => {
            ui.label(format!("{} results for “{q}”", s.hits.len()));
        }
    }
    ui.separator();
    let mut clicked = None;
    let n = tab.search.hits.len();
    egui::ScrollArea::vertical()
        .id_salt(("search", tab.id))
        .auto_shrink([false, false])
        .show_rows(ui, 18.0, n, |ui, rows| {
            for i in rows {
                let hit = &tab.search.hits[i];
                let selected = tab.search.current == Some(i);
                if ui
                    .selectable_label(selected, format!("Page {} — match {}", hit.page + 1, i + 1))
                    .clicked()
                {
                    clicked = Some(i);
                }
            }
        });
    if let Some(i) = clicked {
        tab.show_search_hit(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::LoadState;
    use crate::engine::DocInfo;
    use crate::view::layout::LayoutMode;
    use std::path::PathBuf;

    fn tab(pages: usize) -> DocTab {
        let mut tab = DocTab::new(1, PathBuf::from("t.pdf"), LayoutMode::Horizontal, None);
        tab.state = LoadState::Ready(DocInfo {
            page_sizes: vec![(595.0, 842.0); pages],
            outline: Vec::new(),
            title: None,
        });
        tab
    }

    fn frame(ctx: &egui::Context, tab: &mut DocTab, events: Vec<egui::Event>, mods: egui::Modifiers) -> Vec<PageAction> {
        frame_with(ctx, tab, events, mods, &mut 150.0)
    }

    fn frame_with(
        ctx: &egui::Context,
        tab: &mut DocTab,
        events: Vec<egui::Event>,
        mods: egui::Modifiers,
        thumb_width: &mut f32,
    ) -> Vec<PageAction> {
        let mut cache = TextureCache::new(usize::MAX);
        let mut actions = Vec::new();
        let mut side = SidebarTab::Pages;
        let mut all = vec![egui::Event::ModifiersChanged(mods)];
        all.extend(events);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 1400.0))),
            events: all,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| show(ui, tab, &mut side, &mut cache, &mut actions, thumb_width, "Tester"));
        });
        out.textures_delta.clear();
        actions
    }

    fn rows(ctx: &egui::Context) -> Vec<(usize, Rect)> {
        ctx.data(|d| d.get_temp(egui::Id::unique("test_thumb_rows"))).unwrap()
    }

    fn press(pos: egui::Pos2, pressed: bool, mods: egui::Modifiers) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: mods,
        }
    }

    fn click(ctx: &egui::Context, tab: &mut DocTab, at: egui::Pos2, mods: egui::Modifiers) {
        frame(ctx, tab, vec![egui::Event::PointerMoved(at)], mods);
        frame(ctx, tab, vec![press(at, true, mods)], mods);
        frame(ctx, tab, vec![press(at, false, mods)], mods);
    }

    #[test]
    fn click_ctrl_click_and_shift_click_select_pages() {
        let ctx = egui::Context::default();
        let mut tab = tab(5);
        frame(&ctx, &mut tab, Vec::new(), egui::Modifiers::NONE);
        let r = rows(&ctx);
        click(&ctx, &mut tab, r[1].1.center(), egui::Modifiers::NONE);
        assert_eq!(tab.page_selection.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(tab.pending_goto, Some(1));
        click(&ctx, &mut tab, r[3].1.center(), egui::Modifiers::CTRL);
        assert_eq!(tab.page_selection.iter().copied().collect::<Vec<_>>(), vec![1, 3]);
        click(&ctx, &mut tab, r[0].1.center(), egui::Modifiers::SHIFT);
        assert_eq!(tab.page_selection.iter().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert!(tab.pages_focused);
    }

    fn drag(ctx: &egui::Context, tab: &mut DocTab, start: egui::Pos2, end: egui::Pos2, width: f32) -> Vec<PageAction> {
        let mut actions = Vec::new();
        let none = egui::Modifiers::NONE;
        let mut w = width;
        frame_with(ctx, tab, vec![egui::Event::PointerMoved(start)], none, &mut w);
        frame_with(ctx, tab, vec![press(start, true, none)], none, &mut w);
        for i in 1..=10 {
            let p = start + (end - start) * (i as f32 / 10.0);
            actions.extend(frame_with(ctx, tab, vec![egui::Event::PointerMoved(p)], none, &mut w));
        }
        actions.extend(frame_with(ctx, tab, vec![press(end, false, none)], none, &mut w));
        actions
    }

    #[test]
    fn dragging_a_thumbnail_moves_the_page() {
        // One column: the lower half of page 2's thumbnail lands before page 3.
        let ctx = egui::Context::default();
        let mut tab = tab(5);
        frame_with(&ctx, &mut tab, Vec::new(), egui::Modifiers::NONE, &mut 400.0);
        let r = rows(&ctx);
        assert_eq!(r[0].1.left(), r[1].1.left(), "one column");
        let actions = drag(&ctx, &mut tab, r[0].1.center(), r[1].1.center() + vec2(0.0, 20.0), 400.0);
        assert_eq!(actions, vec![PageAction::MoveTo { pages: vec![0], gap: 2 }]);
    }

    #[test]
    fn dragging_in_the_grid_uses_left_and_right_halves() {
        let ctx = egui::Context::default();
        let mut tab = tab(6);
        frame_with(&ctx, &mut tab, Vec::new(), egui::Modifiers::NONE, &mut 100.0);
        let r = rows(&ctx);
        assert_eq!(r[0].1.top(), r[1].1.top(), "several columns");
        // Right half of page 2 (second column): lands before page 3.
        let actions = drag(&ctx, &mut tab, r[4].1.center(), r[1].1.center() + vec2(20.0, 0.0), 100.0);
        assert_eq!(actions, vec![PageAction::MoveTo { pages: vec![4], gap: 2 }]);
    }

    #[test]
    fn rows_fit_their_pages_and_thumbnails_fill_the_width() {
        let ctx = egui::Context::default();
        let mut tab = tab(3);
        if let LoadState::Ready(info) = &mut tab.state {
            info.page_sizes[1] = (842.0, 595.0); // landscape
        }
        frame_with(&ctx, &mut tab, Vec::new(), egui::Modifiers::NONE, &mut 400.0);
        let r = rows(&ctx);
        assert!(r[1].1.height() < r[0].1.height() * 0.8, "landscape row is shorter: {r:?}");
        assert!(r[0].1.width() > 300.0, "one wide column fills the sidebar");
    }

    #[test]
    fn ctrl_wheel_over_the_thumbnails_resizes_them() {
        let ctx = egui::Context::default();
        let mut tab = tab(3);
        let mut w = 150.0;
        frame_with(&ctx, &mut tab, Vec::new(), egui::Modifiers::NONE, &mut w);
        let at = rows(&ctx)[0].1.center();
        let events = vec![egui::Event::PointerMoved(at), egui::Event::Zoom(1.5)];
        frame_with(&ctx, &mut tab, events, egui::Modifiers::CTRL, &mut w);
        assert!((w - 225.0).abs() < 0.1, "{w}");
    }

    #[test]
    fn sidebar_can_be_narrow() {
        let ctx = egui::Context::default();
        let mut tab = tab(3);
        let mut width = 0.0;
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 800.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let (mut cache, mut actions, mut side) = (TextureCache::new(usize::MAX), Vec::new(), SidebarTab::Pages);
                let panel = egui::Panel::left("sidebar")
                    .resizable(true)
                    .default_size(MIN_WIDTH)
                    .size_range(MIN_WIDTH..=900.0)
                    .show(ui, |ui| show(ui, &mut tab, &mut side, &mut cache, &mut actions, &mut 150.0, ""));
                width = panel.response.rect.width();
            });
            out.textures_delta.clear();
        }
        assert!(width < MIN_WIDTH + 20.0, "content keeps the sidebar {width} wide");
    }

    #[test]
    fn tab_state_follows_deleted_pages() {
        let mut tab = tab(5);
        tab.current_page = 3;
        tab.page_selection = [1, 4].into_iter().collect();
        tab.page_versions.insert(4, 2);
        let m = PageOp::Delete { pages: vec![1] }.mapping(5);
        let info = DocInfo {
            page_sizes: vec![(595.0, 842.0); 4],
            outline: Vec::new(),
            title: None,
        };
        tab.apply_page_mapping(info, &m);
        assert_eq!(tab.current_page, 2, "stays on the same page, now numbered 3");
        assert_eq!(tab.page_selection.iter().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(tab.page_version(3), 2);
        assert_eq!(tab.page_count(), 4);
    }
}
