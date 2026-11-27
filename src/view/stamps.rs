//! Stamp and signature tools: choosing what they place, image loading, and the
//! signature manager (draw a signature with the mouse or use a picture).

use crate::i18n::{t, tf};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::engine::{StampSpec, pdf_date_now};
use crate::settings::{AnnotStyle, STAMPS, Settings, Signature};
use crate::view::document_view::{format_pdf_date, rgb};

/// Longest side of images stored in PDFs (larger pictures are scaled down).
const MAX_IMAGE: u32 = 1600;

/// Loads a PNG or JPEG file as RGBA pixels for a stamp.
pub fn load_image(path: &Path) -> Result<StampSpec, String> {
    let img =
        image::open(path).map_err(|e| tf("Could not read {file}: {e}", &[("file", &path.display()), ("e", &e)]))?;
    let img = if img.width().max(img.height()) > MAX_IMAGE { img.thumbnail(MAX_IMAGE, MAX_IMAGE) } else { img };
    let rgba = img.to_rgba8();
    Ok(StampSpec::Image { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

#[derive(Default)]
pub struct StampState {
    /// Decoded image stamps and image signatures.
    images: HashMap<PathBuf, Result<StampSpec, String>>,
    /// The text stamp the tool places (rebuilt every frame: it shows the date).
    text: Option<StampSpec>,
    new_text: String,
    pub show_signatures: bool,
    /// The signature being drawn (strokes in canvas coordinates 0..1 × 0..ASPECT).
    drawing: Vec<Vec<[f32; 2]>>,
}

/// Height of the drawing canvas relative to its width.
const ASPECT: f32 = 0.35;

impl StampState {
    fn image(&mut self, path: &Path) -> &Result<StampSpec, String> {
        self.images.entry(path.to_path_buf()).or_insert_with(|| load_image(path))
    }

    /// Prepares what the stamp and signature tools place this frame.
    pub fn prepare(&mut self, settings: &Settings) {
        let style = &settings.annot_style;
        self.text = None;
        match &style.stamp_image {
            Some(path) => {
                self.image(path);
            }
            None if !style.stamp_text.is_empty() => {
                let subtitle = if style.stamp_details {
                    let date = format_pdf_date(&pdf_date_now());
                    if settings.author.is_empty() { date } else { format!("{}, {date}", settings.author) }
                } else {
                    String::new()
                };
                self.text = Some(StampSpec::Text { title: style.stamp_text.clone(), subtitle });
            }
            None => {}
        }
        if let Some(Signature::Image(path)) = settings.signatures.get(style.signature) {
            self.image(path);
        }
    }

    pub fn stamp(&self, style: &AnnotStyle) -> Option<&StampSpec> {
        match &style.stamp_image {
            Some(path) => self.images.get(path)?.as_ref().ok(),
            None => self.text.as_ref(),
        }
    }

    pub fn signature_image(&self, style: &AnnotStyle, signatures: &[Signature]) -> Option<&StampSpec> {
        match signatures.get(style.signature)? {
            Signature::Image(path) => self.images.get(path)?.as_ref().ok(),
            Signature::Drawn(_) => None,
        }
    }

    /// An error loading the chosen image, if any.
    pub fn image_error(&self, settings: &Settings) -> Option<String> {
        let style = &settings.annot_style;
        let sig = match settings.signatures.get(style.signature) {
            Some(Signature::Image(p)) => Some(p),
            _ => None,
        };
        [style.stamp_image.as_ref(), sig]
            .into_iter()
            .flatten()
            .find_map(|p| self.images.get(p)?.as_ref().err().cloned())
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Tool options for the stamp tool. Returns true if settings changed.
pub fn stamp_options(ui: &mut Ui, state: &mut StampState, settings: &mut Settings) -> bool {
    let mut changed = false;
    let style = &mut settings.annot_style;
    let current = match &style.stamp_image {
        Some(p) => file_name(p),
        None => style.stamp_text.clone(),
    };
    egui::ComboBox::from_id_salt("stamp_choice").selected_text(current).width(170.0).show_ui(ui, |ui| {
        for (text, color) in STAMPS {
            if ui.selectable_label(style.stamp_image.is_none() && style.stamp_text == text, text).clicked() {
                style.stamp_image = None;
                style.stamp_text = text.to_string();
                style.stamp_color = color;
                changed = true;
            }
        }
        let mut remove = None;
        if !style.custom_stamps.is_empty() || !style.image_stamps.is_empty() {
            ui.separator();
        }
        for (i, text) in style.custom_stamps.iter().enumerate() {
            let r = ui.selectable_label(style.stamp_image.is_none() && style.stamp_text == *text, text);
            if r.clicked() {
                style.stamp_image = None;
                style.stamp_text = text.clone();
                changed = true;
            }
            r.context_menu(|ui| {
                if ui.button(t("Remove")).clicked() {
                    remove = Some((false, i));
                }
            });
        }
        for (i, path) in style.image_stamps.iter().enumerate() {
            let r = ui.selectable_label(style.stamp_image.as_ref() == Some(path), format!("🖼 {}", file_name(path)));
            if r.clicked() {
                style.stamp_image = Some(path.clone());
                changed = true;
            }
            r.context_menu(|ui| {
                if ui.button(t("Remove")).clicked() {
                    remove = Some((true, i));
                }
            });
        }
        match remove {
            Some((false, i)) => {
                style.custom_stamps.remove(i);
                changed = true;
            }
            Some((true, i)) => {
                let p = style.image_stamps.remove(i);
                if style.stamp_image.as_ref() == Some(&p) {
                    style.stamp_image = None;
                }
                changed = true;
            }
            None => {}
        }
    });
    ui.add(egui::TextEdit::singleline(&mut state.new_text).hint_text(t("Own text")).desired_width(110.0));
    let text = state.new_text.trim().to_uppercase();
    if ui.add_enabled(!text.is_empty(), egui::Button::new(t("Add"))).clicked() {
        if !style.custom_stamps.contains(&text) {
            style.custom_stamps.push(text.clone());
        }
        style.stamp_image = None;
        style.stamp_text = text;
        state.new_text.clear();
        changed = true;
    }
    if ui.button(t("Image…")).on_hover_text(t("Use a picture (PNG or JPEG) as a stamp")).clicked()
        && let Some(path) = rfd::FileDialog::new().add_filter(t("Pictures"), &["png", "jpg", "jpeg"]).pick_file()
    {
        if !style.image_stamps.contains(&path) {
            style.image_stamps.push(path.clone());
        }
        state.images.remove(&path);
        style.stamp_image = Some(path);
        changed = true;
    }
    if style.stamp_image.is_none() {
        changed |= ui.checkbox(&mut style.stamp_details, t("Name and date")).changed();
    }
    changed
}

/// Tool options for the signature tool. Returns true if settings changed.
pub fn signature_options(ui: &mut Ui, state: &mut StampState, settings: &mut Settings) -> bool {
    let mut changed = false;
    let label = |i: usize, s: &Signature| match s {
        Signature::Drawn(_) => tf("Signature {n}", &[("n", &(i + 1))]),
        Signature::Image(p) => format!("{} ({})", tf("Signature {n}", &[("n", &(i + 1))]), file_name(p)),
    };
    if settings.signatures.is_empty() {
        ui.weak(t("No signature yet"));
    } else {
        let sel = settings.annot_style.signature.min(settings.signatures.len() - 1);
        egui::ComboBox::from_id_salt("signature_choice").selected_text(label(sel, &settings.signatures[sel])).show_ui(
            ui,
            |ui| {
                for (i, s) in settings.signatures.iter().enumerate() {
                    if ui.selectable_label(i == sel, label(i, s)).clicked() {
                        settings.annot_style.signature = i;
                        changed = true;
                    }
                }
            },
        );
    }
    if ui.button(t("Signatures…")).clicked() {
        state.show_signatures = true;
    }
    changed
}

/// Draws strokes (0..1 × 0..ASPECT coordinates) into `rect`.
fn paint_strokes(ui: &Ui, rect: Rect, strokes: &[Vec<[f32; 2]>], color: Color32) {
    let w = rect.width();
    let to = |p: &[f32; 2]| pos2(rect.left() + p[0] * w, rect.top() + p[1] * w);
    for s in strokes {
        let pts: Vec<Pos2> = s.iter().map(to).collect();
        ui.painter().add(egui::Shape::line(pts, Stroke::new((w / 250.0).max(1.0), color)));
    }
}

/// Fits strokes to x 0..1 (keeping proportions, y from 0).
fn normalize(strokes: &[Vec<[f32; 2]>]) -> Vec<Vec<[f32; 2]>> {
    let pts = strokes.iter().flatten();
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in pts {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    let scale = 1.0 / (x1 - x0).max(y1 - y0).max(1e-3);
    strokes.iter().map(|s| s.iter().map(|p| [(p[0] - x0) * scale, (p[1] - y0) * scale]).collect()).collect()
}

/// The signature manager. Returns true if settings changed.
pub fn signatures_window(ctx: &egui::Context, state: &mut StampState, settings: &mut Settings) -> bool {
    let mut changed = false;
    let mut open = state.show_signatures;
    let ink = rgb(settings.annot_style.signature_color);
    egui::Window::new(t("Signatures"))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.label(t("Draw your signature with the mouse (or a pen):"));
            let size = vec2(420.0, 420.0 * ASPECT);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::drag());
            ui.painter().rect_filled(rect, 4.0, Color32::WHITE);
            ui.painter().hline(
                rect.x_range().shrink(20.0),
                rect.bottom() - 25.0,
                Stroke::new(1.0, Color32::from_gray(200)),
            );
            if let Some(p) = resp.interact_pointer_pos() {
                let q = [(p.x - rect.left()) / rect.width(), (p.y - rect.top()) / rect.width()];
                if resp.drag_started() {
                    state.drawing.push(vec![q]);
                } else if resp.dragged()
                    && let Some(s) = state.drawing.last_mut()
                {
                    s.push(q);
                }
            }
            paint_strokes(ui, rect, &state.drawing, ink);
            ui.horizontal(|ui| {
                let has = state.drawing.iter().any(|s| s.len() > 1);
                if ui.add_enabled(has, egui::Button::new(t("Save signature"))).clicked() {
                    let strokes: Vec<_> = state.drawing.drain(..).filter(|s| s.len() > 1).collect();
                    settings.signatures.push(Signature::Drawn(normalize(&strokes)));
                    settings.annot_style.signature = settings.signatures.len() - 1;
                    changed = true;
                }
                if ui.add_enabled(!state.drawing.is_empty(), egui::Button::new(t("Clear"))).clicked() {
                    state.drawing.clear();
                }
                ui.separator();
                if ui.button(t("Use a picture…")).on_hover_text(t("A scanned signature (PNG with transparency works best)")).clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter(t("Pictures"), &["png", "jpg", "jpeg"]).pick_file()
                {
                    state.images.remove(&path);
                    settings.signatures.push(Signature::Image(path));
                    settings.annot_style.signature = settings.signatures.len() - 1;
                    changed = true;
                }
            });
            if !settings.signatures.is_empty() {
                ui.separator();
                ui.label(t("Saved signatures (click to use):"));
                let mut remove = None;
                for (i, s) in settings.signatures.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let chosen = settings.annot_style.signature == i;
                        let (r, resp) = ui.allocate_exact_size(vec2(200.0, 70.0), Sense::click());
                        ui.painter().rect(
                            r,
                            4.0,
                            Color32::WHITE,
                            Stroke::new(if chosen { 2.0 } else { 1.0 }, if chosen { ui.visuals().selection.stroke.color } else { Color32::GRAY }),
                            egui::StrokeKind::Inside,
                        );
                        match s {
                            Signature::Drawn(strokes) => {
                                let h = strokes.iter().flatten().map(|p| p[1]).fold(0.0_f32, f32::max).max(0.01);
                                let w = (r.width() - 16.0).min((r.height() - 12.0) / h);
                                let inner = Rect::from_center_size(r.center(), vec2(w, w * h));
                                paint_strokes(ui, inner, strokes, ink);
                            }
                            Signature::Image(p) => {
                                ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, format!("🖼 {}", file_name(p)), egui::FontId::proportional(12.0), Color32::DARK_GRAY);
                            }
                        }
                        if resp.clicked() {
                            settings.annot_style.signature = i;
                            changed = true;
                        }
                        if ui.button(t("Delete")).clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    settings.signatures.remove(i);
                    let sel = &mut settings.annot_style.signature;
                    *sel = (*sel).min(settings.signatures.len().saturating_sub(1));
                    changed = true;
                }
            }
            ui.weak(t("Signatures are kept in magic-pdf's settings on this computer. Placed signatures are pictures, not certified digital signatures."));
        });
    state.show_signatures = open;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawn_signatures_are_normalised_to_unit_width() {
        let n = normalize(&[vec![[10.0, 5.0], [30.0, 9.0]], vec![[20.0, 7.0]]]);
        assert_eq!(n[0], vec![[0.0, 0.0], [1.0, 0.2]]);
        assert_eq!(n[1], vec![[0.5, 0.1]]);
    }

    #[test]
    fn images_load_and_large_ones_shrink() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.png");
        image::RgbaImage::from_pixel(3200, 800, image::Rgba([0, 0, 255, 128])).save(&path).unwrap();
        let StampSpec::Image { width, height, rgba } = load_image(&path).unwrap() else { panic!() };
        assert_eq!((width, height), (1600, 400));
        assert_eq!(&rgba[..4], &[0, 0, 255, 128], "transparency kept");
        assert!(load_image(&dir.join("missing.png")).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
