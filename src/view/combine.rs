//! File → Combine files: a list of PDFs to put one after the other into a new file.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Ui};

use crate::engine::Request;
use crate::i18n::{t, tf};
use crate::icons;

pub struct CombineDialog {
    files: Vec<PathBuf>,
    /// A bookmark for each file, holding its own bookmarks.
    bookmarks: bool,
}

impl CombineDialog {
    /// Starts with `first` (the open document, if any).
    pub fn new(first: Option<PathBuf>) -> CombineDialog {
        CombineDialog { files: first.into_iter().collect(), bookmarks: true }
    }

    /// Adds files (e.g. dropped on the dialog).
    pub fn add(&mut self, files: impl IntoIterator<Item = PathBuf>) {
        self.files.extend(files.into_iter().filter(|f| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))));
    }

    /// Shows the dialog; returns the request to send when the user combines, and
    /// whether the dialog stays open. `unsaved` names open files with unsaved changes.
    pub fn show(&mut self, ctx: &egui::Context, unsaved: &[PathBuf]) -> (Option<Request>, bool) {
        let mut open = true;
        let mut request = None;
        let mut close = false;
        egui::Window::new(t("Combine files"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(t("The files' pages are put one after the other into a new PDF, in this order:"));
                ui.add_space(4.0);
                self.list(ui);
                ui.horizontal(|ui| {
                    if ui.button(t("Add files…")).clicked()
                        && let Some(files) = rfd::FileDialog::new()
                            .add_filter(t("PDF documents"), &["pdf", "PDF"])
                            .set_directory(self.files.last().and_then(|f| f.parent()).unwrap_or(Path::new(".")))
                            .pick_files()
                    {
                        self.add(files);
                    }
                    ui.weak(t("or drop them here"));
                });
                ui.add_space(4.0);
                ui.checkbox(&mut self.bookmarks, t("Add a bookmark for each file"))
                    .on_hover_text(t("Each file's own bookmarks are kept under it"));
                let changed: Vec<&PathBuf> = self.files.iter().filter(|f| unsaved.contains(f)).collect();
                if !changed.is_empty() {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        tf(
                            "Unsaved changes in {files} are not included; save first to include them.",
                            &[("files", &changed.iter().map(|f| file_name(f)).collect::<Vec<_>>().join(", "))],
                        ),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let enough = self.files.len() >= 2;
                    let combine = ui
                        .add_enabled(enough, crate::theme::primary_button(t("Combine and save…")))
                        .on_disabled_hover_text(t("Choose at least two files"));
                    if combine.clicked()
                        && let Some(out) = rfd::FileDialog::new()
                            .add_filter(t("PDF documents"), &["pdf"])
                            .set_directory(self.files[0].parent().unwrap_or(Path::new(".")))
                            .set_file_name(format!("{}-combined.pdf", stem(&self.files[0])))
                            .save_file()
                    {
                        request = Some(Request::Combine { files: self.files.clone(), out, bookmarks: self.bookmarks });
                        close = true;
                    }
                    if ui.button(t("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
        (request, open && !close)
    }

    fn list(&mut self, ui: &mut Ui) {
        if self.files.is_empty() {
            ui.weak(t("No files yet"));
            return;
        }
        let (mut up, mut remove) = (None, None);
        let count = self.files.len();
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for (i, f) in self.files.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.weak(format!("{}.", i + 1));
                    ui.label(file_name(f)).on_hover_text(f.display().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(icons::sized(icons::X, 13.0)).on_hover_text(t("Remove")).clicked() {
                            remove = Some(i);
                        }
                        if ui
                            .add_enabled(
                                i + 1 < count,
                                egui::Button::new(icons::sized(icons::CARET_DOWN, 13.0)).small(),
                            )
                            .on_hover_text(t("Move down"))
                            .clicked()
                        {
                            up = Some(i + 1);
                        }
                        if ui
                            .add_enabled(i > 0, egui::Button::new(icons::sized(icons::CARET_UP, 13.0)).small())
                            .on_hover_text(t("Move up"))
                            .clicked()
                        {
                            up = Some(i);
                        }
                    });
                });
            }
        });
        // `up = Some(i)`: swap file i with the one above it.
        if let Some(i) = up {
            self.files.swap(i - 1, i);
        }
        if let Some(i) = remove {
            self.files.remove(i);
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn stem(p: &Path) -> String {
    p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
