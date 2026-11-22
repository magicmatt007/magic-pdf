//! Application shell: tabs, menus, toolbar, keyboard shortcuts and engine plumbing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, Ui};

use crate::document::{ACTUAL_SIZE, DocTab, LoadState, SearchHit, ZOOM_PRESETS};
use crate::engine::{DocId, Engine, Request, Response, SearchQuery};
use crate::settings::Settings;
use crate::view::cache::TextureCache;
use crate::view::document_view::{self, Tool, ViewContext};
use crate::view::layout::LayoutMode;
use crate::view::sidebar::{self, SidebarTab};
use crate::view::viewport::FitMode;

const TEXTURE_BUDGET: usize = 768 * 1024 * 1024;
const SEARCH_ID: &str = "search_field";

pub struct App {
    engine: Result<Engine, String>,
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
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(move || ctx.request_repaint());
        if let Err(e) = &engine {
            log::error!("{e}");
        }
        let settings = Settings::load();
        cc.egui_ctx.set_theme(if settings.dark_ui {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        let mut cache = TextureCache::new(TEXTURE_BUDGET);
        cache.invert = settings.night_mode;
        let mut app = Self {
            engine,
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
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let tab = DocTab::new(
            id,
            path.clone(),
            self.settings.default_mode,
            self.settings.file_state(&path),
        );
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
        if let Some(dir) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.path.parent())
            .or_else(|| self.settings.recent.first().and_then(|p| p.parent()))
        {
            dialog = dialog.set_directory(dir);
        }
        if let Some(files) = dialog.pick_files() {
            for f in files {
                self.open(&f);
            }
        }
    }

    fn close_tab(&mut self, index: usize) {
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

    fn process_responses(&mut self, ctx: &egui::Context) {
        let mut responses = Vec::new();
        if let Ok(engine) = &self.engine {
            while let Some(r) = engine.try_recv() {
                responses.push(r);
            }
        }
        for resp in responses {
            match resp {
                Response::Opened { doc, info } => {
                    if let Some(tab) = self.tab_mut(doc) {
                        if let Some(title) = &info.title {
                            log::info!("opened “{title}” ({} pages)", info.page_sizes.len());
                        }
                        tab.state = LoadState::Ready(info);
                        let page = tab.pending_goto.unwrap_or(0);
                        tab.goto_page(page);
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
                    if let Some(tab) = self.tab_mut(key.doc) {
                        tab.requested.remove(&key);
                        self.cache.insert(ctx, key, size, rgba);
                    }
                }
                Response::RenderFailed { key } => {
                    // Leave it in `requested` so it is not retried every frame.
                    log::debug!("render failed for {key:?}");
                }
                Response::PageText { doc, page, text } => {
                    if let Some(tab) = self.tab_mut(doc) {
                        tab.texts.insert(page, Some(text));
                    }
                }
                Response::SearchResults {
                    doc,
                    generation,
                    page,
                    hits,
                } => {
                    if let Some(tab) = self.tab_mut(doc)
                        && tab.search.generation == generation
                    {
                        let first = tab.search.hits.is_empty();
                        tab.search
                            .hits
                            .extend(hits.into_iter().map(|rects| SearchHit { page, rects }));
                        if first {
                            // Jump to the first hit at or after the current page once known.
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
            }
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

    /// Sends render requests for what the views asked for this frame and cancels the rest.
    fn sync_renders(&mut self) {
        let Ok(engine) = &self.engine else { return };
        for (i, tab) in self.tabs.iter_mut().enumerate() {
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

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let shortcut = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));

        if shortcut(cmd, Key::O) {
            self.open_dialog();
        }
        if shortcut(cmd, Key::W) {
            self.close_tab(self.active);
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
        let tool_hand = !typing && shortcut(Modifiers::NONE, Key::H);
        let tool_select = !typing && shortcut(Modifiers::NONE, Key::S);
        if tool_hand {
            self.tool = Tool::Hand;
        }
        if tool_select {
            self.tool = Tool::SelectText;
        }

        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        let Some(tab) = self.tabs.get_mut(self.active) else { return };

        if !typing && copy
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
        if shortcut(Modifiers::NONE, Key::Escape) {
            tab.selection = None;
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
            if i.consume_key(Modifiers::SHIFT, Key::Space) {
                tab.pending_scroll += if horizontal { egui::vec2(screen, 0.0) } else { egui::vec2(0.0, screen) };
            }
            if i.consume_key(Modifiers::NONE, Key::Space) {
                tab.pending_scroll -= if horizontal { egui::vec2(screen, 0.0) } else { egui::vec2(0.0, screen) };
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
                if ui
                    .add_enabled(!self.tabs.is_empty(), egui::Button::new("Close tab\tCtrl+W"))
                    .clicked()
                {
                    self.close_tab(self.active);
                }
                if ui.button("Quit\tCtrl+Q").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                let has_tab = !self.tabs.is_empty();
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
                if ui.add_enabled(has_tab, egui::Button::new("Rotate clockwise\tCtrl+R")).clicked()
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
            if ui.button("📂 Open").on_hover_text("Open a PDF (Ctrl+O)").clicked() {
                self.open_dialog();
            }
            ui.separator();
            ui.selectable_value(&mut self.tool, Tool::Hand, "✋ Hand")
                .on_hover_text("Pan by dragging (H)");
            ui.selectable_value(&mut self.tool, Tool::SelectText, "𝐈 Select")
                .on_hover_text("Select and copy text (S)");
            ui.separator();

            let Some(tab) = self.tabs.get_mut(self.active) else { return };
            let ready = tab.info().is_some();
            ui.add_enabled_ui(ready, |ui| {
                let n = tab.page_count();
                if ui.button("⏮").on_hover_text("First page (Home)").clicked() {
                    tab.goto_page(0);
                }
                if ui.button("◀").on_hover_text("Previous page (PgUp)").clicked() {
                    tab.step_page(false);
                }
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut tab.page_input)
                        .desired_width(44.0)
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
                if ui.button("▶").on_hover_text("Next page (PgDn)").clicked() {
                    tab.step_page(true);
                }
                if ui.button("⏭").on_hover_text("Last page (End)").clicked() {
                    tab.goto_page(usize::MAX);
                }
                ui.separator();

                if ui.button("➖").on_hover_text("Zoom out (Ctrl+-)").clicked() {
                    tab.zoom_step(false);
                }
                let label = match tab.viewport.fit {
                    FitMode::None => format!("{:.0}%", tab.zoom_percent()),
                    fit => format!("{} ({:.0}%)", fit.label(), tab.zoom_percent()),
                };
                egui::ComboBox::from_id_salt("zoom")
                    .selected_text(label)
                    .width(140.0)
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
                if ui.button("➕").on_hover_text("Zoom in (Ctrl++)").clicked() {
                    tab.zoom_step(true);
                }
                ui.separator();

                for (mode, icon) in LayoutMode::ALL.into_iter().zip(["⬌", "⬍", "▯", "▯▯", "▦"]) {
                    if ui
                        .selectable_label(tab.mode == mode, icon)
                        .on_hover_text(mode.label())
                        .clicked()
                    {
                        tab.set_mode(mode);
                    }
                }
                ui.separator();
                if ui.button("⟲").on_hover_text("Rotate counter-clockwise (Ctrl+Shift+R)").clicked() {
                    tab.rotate(false);
                }
                if ui.button("⟳").on_hover_text("Rotate clockwise (Ctrl+R)").clicked() {
                    tab.rotate(true);
                }
            });

            // Search, right-aligned.
            let mut submit = None;
            let mut clear = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let Some(tab) = self.tabs.get_mut(self.active) else { return };
                if ui.button("▼").on_hover_text("Next match (F3)").clicked() {
                    tab.search_step(true);
                }
                if ui.button("▲").on_hover_text("Previous match (Shift+F3)").clicked() {
                    tab.search_step(false);
                }
                let s = &mut tab.search;
                let before = (s.match_case, s.whole_word);
                ui.toggle_value(&mut s.whole_word, "W").on_hover_text("Whole words");
                ui.toggle_value(&mut s.match_case, "Aa").on_hover_text("Match case");
                if before != (s.match_case, s.whole_word) && s.submitted.is_some() {
                    submit = Some(true);
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
                        .hint_text("🔍 Search (Ctrl+F)")
                        .desired_width(180.0),
                );
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if s.submitted.as_deref() == Some(s.query.trim()) {
                        let forward = !shift;
                        tab.search_step(forward);
                    } else {
                        submit = Some(true);
                    }
                    resp.request_focus();
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    clear = true;
                    resp.surrender_focus();
                }
            });
            if submit.is_some() {
                self.start_search();
            }
            if clear {
                self.clear_search();
            }
        });
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut close = None;
        egui::ScrollArea::horizontal()
            .id_salt("tabbar")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let resp = ui
                            .selectable_label(i == self.active, &tab.name)
                            .on_hover_text(tab.path.display().to_string());
                        if resp.clicked() {
                            self.active = i;
                        }
                        if resp.middle_clicked() {
                            close = Some(i);
                        }
                        if ui.small_button("×").on_hover_text("Close (Ctrl+W)").clicked() {
                            close = Some(i);
                        }
                        ui.add_space(6.0);
                    }
                });
            });
        if let Some(i) = close {
            self.close_tab(i);
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
            self.close_tab(self.active);
        }
    }

    fn info_windows(&mut self, ctx: &egui::Context) {
        egui::Window::new("Keyboard shortcuts")
            .open(&mut self.show_shortcuts)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("shortcuts").striped(true).show(ui, |ui| {
                    for (k, v) in [
                        ("Ctrl+O", "Open file"),
                        ("Ctrl+W / Ctrl+Tab", "Close / switch tab"),
                        ("Wheel", "Scroll (horizontally in horizontal layout)"),
                        ("Shift+Wheel", "Scroll along the other axis"),
                        ("Ctrl+Wheel, Ctrl+± , Ctrl+0", "Zoom, actual size"),
                        ("PgUp / PgDn, Home / End", "Previous / next, first / last page"),
                        ("Space / Shift+Space", "Scroll one screen"),
                        ("Ctrl+1 … Ctrl+5", "Horizontal, vertical, single, facing, grid"),
                        ("Ctrl+R / Ctrl+Shift+R", "Rotate view"),
                        ("H / S", "Hand tool / text selection tool"),
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
                ui.label("Rendering by PDFium; UI by egui.");
            });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in dropped {
            self.open(&path);
        }
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
        self.process_responses(&ctx);
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
                    "Page {} of {}   ·   {}   ·   {:.0}%{}",
                    tab.current_page + 1,
                    info.page_sizes.len(),
                    tab.mode.label(),
                    tab.zoom_percent(),
                    info.title.as_ref().map(|t| format!("   ·   {t}")).unwrap_or_default()
                ),
                LoadState::Loading => "Loading…".into(),
                LoadState::NeedsPassword { .. } => "Password required".into(),
                LoadState::Failed(_) => "Failed to open".into(),
            };
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.weak(status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(tab.path.display().to_string());
                    });
                });
            });
        }

        if self.settings.show_sidebar && self.tabs.get(self.active).is_some_and(|t| t.info().is_some()) {
            let tab = &mut self.tabs[self.active];
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(170.0)
                .size_range(120.0..=500.0)
                .show(ui, |ui| sidebar::show(ui, tab, &mut self.sidebar_tab, &mut self.cache));
        }

        let mut text_requests = Vec::new();
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
                    text_requests: &mut text_requests,
                },
            );
        });

        if let Some(tab) = self.tabs.get_mut(self.active) {
            let doc = tab.id;
            let mut reqs = Vec::new();
            for page in text_requests {
                if let std::collections::hash_map::Entry::Vacant(e) = tab.texts.entry(page) {
                    e.insert(None);
                    reqs.push(Request::PageText { doc, page });
                }
            }
            for r in reqs {
                self.send(r);
            }
        }

        self.password_dialog(&ctx);
        self.info_windows(&ctx);
        self.sync_renders();
        self.cache.evict();

        let title = match self.tabs.get(self.active) {
            Some(t) => format!("{} — magic-pdf", t.name),
            None => "magic-pdf".into(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
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
