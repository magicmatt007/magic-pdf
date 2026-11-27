//! Left sidebar: page thumbnails, document outline and search results.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use crate::i18n::{t, tf};

use crate::document::{DocTab, SearchHit};
use crate::engine::{AnnotChange, AnnotInfo, AnnotKind, OutlineItem, RenderKey, Reply, Request, STATUSES, pdf_date_now};
use crate::icons;
#[cfg(test)]
use crate::engine::PageOp;
use crate::view::document_view::{focus_annot, format_pdf_date, rgb};
use crate::keys::kb;
use crate::view::cache::{TextureCache, exact_width, snap_to_pixels};
use crate::view::layout::rotated_size;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SidebarTab {
    #[default]
    Pages,
    Outline,
    Comments,
    Search,
    /// Files attached to the document (only offered when there are some).
    Attachments,
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
    tab_row(ui, tab, sidebar);
    ui.separator();
    match sidebar {
        SidebarTab::Pages => thumbnails(ui, tab, cache, actions, thumb_width),
        SidebarTab::Outline => outline(ui, tab),
        SidebarTab::Comments => comments(ui, tab, author),
        SidebarTab::Search => search_results(ui, tab),
        // Drawn by the app (it needs the e-invoice data).
        SidebarTab::Attachments => {}
    }
}

/// The row choosing what the sidebar shows.
pub fn tab_row(ui: &mut Ui, tab: &DocTab, sidebar: &mut SidebarTab) {
    let attached = tab.info().map_or(0, |i| i.attachments.len());
    if attached == 0 && *sidebar == SidebarTab::Attachments {
        *sidebar = SidebarTab::Pages;
    }
    let n: usize = tab.annots.values().flatten().filter(|a| a.in_reply_to.is_none()).count();
    let comments = if n == 0 { t("Comments").to_string() } else { tf("Comments ({n})", &[("n", &n)]) };
    let search = if tab.search.hits.is_empty() {
        t("Search").to_string()
    } else {
        tf("Search ({n})", &[("n", &tab.search.hits.len())])
    };
    let mut tabs = vec![
        (SidebarTab::Pages, icons::FILES, t("Pages").to_string()),
        (SidebarTab::Outline, icons::LIST_BULLETS, t("Outline").to_string()),
        (SidebarTab::Comments, icons::CHAT_TEXT, comments),
        (SidebarTab::Search, icons::MAGNIFYING_GLASS, search),
    ];
    if attached > 0 {
        tabs.push((SidebarTab::Attachments, icons::PAPERCLIP, tf("Attachments ({n})", &[("n", &attached)])));
    }
    // Wraps only in a sidebar too slim even for the icons.
    ui.horizontal_wrapped(|ui| {
        // Tight padding so the labels fit on one line at the usual sidebar width.
        ui.spacing_mut().button_padding.x = 6.0;
        ui.spacing_mut().item_spacing.x = 2.0;
        let font = egui::TextStyle::Button.resolve(ui.style());
        let pad = 2.0 * ui.spacing().button_padding.x;
        let width_of = |ui: &Ui, text: String| ui.fonts_mut(|f| f.layout_no_wrap(text, font.clone(), Color32::WHITE)).size().x;
        let labels: f32 = tabs.iter().map(|(_, _, label)| width_of(ui, label.clone())).sum();
        let gaps = pad * tabs.len() as f32 + ui.spacing().item_spacing.x * (tabs.len() - 1) as f32;
        let open_label = tabs.iter().find(|(v, _, _)| v == sidebar).map_or(0.0, |(_, _, l)| width_of(ui, format!(" {l}")));
        // Too narrow (longer labels, counts, a slim sidebar): icons, with the label
        // only on the open tab and in the others' tooltips; slimmer still, icons only.
        let compact = labels + gaps > ui.available_width();
        let icons_only = 16.0 * tabs.len() as f32 + gaps + open_label > ui.available_width();
        for (value, icon, label) in tabs {
            let open = *sidebar == value;
            let text: egui::WidgetText = if !compact {
                label.clone().into()
            } else if open && !icons_only {
                let mut job = egui::text::LayoutJob::default();
                let (style, align) = (ui.style(), egui::Align::Center);
                icons::sized(icon, 15.0).append_to(&mut job, style, egui::FontSelection::Default, align);
                egui::RichText::new(format!(" {label}")).append_to(&mut job, style, egui::FontSelection::Default, align);
                job.into()
            } else {
                icons::sized(icon, 15.0).into()
            };
            let resp = ui.selectable_label(open, text);
            let resp = if compact && (icons_only || !open) { resp.on_hover_text(&label) } else { resp };
            if resp.clicked() {
                *sidebar = value;
            }
        }
    });
}

