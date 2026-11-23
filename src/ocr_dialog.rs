//! The "Recognise text (OCR)" dialog: choose pages and languages, show progress.

use std::path::PathBuf;
use crate::i18n::{t, tf};

use eframe::egui;

use crate::document::DocTab;
use crate::engine::{self, DocId, OcrOutcome, Request, Response};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Current,
    Selected,
}

struct State {
    doc: DocId,
    /// Tesseract and its languages, or why it cannot be used.
    tesseract: Result<(PathBuf, Vec<String>), String>,
    scope: Scope,
    force: bool,
    filter: String,
    /// While running: pages done and total.
    progress: Option<(usize, usize)>,
    error: Option<String>,
}

/// Dialog state. Selected languages are remembered while the app runs.
#[derive(Default)]
pub struct OcrDialog {
    state: Option<State>,
    languages: Vec<String>,
}

/// English names of common Tesseract language codes.
fn language_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "ara" => "Arabic",
        "ces" => "Czech",
        "chi_sim" => "Chinese (simplified)",
        "chi_tra" => "Chinese (traditional)",
        "dan" => "Danish",
        "deu" => "German",
        "ell" => "Greek",
        "eng" => "English",
        "fin" => "Finnish",
        "fra" => "French",
        "heb" => "Hebrew",
        "hun" => "Hungarian",
        "ita" => "Italian",
        "jpn" => "Japanese",
        "kor" => "Korean",
        "nld" => "Dutch",
        "nor" => "Norwegian",
        "pol" => "Polish",
        "por" => "Portuguese",
        "rus" => "Russian",
        "spa" => "Spanish",
        "swe" => "Swedish",
        "tur" => "Turkish",
        "ukr" => "Ukrainian",
        _ => return None,
    })
}

/// The Tesseract code for the user's locale language, if it is a common one.
fn locale_language() -> Option<&'static str> {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))?;
    Some(match lang.get(..2)? {
        "de" => "deu",
        "fr" => "fra",
        "es" => "spa",
        "it" => "ita",
        "nl" => "nld",
        "pt" => "por",
        "pl" => "pol",
        "sv" => "swe",
        "da" => "dan",
        "cs" => "ces",
        "ru" => "rus",
        _ => return None,
    })
}

impl OcrDialog {
    pub fn open(&mut self, doc: DocId, has_selection: bool) {
        if self.state.as_ref().is_some_and(|s| s.progress.is_some()) {
            return;
        }
        let tesseract = match engine::ocr::find_tesseract() {
            Some(exe) => engine::ocr::languages(&exe).map(|langs| (exe, langs)),
            None => Err(engine::ocr::INSTALL_HELP.to_string()),
        };
        if let Ok((_, langs)) = &tesseract {
            self.languages.retain(|l| langs.contains(l));
            if self.languages.is_empty() {
                for l in [locale_language().unwrap_or("eng"), "eng"] {
                    if langs.iter().any(|x| x == l) && !self.languages.iter().any(|x| x == l) {
                        self.languages.push(l.to_string());
                    }
                }
            }
            if self.languages.is_empty() {
                self.languages.push(langs[0].clone());
            }
        }
        self.state = Some(State {
            doc,
            tesseract,
            scope: if has_selection { Scope::Selected } else { Scope::All },
            force: false,
            filter: String::new(),
            progress: None,
            error: None,
        });
    }

    /// Handles engine responses about recognition; returns a status message to show.
    pub fn on_response(&mut self, resp: &Response) -> Option<String> {
        let state = self.state.as_mut()?;
        match resp {
            Response::OcrProgress { doc, done, total } if *doc == state.doc => {
                state.progress = Some((*done, *total));
                None
            }
            Response::OcrFinished { doc, result } if *doc == state.doc => match result {
                OcrOutcome::Done {
                    recognised,
                    skipped,
                    words,
                } => {
                    self.state = None;
                    let mut msg = match recognised {
                        0 => "No text found".to_string(),
                        1 => tf("Recognised {words} words on 1 page", &[("words", &words)]),
                        n => tf("Recognised {words} words on {n} pages", &[("words", &words), ("n", &n)]),
                    };
                    if *skipped > 0 {
                        msg += &tf(" ({skipped} already had text)", &[("skipped", &skipped)]);
                    }
                    Some(msg)
                }
                OcrOutcome::Cancelled => {
                    self.state = None;
                    Some("Text recognition cancelled".into())
                }
                OcrOutcome::Failed(e) => {
                    state.progress = None;
                    state.error = Some(e.clone());
                    None
                }
            },
            _ => None,
        }
    }

