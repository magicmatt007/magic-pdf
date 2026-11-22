//! Left sidebar: page thumbnails, document outline and search results.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::document::DocTab;
use crate::engine::{OutlineItem, RenderKey};
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

const THUMB_WIDTH: f32 = 130.0;

pub fn show(
    ui: &mut Ui,
    tab: &mut DocTab,
    sidebar: &mut SidebarTab,
    cache: &mut TextureCache,
    actions: &mut Vec<PageAction>,
) {
    ui.horizontal(|ui| {
        ui.selectable_value(sidebar, SidebarTab::Pages, "Pages");
        ui.selectable_value(sidebar, SidebarTab::Outline, "Outline");
        let n: usize = tab.annots.values().map(Vec::len).sum();
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
        SidebarTab::Pages => thumbnails(ui, tab, cache, actions),
        SidebarTab::Outline => outline(ui, tab),
        SidebarTab::Comments => comments(ui, tab),
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

fn thumbnails(ui: &mut Ui, tab: &mut DocTab, cache: &mut TextureCache, actions: &mut Vec<PageAction>) {
    let Some(info) = tab.info() else { return };
    let count = info.page_sizes.len();
    let sizes: Vec<_> = info
        .page_sizes
        .iter()
        .map(|&s| rotated_size(s, tab.rotation))
        .collect();
    let max_aspect = sizes
        .iter()
        .map(|s| s.y / s.x.max(1.0))
        .fold(0.5_f32, f32::max)
        .min(2.0);
    let thumb_h = THUMB_WIDTH * max_aspect;
    let row_h = thumb_h + 22.0;
    let spacing = ui.spacing().item_spacing.y;
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
    if let Some(page) = tab.thumbs_follow_page.take() {
        let top = ui.ctx().data(|d| d.get_temp::<f32>(egui::Id::unique(("thumb_top", tab.id))));
        let visible_h = ui.available_height();
        let y = page as f32 * (row_h + spacing);
        let out_of_view = top.is_none_or(|t| y < t || y + row_h > t + visible_h);
        if out_of_view {
            area = area.vertical_scroll_offset((y - visible_h / 2.0 + row_h / 2.0).max(0.0));
        }
    }
    let mut rows_drawn: Vec<(usize, Rect)> = Vec::new();
    let mut clip = Rect::NOTHING;
    let output = area.show_rows(ui, row_h, sizes.len(), |ui, rows| {
        clip = ui.clip_rect();
        for page in rows {
            let (rect, response) =
                ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click_and_drag());
            rows_drawn.push((page, rect));
            let s = sizes[page];
            let scale = (THUMB_WIDTH / s.x.max(1.0)).min(thumb_h / s.y.max(1.0));
            let thumb_size = s * scale;
            let thumb = Rect::from_center_size(
                pos2(rect.center().x, rect.top() + 2.0 + thumb_h / 2.0),
                thumb_size,
            );
            let current = page == tab.current_page;
            let selected = tab.page_selection.contains(&page);
            let painter = ui.painter();
            if selected {
                painter.rect_filled(
                    rect.shrink2(vec2(6.0, 0.0)),
                    CornerRadius::same(4),
                    ui.visuals().selection.bg_fill.gamma_multiply(0.35),
                );
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
                Stroke::new(3.0, ui.visuals().selection.bg_fill)
            } else if response.hovered() {
                Stroke::new(1.0, ui.visuals().widgets.hovered.fg_stroke.color)
            } else {
                Stroke::new(1.0, Color32::from_gray(80))
            };
            painter.rect_stroke(thumb, CornerRadius::ZERO, stroke, StrokeKind::Outside);
            painter.text(
                pos2(rect.center().x, thumb.bottom() + 4.0),
                egui::Align2::CENTER_TOP,
                (page + 1).to_string(),
                egui::FontId::proportional(12.0),
                ui.visuals().text_color(),
            );

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
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("test_thumb_rows"), rows_drawn.clone()));

    // Where a drop (dragged pages or files from outside) would land.
    let gap = pointer
        .filter(|p| clip.x_range().contains(p.x) && clip.y_range().contains(p.y))
        .and_then(|p| drop_gap(&rows_drawn, p.y, count));
    let show_gap = |ui: &Ui, gap: usize| {
        let y = rows_drawn
            .iter()
            .find(|(page, _)| *page == gap)
            .map(|(_, r)| r.top() - spacing / 2.0)
            .or_else(|| rows_drawn.last().map(|(_, r)| r.bottom() + spacing / 2.0));
        if let Some(y) = y {
            let stroke = Stroke::new(3.0, ui.visuals().selection.stroke.color);
            ui.painter().with_clip_rect(clip).hline(clip.x_range().shrink(8.0), y, stroke);
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

/// Index of the page a drop at height `y` lands before (or `count` for the end).
fn drop_gap(rows: &[(usize, Rect)], y: f32, count: usize) -> Option<usize> {
    let (first, _) = rows.first()?;
    if let Some((page, _)) = rows.iter().find(|(_, r)| y < r.center().y) {
        return Some(*page);
    }
    let (last, _) = rows.last()?;
    let _ = first;
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

fn comments(ui: &mut Ui, tab: &mut DocTab) {
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
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt(("comments", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for page in pages {
                ui.add_space(4.0);
                ui.weak(format!("Page {}", page + 1));
                for a in &tab.annots[&page] {
                    let selected = tab
                        .selected_annot
                        .as_ref()
                        .is_some_and(|s| s.page == page && s.index == a.index);
                    let frame = egui::Frame::group(ui.style()).fill(if selected {
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
                            });
                            if !a.author.is_empty() {
                                ui.weak(&a.author);
                            }
                            if !a.contents.is_empty() {
                                ui.label(&a.contents);
                            }
                        })
                        .response
                        .interact(Sense::click());
                    if resp.clicked() {
                        clicked = Some((page, a.index));
                    }
                }
            }
        });
    if let Some((page, index)) = clicked {
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
            egui::CentralPanel::default().show(ui, |ui| show(ui, tab, &mut side, &mut cache, &mut actions));
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

    #[test]
    fn dragging_a_thumbnail_moves_the_page() {
        let ctx = egui::Context::default();
        let mut tab = tab(5);
        frame(&ctx, &mut tab, Vec::new(), egui::Modifiers::NONE);
        let r = rows(&ctx);
        let start = r[0].1.center();
        // Drop just below the middle of page 3's thumbnail: lands before page 4.
        let end = r[2].1.center() + vec2(0.0, 20.0);
        let mut actions = Vec::new();
        let none = egui::Modifiers::NONE;
        frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(start)], none);
        frame(&ctx, &mut tab, vec![press(start, true, none)], none);
        for i in 1..=10 {
            let p = start + (end - start) * (i as f32 / 10.0);
            actions.extend(frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(p)], none));
        }
        actions.extend(frame(&ctx, &mut tab, vec![press(end, false, none)], none));
        assert_eq!(actions, vec![PageAction::MoveTo { pages: vec![0], gap: 3 }]);
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