/// A comment's text, cut to `max_rows` lines with an ellipsis when given.
fn comment_text(ui: &mut Ui, text: &str, max_rows: Option<usize>) {
    let Some(max_rows) = max_rows else {
        ui.label(text);
        return;
    };
    let format = egui::TextFormat::simple(egui::TextStyle::Body.resolve(ui.style()), ui.visuals().text_color());
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), format);
    job.wrap = egui::text::TextWrapping {
        max_width: ui.available_width(),
        max_rows,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    ui.label(job);
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
    let pages = if n == 1 { t("page").to_string() } else { tf("{n} pages", &[("n", &n)]) };
    let mut action = None;
    let mut item = |ui: &mut Ui, enabled: bool, label: String, a: PageAction| {
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            action = Some(a);
        }
    };
    item(ui, true, tf("Rotate {pages} clockwise", &[("pages", &pages)]), PageAction::Rotate(1));
    item(ui, true, tf("Rotate {pages} counter-clockwise", &[("pages", &pages)]), PageAction::Rotate(-1));
    ui.separator();
    item(ui, true, tf("Move {pages} to the start", &[("pages", &pages)]), PageAction::MoveToStart);
    item(ui, true, tf("Move {pages} to the end", &[("pages", &pages)]), PageAction::MoveToEnd);
    ui.separator();
    item(ui, true, t("Insert blank page after").into(), PageAction::InsertBlankAfter);
    item(ui, true, t("Insert pages from file…").into(), PageAction::InsertFileAfter);
    ui.separator();
    item(ui, true, tf("Extract {pages} to new file…", &[("pages", &pages)]), PageAction::Extract);
    item(ui, total > 1, t("Split document…").into(), PageAction::Split);
    ui.separator();
    item(ui, n < total, tf("Delete {pages}\tDel", &[("pages", &pages)]), PageAction::Delete);
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
        ui.label(egui::RichText::new(t("Size")).small().weak());
        ui.spacing_mut().slider_width = (ui.available_width() - 8.0).clamp(40.0, 160.0);
        ui.add(egui::Slider::new(thumb_width, THUMB_MIN..=THUMB_MAX).logarithmic(true).show_value(false))
            .on_hover_text(tf("Thumbnail size ({key} + mouse wheel over the thumbnails)", &[("key", &kb("Ctrl"))]));
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
    // The solid accent marks the current page's border and badge.
    let (frame, badge_fill) = (crate::theme::ACCENT, crate::theme::ACCENT_DEEP);
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
            let thumb = snap_to_pixels(thumb, ppp);
            painter.rect_filled(thumb, CornerRadius::ZERO, Color32::WHITE);
            let want = exact_width(thumb.width() * ppp, s.y / s.x.max(1.0));
            let version = tab.page_version(page);
            let key = RenderKey {
                doc: tab.id,
                page,
                width_px: want,
                rotation: tab.rotation,
                version,
                tile: 0,
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
                Stroke::new(2.0, frame)
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
                painter.rect_filled(badge, CornerRadius::same(255), badge_fill);
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

/// Something done to a bookmark in the Outline tab.
enum OutlineAction {
    Goto(usize),
    Rename(Vec<usize>),
    Delete(Vec<usize>),
    Shift(Vec<usize>, isize),
    Indent(Vec<usize>),
    Outdent(Vec<usize>),
    SetPage(Vec<usize>),
    AddAfter(Option<Vec<usize>>),
}

fn outline(ui: &mut Ui, tab: &mut DocTab) {
    use crate::view::outline_edit as oe;
    let Some(info) = tab.info() else { return };
    let items = info.outline.clone();
    let mut action = None;
    ui.horizontal(|ui| {
        if ui
            .button(t("+ Bookmark"))
            .on_hover_text(t("Add a bookmark for the current page (named after the selected text, if any)"))
            .clicked()
        {
            action = Some(OutlineAction::AddAfter(None));
        }
        if !items.is_empty() {
            ui.weak(t("Right-click to edit"));
        }
    });
    if items.is_empty() {
        ui.weak(t("This document has no bookmarks."));
    }
    let mut commit_rename = None;
    egui::ScrollArea::both()
        .id_salt(("outline", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, item) in items.iter().enumerate() {
                outline_item(ui, tab, item, vec![i], &mut action, &mut commit_rename);
            }
        });

    let current = tab.current_page;
    let mut outline = items.clone();
    let mut changed = false;
    if let Some((path, title)) = commit_rename {
        if let Some(it) = oe::get_mut(&mut outline, &path)
            && it.title != title
        {
            it.title = title;
            changed = true;
        }
        tab.outline_rename = None;
    }
    match action {
        Some(OutlineAction::Goto(p)) => tab.goto_page(p),
        Some(OutlineAction::Rename(path)) => {
            let title = oe::get(&outline, &path).map(|i| i.title.clone()).unwrap_or_default();
            tab.outline_rename = Some((path, title));
        }
        Some(OutlineAction::Delete(path)) => changed |= oe::remove(&mut outline, &path).is_some(),
        Some(OutlineAction::Shift(path, by)) => changed |= oe::shift(&mut outline, &path, by).is_some(),
        Some(OutlineAction::Indent(path)) => changed |= oe::indent(&mut outline, &path).is_some(),
        Some(OutlineAction::Outdent(path)) => changed |= oe::outdent(&mut outline, &path).is_some(),
        Some(OutlineAction::SetPage(path)) => {
            if let Some(it) = oe::get_mut(&mut outline, &path) {
                it.page = Some(current);
                changed = true;
            }
        }
        Some(OutlineAction::AddAfter(after)) => {
            let title = tab
                .selected_text()
                .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|t| !t.is_empty())
                .map(|t| t.chars().take(80).collect())
                .unwrap_or_else(|| tf("Page {page}", &[("page", &(current + 1))]));
            let item = OutlineItem {
                title: title.clone(),
                page: Some(current),
                ..Default::default()
            };
            let path = oe::insert_after(&mut outline, after.as_deref(), item);
            // Name it right away.
            tab.outline_rename = Some((path, title));
            changed = true;
        }
        None => {}
    }
    if changed {
        tab.outbox.push(Request::EditPages {
            doc: tab.id,
            op: crate::engine::PageOp::SetOutline { outline },
        });
    }
}

