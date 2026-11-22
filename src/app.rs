//! Application shell: tabs, menus, toolbar, keyboard shortcuts and engine plumbing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, Ui};

use crate::document::{ACTUAL_SIZE, DocTab, LoadState, SearchHit, ZOOM_PRESETS};
use crate::icons;
use crate::engine::{DocId, Engine, FormKey, PageOp, PrintJob, Request, Response, SearchQuery, form_mods, move_target};
use crate::settings::Settings;
use crate::view::cache::TextureCache;
use crate::view::document_view::{self, PALETTE, Tool, ViewContext, color_swatch};
use crate::view::layout::LayoutMode;
use crate::view::sidebar::{self, PageAction, SidebarTab};
use crate::view::viewport::FitMode;

const TEXTURE_BUDGET: usize = 768 * 1024 * 1024;
const SEARCH_ID: &str = "search_field";

/// What to do once unsaved documents have been dealt with.
#[derive(Clone, Debug)]
struct CloseIntent {
    /// Tabs (by id) to close.
    docs: Vec<DocId>,
    quit: bool,
    /// Saves still outstanding before the close can proceed.
    awaiting_save: HashSet<DocId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageChoice {
    All,
    Current,
    Range,
}

/// State of the CUPS print dialog.
struct PrintDialog {
    doc: DocId,
    printers: Vec<String>,
    printer: Option<String>,
    copies: u32,
    pages: PageChoice,
    range: String,
    two_sided: bool,
    fit_to_page: bool,
    error: Option<String>,
}

pub struct App {
    engine: Result<Engine, String>,
    ctx: egui::Context,
    tabs: Vec<DocTab>,
    active: usize,
    next_id: DocId,
    cache: TextureCache,
    settings: Settings,
    tool: Tool,
    sidebar_tab: SidebarTab,
    focus_search: bool,
    show_shortcuts: bool,
    show_about: bool,
    close_intent: Option<CloseIntent>,
    /// Quit has been confirmed; let the next close request through.
    allow_quit: bool,
    error: Option<String>,
    print_dialog: Option<PrintDialog>,
    /// Short status message and when it was shown.
    notice: Option<(String, f64)>,
    print_results: (Sender<PrintResult>, Receiver<PrintResult>),
    /// Debug aid: with MAGIC_PDF_SCREENSHOT=<file.ppm> the app captures its own window
    /// once pages have rendered, saves it and quits.
    screenshot_to: Option<PathBuf>,
    screenshot_requested: bool,
    /// Pages per file for the "Split document" dialog (None: dialog closed).
    split_every: Option<usize>,
}

type PrintResult = Result<String, String>;

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let wake_ctx = ctx.clone();
        let engine = Engine::start(move || wake_ctx.request_repaint());
        if let Err(e) = &engine {
            log::error!("{e}");
        }
        icons::install(&ctx);
        let settings = Settings::load();
        ctx.set_theme(if settings.dark_ui {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        let mut cache = TextureCache::new(TEXTURE_BUDGET);
        cache.invert = settings.night_mode;
        let mut app = Self {
            engine,
            ctx,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            cache,
            settings,
            tool: Tool::Hand,
            sidebar_tab: SidebarTab::Pages,
            focus_search: false,
            show_shortcuts: false,
            show_about: false,
            close_intent: None,
            allow_quit: false,
            error: None,
            print_dialog: None,
            notice: None,
            print_results: std::sync::mpsc::channel(),
            screenshot_to: std::env::var_os("MAGIC_PDF_SCREENSHOT").map(PathBuf::from),
            screenshot_requested: false,
            split_every: None,
        };
        for f in files {
            app.open(&f);
        }
        app
    }

    fn send(&self, req: Request) {
        if let Ok(engine) = &self.engine {
            engine.send(req);
        }
    }

    pub fn open(&mut self, path: &Path) {
        let path = simplify_path(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let tab = DocTab::new(id, path.clone(), self.settings.default_mode, self.settings.file_state(&path));
        self.send(Request::Open {
            doc: id,
            path: path.clone(),
            password: None,
        });
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.settings.add_recent(&path);
        self.settings.save();
    }

    fn open_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("PDF documents", &["pdf", "PDF"]);
        if let Some(dir) = self.current_dir() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(files) = dialog.pick_files() {
            for f in files {
                self.open(&f);
            }
        }
    }

    fn current_dir(&self) -> Option<PathBuf> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.path.parent())
            .or_else(|| self.settings.recent.first().and_then(|p| p.parent()))
            .map(Path::to_path_buf)
    }

    fn save_active(&mut self, save_as: bool) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.info().is_none() {
            return;
        }
        let path = if save_as {
            let mut dialog = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .set_file_name(&tab.name);
            if let Some(dir) = tab.path.parent() {
                dialog = dialog.set_directory(dir);
            }
            match dialog.save_file() {
                Some(p) => p,
                None => return,
            }
        } else {
            tab.path.clone()
        };
        self.send(Request::Save { doc: tab.id, path });
    }

    /// Closes tabs, asking first if any has unsaved changes.
    fn request_close(&mut self, docs: Vec<DocId>, quit: bool) {
        let dirty = docs.iter().any(|d| self.tabs.iter().any(|t| t.id == *d && t.dirty));
        if dirty {
            self.close_intent = Some(CloseIntent {
                docs,
                quit,
                awaiting_save: HashSet::new(),
            });
        } else {
            self.finish_close(&docs, quit);
        }
    }

    fn finish_close(&mut self, docs: &[DocId], quit: bool) {
        for doc in docs {
            if let Some(i) = self.tabs.iter().position(|t| t.id == *doc) {
                self.close_tab_now(i);
            }
        }
        if quit {
            self.allow_quit = true;
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn close_tab_now(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(index);
        if tab.info().is_some() {
            self.settings.set_file_state(&tab.path, tab.file_state());
            self.settings.save();
        }
        self.send(Request::Close { doc: tab.id });
        self.cache.remove_doc(tab.id);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if index < self.active {
            self.active -= 1;
        }
    }

    fn tab_mut(&mut self, doc: DocId) -> Option<&mut DocTab> {
        self.tabs.iter_mut().find(|t| t.id == doc)
    }

    fn active_tab(&mut self) -> Option<&mut DocTab> {
        self.tabs.get_mut(self.active)
    }

    fn process_responses(&mut self) {
        let mut responses = Vec::new();
        if let Ok(engine) = &self.engine {
            while let Some(r) = engine.try_recv() {
                responses.push(r);
            }
        }
        for resp in responses {
            self.process_response(resp);
        }
    }

    fn process_response(&mut self, resp: Response) {
        match resp {
            Response::Opened { doc, info } => {
                if let Some(tab) = self.tab_mut(doc) {
                    log::info!(
                        "opened “{}” ({} pages)",
                        info.title.as_deref().unwrap_or(&tab.name),
                        info.page_sizes.len()
                    );
                    tab.state = LoadState::Ready(info);
                    let page = tab.pending_goto.unwrap_or(0);
                    tab.goto_page(page);
                    self.send(Request::ScanAnnots { doc });
                }
            }
            Response::OpenFailed {
                doc,
                error,
                password_required,
            } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.state = if password_required {
                        let wrong = matches!(tab.state, LoadState::NeedsPassword { .. });
                        LoadState::NeedsPassword {
                            input: String::new(),
                            wrong,
                        }
                    } else {
                        LoadState::Failed(error)
                    };
                }
            }
            Response::Rendered { key, size, rgba } => {
                let ctx = self.ctx.clone();
                if let Some(tab) = self.tab_mut(key.doc) {
                    tab.requested.remove(&key);
                    self.cache.insert(&ctx, key, size, rgba);
                }
            }
            Response::RenderFailed { key } => {
                // Left in `requested` so it is not retried every frame.
                log::debug!("render failed for {key:?}");
            }
            Response::PageText { doc, page, geom, text } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.geoms.insert(page, geom);
                    tab.texts.insert(page, Some(text));
                }
            }
            Response::SearchResults {
                doc,
                generation,
                page,
                geom,
                hits,
            } => {
                if let Some(tab) = self.tab_mut(doc)
                    && tab.search.generation == generation
                {
                    tab.geoms.insert(page, geom);
                    let first = tab.search.hits.is_empty();
                    tab.search
                        .hits
                        .extend(hits.into_iter().map(|rects| SearchHit { page, rects }));
                    if first {
                        // Jump to the first hit at or after the current page.
                        let start = tab.current_page;
                        let idx = tab.search.hits.iter().position(|h| h.page >= start).unwrap_or(0);
                        tab.show_search_hit(idx);
                    }
                }
            }
            Response::SearchDone { doc, generation } => {
                if let Some(tab) = self.tab_mut(doc)
                    && tab.search.generation == generation
                {
                    tab.search.running = false;
                }
            }
            Response::PageAnnots {
                doc,
                page,
                geom,
                annots,
            } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.geoms.insert(page, geom);
                    if annots.is_empty() {
                        tab.annots.remove(&page);
                    } else {
                        tab.annots.insert(page, annots);
                    }
                }
            }
            Response::AnnotScanDone { doc } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.annots_scanned = true;
                }
            }
            Response::PageChanged { doc, page } => {
                if let Some(tab) = self.tab_mut(doc) {
                    *tab.page_versions.entry(page).or_insert(0) += 1;
                }
            }
            Response::AnnotAdded { doc, page, index } => {
                let tool = self.tool;
                if let Some(tab) = self.tab_mut(doc)
                    && tool == Tool::Note
                {
                    // Let the user type the note right away.
                    document_view::focus_annot(tab, page, index, true);
                }
            }
            Response::EditState {
                doc,
                can_undo,
                can_redo,
                dirty,
            } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.can_undo = can_undo;
                    tab.can_redo = can_redo;
                    tab.dirty = dirty;
                }
            }
            Response::Saved { doc, path } => {
                let mut renamed = None;
                if let Some(tab) = self.tab_mut(doc)
                    && tab.path != path
                {
                    tab.path = path.clone();
                    tab.name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    renamed = Some(path);
                }
                if let Some(path) = renamed {
                    self.settings.add_recent(&path);
                    self.settings.save();
                }
                if let Some(intent) = &mut self.close_intent
                    && intent.awaiting_save.remove(&doc)
                    && intent.awaiting_save.is_empty()
                {
                    let intent = self.close_intent.take().unwrap();
                    self.finish_close(&intent.docs, intent.quit);
                }
            }
            Response::PagesChanged { doc, info, mapping } => {
                self.cache.remap(doc, &mapping);
                if let Some(tab) = self.tab_mut(doc) {
                    tab.apply_page_mapping(info, &mapping);
                    // Annotation and field positions are re-read for the new pages.
                    self.send(Request::ScanAnnots { doc });
                }
            }
            Response::Exported { message } => self.notice(&message),
            Response::PageWidgets { doc, page, widgets } => {
                if let Some(tab) = self.tab_mut(doc) {
                    if widgets.is_empty() {
                        tab.widgets.remove(&page);
                    } else {
                        tab.widgets.insert(page, widgets);
                    }
                }
            }
            Response::FormFocus { doc, page, .. } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.form_focus = page;
                }
            }
            Response::Clipboard { text } => self.ctx.copy_text(text),
            Response::PrintReady { job, bytes, .. } => {
                let tx = self.print_results.0.clone();
                let ctx = self.ctx.clone();
                self.notice("Sending to the printer…");
                std::thread::spawn(move || {
                    let _ = tx.send(crate::print::submit(&job, &bytes));
                    ctx.request_repaint();
                });
            }
            Response::Printed { pages } => {
                if let Some(n) = pages {
                    self.notice(&format!("Sent {n} page(s) to the printer"));
                }
            }
            Response::Error { doc, message } => {
                log::warn!("doc {doc}: {message}");
                if self
                    .close_intent
                    .as_ref()
                    .is_some_and(|i| i.awaiting_save.contains(&doc))
                {
                    // A save we were waiting for failed: abort closing.
                    self.close_intent = None;
                }
                self.error = Some(message);
            }
        }
    }

    /// Carries out a page operation on the active document.
    fn page_action(&mut self, action: PageAction) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(info) = tab.info() else { return };
        let (doc, count) = (tab.id, info.page_sizes.len());
        let targets = tab.target_pages();
        let last = *targets.iter().max().unwrap_or(&tab.current_page);
        let stem = tab
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let dir = tab.path.parent().map(Path::to_path_buf);
        let edit = |op| Request::EditPages { doc, op };
        let req = match action {
            PageAction::Rotate(q) => Some(edit(PageOp::Rotate {
                pages: targets,
                quarter_turns: q,
            })),
            PageAction::Delete => Some(edit(PageOp::Delete { pages: targets })),
            PageAction::MoveToStart => Some(edit(PageOp::Move { pages: targets, to: 0 })),
            PageAction::MoveToEnd => {
                let to = count - targets.len();
                Some(edit(PageOp::Move { pages: targets, to }))
            }
            PageAction::MoveTo { pages, gap } => {
                move_target(&pages, gap, count).map(|to| edit(PageOp::Move { pages, to }))
            }
            PageAction::InsertBlankAfter => Some(edit(PageOp::InsertBlank {
                at: last + 1,
                size: info.page_sizes[last.min(count - 1)],
            })),
            PageAction::InsertFileAfter => {
                let mut dialog = rfd::FileDialog::new().add_filter("PDF documents", &["pdf", "PDF"]);
                if let Some(d) = &dir {
                    dialog = dialog.set_directory(d);
                }
                dialog.pick_file().map(|path| Request::InsertFile { doc, path, at: last + 1 })
            }
            PageAction::InsertFiles { gap, paths } => {
                // Insert in reverse so the files end up in the order they were dropped.
                for path in paths.into_iter().rev() {
                    self.send(Request::InsertFile { doc, path, at: gap });
                }
                None
            }
            PageAction::Extract => {
                let mut dialog = rfd::FileDialog::new()
                    .add_filter("PDF documents", &["pdf"])
                    .set_file_name(format!("{stem}-pages.pdf"));
                if let Some(d) = &dir {
                    dialog = dialog.set_directory(d);
                }
                dialog.save_file().map(|path| Request::ExtractPages {
                    doc,
                    pages: targets,
                    path,
                })
            }
            PageAction::Split => {
                self.split_every = Some(1);
                None
            }
        };
        if let Some(r) = req {
            self.send(r);
        }
    }

    fn split_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut every) = self.split_every else { return };
        let Some(tab) = self.tabs.get(self.active) else {
            self.split_every = None;
            return;
        };
        let (doc, count) = (tab.id, tab.page_count());
        let stem = tab
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let dir = tab.path.parent().map(Path::to_path_buf);
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Split document")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Pages per file");
                    ui.add(egui::DragValue::new(&mut every).range(1..=count.max(1)));
                });
                let files = count.div_ceil(every.max(1));
                ui.weak(format!("Creates {files} files named “{stem}-01.pdf”, “{stem}-02.pdf”, …"));
                ui.horizontal(|ui| {
                    go = ui.button("Choose folder…").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        self.split_every = Some(every);
        if cancel {
            self.split_every = None;
        } else if go {
            let mut dialog = rfd::FileDialog::new();
            if let Some(d) = &dir {
                dialog = dialog.set_directory(d);
            }
            if let Some(dir) = dialog.pick_folder() {
                self.split_every = None;
                self.send(Request::SplitDocument { doc, every, dir, stem });
            }
        }
    }

    /// Windows: opens the Default apps page for magic-pdf (Windows does not let apps
    /// change defaults themselves). Linux: sets the default via xdg-mime.
    fn make_default_viewer(&mut self) {
        if cfg!(windows) {
            if let Err(e) = open::that_detached("ms-settings:defaultapps?registeredAppUser=magic-pdf") {
                self.error = Some(format!("Could not open Windows Settings: {e}"));
            }
            return;
        }
        let result = std::process::Command::new("xdg-mime")
            .args(["default", "magic-pdf.desktop", "application/pdf"])
            .status();
        match result {
            Ok(s) if s.success() => self.notice("magic-pdf is now the default PDF viewer"),
            Ok(_) | Err(_) => {
                self.error = Some(
                    "Could not set the default PDF viewer. Install magic-pdf with \
                     packaging/linux/install.sh first."
                        .into(),
                )
            }
        }
    }

    fn notice(&mut self, text: &str) {
        let now = self.ctx.input(|i| i.time);
        self.notice = Some((text.to_string(), now));
    }

    /// Opens the print dialog (CUPS) or the native one (Windows).
    fn print(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.info().is_none() {
            return;
        }
        if cfg!(windows) {
            let req = Request::PrintNative {
                doc: tab.id,
                title: tab.name.clone(),
                current_page: tab.current_page,
            };
            self.send(req);
            return;
        }
        let doc = tab.id;
        match crate::print::list_printers() {
            Ok(p) if p.names.is_empty() => self.error = Some("No printers are set up.".into()),
            Ok(p) => {
                let printer = p.default.clone().or_else(|| p.names.first().cloned());
                self.print_dialog = Some(PrintDialog {
                    doc,
                    printers: p.names,
                    printer,
                    copies: 1,
                    pages: PageChoice::All,
                    range: String::new(),
                    two_sided: false,
                    fit_to_page: true,
                    error: None,
                });
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn print_dialog_ui(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.print_results.1.try_recv() {
            match result {
                Ok(msg) if msg.is_empty() => self.notice("Sent to the printer"),
                Ok(msg) => self.notice(&format!("Sent to the printer ({msg})")),
                Err(e) => self.error = Some(format!("Printing failed: {e}")),
            }
        }
        let Some(dlg) = &mut self.print_dialog else { return };
        let Some(tab) = self.tabs.iter().find(|t| t.id == dlg.doc) else {
            self.print_dialog = None;
            return;
        };
        let (count, current, title) = (tab.page_count(), tab.current_page, tab.name.clone());
        let mut submit = None;
        let mut close = false;
        egui::Window::new("Print")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("print_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Printer");
                    egui::ComboBox::from_id_salt("printer")
                        .selected_text(dlg.printer.clone().unwrap_or_default())
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            for p in &dlg.printers {
                                ui.selectable_value(&mut dlg.printer, Some(p.clone()), p);
                            }
                        });
                    ui.end_row();
                    ui.label("Copies");
                    ui.add(egui::DragValue::new(&mut dlg.copies).range(1..=99));
                    ui.end_row();
                    ui.label("Pages");
                    ui.vertical(|ui| {
                        ui.radio_value(&mut dlg.pages, PageChoice::All, format!("All ({count})"));
                        ui.radio_value(&mut dlg.pages, PageChoice::Current, format!("Current page ({})", current + 1));
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut dlg.pages, PageChoice::Range, "Pages:");
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut dlg.range)
                                    .hint_text("e.g. 1-3, 5")
                                    .desired_width(110.0),
                            );
                            if r.changed() {
                                dlg.pages = PageChoice::Range;
                            }
                        });
                    });
                    ui.end_row();
                    ui.label("Options");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut dlg.two_sided, "Print on both sides");
                        ui.checkbox(&mut dlg.fit_to_page, "Fit to paper");
                    });
                    ui.end_row();
                });
                if let Some(e) = &dlg.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Print").clicked() {
                        let pages = match dlg.pages {
                            PageChoice::All => Ok(None),
                            PageChoice::Current => Ok(Some((current + 1).to_string())),
                            PageChoice::Range => crate::print::normalize_ranges(&dlg.range, count).map(Some),
                        };
                        match pages {
                            Ok(pages) => {
                                submit = Some(PrintJob {
                                    printer: dlg.printer.clone(),
                                    copies: dlg.copies,
                                    pages,
                                    two_sided: dlg.two_sided,
                                    fit_to_page: dlg.fit_to_page,
                                    title: title.clone(),
                                })
                            }
                            Err(e) => dlg.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if let Some(job) = submit {
            let doc = dlg.doc;
            self.print_dialog = None;
            self.send(Request::ExportForPrint { doc, job });
        } else if close {
            self.print_dialog = None;
        }
    }

    /// Sends keyboard input to the focused form field. Runs on the raw input before
    /// egui sees it, so egui neither moves its own focus on Tab nor fires shortcuts.
    /// Does nothing when no field has focus or an egui text box is being typed into.
    fn route_form_keys(&mut self, ctx: &egui::Context, events: &mut Vec<egui::Event>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.form_focus.is_none() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let doc = tab.id;
        let (mut reqs, kill_focus) = take_form_input(events, doc);
        if kill_focus {
            reqs.push(Request::FormKillFocus { doc });
            if let Some(t) = self.tabs.get_mut(self.active) {
                t.form_focus = None;
            }
        }
        for r in reqs {
            self.send(r);
        }
    }

    fn start_search(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let query = tab.search.query.trim().to_string();
        tab.search.generation += 1;
        tab.search.hits.clear();
        tab.search.current = None;
        if query.is_empty() {
            tab.search.submitted = None;
            tab.search.running = false;
            let doc = tab.id;
            self.send(Request::CancelSearch { doc });
            return;
        }
        tab.search.submitted = Some(query.clone());
        tab.search.running = true;
        let req = Request::Search {
            doc: tab.id,
            generation: tab.search.generation,
            query: SearchQuery {
                text: query,
                match_case: tab.search.match_case,
                whole_word: tab.search.whole_word,
            },
        };
        self.sidebar_tab = SidebarTab::Search;
        self.send(req);
    }

    fn clear_search(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.search.hits.clear();
            tab.search.current = None;
            tab.search.submitted = None;
            tab.search.running = false;
            let doc = tab.id;
            self.send(Request::CancelSearch { doc });
        }
    }

    /// Sends requests the views produced, render requests for what they asked for this
    /// frame, and cancels renders nobody wants any more.
    fn flush_requests(&mut self) {
        let Ok(engine) = &self.engine else { return };
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            for req in tab.outbox.drain(..) {
                engine.send(req);
            }
            if i != self.active {
                tab.wanted.clear();
            }
            let mut wanted: HashSet<_> = HashSet::with_capacity(tab.wanted.len());
            for &(key, priority) in &tab.wanted {
                wanted.insert(key);
                if self.cache.contains(&key) {
                    self.cache.touch(&key);
                } else if tab.requested.insert(key) {
                    engine.send(Request::Render { key, priority });
                }
            }
            if wanted != tab.last_wanted {
                engine.send(Request::Retain {
                    doc: tab.id,
                    keep: wanted.clone(),
                });
                tab.requested.retain(|k| wanted.contains(k));
                tab.last_wanted = wanted;
            }
            tab.wanted.clear();
        }
    }

    fn set_night_mode(&mut self, on: bool) {
        self.settings.night_mode = on;
        self.cache.invert = on;
        self.cache.clear();
        for tab in &mut self.tabs {
            tab.requested.clear();
            tab.last_wanted.clear();
        }
        self.settings.save();
    }

    fn undo(&mut self, redo: bool) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let doc = tab.id;
            tab.deselect_annot();
            self.send(if redo { Request::Redo { doc } } else { Request::Undo { doc } });
        }
    }

    fn delete_selected_annot(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(sel) = tab.selected_annot.take()
        {
            let req = Request::DeleteAnnot {
                doc: tab.id,
                page: sel.page,
                index: sel.index,
            };
            self.send(req);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let shortcut = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));

        if shortcut(cmd, Key::O) {
            self.open_dialog();
        }
        if shortcut(cmd, Key::P) {
            self.print();
        }
        if shortcut(cmd | Modifiers::SHIFT, Key::S) {
            self.save_active(true);
        } else if shortcut(cmd, Key::S) {
            self.save_active(false);
        }
        if shortcut(cmd, Key::W)
            && let Some(t) = self.tabs.get(self.active)
        {
            self.request_close(vec![t.id], false);
        }
        if shortcut(cmd, Key::Q) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if !self.tabs.is_empty() {
            if shortcut(cmd | Modifiers::SHIFT, Key::Tab) {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
            } else if shortcut(cmd, Key::Tab) {
                self.active = (self.active + 1) % self.tabs.len();
            }
        }
        if shortcut(cmd, Key::F) {
            self.focus_search = true;
        }
        if shortcut(Modifiers::NONE, Key::F9) {
            self.settings.show_sidebar = !self.settings.show_sidebar;
        }
        if shortcut(Modifiers::SHIFT, Key::F3) {
            if let Some(t) = self.active_tab() {
                t.search_step(false);
            }
        } else if shortcut(Modifiers::NONE, Key::F3)
            && let Some(t) = self.active_tab()
        {
            t.search_step(true);
        }

        let typing = ctx.egui_wants_keyboard_input();
        if !typing {
            if shortcut(cmd | Modifiers::SHIFT, Key::Z) || shortcut(cmd, Key::Y) {
                self.undo(true);
            } else if shortcut(cmd, Key::Z) {
                self.undo(false);
            }
            if shortcut(Modifiers::NONE, Key::Delete) || shortcut(Modifiers::NONE, Key::Backspace) {
                let delete_pages = self
                    .tabs
                    .get(self.active)
                    .is_some_and(|t| t.selected_annot.is_none() && t.pages_focused && !t.page_selection.is_empty());
                if delete_pages {
                    self.page_action(PageAction::Delete);
                } else {
                    self.delete_selected_annot();
                }
            }
            for (key, tool) in [
                (Key::H, Tool::Hand),
                (Key::S, Tool::SelectText),
                (Key::U, Tool::Highlight),
                (Key::N, Tool::Note),
                (Key::D, Tool::Ink),
                (Key::R, Tool::Rect),
                (Key::E, Tool::Ellipse),
            ] {
                if shortcut(Modifiers::NONE, key) {
                    self.tool = tool;
                }
            }
        }

        // Esc cancels whatever is in progress and returns to the Hand tool.
        if !typing && shortcut(Modifiers::NONE, Key::Escape) {
            self.tool = Tool::Hand;
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.cancel_interaction();
            }
        }

        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        let Some(tab) = self.tabs.get_mut(self.active) else { return };

        if !typing
            && copy
            && let Some(text) = tab.selected_text()
        {
            ctx.copy_text(text);
        }
        if !typing && shortcut(cmd, Key::A) {
            tab.select_all_on_page();
        }
        if shortcut(cmd, Key::Plus) || shortcut(cmd, Key::Equals) {
            tab.zoom_step(true);
        }
        if shortcut(cmd, Key::Minus) {
            tab.zoom_step(false);
        }
        if shortcut(cmd, Key::Num0) {
            tab.set_zoom(ACTUAL_SIZE);
        }
        if shortcut(cmd | Modifiers::SHIFT, Key::R) {
            tab.rotate(false);
        } else if shortcut(cmd, Key::R) {
            tab.rotate(true);
        }
        for (key, mode) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5]
            .into_iter()
            .zip(LayoutMode::ALL)
        {
            if shortcut(cmd, key) {
                tab.set_mode(mode);
            }
        }
        if typing {
            return;
        }
        let horizontal = tab.mode.is_horizontal();
        let single = tab.mode == LayoutMode::SinglePage;
        let view = tab.view_rect;
        let step = 60.0;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                tab.step_page(true);
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                tab.step_page(false);
            }
            if i.consume_key(Modifiers::NONE, Key::Home) {
                tab.goto_page(0);
            }
            if i.consume_key(Modifiers::NONE, Key::End) {
                tab.goto_page(usize::MAX);
            }
            let screen = if horizontal { view.width() } else { view.height() } * 0.9;
            let along = |d: f32| if horizontal { egui::vec2(d, 0.0) } else { egui::vec2(0.0, d) };
            if i.consume_key(Modifiers::SHIFT, Key::Space) {
                tab.pending_scroll += along(screen);
            }
            if i.consume_key(Modifiers::NONE, Key::Space) {
                tab.pending_scroll -= along(screen);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                if single {
                    tab.step_page(false);
                } else {
                    tab.pending_scroll.x += step;
                }
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                if single {
                    tab.step_page(true);
                } else {
                    tab.pending_scroll.x -= step;
                }
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                tab.pending_scroll.y += step;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                tab.pending_scroll.y -= step;
            }
        });
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            let has_tab = self.tabs.get(self.active).is_some_and(|t| t.info().is_some());
            ui.menu_button("File", |ui| {
                if ui.button("Open…\tCtrl+O").clicked() {
                    self.open_dialog();
                }
                let recent = self.settings.recent.clone();
                ui.add_enabled_ui(!recent.is_empty(), |ui| {
                    ui.menu_button("Open recent", |ui| {
                        for path in recent {
                            if ui.button(path.display().to_string()).clicked() {
                                self.open(&path);
                            }
                        }
                    });
                });
                ui.separator();
                if ui.add_enabled(has_tab, egui::Button::new("Save\tCtrl+S")).clicked() {
                    self.save_active(false);
                }
                if ui
                    .add_enabled(has_tab, egui::Button::new("Save as…\tCtrl+Shift+S"))
                    .clicked()
                {
                    self.save_active(true);
                }
                if ui.add_enabled(has_tab, egui::Button::new("Print…\tCtrl+P")).clicked() {
                    self.print();
                }
                ui.separator();
                if ui
                    .add_enabled(!self.tabs.is_empty(), egui::Button::new("Close tab\tCtrl+W"))
                    .clicked()
                    && let Some(t) = self.tabs.get(self.active)
                {
                    self.request_close(vec![t.id], false);
                }
                if ui.button("Quit\tCtrl+Q").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                let (can_undo, can_redo, has_sel) = self
                    .tabs
                    .get(self.active)
                    .map(|t| (t.can_undo, t.can_redo, t.selected_annot.is_some()))
                    .unwrap_or_default();
                if ui.add_enabled(can_undo, egui::Button::new("Undo\tCtrl+Z")).clicked() {
                    self.undo(false);
                }
                if ui.add_enabled(can_redo, egui::Button::new("Redo\tCtrl+Y")).clicked() {
                    self.undo(true);
                }
                ui.separator();
                if ui
                    .add_enabled(has_sel, egui::Button::new("Delete annotation\tDel"))
                    .clicked()
                {
                    self.delete_selected_annot();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Author name");
                    if ui
                        .add(egui::TextEdit::singleline(&mut self.settings.author).desired_width(140.0))
                        .lost_focus()
                    {
                        self.settings.save();
                    }
                });
            });
            ui.menu_button("Pages", |ui| {
                let (n, total) = self
                    .tabs
                    .get(self.active)
                    .filter(|t| t.info().is_some())
                    .map(|t| (t.target_pages().len(), t.page_count()))
                    .unwrap_or((0, 0));
                ui.add_enabled_ui(total > 0, |ui| {
                    ui.weak(if n == 1 {
                        "Acts on the selected (or current) page".to_string()
                    } else {
                        format!("Acts on {n} selected pages")
                    });
                    if let Some(a) = sidebar::page_menu(ui, n, total) {
                        self.page_action(a);
                    }
                });
            });
            ui.menu_button("View", |ui| {
                for (i, mode) in LayoutMode::ALL.into_iter().enumerate() {
                    let selected = self.tabs.get(self.active).is_some_and(|t| t.mode == mode);
                    if ui
                        .add_enabled(
                            has_tab,
                            egui::Button::selectable(selected, format!("{}\tCtrl+{}", mode.label(), i + 1)),
                        )
                        .clicked()
                        && let Some(t) = self.active_tab()
                    {
                        t.set_mode(mode);
                    }
                }
                ui.separator();
                ui.menu_button("Default layout for new files", |ui| {
                    for mode in LayoutMode::ALL {
                        if ui
                            .selectable_label(self.settings.default_mode == mode, mode.label())
                            .clicked()
                        {
                            self.settings.default_mode = mode;
                            self.settings.save();
                        }
                    }
                });
                ui.separator();
                for fit in [FitMode::Width, FitMode::Height, FitMode::Page] {
                    if ui.add_enabled(has_tab, egui::Button::new(fit.label())).clicked()
                        && let Some(t) = self.active_tab()
                    {
                        t.viewport.fit = fit;
                    }
                }
                if ui.add_enabled(has_tab, egui::Button::new("Actual size\tCtrl+0")).clicked()
                    && let Some(t) = self.active_tab()
                {
                    t.set_zoom(ACTUAL_SIZE);
                }
                ui.separator();
                if ui
                    .add_enabled(has_tab, egui::Button::new("Rotate clockwise\tCtrl+R"))
                    .clicked()
                    && let Some(t) = self.active_tab()
                {
                    t.rotate(true);
                }
                if ui
                    .add_enabled(has_tab, egui::Button::new("Rotate counter-clockwise\tCtrl+Shift+R"))
                    .clicked()
                    && let Some(t) = self.active_tab()
                {
                    t.rotate(false);
                }
                ui.separator();
                ui.checkbox(&mut self.settings.show_sidebar, "Sidebar\tF9");
                let mut night = self.settings.night_mode;
                if ui.checkbox(&mut night, "Night mode (invert pages)").changed() {
                    self.set_night_mode(night);
                }
                let mut dark = self.settings.dark_ui;
                if ui.checkbox(&mut dark, "Dark interface").changed() {
                    self.settings.dark_ui = dark;
                    ui.ctx().set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
                    self.settings.save();
                }
            });
            ui.menu_button("Help", |ui| {
                if cfg!(any(windows, target_os = "linux")) && ui.button("Make magic-pdf the default PDF viewer…").clicked() {
                    self.make_default_viewer();
                }
                if ui.button("Keyboard shortcuts").clicked() {
                    self.show_shortcuts = true;
                }
                if ui.button("About magic-pdf").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button(icons::text(icons::FOLDER_OPEN)).on_hover_text("Open a PDF (Ctrl+O)").clicked() {
                self.open_dialog();
            }
            let (has_doc, dirty, can_undo, can_redo) = self
                .tabs
                .get(self.active)
                .map(|t| (t.info().is_some(), t.dirty, t.can_undo, t.can_redo))
                .unwrap_or_default();
            if ui
                .add_enabled(has_doc && dirty, egui::Button::new(icons::text(icons::FLOPPY_DISK)))
                .on_hover_text("Save (Ctrl+S)")
                .clicked()
            {
                self.save_active(false);
            }
            if ui
                .add_enabled(has_doc, egui::Button::new(icons::text(icons::PRINTER)))
                .on_hover_text("Print (Ctrl+P)")
                .clicked()
            {
                self.print();
            }
            if ui
                .add_enabled(can_undo, egui::Button::new(icons::text(icons::UNDO)))
                .on_hover_text("Undo (Ctrl+Z)")
                .clicked()
            {
                self.undo(false);
            }
            if ui
                .add_enabled(can_redo, egui::Button::new(icons::text(icons::REDO)))
                .on_hover_text("Redo (Ctrl+Y)")
                .clicked()
            {
                self.undo(true);
            }
            ui.separator();
            for tool in [Tool::Hand, Tool::SelectText] {
                ui.selectable_value(&mut self.tool, tool, tool.icon())
                    .on_hover_text(tool.label());
            }
            ui.separator();
            for tool in Tool::ANNOTATION_TOOLS {
                ui.selectable_value(&mut self.tool, tool, tool.icon())
                    .on_hover_text(tool.label());
            }
            self.tool_style(ui);
            ui.separator();

            let Some(tab) = self.tabs.get_mut(self.active) else { return };
            let ready = tab.info().is_some();
            ui.add_enabled_ui(ready, |ui| {
                let n = tab.page_count();
                if ui.button(icons::text(icons::CARET_LEFT)).on_hover_text("Previous page (PgUp)").clicked() {
                    tab.step_page(false);
                }
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut tab.page_input)
                        .desired_width(40.0)
                        .align(egui::Align2::CENTER_CENTER),
                );
                if resp.lost_focus() {
                    if ui.input(|i| i.key_pressed(Key::Enter))
                        && let Ok(p) = tab.page_input.trim().parse::<usize>()
                    {
                        tab.goto_page(p.max(1) - 1);
                    }
                    tab.page_input = (tab.current_page + 1).to_string();
                }
                ui.label(format!("/ {n}"));
                if ui.button(icons::text(icons::CARET_RIGHT)).on_hover_text("Next page (PgDn)").clicked() {
                    tab.step_page(true);
                }
                ui.separator();

                if ui.button(icons::text(icons::ZOOM_OUT)).on_hover_text("Zoom out (Ctrl+-)").clicked() {
                    tab.zoom_step(false);
                }
                let label = match tab.viewport.fit {
                    FitMode::None => format!("{:.0}%", tab.zoom_percent()),
                    fit => format!("{} ({:.0}%)", fit.label(), tab.zoom_percent()),
                };
                egui::ComboBox::from_id_salt("zoom")
                    .selected_text(label)
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for fit in [FitMode::Width, FitMode::Height, FitMode::Page] {
                            if ui.selectable_label(tab.viewport.fit == fit, fit.label()).clicked() {
                                tab.viewport.fit = fit;
                            }
                        }
                        ui.separator();
                        for pct in ZOOM_PRESETS {
                            if ui.selectable_label(false, format!("{pct:.0}%")).clicked() {
                                tab.set_zoom(pct / 100.0 * ACTUAL_SIZE);
                            }
                        }
                    });
                if ui.button(icons::text(icons::ZOOM_IN)).on_hover_text("Zoom in (Ctrl++)").clicked() {
                    tab.zoom_step(true);
                }
                ui.separator();

                for (mode, icon) in LayoutMode::ALL.into_iter().zip([
                    icons::COLUMNS,
                    icons::ROWS,
                    icons::FILE,
                    icons::BOOK_OPEN,
                    icons::SQUARES_FOUR,
                ]) {
                    if ui
                        .selectable_label(tab.mode == mode, icons::text(icon))
                        .on_hover_text(format!("{} layout", mode.label()))
                        .clicked()
                    {
                        tab.set_mode(mode);
                    }
                }
            });

            // Search, right-aligned.
            let mut submit = false;
            let mut clear = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let Some(tab) = self.tabs.get_mut(self.active) else { return };
                if ui.button(icons::text(icons::CARET_DOWN)).on_hover_text("Next match (F3)").clicked() {
                    tab.search_step(true);
                }
                if ui.button(icons::text(icons::CARET_UP)).on_hover_text("Previous match (Shift+F3)").clicked() {
                    tab.search_step(false);
                }
                let s = &mut tab.search;
                let before = (s.match_case, s.whole_word);
                ui.toggle_value(&mut s.whole_word, egui::RichText::new("ab").strong().underline())
                    .on_hover_text("Whole words");
                ui.toggle_value(&mut s.match_case, icons::text(icons::TEXT_AA))
                    .on_hover_text("Match case");
                if before != (s.match_case, s.whole_word) && s.submitted.is_some() {
                    submit = true;
                }
                if s.submitted.is_some() {
                    let status = match (s.current, s.hits.len()) {
                        (_, 0) if s.running => "…".to_string(),
                        (_, 0) => "No matches".to_string(),
                        (Some(c), n) => format!("{}/{}{}", c + 1, n, if s.running { "+" } else { "" }),
                        (None, n) => format!("{n}"),
                    };
                    ui.weak(status);
                }
                let id = egui::Id::unique(SEARCH_ID);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut s.query)
                        .id(id)
                        .hint_text("Search (Ctrl+F)")
                        .desired_width(160.0),
                );
                ui.label(icons::text(icons::MAGNIFYING_GLASS));
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if s.submitted.as_deref() == Some(s.query.trim()) {
                        tab.search_step(!shift);
                    } else {
                        submit = true;
                    }
                    resp.request_focus();
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    clear = true;
                    resp.surrender_focus();
                }
            });
            if submit {
                self.start_search();
            }
            if clear {
                self.clear_search();
            }
        });
    }

    /// Colour (and width) picker for the active annotation tool.
    fn tool_style(&mut self, ui: &mut Ui) {
        let tool = self.tool;
        let style = &mut self.settings.annot_style;
        let Some(color) = tool.color(style).copied() else { return };
        let mut changed = false;
        let resp = color_swatch(ui, color, false).on_hover_text("Colour for new annotations");
        egui::Popup::from_toggle_button_response(&resp)
            // Stay open while ticking Fill or picking colours.
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
            ui.horizontal(|ui| {
                for c in PALETTE {
                    if color_swatch(ui, c, c == color).clicked()
                        && let Some(slot) = tool.color(style)
                    {
                        *slot = c;
                        changed = true;
                    }
                }
            });
            if let Some(slot) = tool.color(style) {
                changed |= ui.color_edit_button_srgb(slot).changed();
            }
            match tool {
                Tool::Ink => {
                    changed |= ui
                        .add(egui::Slider::new(&mut style.ink_width, 0.5..=12.0).text("Width"))
                        .changed();
                }
                Tool::Rect | Tool::Ellipse => {
                    changed |= ui
                        .add(egui::Slider::new(&mut style.shape_width, 0.5..=12.0).text("Width"))
                        .changed();
                    let mut fill = style.shape_fill.is_some();
                    if ui.checkbox(&mut fill, "Fill").changed() {
                        style.shape_fill = fill.then(|| document_view::tint(style.shape));
                        changed = true;
                    }
                    if let Some(f) = &mut style.shape_fill {
                        ui.horizontal(|ui| {
                            for c in document_view::FILL_PALETTE {
                                if color_swatch(ui, c, *f == c).clicked() {
                                    *f = c;
                                    changed = true;
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Fill colour");
                            changed |= ui.color_edit_button_srgb(f).changed();
                        });
                    }
                }
                _ => {}
            }
        });
        if changed {
            self.settings.save();
        }
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tabbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let resp = ui
                        .selectable_label(i == self.active, tab.title())
                        .on_hover_text(tab.path.display().to_string());
                    if resp.clicked() {
                        self.active = i;
                    }
                    if resp.middle_clicked() {
                        close = Some(tab.id);
                    }
                    if ui.small_button(icons::sized(icons::X, 12.0)).on_hover_text("Close (Ctrl+W)").clicked() {
                        close = Some(tab.id);
                    }
                    ui.add_space(6.0);
                }
            });
        });
        if let Some(id) = close {
            self.request_close(vec![id], false);
        }
    }

    fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let LoadState::NeedsPassword { input, wrong } = &mut tab.state else { return };
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("Password required")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!("“{}” is protected by a password.", tab.name));
                if *wrong {
                    ui.colored_label(ui.visuals().error_fg_color, "Incorrect password, try again.");
                }
                let resp = ui.add(egui::TextEdit::singleline(input).password(true));
                resp.request_focus();
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                ui.horizontal(|ui| {
                    submit |= ui.button("Open").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if submit {
            let req = Request::Open {
                doc: tab.id,
                path: tab.path.clone(),
                password: Some(std::mem::take(input)),
            };
            self.send(req);
        } else if cancel {
            self.close_tab_now(self.active);
        }
    }

    /// "Save changes?" dialog for a pending close or quit.
    fn unsaved_dialog(&mut self, ctx: &egui::Context) {
        let Some(intent) = self.close_intent.clone() else { return };
        let dirty: Vec<(DocId, String)> = intent
            .docs
            .iter()
            .filter_map(|d| self.tabs.iter().find(|t| t.id == *d && t.dirty))
            .map(|t| (t.id, t.name.clone()))
            .collect();
        let waiting = !intent.awaiting_save.is_empty();
        let mut choice = None;
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                if dirty.len() == 1 {
                    ui.label(format!("Save changes to “{}” before closing?", dirty[0].1));
                } else {
                    ui.label("These documents have unsaved changes:");
                    for (_, name) in &dirty {
                        ui.label(format!("  • {name}"));
                    }
                }
                ui.add_space(8.0);
                if waiting {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Saving…");
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        choice = Some(0);
                    }
                    if ui.button("Don't save").clicked() {
                        choice = Some(1);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(2);
                    }
                });
            });
        match choice {
            Some(0) => {
                let mut awaiting = HashSet::new();
                for (doc, _) in &dirty {
                    if let Some(t) = self.tabs.iter().find(|t| t.id == *doc) {
                        awaiting.insert(*doc);
                        self.send(Request::Save {
                            doc: *doc,
                            path: t.path.clone(),
                        });
                    }
                }
                if let Some(i) = &mut self.close_intent {
                    i.awaiting_save = awaiting;
                }
            }
            Some(1) => {
                self.close_intent = None;
                self.finish_close(&intent.docs, intent.quit);
            }
            Some(2) => self.close_intent = None,
            _ => {}
        }
    }

    fn error_dialog(&mut self, ctx: &egui::Context) {
        let Some(message) = self.error.clone() else { return };
        let mut ok = false;
        egui::Window::new("Error")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(message);
                ok = ui.button("OK").clicked();
            });
        if ok {
            self.error = None;
        }
    }

    fn info_windows(&mut self, ctx: &egui::Context) {
        egui::Window::new("Keyboard shortcuts")
            .open(&mut self.show_shortcuts)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("shortcuts").striped(true).show(ui, |ui| {
                    for (k, v) in [
                        ("Ctrl+O / Ctrl+S / Ctrl+Shift+S", "Open, save, save as"),
                        ("Ctrl+P", "Print"),
                        ("Tab / Esc in a form field", "Next field / stop editing"),
                        ("Ctrl+W / Ctrl+Tab", "Close / switch tab"),
                        ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
                        ("Wheel", "Scroll (horizontally in horizontal layout)"),
                        ("Shift+Wheel", "Scroll along the other axis"),
                        ("Ctrl+Wheel, Ctrl+± , Ctrl+0", "Zoom, actual size"),
                        ("PgUp / PgDn, Home / End", "Previous / next, first / last page"),
                        ("Space / Shift+Space", "Scroll one screen"),
                        ("Ctrl+1 … Ctrl+5", "Horizontal, vertical, single, facing, grid"),
                        ("Ctrl+R / Ctrl+Shift+R", "Rotate view"),
                        ("H / S", "Hand tool / text selection tool"),
                        ("U / N / D / R / E", "Highlight, note, draw, rectangle, ellipse"),
                        ("Del", "Delete the selected annotation or thumbnail pages"),
                        ("Ctrl / Shift + click thumbnail", "Select several pages"),
                        ("Drag thumbnails", "Reorder pages"),
                        ("Ctrl+C / Ctrl+A", "Copy selection / select page text"),
                        ("Ctrl+F, Enter, F3 / Shift+F3", "Search, next / previous match"),
                        ("F9", "Toggle sidebar"),
                        ("Middle-drag", "Pan with any tool"),
                    ] {
                        ui.strong(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            });
        egui::Window::new("About magic-pdf")
            .open(&mut self.show_about)
            .resizable(false)
            .show(ctx, |ui| {
                ui.heading(format!("magic-pdf {}", env!("CARGO_PKG_VERSION")));
                ui.label("A lightweight PDF viewer built for wide screens.");
                ui.label("Rendering by PDFium; UI by egui. Licensed under the AGPL-3.0.");
            });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let gap = self.tabs.get_mut(self.active).and_then(|t| t.thumb_drop_gap.take());
        match gap {
            // Dropped between thumbnails: insert the pages there.
            Some(gap) => self.page_action(PageAction::InsertFiles { gap, paths: dropped }),
            None => {
                for path in dropped {
                    self.open(&path);
                }
            }
        }
    }

    /// Intercepts window close when there are unsaved documents.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.allow_quit {
            return;
        }
        if self.tabs.iter().any(|t| t.dirty) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.close_intent.is_none() {
                let docs = self.tabs.iter().map(|t| t.id).collect();
                self.request_close(docs, true);
            }
        }
    }

    fn self_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot_to.clone() else { return };
        if self.screenshot_requested {
            return;
        }
        if ctx.input(|i| i.time) < 3.0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        self.screenshot_requested = true;
        let close_ctx = ctx.clone();
        let callback = egui::ScreenshotCallback::new(move |image| {
            // Binary PPM: trivial to write, readable by most image tools.
            let mut out = format!("P6\n{} {}\n255\n", image.size[0], image.size[1]).into_bytes();
            for px in &image.pixels {
                out.extend_from_slice(&[px.r(), px.g(), px.b()]);
            }
            if let Err(e) = std::fs::write(&path, out) {
                log::error!("screenshot: {e}");
            }
            close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            close_ctx.request_repaint();
        });
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(callback));
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.heading("magic-pdf");
            ui.add_space(8.0);
            if let Err(e) = &self.engine {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            if ui.button("Open a PDF…").clicked() {
                self.open_dialog();
            }
            ui.weak("or drop files here");
            if !self.settings.recent.is_empty() {
                ui.add_space(16.0);
                ui.strong("Recent files");
                for path in self.settings.recent.clone() {
                    if ui.link(path.display().to_string()).clicked() {
                        self.open(&path);
                    }
                }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.cache.begin_frame();
        self.process_responses();
        self.handle_close_request(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if !self.tabs.is_empty() {
            egui::Panel::top("tabs").show(ui, |ui| self.tab_bar(ui));
        }

        if let Some(tab) = self.tabs.get(self.active) {
            let status = match &tab.state {
                LoadState::Ready(info) => format!(
                    "Page {} of {}   ·   {}   ·   {:.0}%{}{}",
                    tab.current_page + 1,
                    info.page_sizes.len(),
                    tab.mode.label(),
                    tab.zoom_percent(),
                    info.title.as_ref().map(|t| format!("   ·   {t}")).unwrap_or_default(),
                    if tab.dirty { "   ·   unsaved changes" } else { "" }
                ),
                LoadState::Loading => "Loading…".into(),
                LoadState::NeedsPassword { .. } => "Password required".into(),
                LoadState::Failed(_) => "Failed to open".into(),
            };
            let now = ctx.input(|i| i.time);
            let notice = self
                .notice
                .as_ref()
                .filter(|(_, t)| now - t < 6.0)
                .map(|(n, _)| n.clone());
            if notice.is_some() {
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.weak(status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match notice {
                        Some(n) => {
                            ui.strong(n);
                        }
                        None => {
                            ui.weak(tab.path.display().to_string());
                        }
                    });
                });
            });
        }

        let mut page_actions = Vec::new();
        if self.settings.show_sidebar && self.tabs.get(self.active).is_some_and(|t| t.info().is_some()) {
            let tab = &mut self.tabs[self.active];
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(190.0)
                .size_range(120.0..=500.0)
                .show(ui, |ui| sidebar::show(ui, tab, &mut self.sidebar_tab, &mut self.cache, &mut page_actions));
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.tabs.is_empty() {
                self.welcome(ui);
                return;
            }
            let tab = &mut self.tabs[self.active];
            document_view::show(
                ui,
                tab,
                ViewContext {
                    cache: &mut self.cache,
                    tool: self.tool,
                    night: self.settings.night_mode,
                    style: &self.settings.annot_style,
                    author: &self.settings.author,
                },
            );
        });

        for a in page_actions {
            self.page_action(a);
        }
        self.split_dialog(&ctx);
        self.password_dialog(&ctx);
        self.unsaved_dialog(&ctx);
        self.print_dialog_ui(&ctx);
        self.error_dialog(&ctx);
        self.info_windows(&ctx);
        self.flush_requests();
        self.cache.evict();

        let title = match self.tabs.get(self.active) {
            Some(t) => format!("{} — magic-pdf", t.title()),
            None => "magic-pdf".into(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        self.self_screenshot(&ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.route_form_keys(ctx, &mut raw_input.events);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for tab in &self.tabs {
            if tab.info().is_some() {
                self.settings.set_file_state(&tab.path, tab.file_state());
            }
        }
        self.settings.save();
    }
}

/// On Windows, `canonicalize` returns `\\?\C:\…` paths; show and store the
/// familiar `C:\…` form instead (network paths keep their prefix).
fn simplify_path(path: PathBuf) -> PathBuf {
    if cfg!(windows)
        && let Some(rest) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && !rest.starts_with("UNC\\")
    {
        return PathBuf::from(rest);
    }
    path
}

/// Removes the keyboard input meant for a focused form field from `events` and turns it
/// into engine requests. Returns the requests and whether Esc asked to leave the field.
///
/// Plain key presses all belong to the field, so single-letter tool shortcuts cannot
/// fire while typing. Ctrl shortcuts other than the field's own editing commands
/// (save, print, …) and the function keys stay in `events` for the app.
fn take_form_input(events: &mut Vec<egui::Event>, doc: DocId) -> (Vec<Request>, bool) {
    let mut reqs = Vec::new();
    let mut kill_focus = false;
    events.retain(|e| match e {
        egui::Event::Text(t) => {
            for c in t.chars() {
                reqs.push(Request::FormKey {
                    doc,
                    key: FormKey::Char(c as u32),
                    modifiers: 0,
                });
            }
            false
        }
        egui::Event::Paste(text) => {
            reqs.push(Request::FormPaste { doc, text: text.clone() });
            false
        }
        egui::Event::Copy => {
            reqs.push(Request::FormCopy { doc, cut: false });
            false
        }
        egui::Event::Cut => {
            reqs.push(Request::FormCopy { doc, cut: true });
            false
        }
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if modifiers.command || modifiers.ctrl {
                match key {
                    Key::A => reqs.push(Request::FormSelectAll { doc }),
                    Key::Z if modifiers.shift => reqs.push(Request::FormUndo { doc, redo: true }),
                    Key::Z => reqs.push(Request::FormUndo { doc, redo: false }),
                    Key::Y => reqs.push(Request::FormUndo { doc, redo: true }),
                    // Other shortcuts (save, print, …) keep working.
                    _ => return true,
                }
                return false;
            }
            if *key == Key::Escape {
                kill_focus = true;
                return false;
            }
            let Some(vk) = virtual_key(*key) else {
                // Letters and other keys arrive as Text; keep only function keys (F3, F9…).
                return is_function_key(*key);
            };
            let mods = (if modifiers.shift { form_mods::SHIFT } else { 0 })
                | (if modifiers.alt { form_mods::ALT } else { 0 });
            reqs.push(Request::FormKey {
                doc,
                key: FormKey::Down(vk),
                modifiers: mods,
            });
            // Keys that produce characters PDFium expects as WM_CHAR-style input.
            let ch = match key {
                Key::Backspace => Some(8),
                Key::Enter => Some(13),
                _ => None,
            };
            if let Some(c) = ch {
                reqs.push(Request::FormKey {
                    doc,
                    key: FormKey::Char(c),
                    modifiers: mods,
                });
            }
            false
        }
        _ => true,
    });
    (reqs, kill_focus)
}

