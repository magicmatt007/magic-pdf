//! Dialogs for page tools: crop, header and footer, and saving an optimized copy.

use std::path::PathBuf;

use eframe::egui::{self, Ui};

use crate::document::DocTab;
use crate::engine::{HeaderFooter, Margins, PageOp, Request, Rgb, pdf_date_now};
use crate::view::color_picker;
use crate::view::document_view::format_pdf_date;

const MM: f32 = 72.0 / 25.4;

/// Which pages a dialog acts on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scope {
    #[default]
    All,
    Selected,
}

fn scope_ui(ui: &mut Ui, scope: &mut Scope, tab: &DocTab) {
    let n = tab.target_pages().len();
    ui.horizontal(|ui| {
        ui.label("Pages");
        ui.radio_value(scope, Scope::All, format!("All ({})", tab.page_count()));
        let label = if tab.page_selection.is_empty() {
            format!("Current page ({})", tab.current_page + 1)
        } else {
            format!("Selected ({n})")
        };
        ui.radio_value(scope, Scope::Selected, label);
    });
}

fn pages(scope: Scope, tab: &DocTab) -> Vec<usize> {
    match scope {
        Scope::All => (0..tab.page_count()).collect(),
        Scope::Selected => tab.target_pages(),
    }
}

/// Dialog buttons; returns (ok, cancel).
fn buttons(ui: &mut Ui, ok: &str) -> (bool, bool) {
    ui.add_space(6.0);
    ui.horizontal(|ui| (ui.button(ok).clicked(), ui.button("Cancel").clicked())).inner
}

fn window(title: &str) -> egui::Window<'_> {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
}

#[derive(Default)]
pub struct CropDialog {
    /// Top, right, bottom, left in millimetres.
    mm: [f32; 4],
    scope: Scope,
}

/// Shows the crop dialog. Returns the request to send when confirmed, and whether
/// the dialog stays open.
pub fn crop_dialog(ctx: &egui::Context, d: &mut CropDialog, tab: &DocTab) -> (Option<Request>, bool) {
    let mut result = (None, true);
    window("Crop pages").show(ctx, |ui| {
        ui.label("Trim the visible area of the pages (as they are shown):");
        egui::Grid::new("crop").num_columns(4).show(ui, |ui| {
            for (i, side) in ["Top", "Right", "Bottom", "Left"].iter().enumerate() {
                ui.label(*side);
                ui.add(egui::DragValue::new(&mut d.mm[i]).range(0.0..=500.0).speed(0.5).suffix(" mm"));
                if i % 2 == 1 {
                    ui.end_row();
                }
            }
        });
        if ui.small_button("Same on all sides").clicked() {
            d.mm = [d.mm[0]; 4];
        }
        scope_ui(ui, &mut d.scope, tab);
        ui.weak("Cropping hides content outside; Undo brings it back.");
        let (ok, cancel) = buttons(ui, "Crop");
        if ok && d.mm.iter().any(|&m| m > 0.0) {
            let [top, right, bottom, left] = d.mm.map(|m| m * MM);
            result = (
                Some(Request::EditPages {
                    doc: tab.id,
                    op: PageOp::Crop {
                        pages: pages(d.scope, tab),
                        margins: Margins { top, right, bottom, left },
                    },
                }),
                false,
            );
        } else if cancel {
            result = (None, false);
        }
    });
    result
}

pub struct HeaderFooterDialog {
    /// Header left, centre, right, then footer left, centre, right.
    texts: [String; 6],
    size: f32,
    color: Rgb,
    margin_mm: f32,
    first_number: i64,
    scope: Scope,
}

impl Default for HeaderFooterDialog {
    fn default() -> Self {
        Self {
            texts: [
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                "Page {page} of {pages}".into(),
                String::new(),
            ],
            size: 9.0,
            color: [60, 60, 60],
            margin_mm: 10.0,
            first_number: 1,
            scope: Scope::All,
        }
    }
}

