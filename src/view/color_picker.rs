//! Colour picker: a swatch button opening a palette of 70 colours, recent colours,
//! a custom colour (colour square plus hex input) and optionally "No fill".

use std::hash::Hash;
use crate::i18n::t;

use eframe::egui::{self, Color32, CornerRadius, Id, Sense, Stroke, StrokeKind, Ui, vec2};

use crate::engine::Rgb;

pub const MAX_RECENT: usize = 10;
const CELL: f32 = 18.0;

fn rgb(c: Rgb) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// HSL (h in degrees, s and l in 0..=1) to RGB.
fn hsl(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// Rows of the palette: a grey ramp, then 10 hues from very light to very dark.
pub fn palette() -> Vec<Vec<Rgb>> {
    let hues = [0.0, 25.0, 45.0, 60.0, 100.0, 150.0, 185.0, 215.0, 265.0, 310.0];
    let mut rows = vec![(0..10).map(|i| [((9 - i) as f32 * 255.0 / 9.0).round() as u8; 3]).collect()];
    for l in [0.92, 0.8, 0.65, 0.5, 0.35, 0.22] {
        rows.push(hues.iter().map(|&h| hsl(h, 0.85, l)).collect());
    }
    rows
}

/// Remembers `c` as the most recent colour.
pub fn push_recent(recent: &mut Vec<Rgb>, c: Rgb) {
    recent.retain(|r| *r != c);
    recent.insert(0, c);
    recent.truncate(MAX_RECENT);
}

pub fn to_hex(c: Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

pub fn from_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    let full: String = match s.len() {
        3 => s.chars().flat_map(|c| [c, c]).collect(),
        6 => s.to_string(),
        _ => return None,
    };
    let v = u32::from_str_radix(&full, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// One colour cell; `None` draws "no colour" (white with a red slash).
fn cell(ui: &mut Ui, c: Option<Rgb>, selected: bool, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let inner = rect.shrink(1.0);
    match c {
        Some(c) => {
            ui.painter().rect_filled(inner, CornerRadius::same(3), rgb(c));
            // Keep light colours visible against light backgrounds.
            ui.painter().rect_stroke(
                inner,
                CornerRadius::same(3),
                Stroke::new(1.0, Color32::from_black_alpha(40)),
                StrokeKind::Inside,
            );
        }
        None => {
            ui.painter().rect_filled(inner, CornerRadius::same(3), Color32::WHITE);
            ui.painter().line_segment(
                [inner.left_bottom(), inner.right_top()],
                Stroke::new(2.0, Color32::from_rgb(220, 30, 30)),
            );
            ui.painter().rect_stroke(
                inner,
                CornerRadius::same(3),
                Stroke::new(1.0, Color32::from_gray(150)),
                StrokeKind::Inside,
            );
        }
    }
    if selected || resp.hovered() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(2.0, ui.visuals().strong_text_color()),
            StrokeKind::Inside,
        );
    }
    resp
}

/// A swatch showing `current`; clicking it opens the picker. Returns the user's
/// choice (`Some(None)` = "No fill", only offered if `allow_none`).
pub fn color_button(
    ui: &mut Ui,
    id_salt: impl Hash + std::fmt::Debug,
    current: Option<Rgb>,
    allow_none: bool,
    recent: &mut Vec<Rgb>,
) -> Option<Option<Rgb>> {
    let id = ui.make_persistent_id(("color_picker", id_salt));
    let button = cell(ui, current, false, 22.0).on_hover_text(match current {
        Some(c) => format!("{} — click to change", to_hex(c)),
        None => "None — click to choose a colour".into(),
    });
    #[cfg(test)]
    ui.ctx().data_mut(|d| d.insert_temp(Id::unique("test_color_button"), button.rect));
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&button)
        .id(id)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            picked = picker(ui, id, current, allow_none, recent);
            if picked.is_some() && !ui.ctx().input(|i| i.pointer.any_down()) {
                ui.close();
            }
        });
    if let Some(Some(c)) = picked {
        push_recent(recent, c);
    }
    picked
}