fn is_function_key(key: Key) -> bool {
    key.name().strip_prefix('F').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Windows virtual-key codes, which PDFium's form module uses on every platform.
fn virtual_key(key: Key) -> Option<u32> {
    Some(match key {
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::Enter => 0x0D,
        Key::Space => 0x20,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::End => 0x23,
        Key::Home => 0x24,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn chars(reqs: &[Request]) -> String {
        reqs.iter()
            .filter_map(|r| match r {
                Request::FormKey {
                    key: FormKey::Char(c), ..
                } => char::from_u32(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn simplify_path_only_touches_windows_verbatim_paths() {
        let p = PathBuf::from("/home/u/a.pdf");
        assert_eq!(simplify_path(p.clone()), p);
        if cfg!(windows) {
            assert_eq!(simplify_path(PathBuf::from(r"\\?\C:\x\a.pdf")), PathBuf::from(r"C:\x\a.pdf"));
            let unc = PathBuf::from(r"\\?\UNC\server\share\a.pdf");
            assert_eq!(simplify_path(unc.clone()), unc);
        }
    }

    #[test]
    fn typing_letters_never_reaches_tool_shortcuts() {
        // Typing "He": each letter arrives as a key press plus a text event.
        let mut events = vec![
            key(Key::H, Modifiers::SHIFT),
            egui::Event::Text("H".into()),
            key(Key::E, Modifiers::NONE),
            egui::Event::Text("e".into()),
        ];
        let (reqs, kill) = take_form_input(&mut events, 1);
        assert!(events.is_empty(), "letter keys left for shortcuts: {events:?}");
        assert_eq!(chars(&reqs), "He");
        assert!(!kill);
    }

    #[test]
    fn app_shortcuts_and_function_keys_still_work() {
        let mut events = vec![
            key(Key::S, Modifiers::COMMAND),
            key(Key::P, Modifiers::COMMAND),
            key(Key::F3, Modifiers::NONE),
            egui::Event::PointerGone,
        ];
        let (reqs, _) = take_form_input(&mut events, 1);
        assert!(reqs.is_empty());
        assert_eq!(events.len(), 4);
    }

    /// Runs one egui frame containing a button with `events` and returns whether egui
    /// gave keyboard focus to a widget.
    fn egui_takes_focus(events: Vec<egui::Event>) -> bool {
        let ctx = egui::Context::default();
        let input = |events| egui::RawInput {
            events,
            ..Default::default()
        };
        let frame = |ctx: &egui::Context, events| {
            let mut output = ctx.run_ui(input(events), |ui| {
                let _ = ui.button("File");
            });
            output.textures_delta.clear();
        };
        frame(&ctx, Vec::new());
        frame(&ctx, events);
        ctx.memory(|m| m.focused().is_some())
    }

    #[test]
    fn tab_in_a_field_does_not_move_egui_focus() {
        let tab = vec![key(Key::Tab, Modifiers::NONE)];
        // Unfiltered, egui uses Tab to focus the next widget (the menu bar).
        assert!(egui_takes_focus(tab.clone()));
        // Filtered before egui sees it (as the raw input hook does), focus stays put
        // and the field gets the key instead.
        let mut events = tab;
        let (reqs, _) = take_form_input(&mut events, 1);
        assert!(matches!(reqs[0], Request::FormKey { key: FormKey::Down(0x09), .. }));
        assert!(!egui_takes_focus(events));
    }

    #[test]
    fn editing_keys_go_to_the_field() {
        let mut events = vec![
            key(Key::Backspace, Modifiers::NONE),
            key(Key::Delete, Modifiers::NONE),
            key(Key::Z, Modifiers::COMMAND),
            key(Key::Escape, Modifiers::NONE),
        ];
        let (reqs, kill) = take_form_input(&mut events, 1);
        assert!(events.is_empty());
        assert!(kill);
        assert!(matches!(reqs[0], Request::FormKey { key: FormKey::Down(0x08), .. }));
        assert!(matches!(reqs[1], Request::FormKey { key: FormKey::Char(8), .. }));
        assert!(matches!(reqs[2], Request::FormKey { key: FormKey::Down(0x2E), .. }));
        assert!(matches!(reqs[3], Request::FormUndo { redo: false, .. }));
    }
}