fn outline_item(
    ui: &mut Ui,
    tab: &mut DocTab,
    item: &OutlineItem,
    path: Vec<usize>,
    action: &mut Option<OutlineAction>,
    commit_rename: &mut Option<(Vec<usize>, String)>,
) {
    let title = if item.title.is_empty() { "(untitled)" } else { &item.title };
    let mut label = |ui: &mut Ui| {
        if let Some((p, text)) = &mut tab.outline_rename
            && *p == path
        {
            let r = ui.add(egui::TextEdit::singleline(text).desired_width(ui.available_width().max(80.0)));
            if !r.has_focus() && !r.lost_focus() {
                r.request_focus();
            }
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    tab.outline_rename = None;
                } else {
                    *commit_rename = Some((path.clone(), text.trim().to_string()));
                }
            }
            return;
        }
        let r = ui.selectable_label(false, title);
        if r.clicked()
            && let Some(p) = item.page
        {
            *action = Some(OutlineAction::Goto(p));
        }
        if r.double_clicked() {
            *action = Some(OutlineAction::Rename(path.clone()));
        }
        let r = match item.page {
            Some(p) => r.on_hover_text(tf("Page {page} · double-click to rename", &[("page", &(p + 1))])),
            None => r,
        };
        r.context_menu(|ui| {
            let mut pick = |a: OutlineAction| {
                *action = Some(a);
            };
            if ui.button(t("Rename")).clicked() {
                pick(OutlineAction::Rename(path.clone()));
            }
            if ui.button(t("Add bookmark after")).clicked() {
                pick(OutlineAction::AddAfter(Some(path.clone())));
            }
            if ui.button(t("Point to the current page")).clicked() {
                pick(OutlineAction::SetPage(path.clone()));
            }
            ui.separator();
            if ui.button(t("Move up")).clicked() {
                pick(OutlineAction::Shift(path.clone(), -1));
            }
            if ui.button(t("Move down")).clicked() {
                pick(OutlineAction::Shift(path.clone(), 1));
            }
            if ui.add_enabled(path.last() != Some(&0), egui::Button::new(t("Indent (make sub-bookmark)"))).clicked() {
                pick(OutlineAction::Indent(path.clone()));
            }
            if ui.add_enabled(path.len() > 1, egui::Button::new(t("Outdent"))).clicked() {
                pick(OutlineAction::Outdent(path.clone()));
            }
            ui.separator();
            if ui.button(t("Delete")).clicked() {
                pick(OutlineAction::Delete(path.clone()));
            }
            if action.is_some() {
                ui.close();
            }
        });
    };
    if item.children.is_empty() {
        label(ui);
    } else {
        let id = ui.make_persistent_id(("outline_node", &path));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| label(ui))
            .body(|ui| {
                for (i, child) in item.children.iter().enumerate() {
                    let mut p = path.clone();
                    p.push(i);
                    outline_item(ui, tab, child, p, action, commit_rename);
                }
            });
    }
}