/// The picker contents (palette, recent, custom). Returns a choice when made.
fn picker(ui: &mut Ui, id: Id, current: Option<Rgb>, allow_none: bool, recent: &[Rgb]) -> Option<Option<Rgb>> {
    let mut picked = None;
    ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
    if allow_none {
        ui.horizontal(|ui| {
            if cell(ui, None, current.is_none(), CELL).clicked() {
                picked = Some(None);
            }
            ui.label(t("No fill"));
        });
        ui.add_space(4.0);
    }
    for (r, row) in palette().iter().enumerate() {
        ui.horizontal(|ui| {
            for (k, &c) in row.iter().enumerate() {
                let resp = cell(ui, Some(c), current == Some(c), CELL).on_hover_text(to_hex(c));
                #[cfg(test)]
                ui.ctx().data_mut(|d| d.insert_temp(Id::unique(("test_palette_cell", r, k)), resp.rect));
                let _ = (r, k);
                if resp.clicked() {
                    picked = Some(Some(c));
                }
            }
        });
    }
    if !recent.is_empty() {
        ui.add_space(6.0);
        ui.weak(t("Recent"));
        ui.horizontal(|ui| {
            for &c in recent {
                if cell(ui, Some(c), current == Some(c), CELL).on_hover_text(to_hex(c)).clicked() {
                    picked = Some(Some(c));
                }
            }
        });
    }
    ui.add_space(6.0);
    ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
    egui::CollapsingHeader::new(t("Custom colour"))
        .id_salt(id.with("custom"))
        .show(ui, |ui| {
            let work_id = id.with("work");
            let hex_id = id.with("hex");
            let mut work: Color32 = ui
                .ctx()
                .data(|d| d.get_temp(work_id))
                .unwrap_or_else(|| rgb(current.unwrap_or([0, 0, 0])));
            if egui::color_picker::color_picker_color32(ui, &mut work, egui::color_picker::Alpha::Opaque) {
                ui.ctx().data_mut(|d| {
                    d.insert_temp(work_id, work);
                    d.insert_temp(hex_id, to_hex([work.r(), work.g(), work.b()]));
                });
            }
            ui.horizontal(|ui| {
                let mut hex: String = ui
                    .ctx()
                    .data(|d| d.get_temp(hex_id))
                    .unwrap_or_else(|| to_hex([work.r(), work.g(), work.b()]));
                let edit = ui.add(egui::TextEdit::singleline(&mut hex).desired_width(80.0));
                if edit.changed()
                    && let Some(c) = from_hex(&hex)
                {
                    work = rgb(c);
                    ui.ctx().data_mut(|d| d.insert_temp(work_id, work));
                }
                ui.ctx().data_mut(|d| d.insert_temp(hex_id, hex));
                let (r, _) = ui.allocate_exact_size(vec2(22.0, 18.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(3), work);
                if ui.button(t("Use")).clicked() {
                    picked = Some(Some([work.r(), work.g(), work.b()]));
                }
            });
        });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Rect;

    #[test]
    fn palette_has_seventy_distinct_colours() {
        let p = palette();
        assert_eq!(p.len(), 7);
        assert!(p.iter().all(|r| r.len() == 10));
        let mut all: Vec<Rgb> = p.concat();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 70);
        assert_eq!(p[0][0], [255, 255, 255]);
        assert_eq!(p[0][9], [0, 0, 0]);
    }

    #[test]
    fn recent_colours_and_hex() {
        let mut recent = Vec::new();
        for i in 0..12u8 {
            push_recent(&mut recent, [i, 0, 0]);
        }
        push_recent(&mut recent, [5, 0, 0]);
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], [5, 0, 0]);
        assert_eq!(recent.iter().filter(|c| **c == [5, 0, 0]).count(), 1);
        assert_eq!(to_hex([31, 78, 156]), "#1F4E9C");
        assert_eq!(from_hex("#1f4e9c"), Some([31, 78, 156]));
        assert_eq!(from_hex("f00"), Some([255, 0, 0]));
        assert_eq!(from_hex("xyz"), None);
    }

    #[test]
    fn clicking_a_palette_colour_returns_it() {
        let ctx = egui::Context::default();
        let mut recent = Vec::new();
        let mut result = None;
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 800.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(p) = color_button(ui, "t", Some([0, 0, 0]), true, &mut recent) {
                        result = Some(p);
                    }
                });
            });
            out.textures_delta.clear();
        };
        let click = |p: egui::Pos2| {
            let button = |pressed| egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            };
            vec![egui::Event::PointerMoved(p), button(true), button(false)]
        };
        frame(Vec::new());
        let swatch: Rect = ctx.data(|d| d.get_temp(Id::unique("test_color_button"))).unwrap();
        frame(click(swatch.center()));
        frame(Vec::new());
        let cell_rect: Rect = ctx
            .data(|d| d.get_temp(Id::unique(("test_palette_cell", 3usize, 7usize))))
            .expect("picker open");
        frame(click(cell_rect.center()));
        let expected = palette()[3][7];
        assert_eq!(result, Some(Some(expected)));
        assert_eq!(recent, vec![expected]);
    }
}