    /// Shows the dialog for `tab` (the active document); returns requests to send.
    pub fn show(&mut self, ctx: &egui::Context, tab: Option<&DocTab>) -> Vec<Request> {
        let mut out = Vec::new();
        let Some(state) = self.state.as_mut() else { return out };
        let Some(tab) = tab.filter(|t| t.id == state.doc && t.info().is_some()) else {
            if state.progress.is_some() {
                out.push(Request::CancelOcr { doc: state.doc });
            }
            self.state = None;
            return out;
        };
        let doc = state.doc;
        let selected = tab.target_pages();
        let count = tab.page_count();
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("ocr_dialog")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading(t("Recognise text (OCR)"));
            ui.add_space(4.0);
            let (exe, langs) = match &state.tesseract {
                Ok((exe, langs)) => (exe, langs),
                Err(help) => {
                    ui.label(help.as_str());
                    ui.add_space(8.0);
                    close |= ui.button(t("Close")).clicked();
                    return;
                }
            };
            if let Some((done, total)) = state.progress {
                ui.label(tf("Recognising page {page} of {total}…", &[("page", &(done + 1).min(total)), ("total", &total)]));
                ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).show_percentage());
                ui.weak(t("Each page takes a few seconds."));
                ui.add_space(8.0);
                if ui.button(t("Cancel")).clicked() {
                    out.push(Request::CancelOcr { doc });
                }
                return;
            }
            ui.label(t("Makes scanned pages searchable and their text selectable, by adding invisible text over the page image."));
            ui.add_space(6.0);
            ui.strong(t("Pages"));
            ui.radio_value(&mut state.scope, Scope::All, tf("All pages ({count})", &[("count", &count)]));
            ui.radio_value(&mut state.scope, Scope::Current, tf("Current page ({page})", &[("page", &(tab.current_page + 1))]));
            if selected.len() > 1 || state.scope == Scope::Selected {
                ui.radio_value(&mut state.scope, Scope::Selected, tf("Selected pages ({n})", &[("n", &selected.len())]));
            }
            ui.checkbox(&mut state.force, t("Also pages that already have text"))
                .on_hover_text(t("Pages with text are normally skipped. This replaces text added by an earlier recognition; other text stays."));
            ui.add_space(6.0);
            ui.strong(t("Languages"));
            if langs.len() > 12 {
                ui.add(egui::TextEdit::singleline(&mut state.filter).hint_text(t("Filter")).desired_width(f32::INFINITY));
            }
            let filter = state.filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                for code in langs {
                    let name = language_name(code);
                    let label = name.map_or_else(|| code.clone(), |n| format!("{n} ({code})"));
                    if !filter.is_empty() && !label.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let mut on = self.languages.contains(code);
                    if ui.checkbox(&mut on, label).changed() {
                        if on {
                            self.languages.push(code.clone());
                        } else {
                            self.languages.retain(|l| l != code);
                        }
                    }
                }
            });
            ui.weak(format!("Tesseract: {}", exe.display()));
            if let Some(e) = &state.error {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready = !self.languages.is_empty();
                if ui.add_enabled(ready, egui::Button::new(t("Recognise"))).clicked() {
                    let pages = match state.scope {
                        Scope::All => (0..count).collect(),
                        Scope::Current => vec![tab.current_page],
                        Scope::Selected => selected.clone(),
                    };
                    out.push(Request::Ocr {
                        doc,
                        pages,
                        exe: exe.clone(),
                        languages: self.languages.join("+"),
                        force: state.force,
                    });
                    state.error = None;
                    state.progress = Some((0, 1));
                }
                close |= ui.button(t("Cancel")).clicked();
            });
        });
        if (close || modal.should_close()) && state.progress.is_none() {
            self.state = None;
        }
        out
    }
}