/// A review status in the user's language.
fn status_name(status: &str) -> &str {
    STATUSES.iter().find(|s| **s == status).map_or(status, |s| t(s))
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
            ui.weak(t("No annotations yet. Pick a tool from the toolbar to add one."));
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t("Looking for annotations…"));
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
        ui.label(icons::text(icons::FUNNEL).size(14.0)).on_hover_text(t("Filter the list"));
        ui.add(egui::TextEdit::singleline(&mut f.text).hint_text(t("Search comments")).desired_width(f32::INFINITY));
    });
    ui.horizontal_wrapped(|ui| {
        let any = "All";
        egui::ComboBox::from_id_salt(("filter_kind", tab.id))
            .selected_text(f.kind.map_or(t(any), |k| k.label()))
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.kind, None, t("All types"));
                for k in &kinds {
                    ui.selectable_value(&mut f.kind, Some(*k), k.label());
                }
            });
        egui::ComboBox::from_id_salt(("filter_author", tab.id))
            .selected_text(f.author.as_deref().unwrap_or(t("Anyone")))
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.author, None, t("Anyone"));
                for a in &authors {
                    ui.selectable_value(&mut f.author, Some(a.clone()), a);
                }
            });
        egui::ComboBox::from_id_salt(("filter_status", tab.id))
            .selected_text(match f.status.as_deref() {
                None => t("Any status"),
                Some("") => t("No status"),
                Some(s) => status_name(s),
            })
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.status, None, t("Any status"));
                ui.selectable_value(&mut f.status, Some(String::new()), t("No status"));
                for s in &STATUSES[1..] {
                    ui.selectable_value(&mut f.status, Some(s.to_string()), status_name(s));
                }
            });
        ui.checkbox(&mut f.with_comment, t("With comment"))
            .on_hover_text(t("Only annotations someone wrote a comment on or replied to"));
        if f.active() && ui.small_button(t("Clear")).clicked() {
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
                ui.weak(tf("Page {page}", &[("page", &(page + 1))]));
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
                                    let chip = egui::RichText::new(status_name(&a.status)).small().color(Color32::WHITE);
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
                            // Long texts are cut to a short preview; the selected
                            // comment shows them in full.
                            if !a.contents.is_empty() {
                                comment_text(ui, &a.contents, (!is_selected).then_some(4));
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
                                    comment_text(ui, &r.text, (!is_selected).then_some(2));
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
                                .selected_text(tf("Status: {status}", &[("status", &status_name(status))]))
                                .show_ui(ui, |ui| {
                                    for s in STATUSES {
                                        if ui.selectable_label(status == s, status_name(s)).clicked() && status != s {
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
                                    .hint_text(t("Reply…"))
                                    .desired_rows(1)
                                    .desired_width(ui.available_width() - 60.0),
                            );
                            let enter = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                            let text = tab.reply_draft.trim().to_string();
                            if (ui.add_enabled(!text.is_empty(), egui::Button::new(t("Reply"))).clicked() || enter) && !text.is_empty() {
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
                ui.weak(t("No comments match the filter."));
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

/// A search hit as "…text before **match** text after…".
fn hit_text(ui: &Ui, hit: &SearchHit) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let font = egui::FontId::proportional(12.5);
    let mut job = LayoutJob::default();
    let weak = TextFormat::simple(font.clone(), ui.visuals().weak_text_color());
    job.append(&hit.context.before, 0.0, weak.clone());
    let text = if hit.context.text.is_empty() { "…" } else { hit.context.text.as_str() };
    job.append(
        text,
        0.0,
        TextFormat {
            background: Color32::from_rgba_unmultiplied(255, 200, 0, 110),
            ..TextFormat::simple(font, ui.visuals().strong_text_color())
        },
    );
    job.append(&hit.context.after, 0.0, weak);
    job.wrap.max_width = ui.available_width();
    job
}

/// One search hit in the list; true if clicked.
fn hit_row(ui: &mut Ui, hit: &SearchHit, selected: bool) -> bool {
    let frame = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(4, 3))
        .corner_radius(CornerRadius::same(3))
        .fill(if selected { ui.visuals().selection.bg_fill.gamma_multiply(0.4) } else { Color32::TRANSPARENT });
    frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.add_sized([34.0, 14.0], egui::Label::new(egui::RichText::new(format!("p.{}", hit.page + 1)).small().weak()));
                ui.add(egui::Label::new(hit_text(ui, hit)).wrap());
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn search_status(ui: &mut Ui, s: &crate::document::SearchState) -> bool {
    match (&s.submitted, s.running) {
        (None, _) => {
            ui.weak(kb(t("Use the search box (Ctrl+F) to search the document.")));
            false
        }
        (Some(q), true) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tf("Searching for “{q}”… {n} found", &[("q", q), ("n", &s.hits.len())]));
            });
            true
        }
        (Some(q), false) => {
            ui.label(tf("{n} results for “{q}”", &[("n", &s.hits.len()), ("q", q)]));
            true
        }
    }
}

fn search_results(ui: &mut Ui, tab: &mut DocTab) {
    if !search_status(ui, &tab.search) {
        return;
    }
    ui.separator();
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt(("search", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, hit) in tab.search.hits.iter().enumerate() {
                if hit_row(ui, hit, tab.search.current == Some(i)) {
                    clicked = Some(i);
                }
            }
        });
    if let Some(i) = clicked {
        tab.show_search_hit(i);
    }
}

