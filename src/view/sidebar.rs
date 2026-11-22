//! Left sidebar: page thumbnails, document outline and search results.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::document::DocTab;
use crate::engine::{OutlineItem, RenderKey};
use crate::view::document_view::{focus_annot, format_pdf_date, rgb};
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

pub fn show(ui: &mut Ui, tab: &mut DocTab, sidebar: &mut SidebarTab, cache: &mut TextureCache) {
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
        SidebarTab::Pages => thumbnails(ui, tab, cache),
        SidebarTab::Outline => outline(ui, tab),
        SidebarTab::Comments => comments(ui, tab),
        SidebarTab::Search => search_results(ui, tab),
    }
}

fn thumbnails(ui: &mut Ui, tab: &mut DocTab, cache: &mut TextureCache) {
    let Some(info) = tab.info() else { return };
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
    let output = area.show_rows(ui, row_h, sizes.len(), |ui, rows| {
        for page in rows {
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
            let s = sizes[page];
            let scale = (THUMB_WIDTH / s.x.max(1.0)).min(thumb_h / s.y.max(1.0));
            let thumb_size = s * scale;
            let thumb = Rect::from_center_size(
                pos2(rect.center().x, rect.top() + 2.0 + thumb_h / 2.0),
                thumb_size,
            );
            let current = page == tab.current_page;
            let painter = ui.painter();
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
                tab.goto_page(page);
            }
        }
    });
    ui.ctx().data_mut(|d| {
        d.insert_temp(egui::Id::unique(("thumb_top", tab.id)), output.state.clamped_offset().y)
    });
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
            ui.weak("Use the search box (Ctrl+F) to search the document.");
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