pub fn header_footer_dialog(
    ctx: &egui::Context,
    d: &mut HeaderFooterDialog,
    tab: &DocTab,
    recent: &mut Vec<Rgb>,
) -> (Option<Request>, bool) {
    let mut result = (None, true);
    window("Header and footer").show(ctx, |ui| {
        egui::Grid::new("hf").num_columns(4).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("");
            for h in ["Left", "Centre", "Right"] {
                ui.strong(h);
            }
            ui.end_row();
            for (row, name) in ["Header", "Footer"].iter().enumerate() {
                ui.label(*name);
                for col in 0..3 {
                    ui.add(egui::TextEdit::singleline(&mut d.texts[row * 3 + col]).desired_width(150.0));
                }
                ui.end_row();
            }
        });
        ui.weak("Placeholders: {page}  {pages}  {date}  {file}");
        ui.horizontal(|ui| {
            ui.label("Size");
            ui.add(egui::DragValue::new(&mut d.size).range(5.0..=36.0).suffix(" pt"));
            ui.label("Colour");
            if let Some(Some(c)) = color_picker::color_button(ui, "hf_color", Some(d.color), false, recent) {
                d.color = c;
            }
            ui.label("Distance from edge");
            ui.add(egui::DragValue::new(&mut d.margin_mm).range(2.0..=50.0).suffix(" mm"));
            ui.label("First page number");
            ui.add(egui::DragValue::new(&mut d.first_number).range(-9999..=99999));
        });
        scope_ui(ui, &mut d.scope, tab);
        ui.weak("Applying again replaces the header and footer magic-pdf added before.");
        let (ok, cancel) = buttons(ui, "Apply");
        if ok {
            let date = format_pdf_date(&pdf_date_now()).chars().take(10).collect::<String>();
            let file = tab.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let fill = |s: &String| s.replace("{date}", &date).replace("{file}", &file);
            let t: Vec<String> = d.texts.iter().map(fill).collect();
            let spec = HeaderFooter {
                header: [t[0].clone(), t[1].clone(), t[2].clone()],
                footer: [t[3].clone(), t[4].clone(), t[5].clone()],
                size: d.size,
                color: d.color,
                margin: d.margin_mm * MM,
                first_number: d.first_number,
            };
            result = (
                Some(Request::EditPages {
                    doc: tab.id,
                    op: PageOp::HeaderFooter {
                        pages: pages(d.scope, tab),
                        spec,
                    },
                }),
                false,
            );
        } else if cancel {
            result = (None, false);
        }
    });
    result
}

pub struct OptimizeDialog {
    scale_images: bool,
    dpi: u32,
}

impl Default for OptimizeDialog {
    fn default() -> Self {
        Self { scale_images: true, dpi: 150 }
    }
}

pub fn optimize_dialog(ctx: &egui::Context, d: &mut OptimizeDialog, tab: &DocTab) -> (Option<Request>, bool) {
    let mut result = (None, true);
    window("Save optimized copy").show(ctx, |ui| {
        ui.label("Writes a smaller copy; the open document stays as it is.");
        ui.label("• Compresses the file structure (lossless)");
        ui.horizontal(|ui| {
            ui.checkbox(&mut d.scale_images, "Scale down pictures above");
            ui.add_enabled(d.scale_images, egui::DragValue::new(&mut d.dpi).range(72..=600).suffix(" dpi"));
        });
        ui.weak("150 dpi suits screens and office printers; 300 dpi keeps print quality.\nPictures become JPEG; black-and-white scans and transparent pictures stay as they are.");
        let (ok, cancel) = buttons(ui, "Save as…");
        if ok {
            let stem = tab.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
            let mut dialog = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .set_file_name(format!("{stem}-small.pdf"));
            if let Some(dir) = tab.path.parent() {
                dialog = dialog.set_directory(dir);
            }
            if let Some(path) = dialog.save_file() {
                let path: PathBuf = path;
                result = (
                    Some(Request::SaveOptimized {
                        doc: tab.id,
                        path,
                        image_dpi: d.scale_images.then_some(d.dpi),
                    }),
                    false,
                );
            }
        } else if cancel {
            result = (None, false);
        }
    });
    result
}