/// Search results of every open tab, grouped by document.
pub fn search_all_results(ui: &mut Ui, tabs: &mut [DocTab], active: &mut usize) {
    let total: usize = tabs.iter().map(|t| t.search.hits.len()).sum();
    let running = tabs.iter().any(|t| t.search.running);
    let Some(query) = tabs.iter().find_map(|t| t.search.submitted.clone()) else {
        ui.weak(kb(t("Use the search box (Ctrl+F) to search all open tabs.")));
        return;
    };
    ui.horizontal(|ui| {
        if running {
            ui.spinner();
        }
        let files = tabs.iter().filter(|t| !t.search.hits.is_empty()).count();
        ui.label(tf("{total} results for “{query}” in {files} of {count} files", &[("total", &total), ("query", &query), ("files", &files), ("count", &tabs.len())]));
    });
    ui.separator();
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt("search_all")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (ti, tab) in tabs.iter().enumerate() {
                if tab.search.hits.is_empty() {
                    continue;
                }
                let id = ui.make_persistent_id(("search_doc", tab.id));
                egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                    .show_header(ui, |ui| {
                        ui.strong(tab.title());
                        ui.weak(format!("({})", tab.search.hits.len()));
                    })
                    .body(|ui| {
                        for (i, hit) in tab.search.hits.iter().enumerate() {
                            if hit_row(ui, hit, ti == *active && tab.search.current == Some(i)) {
                                clicked = Some((ti, i));
                            }
                        }
                    });
            }
        });
    if let Some((ti, i)) = clicked {
        *active = ti;
        tabs[ti].show_search_hit(i);
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
            ..Default::default()
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
            ..Default::default()
        };
        tab.apply_page_mapping(info, &m);
        assert_eq!(tab.current_page, 2, "stays on the same page, now numbered 3");
        assert_eq!(tab.page_selection.iter().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(tab.page_version(3), 2);
        assert_eq!(tab.page_count(), 4);
    }
}
