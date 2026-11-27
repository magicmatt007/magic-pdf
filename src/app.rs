//! Application shell: tabs, menus, toolbar, keyboard shortcuts and engine plumbing.

use crate::i18n::{t, tf};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, Ui};

use crate::document::{ACTUAL_SIZE, DocTab, LoadState, SearchHit, ZOOM_PRESETS};
use crate::engine::{DocId, Engine, FormKey, PageOp, PrintJob, Request, Response, SearchQuery, form_mods, move_target};
use crate::icons;
use crate::keys::kb;
use crate::settings::Settings;
use crate::view::cache::TextureCache;
use crate::view::color_picker;
use crate::view::document_view::{self, Tool, ViewContext};
use crate::view::layout::LayoutMode;
use crate::view::sidebar::{self, PageAction, SidebarTab};
use crate::view::viewport::FitMode;

const TEXTURE_BUDGET: usize = 384 * 1024 * 1024;
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
    /// The app icon, shown at the start of the menu bar.
    app_icon: egui::TextureHandle,
    /// The large app icon on the start screen (decoded when first shown).
    logo: Option<egui::TextureHandle>,
    ctx: egui::Context,
    tabs: Vec<DocTab>,
    active: usize,
    next_id: DocId,
    cache: TextureCache,
    settings: Settings,
    tool: Tool,
    sidebar_tab: SidebarTab,
    focus_search: bool,
    /// Search in every open tab, not only the active one.
    search_all: bool,
    show_shortcuts: bool,
    show_about: bool,
    show_settings: bool,
    close_intent: Option<CloseIntent>,
    /// Quit has been confirmed; let the next close request through.
    allow_quit: bool,
    error: Option<String>,
    print_dialog: Option<PrintDialog>,
    /// Short status message and when it was shown.
    notice: Option<(String, f64)>,
    print_results: (Sender<PrintResult>, Receiver<PrintResult>),
    sign_dialog: Option<crate::sign_dialog::SignDialog>,
    /// Certificates opened this session (the password is asked once).
    identities: HashMap<PathBuf, std::sync::Arc<crate::engine::sign::SigningIdentity>>,
    /// Signing in the background: the document, where the result goes, the outcome.
    sign_results: (Sender<SignResult>, Receiver<SignResult>),
    signing: bool,
    combine_dialog: Option<crate::view::combine::CombineDialog>,
    /// Combining files in the engine.
    combining: bool,
    /// Debug aid: with MAGIC_PDF_SCREENSHOT=<file.ppm> the app captures its own window
    /// once pages have rendered, saves it and quits.
    screenshot_to: Option<PathBuf>,
    /// Debug aid: with MAGIC_PDF_BENCH=<report.txt> the app pages through the file,
    /// searches it, writes timings and memory use to the report and quits.
    bench: Option<Bench>,
    screenshot_requested: bool,
    /// Debug aid for automated launch tests: with MAGIC_PDF_SELFTEST=<file> the app
    /// writes the documents it opened (path and page count) to the file and quits.
    selftest_to: Option<PathBuf>,
    /// Pages per file for the "Split document" dialog (None: dialog closed).
    split_every: Option<usize>,
    ocr: crate::ocr_dialog::OcrDialog,
    /// Connection to other magic-pdf windows.
    ipc: Option<crate::ipc::Server>,
    /// Touchpad pinch on Wayland, which winit does not report.
    #[cfg(target_os = "linux")]
    pinch: Option<crate::pinch::Pinch>,
    /// Index of the tab being dragged in the tab bar.
    tab_drag: Option<usize>,
    /// Other windows, listed in a tab's "Move to window" menu.
    tab_menu_peers: Vec<(crate::ipc::Peer, String)>,
    /// Stamp and signature tool state.
    stamps: crate::view::stamps::StampState,
    /// The shape tool shown in the toolbar's shape group.
    last_shape: Tool,
    /// Text put on the clipboard when an annotation was copied: pasting exactly this
    /// text pastes the annotation.
    annot_clip_text: Option<String>,
    /// Two documents shown side by side with their text differences.
    compare: Option<Comparison>,
    /// Document properties dialog (None: closed).
    props_dialog: Option<crate::view::doc_info::PropertiesDialog>,
    /// E-invoice data found in open documents, and the one shown.
    einvoices: HashMap<DocId, crate::view::doc_info::EInvoice>,
    show_einvoice: Option<DocId>,
    /// The document whose signature details are shown.
    show_signatures: Option<DocId>,
    /// What to do with attached files the engine is reading.
    attachment_actions: HashMap<(DocId, usize), crate::view::doc_info::AttachmentAction>,
    /// Page tool dialogs (None: closed).
    crop_dialog: Option<crate::view::page_tools::CropDialog>,
    /// The tab whose redactions wait for confirmation.
    confirm_redact: Option<DocId>,
    hf_dialog: Option<crate::view::page_tools::HeaderFooterDialog>,
    optimize_dialog: Option<crate::view::page_tools::OptimizeDialog>,
    images_dialog: Option<crate::view::page_tools::ImagesDialog>,
    /// Width of the toolbar's buttons when they last fitted on one row, and of the
    /// search box with its buttons (they share the row when both fit).
    search_width: f32,
    watermark_dialog: Option<crate::view::page_tools::WatermarkDialog>,
    flatten_dialog: Option<crate::view::page_tools::FlattenDialog>,
    /// Presentation mode (full screen, one page at a time) and what to restore after.
    presenting: Option<Presentation>,
    /// Debug aid (screenshots): start a presentation once the document is ready.
    present_on_load: bool,
    /// Debug aid (screenshots): show the signature details once the document is ready.
    signatures_on_load: bool,
    /// Tabs being moved to another window, waiting for their export.
    moves: HashMap<DocId, MoveTarget>,
    /// Temporary files of tabs received from other windows, deleted once opened.
    temp_files: HashMap<DocId, PathBuf>,
    was_focused: bool,
    update: UpdateUi,
    update_events: (Sender<UpdateEvent>, Receiver<UpdateEvent>),
    /// Installer to run when magic-pdf exits (after an update was accepted).
    pending_installer: Option<PathBuf>,
}

/// The view a presentation started from, restored when it ends.
struct Presentation {
    doc: DocId,
    mode: LayoutMode,
    fit: crate::view::viewport::FitMode,
    zoom: f32,
    sidebar: bool,
    tool: Tool,
    /// When it started (for the "Esc ends" hint).
    since: f64,
}

/// A finished signing: the document, where the signed file went, the outcome.
type SignResult = (DocId, PathBuf, Result<(), String>);

/// Results from background update work.
enum UpdateEvent {
    Checked { result: Result<Option<crate::update::Release>, String>, manual: bool },
    Downloaded(Result<PathBuf, String>),
}

/// State of the update notice.
#[derive(Default)]
enum UpdateUi {
    #[default]
    Hidden,
    Available(crate::update::Release),
    Downloading(crate::update::Release),
    Ready(crate::update::Release, PathBuf),
}

/// Where a tab is moved.
#[derive(Clone, Debug, PartialEq)]
enum MoveTarget {
    /// A new magic-pdf window, opened at this screen position if known.
    NewWindow(Option<[f32; 2]>),
    Peer(crate::ipc::Peer),
}

/// Outcome of releasing a dragged tab.
#[derive(Clone, Debug, PartialEq)]
enum TabDrop {
    Stay,
    /// Move the tab to this position in the tab bar.
    Reorder(usize),
    /// Over another magic-pdf window (index into the peer list).
    Peer(usize),
    /// Outside every window, or pulled well away from the tab bar: open a new one
    /// (at this screen position if known).
    NewWindow(Option<[f32; 2]>),
}

/// How far below the tab bar a dragged tab must be released to get its own window.
/// Like in web browsers, this also works in a maximized window, which has no
/// "outside" to drop on (the usual case on Windows).
const TAB_DETACH_DISTANCE: f32 = 80.0;

/// The mouse position on the screen (in pixels) and whether the primary button is
/// held, asked from Windows directly. While a tab is dragged out of the window,
/// the window's own mouse events can't be relied on there (the release outside the
/// window can get lost), so the drag follows the real mouse instead.
#[cfg(windows)]
fn os_pointer() -> Option<(egui::Pos2, bool)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SM_SWAPBUTTON};
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: plain Win32 calls; `p` is a valid out pointer.
    unsafe {
        if GetCursorPos(&mut p) == 0 {
            return None;
        }
        let button = if GetSystemMetrics(SM_SWAPBUTTON) != 0 { VK_RBUTTON } else { VK_LBUTTON };
        let down = (GetAsyncKeyState(button as i32) as u16 & 0x8000) != 0;
        Some((egui::pos2(p.x as f32, p.y as f32), down))
    }
}

/// Decides what dropping a dragged tab at `pointer` (window coordinates) means.
/// `origin` is the window's screen position (unknown on Wayland); `peer_rects` are the
/// other windows' screen rectangles as `[x, y, w, h]`.
fn decide_tab_drop(
    pointer: Option<egui::Pos2>,
    window: egui::Rect,
    origin: Option<egui::Pos2>,
    tab_rects: &[egui::Rect],
    peer_rects: &[Option<[f32; 4]>],
) -> TabDrop {
    let Some(p) = pointer else { return TabDrop::NewWindow(None) };
    let global = origin.map(|o| o + p.to_vec2());
    if window.contains(p) {
        // The whole width of the tab bar counts (dropping right of the last tab moves
        // the tab to the end).
        let tabs = tab_rects.iter().fold(egui::Rect::NOTHING, |a, r| a.union(*r));
        if (tabs.top() - 12.0..=tabs.bottom() + 12.0).contains(&p.y) {
            let index = tab_rects.iter().position(|r| p.x < r.center().x).unwrap_or(tab_rects.len().saturating_sub(1));
            return TabDrop::Reorder(index);
        }
        if p.y > tabs.bottom() + TAB_DETACH_DISTANCE {
            return TabDrop::NewWindow(global.map(|g| [g.x, g.y]));
        }
        return TabDrop::Stay;
    }
    if let Some(g) = global
        && let Some(i) = peer_rects
            .iter()
            .position(|r| r.is_some_and(|[x, y, w, h]| g.x >= x && g.x <= x + w && g.y >= y && g.y <= y + h))
    {
        return TabDrop::Peer(i);
    }
    TabDrop::NewWindow(global.map(|g| [g.x, g.y]))
}

type PrintResult = Result<String, String>;

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: crate::Args) -> Self {
        let ctx = cc.egui_ctx.clone();
        let wake_ctx = ctx.clone();
        let engine = Engine::start(move || wake_ctx.request_repaint());
        if let Err(e) = &engine {
            log::error!("{e}");
        }
        icons::install(&ctx);
        crate::fonts::install(&ctx);
        crate::fonts::install_ui(&ctx);
        crate::theme::install(&ctx);
        let settings = Settings::load();
        crate::i18n::set_language(&settings.language);
        ctx.set_theme(if settings.dark_ui { egui::Theme::Dark } else { egui::Theme::Light });
        let mut cache = TextureCache::new(TEXTURE_BUDGET);
        cache.invert = settings.night_mode;
        let app_icon = ctx.load_texture(
            "app-icon",
            egui::ColorImage::from_rgba_unmultiplied([64, 64], include_bytes!("../assets/icons/magic-pdf-64.rgba")),
            // Mipmaps keep it smooth at menu bar size.
            egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR },
        );
        let mut app = Self {
            engine,
            app_icon,
            logo: None,
            ctx,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            cache,
            settings,
            tool: Tool::Hand,
            sidebar_tab: SidebarTab::Pages,
            focus_search: false,
            search_all: false,
            show_shortcuts: false,
            show_about: false,
            show_settings: false,
            close_intent: None,
            allow_quit: false,
            error: None,
            print_dialog: None,
            notice: None,
            print_results: std::sync::mpsc::channel(),
            sign_dialog: None,
            identities: HashMap::new(),
            sign_results: std::sync::mpsc::channel(),
            signing: false,
            combine_dialog: None,
            combining: false,
            screenshot_to: std::env::var_os("MAGIC_PDF_SCREENSHOT").map(PathBuf::from),
            bench: std::env::var_os("MAGIC_PDF_BENCH").map(|p| Bench::new(p.into())),
            screenshot_requested: false,
            selftest_to: std::env::var_os("MAGIC_PDF_SELFTEST").map(PathBuf::from),
            split_every: None,
            ocr: Default::default(),
            ipc: None,
            #[cfg(target_os = "linux")]
            pinch: crate::pinch::Pinch::start(cc),
            tab_drag: None,
            tab_menu_peers: Vec::new(),
            stamps: Default::default(),
            last_shape: Tool::Rect,
            annot_clip_text: None,
            compare: None,
            props_dialog: None,
            einvoices: HashMap::new(),
            show_einvoice: None,
            show_signatures: None,
            attachment_actions: HashMap::new(),
            crop_dialog: None,
            confirm_redact: None,
            hf_dialog: None,
            optimize_dialog: None,
            images_dialog: None,
            search_width: 290.0,
            watermark_dialog: None,
            flatten_dialog: None,
            presenting: None,
            present_on_load: false,
            signatures_on_load: false,
            moves: HashMap::new(),
            temp_files: HashMap::new(),
            was_focused: true,
            update: UpdateUi::Hidden,
            update_events: std::sync::mpsc::channel(),
            pending_installer: None,
        };
        if app.screenshot_to.is_some() {
            // Debug aid for screenshots: start with a given tool selected.
            let wanted = std::env::var("MAGIC_PDF_TOOL").unwrap_or_default();
            if let Some(t) = Tool::ANNOTATION_TOOLS
                .iter()
                .chain([&Tool::Redact])
                .find(|t| format!("{t:?}").eq_ignore_ascii_case(&wanted))
            {
                app.tool = *t;
            }
            app.show_settings = wanted == "settings";
            app.signatures_on_load = wanted == "signatures";
            app.present_on_load = wanted == "present";
            if wanted == "combine" {
                // The Combine files window with the files given on the command line.
                let mut d = crate::view::combine::CombineDialog::new(None);
                d.add(args.files.clone());
                app.combine_dialog = Some(d);
            }
            if wanted == "sign" {
                // The signing dialog for a box on the first page; MAGIC_PDF_P12 and
                // MAGIC_PDF_P12_PASSWORD open a certificate file in it.
                app.sign_dialog = Some(crate::sign_dialog::SignDialog::new(
                    app.next_id,
                    0,
                    crate::engine::PdfBox { left: 300.0, bottom: 80.0, right: 540.0, top: 150.0 },
                ));
                if let Ok(p12) = std::env::var("MAGIC_PDF_P12") {
                    let pw = std::env::var("MAGIC_PDF_P12_PASSWORD").unwrap_or_default();
                    if let Ok(id) =
                        crate::engine::sign::SigningIdentity::from_pkcs12(&std::fs::read(&p12).unwrap_or_default(), &pw)
                    {
                        app.identities.insert(p12.clone().into(), std::sync::Arc::new(id));
                    }
                    app.settings.sign_certificate = Some(p12.into());
                }
            }
        }
        let day = 24 * 60 * 60;
        if app.settings.check_updates && crate::settings::now().saturating_sub(app.settings.last_update_check) >= day {
            app.check_for_updates(false);
        }
        #[cfg(target_os = "macos")]
        {
            let wake = app.ctx.clone();
            crate::macos::set_wake(move || wake.request_repaint());
        }
        let wake = app.ctx.clone();
        match crate::ipc::Server::start(move || wake.request_repaint()) {
            Ok(server) => app.ipc = Some(server),
            Err(e) => log::warn!("other windows cannot reach this one: {e}"),
        }
        // Files opened together can now be handed to this window.
        crate::ipc::release_startup();
        let restore = app.settings.restore_session
            && args.files.is_empty()
            && args.take.is_none()
            && !args.new_window
            && app.screenshot_to.is_none()
            && app.selftest_to.is_none()
            // Another window already shows them.
            && crate::ipc::peers().is_empty();
        if restore {
            let (files, active) = (app.settings.session.clone(), app.settings.session_active);
            for f in files.iter().filter(|f| f.exists()) {
                app.open(f);
            }
            app.active = active.min(app.tabs.len().saturating_sub(1));
        }
        for f in args.files {
            app.open(&f);
        }
        if let Some(t) = args.take {
            app.take(t);
        }
        if app.screenshot_to.is_some() {
            // Debug aids for screenshots: a search, a zoom level, a sidebar tab.
            if let Ok(q) = std::env::var("MAGIC_PDF_SEARCH") {
                app.search_all = std::env::var_os("MAGIC_PDF_SEARCH_ALL").is_some();
                for t in &mut app.tabs {
                    t.search.query = q.clone();
                }
                app.start_search();
            }
            if let Some(z) = std::env::var("MAGIC_PDF_ZOOM").ok().and_then(|z| z.parse::<f32>().ok())
                && let Some(t) = app.tabs.get_mut(app.active)
            {
                t.set_zoom(z / 100.0);
            }
            if std::env::var_os("MAGIC_PDF_COMPARE").is_some() && app.tabs.len() >= 2 {
                app.active = 0;
                let other = app.tabs[1].id;
                app.start_compare(Some(other));
            }
            match std::env::var("MAGIC_PDF_SIDEBAR").as_deref() {
                Ok("outline") => app.sidebar_tab = SidebarTab::Outline,
                Ok("comments") => app.sidebar_tab = SidebarTab::Comments,
                Ok("search") => app.sidebar_tab = SidebarTab::Search,
                _ => {}
            }
            match std::env::var("MAGIC_PDF_THEME").as_deref() {
                Ok("dark") => app.ctx.set_theme(egui::Theme::Dark),
                Ok("light") => app.ctx.set_theme(egui::Theme::Light),
                _ => {}
            }
            // The update window with the latest release (asks GitHub).
            if std::env::var_os("MAGIC_PDF_SHOW_UPDATE").is_some()
                && let Ok(release) = crate::update::latest_release()
            {
                app.update = UpdateUi::Available(release);
            }
        }
        app
    }

    /// Looks for a new release in the background.
    fn check_for_updates(&mut self, manual: bool) {
        self.settings.last_update_check = crate::settings::now();
        self.settings.save();
        let tx = self.update_events.0.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(UpdateEvent::Checked { result: crate::update::check(), manual });
            ctx.request_repaint();
        });
    }

    fn update_ui(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.update_events.1.try_recv() {
            match ev {
                UpdateEvent::Checked { result, manual } => match result {
                    Ok(Some(r)) if manual || self.settings.skipped_version.as_deref() != Some(r.version()) => {
                        self.update = UpdateUi::Available(r);
                    }
                    Ok(_) if manual => self
                        .notice(&tf("magic-pdf {version} is up to date", &[("version", &env!("CARGO_PKG_VERSION"))])),
                    Err(e) if manual => self.error = Some(tf("Could not check for updates: {e}", &[("e", &e)])),
                    Err(e) => log::info!("update check failed: {e}"),
                    _ => {}
                },
                UpdateEvent::Downloaded(result) => match (result, std::mem::take(&mut self.update)) {
                    (Ok(path), UpdateUi::Downloading(r)) => self.update = UpdateUi::Ready(r, path),
                    (Err(e), _) => self.error = Some(tf("The update could not be downloaded: {e}", &[("e", &e)])),
                    _ => {}
                },
            }
        }
        let release = match &self.update {
            UpdateUi::Hidden => return,
            UpdateUi::Available(r) | UpdateUi::Downloading(r) | UpdateUi::Ready(r, _) => r.clone(),
        };
        let mut action = None;
        egui::Window::new(t("Update available"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -40.0))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(tf(
                    "magic-pdf {new} is available (you have {current}).",
                    &[("new", &release.version()), ("current", &env!("CARGO_PKG_VERSION"))],
                ));
                // Only the "What's new" part; the rest are installation notes.
                if let Some(blocks) = release.body.as_deref().and_then(crate::update::whats_new) {
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| release_notes(ui, &blocks, 370.0));
                }
                ui.separator();
                let can_install = crate::update::can_install() && release.installer().is_some();
                match &self.update {
                    UpdateUi::Downloading(_) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(t("Downloading and verifying…"));
                        });
                    }
                    UpdateUi::Ready(..) => {
                        ui.label(if cfg!(target_os = "macos") {
                            t("Ready. magic-pdf closes and opens the new version's disk image: drag magic-pdf to Applications.")
                        } else {
                            t("Ready. magic-pdf closes to install the update and then starts again.")
                        });
                        ui.horizontal(|ui| {
                            if ui.add(crate::theme::primary_button(t("Install now"))).clicked() {
                                action = Some("install");
                            }
                            if ui.button(t("Later")).clicked() {
                                action = Some("later");
                            }
                        });
                    }
                    _ => {
                        if !can_install {
                            ui.weak(t("Update from source: git pull, then packaging/linux/install.sh"));
                        }
                        ui.horizontal(|ui| {
                            if can_install {
                                if ui.add(crate::theme::primary_button(t("Install"))).clicked() {
                                    action = Some("download");
                                }
                            } else if ui.add(crate::theme::primary_button(t("Open download page"))).clicked() {
                                action = Some("page");
                            }
                            if ui.button(t("Later")).clicked() {
                                action = Some("later");
                            }
                            if ui.button(t("Skip this version")).clicked() {
                                action = Some("skip");
                            }
                        });
                    }
                }
            });
        match action {
            Some("download") => {
                let asset = release.installer().cloned();
                self.update = UpdateUi::Downloading(release);
                let tx = self.update_events.0.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let result = asset
                        .ok_or_else(|| "no installer in this release".to_string())
                        .and_then(|a| crate::update::download_installer(&a));
                    let _ = tx.send(UpdateEvent::Downloaded(result));
                    ctx.request_repaint();
                });
            }
            Some("install") => {
                if let UpdateUi::Ready(_, path) = std::mem::take(&mut self.update) {
                    // Runs when magic-pdf exits (after any unsaved-changes question).
                    self.pending_installer = Some(path);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Some("page") => {
                let _ = open::that_detached(crate::update::RELEASES_PAGE);
                self.update = UpdateUi::Hidden;
            }
            Some("skip") => {
                self.settings.skipped_version = Some(release.version().to_string());
                self.settings.save();
                self.update = UpdateUi::Hidden;
            }
            Some(_) => self.update = UpdateUi::Hidden,
            None => {}
        }
    }

    /// Opens a tab handed over from another window.
    fn take(&mut self, t: crate::ipc::TabTransfer) {
        let original = simplify_path(t.original.clone());
        if self.tabs.iter().any(|tab| tab.path == original) {
            self.error = Some(tf("{file} is already open in this window.", &[("file", &original.display())]));
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut tab =
            DocTab::new(id, original.clone(), self.settings.default_mode, self.settings.file_state(&original));
        tab.pending_goto = Some(t.page);
        tab.mark_dirty_on_open = t.dirty;
        self.send(Request::Open { doc: id, path: t.file.clone(), password: None });
        if t.file != t.original {
            self.temp_files.insert(id, t.file);
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Moves a tab to another window (exporting unsaved changes first).
    fn move_tab(&mut self, doc: DocId, target: MoveTarget) {
        // A comment still being typed goes along (it makes the document changed).
        let pending = self.commit_edits(doc);
        let Some(tab) = self.tabs.iter().find(|t| t.id == doc) else { return };
        if tab.info().is_none() {
            return;
        }
        if tab.dirty || pending {
            let name = tab.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let path = std::env::temp_dir().join(format!("magic-pdf-move-{}-{doc}-{name}", std::process::id()));
            self.moves.insert(doc, target);
            self.send(Request::ExportToFile { doc, path });
        } else {
            let path = tab.path.clone();
            self.complete_move(doc, path, target);
        }
    }

    fn complete_move(&mut self, doc: DocId, file: PathBuf, target: MoveTarget) {
        let Some(index) = self.tabs.iter().position(|t| t.id == doc) else { return };
        let tab = &self.tabs[index];
        let transfer = crate::ipc::TabTransfer {
            file: file.clone(),
            original: tab.path.clone(),
            page: tab.current_page,
            dirty: tab.dirty,
        };
        let result = match target {
            MoveTarget::Peer(peer) => {
                if peer.send(&crate::ipc::Message::Take(transfer)) {
                    Ok(())
                } else {
                    Err("the other window did not respond".to_string())
                }
            }
            MoveTarget::NewWindow(at) => {
                let mut cmd =
                    std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "magic-pdf".into()));
                if let Some([x, y]) = at {
                    cmd.arg("--at").arg(format!("{x},{y}"));
                }
                cmd.arg("--take")
                    .arg(&transfer.file)
                    .arg(&transfer.original)
                    .arg(transfer.page.to_string())
                    .arg(if transfer.dirty { "1" } else { "0" });
                cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
            }
        };
        match result {
            // The other window has it now (including unsaved changes): close here
            // without asking to save.
            Ok(()) => self.close_tab_now(index),
            Err(e) => {
                if file != self.tabs[index].path {
                    let _ = std::fs::remove_file(&file);
                }
                self.error = Some(tf("Could not move the tab: {e}", &[("e", &e)]));
            }
        }
    }

    /// Handles requests from other windows and tells them where this window is.
    fn handle_ipc(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        for path in crate::macos::take_opened() {
            // Finder: double-click, Open With, or drop on the Dock icon.
            self.open(&path);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let Some(server) = &self.ipc else { return };
        let messages: Vec<_> = server.messages.try_iter().collect();
        let (rect, focused) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.focused.unwrap_or(false))
        });
        if let Ok(mut w) = server.window.lock() {
            w.rect = rect.map(|r| [r.min.x, r.min.y, r.width(), r.height()]);
            w.title = match self.tabs.get(self.active) {
                Some(t) => format!("{} — magic-pdf", t.name),
                None => "magic-pdf".into(),
            };
        }
        if focused && !self.was_focused {
            // The most recently used window receives files opened from the desktop.
            server.touch();
        }
        self.was_focused = focused;
        for m in messages {
            match m {
                crate::ipc::Message::Open(path) => self.open(&path),
                crate::ipc::Message::Take(t) => self.take(t),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
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
        self.send(Request::Open { doc: id, path: path.clone(), password: None });
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.settings.add_recent(&path);
        self.settings.save();
    }

    fn open_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter(t("PDF documents"), &["pdf", "PDF"]);
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

    /// Sends a comment or text box text still being typed, and everything queued for the
    /// engine, so a request sent next (save, print) includes it. True if text was pending.
    fn commit_edits(&mut self, doc: DocId) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == doc) else { return false };
        let pending = tab.commit_annot_edit();
        let queued: Vec<Request> = std::mem::take(&mut tab.outbox);
        for req in queued {
            self.send(req);
        }
        pending
    }

    fn save_active(&mut self, save_as: bool) {
        if let Some(doc) = self.tabs.get(self.active).map(|t| t.id) {
            self.commit_edits(doc);
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.info().is_none() {
            return;
        }
        let path = if save_as {
            let mut dialog = rfd::FileDialog::new().add_filter(t("PDF documents"), &["pdf"]).set_file_name(&tab.name);
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
        // Text still being typed counts as an unsaved change (the engine has not
        // confirmed it yet).
        let mut pending = false;
        for &d in &docs {
            pending |= self.commit_edits(d);
        }
        let dirty = pending || docs.iter().any(|d| self.tabs.iter().any(|t| t.id == *d && t.dirty));
        if dirty {
            self.close_intent = Some(CloseIntent { docs, quit, awaiting_save: HashSet::new() });
        } else {
            self.finish_close(&docs, quit);
        }
    }

    fn finish_close(&mut self, docs: &[DocId], quit: bool) {
        if quit && self.screenshot_to.is_none() && self.selftest_to.is_none() {
            // Reopened at the next start (the last window closed wins).
            self.settings.session = self.tabs.iter().map(|t| t.path.clone()).collect();
            self.settings.session_active = self.active;
            self.settings.save();
        }
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
        if let Some(c) = &self.compare
            && (c.a == self.tabs[index].id || c.b == self.tabs[index].id)
        {
            self.end_compare();
        }
        let tab = self.tabs.remove(index);
        self.einvoices.remove(&tab.id);
        if self.show_einvoice == Some(tab.id) {
            self.show_einvoice = None;
        }
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
                    // XML attachments may be e-invoice data.
                    let xml: Vec<usize> = info
                        .attachments
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| a.name.to_lowercase().ends_with(".xml"))
                        .map(|(i, _)| i)
                        .collect();
                    for &index in &xml {
                        tab.outbox.push(Request::GetAttachment { doc, index });
                    }
                    tab.state = LoadState::Ready(info);
                    let page = tab.pending_goto.unwrap_or(0);
                    tab.goto_page(page);
                    let mark_dirty = std::mem::take(&mut tab.mark_dirty_on_open);
                    self.send(Request::ScanAnnots { doc });
                    if mark_dirty {
                        self.send(Request::MarkDirty { doc });
                    }
                    if let Some(tmp) = self.temp_files.remove(&doc) {
                        // The document is in memory now.
                        let _ = std::fs::remove_file(tmp);
                    }
                    for index in xml {
                        self.attachment_actions.insert((doc, index), crate::view::doc_info::AttachmentAction::Inspect);
                    }
                }
            }
            Response::OpenFailed { doc, error, password_required } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.state = if password_required {
                        let wrong = matches!(tab.state, LoadState::NeedsPassword { .. });
                        LoadState::NeedsPassword { input: String::new(), wrong }
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
            Response::Attachment { doc, index, name, data } => {
                use crate::view::doc_info::{AttachmentAction, EInvoice};
                match self.attachment_actions.remove(&(doc, index)) {
                    Some(AttachmentAction::Save(path)) => match std::fs::write(&path, &data) {
                        Ok(()) => self.notice(&tf("Saved {file}", &[("file", &path.display())])),
                        Err(e) => {
                            self.error = Some(tf("Could not save {file}: {e}", &[("file", &path.display()), ("e", &e)]))
                        }
                    },
                    Some(AttachmentAction::Open) => {
                        // A copy in the temporary folder, opened with the system's program.
                        let safe: String = name
                            .chars()
                            .map(|c| if c.is_alphanumeric() || ".-_ ".contains(c) { c } else { '_' })
                            .collect();
                        let dir = std::env::temp_dir().join(format!("magic-pdf-attachments-{}", std::process::id()));
                        let path = dir.join(if safe.is_empty() { "attachment".into() } else { safe });
                        let result = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &data));
                        match result {
                            Ok(()) => {
                                if let Err(e) = open::that_detached(&path) {
                                    self.error =
                                        Some(tf("Could not open {file}: {e}", &[("file", &path.display()), ("e", &e)]));
                                }
                            }
                            Err(e) => {
                                self.error =
                                    Some(tf("Could not save {file}: {e}", &[("file", &path.display()), ("e", &e)]))
                            }
                        }
                    }
                    Some(AttachmentAction::Inspect) | None => {
                        if crate::einvoice::is_invoice_xml(&data) {
                            self.einvoices.insert(doc, EInvoice::new(name, data));
                            if self.screenshot_to.is_some() && std::env::var_os("MAGIC_PDF_SHOW_EINVOICE").is_some() {
                                self.show_einvoice = Some(doc);
                            }
                        }
                    }
                }
            }
            Response::Compared { a, b, diffs } => {
                if let Some(c) = &mut self.compare
                    && (c.a, c.b) == (a, b)
                {
                    for (doc, added) in [(a, false), (b, true)] {
                        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == doc) {
                            t.diff_marks = diffs
                                .iter()
                                .map(|d| {
                                    let rects = if added { &d.added } else { &d.removed };
                                    (d.page, rects.iter().map(|r| (*r, added)).collect())
                                })
                                .collect();
                        }
                    }
                    c.diffs = Some(diffs);
                }
            }
            Response::PageText { doc, page, geom, text } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.geoms.insert(page, geom);
                    tab.texts.insert(page, Some(text));
                }
            }
            Response::SearchResults { doc, generation, page, geom, hits, context } => {
                let active_doc = self.tabs.get(self.active).map(|t| t.id);
                if let Some(tab) = self.tab_mut(doc)
                    && tab.search.generation == generation
                {
                    tab.geoms.insert(page, geom);
                    let first = tab.search.hits.is_empty();
                    tab.search.hits.extend(
                        hits.into_iter()
                            .zip(context.into_iter().chain(std::iter::repeat_with(Default::default)))
                            .map(|(rects, context)| SearchHit { page, rects, context }),
                    );
                    // Jump only in the tab being looked at.
                    if first && active_doc == Some(doc) {
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
            Response::PageAnnots { doc, page, geom, annots } => {
                if let Some(tab) = self.tab_mut(doc) {
                    tab.geoms.insert(page, geom);
                    if annots.is_empty() {
                        tab.annots.remove(&page);
                    } else {
                        tab.annots.insert(page, annots);
                    }
                    // A just-created note or text box: open it for typing now that it
                    // is known (its kind decides between comment box and in-place text).
                    if let Some((p, i)) = tab.pending_edit
                        && p == page
                        && tab.annot(p, i).is_some()
                    {
                        tab.pending_edit = None;
                        document_view::focus_annot(tab, p, i, true);
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
                    && matches!(tool, Tool::Note | Tool::TextBox)
                {
                    // Let the user type the note or the box's text right away.
                    tab.pending_edit = Some((page, index));
                }
            }
            Response::EditState { doc, can_undo, can_redo, dirty } => {
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
                    tab.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
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
            Response::Exported { message } => self.notice(&exported_text(&message)),
            Response::Combined { path } => {
                self.combining = false;
                self.notice(&tf("Combined into {file}", &[("file", &path.display())]));
                self.open(&path);
            }
            Response::CombineFailed { file, error } => {
                self.combining = false;
                self.error = Some(match file {
                    Some(f) if error == crate::engine::PASSWORD_PROTECTED => tf(
                        "{file} is password protected. Open it, save a copy without the password, then combine that.",
                        &[("file", &f.display())],
                    ),
                    Some(f) => tf("{file} could not be read: {e}", &[("file", &f.display()), ("e", &error)]),
                    None => tf("Combining failed: {e}", &[("e", &error)]),
                });
            }
            Response::ExportedTo { doc, path } => {
                if let Some(target) = self.moves.remove(&doc) {
                    self.complete_move(doc, path, target);
                }
            }
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
                self.notice(t("Sending to the printer…"));
                std::thread::spawn(move || {
                    let _ = tx.send(crate::print::submit(&job, &bytes));
                    ctx.request_repaint();
                });
            }
            Response::OcrProgress { .. } | Response::OcrFinished { .. } => {
                if let Some(n) = self.ocr.on_response(&resp) {
                    self.notice(&n);
                }
            }
            Response::Printed { pages } => {
                if let Some(n) = pages {
                    self.notice(&tf("Sent {n} page(s) to the printer", &[("n", &n)]));
                }
            }
            Response::Error { doc, message } => {
                log::warn!("doc {doc}: {message}");
                if self.close_intent.as_ref().is_some_and(|i| i.awaiting_save.contains(&doc)) {
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
        let stem = tab.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let dir = tab.path.parent().map(Path::to_path_buf);
        let edit = |op| Request::EditPages { doc, op };
        let req = match action {
            PageAction::Rotate(q) => Some(edit(PageOp::Rotate { pages: targets, quarter_turns: q })),
            PageAction::Delete => Some(edit(PageOp::Delete { pages: targets })),
            PageAction::MoveToStart => Some(edit(PageOp::Move { pages: targets, to: 0 })),
            PageAction::MoveToEnd => {
                let to = count - targets.len();
                Some(edit(PageOp::Move { pages: targets, to }))
            }
            PageAction::MoveTo { pages, gap } => {
                move_target(&pages, gap, count).map(|to| edit(PageOp::Move { pages, to }))
            }
            PageAction::InsertBlankAfter => {
                Some(edit(PageOp::InsertBlank { at: last + 1, size: info.page_sizes[last.min(count - 1)] }))
            }
            PageAction::InsertFileAfter => {
                let mut dialog = rfd::FileDialog::new().add_filter(t("PDF documents"), &["pdf", "PDF"]);
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
                    .add_filter(t("PDF documents"), &["pdf"])
                    .set_file_name(format!("{stem}-pages.pdf"));
                if let Some(d) = &dir {
                    dialog = dialog.set_directory(d);
                }
                dialog.save_file().map(|path| Request::ExtractPages { doc, pages: targets, path })
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

    fn doc_info_windows(&mut self, ctx: &egui::Context) {
        use crate::view::doc_info::*;
        if let Some(d) = &mut self.props_dialog {
            match self.tabs.iter_mut().find(|t| t.id == d.doc) {
                Some(tab) => {
                    let (req, open) = properties_dialog(ctx, d, tab);
                    tab.outbox.extend(req);
                    if !open {
                        self.props_dialog = None;
                    }
                }
                None => self.props_dialog = None,
            }
        }
        let Some(doc) = self.show_einvoice else { return };
        let (Some(inv), Some(tab)) = (self.einvoices.get(&doc), self.tabs.iter_mut().find(|t| t.id == doc)) else {
            self.show_einvoice = None;
            return;
        };
        // The text of all pages, to look for the total (invoices are short).
        let pages = tab.page_count().min(20);
        for p in 0..pages {
            tab.request_text(p);
        }
        let text = (0..pages)
            .map(|p| {
                tab.texts.get(&p).and_then(|t| t.as_ref()).map(|t| t.chars.iter().map(|c| c.ch).collect::<String>())
            })
            .collect::<Option<Vec<_>>>()
            .map(|v| v.join("\n"));
        let mut save = false;
        if !einvoice_window(ctx, inv, text.as_deref(), &mut save) {
            self.show_einvoice = None;
        }
        if save {
            let stem = tab.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mut dialog = rfd::FileDialog::new().add_filter("XML", &["xml"]).set_file_name(format!("{stem}.xml"));
            if let Some(dir) = tab.path.parent() {
                dialog = dialog.set_directory(dir);
            }
            if let Some(path) = dialog.save_file() {
                match std::fs::write(&path, &inv.xml) {
                    Ok(()) => self.notice(&tf("Saved {file}", &[("file", &path.display())])),
                    Err(e) => {
                        self.error = Some(tf("Could not save {file}: {e}", &[("file", &path.display()), ("e", &e)]))
                    }
                }
            }
        }
    }

    fn redaction_bar(&mut self, ui: &mut Ui) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let n = tab.redactions.len();
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(t("Redaction")).strong());
            if n == 0 {
                ui.label(t("Drag over an area or click a word to mark it. Nothing is removed until you apply."));
            } else {
                ui.label(tf("{n} areas marked.", &[("n", &n)]));
                if ui.button(t("Apply redactions…")).clicked() {
                    self.confirm_redact = Some(tab.id);
                }
                if ui.button(t("Clear marks")).clicked() {
                    tab.redactions.clear();
                }
            }
            let hits = tab.search.hits.len();
            if hits > 0
                && ui
                    .button(tf("Mark all {n} search matches", &[("n", &hits)]))
                    .on_hover_text(t("Marks every match of the current search for redaction"))
                    .clicked()
            {
                tab.mark_search_hits();
            }
        });
    }

    fn redaction_dialog(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.confirm_redact else { return };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == doc && !t.redactions.is_empty()) else {
            self.confirm_redact = None;
            return;
        };
        let pages: BTreeSet<usize> = tab.redactions.iter().map(|(p, _)| *p).collect();
        let mut choice = None;
        egui::Window::new(t("Apply redactions"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(tf(
                    "{n} marked areas on {pages} pages are blacked out and what lies under them is removed: text, parts of pictures, shapes and comments.",
                    &[("n", &tab.redactions.len()), ("pages", &pages.len())],
                ));
                ui.label(t("Undo works until the document is closed. Once saved, the removed content cannot be recovered from the file."));
                if tab.info().is_some_and(|i| !i.signatures.is_empty()) {
                    ui.add_space(4.0);
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        t("This document is signed. To remove the content for good, redacting rewrites the whole file, so its signatures will no longer be valid."),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(t("Apply redactions")).clicked() {
                        choice = Some(true);
                    }
                    if ui.button(t("Cancel")).clicked() {
                        choice = Some(false);
                    }
                });
            });
        match choice {
            Some(true) => {
                if let Some(op) = tab.take_redactions() {
                    tab.outbox.push(Request::EditPages { doc, op });
                }
                self.confirm_redact = None;
                self.tool = Tool::Hand;
            }
            Some(false) => self.confirm_redact = None,
            None => {}
        }
    }

    fn einvoice_bar(&mut self, ui: &mut Ui) {
        let Some(doc) = self.tabs.get(self.active).map(|t| t.id) else { return };
        let Some(inv) = self.einvoices.get(&doc) else { return };
        let ok = inv.invoice.as_ref().is_ok_and(|i| {
            crate::einvoice::checks(i).iter().all(|c| c.ok) && crate::einvoice::missing_required(i).is_empty()
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(t("E-invoice")).strong());
            ui.label(tf("This PDF contains invoice data ({format}).", &[("format", &inv.label())]));
            if let Ok(i) = &inv.invoice
                && let Some(total) = i.totals.due.or(i.totals.grand)
            {
                ui.label(egui::RichText::new(format!("{} {}", crate::einvoice::money(total), i.currency)).strong());
            }
            if !ok {
                ui.colored_label(egui::Color32::from_rgb(210, 90, 0), t("⚠ see the invoice data"));
            }
            if ui.button(t("Show invoice data")).clicked() {
                self.show_einvoice = Some(doc);
            }
        });
    }

    /// Compares the active document with an open one (`other`) or a file to pick.
    fn start_compare(&mut self, other: Option<DocId>) {
        let Some(a) = self.tabs.get(self.active).map(|t| t.id) else { return };
        let b = match other {
            Some(b) => b,
            None => {
                let mut dialog = rfd::FileDialog::new().add_filter(t("PDF documents"), &["pdf", "PDF"]);
                if let Some(dir) = self.current_dir() {
                    dialog = dialog.set_directory(dir);
                }
                let Some(path) = dialog.pick_file() else { return };
                self.open(&path);
                match self.tabs.get(self.active) {
                    Some(t) if t.id != a => t.id,
                    _ => return,
                }
            }
        };
        self.end_compare();
        // The engine handles requests in order, so a document opened just now is
        // ready by the time it compares.
        self.send(Request::Compare { a, b });
        self.compare = Some(Comparison { a, b, diffs: None, current: None, last: None });
        if let Some(i) = self.tabs.iter().position(|t| t.id == a) {
            self.active = i;
        }
        let page = self.tabs[self.active].current_page;
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == b) {
            t.goto_page(page);
        }
    }

    fn end_compare(&mut self) {
        if let Some(c) = self.compare.take() {
            for t in &mut self.tabs {
                if t.id == c.a || t.id == c.b {
                    t.diff_marks.clear();
                }
            }
        }
    }

    /// Tab indices (left, right) when the comparison is on screen.
    fn compare_panes(&self) -> Option<(usize, usize)> {
        let c = self.compare.as_ref()?;
        let a = self.tabs.iter().position(|t| t.id == c.a)?;
        let b = self.tabs.iter().position(|t| t.id == c.b)?;
        (self.active == a || self.active == b).then_some((a, b))
    }

    /// Keeps both sides on the same page and zoom, following whichever side moved.
    fn sync_comparison(&mut self) {
        let Some((ia, ib)) = self.compare_panes() else { return };
        let Some(c) = self.compare.as_mut() else { return };
        let state = |t: &DocTab| (t.current_page, t.viewport.zoom);
        let (now_a, now_b) = (state(&self.tabs[ia]), state(&self.tabs[ib]));
        if let Some((last_a, last_b)) = c.last {
            let (from, to) = if now_a != last_a {
                (ia, ib)
            } else if now_b != last_b {
                (ib, ia)
            } else {
                (ia, ia)
            };
            if from != to {
                let (page, zoom, fit, mode) = {
                    let t = &self.tabs[from];
                    (t.current_page, t.viewport.zoom, t.viewport.fit, t.mode)
                };
                let t = &mut self.tabs[to];
                if t.mode != mode {
                    t.set_mode(mode);
                }
                t.viewport.fit = fit;
                if (t.viewport.zoom - zoom).abs() > 1e-4 {
                    t.set_zoom(zoom);
                    t.viewport.fit = fit;
                }
                if t.current_page != page && page < t.page_count() {
                    t.goto_page(page);
                }
            }
        }
        c.last = Some((state(&self.tabs[ia]), state(&self.tabs[ib])));
    }

    fn compare_bar(&mut self, ui: &mut Ui) {
        let Some(c) = &self.compare else { return };
        let name = |id: DocId| self.tabs.iter().find(|t| t.id == id).map(|t| t.title()).unwrap_or_default();
        let (na, nb) = (name(c.a), name(c.b));
        let mut go: Option<usize> = None;
        let mut end = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(tf("Comparing {na} ↔ {nb}", &[("na", &na), ("nb", &nb)])).strong());
            ui.separator();
            match &c.diffs {
                None => {
                    ui.spinner();
                    ui.label(t("Comparing the text…"));
                }
                Some(d) if d.is_empty() => {
                    ui.label(t("The text is the same."));
                }
                Some(d) => {
                    let changes: usize = d.iter().map(|p| p.summary.len()).sum();
                    let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
                    ui.label(format!(
                        "{} on {}",
                        plural(changes, "change", "changes"),
                        plural(d.len(), "page", "pages")
                    ));
                    let cur = c.current;
                    if ui
                        .button(icons::text(icons::CARET_LEFT))
                        .on_hover_text(t("Previous page with changes"))
                        .clicked()
                    {
                        go = Some(cur.map_or(d.len() - 1, |i| (i + d.len() - 1) % d.len()));
                    }
                    if ui.button(icons::text(icons::CARET_RIGHT)).on_hover_text(t("Next page with changes")).clicked() {
                        go = Some(cur.map_or(0, |i| (i + 1) % d.len()));
                    }
                    if let Some(p) = cur.and_then(|i| d.get(i)) {
                        let text = p.summary.iter().take(3).cloned().collect::<Vec<_>>().join("   ·   ");
                        let more =
                            if p.summary.len() > 3 { format!("  (+{})", p.summary.len() - 3) } else { String::new() };
                        ui.label(format!("{}: {text}{more}", tf("Page {page}", &[("page", &(p.page + 1))])))
                            .on_hover_text(p.summary.join("\n"));
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                end = ui.button(t("End comparison")).clicked();
                ui.weak(t("Red: only on the left · Green: only on the right"));
            });
        });
        if end {
            self.end_compare();
            return;
        }
        if let Some(i) = go
            && let Some(c) = self.compare.as_mut()
            && let Some(page) = c.diffs.as_ref().and_then(|d| d.get(i)).map(|d| d.page)
        {
            c.current = Some(i);
            for t in &mut self.tabs {
                if (t.id == c.a || t.id == c.b) && page < t.page_count() {
                    t.goto_page(page);
                }
            }
            c.last = None;
        }
    }

    fn page_tool_dialogs(&mut self, ctx: &egui::Context) {
        use crate::view::page_tools::*;
        let Some(tab) = self.tabs.get_mut(self.active).filter(|t| t.info().is_some()) else {
            (self.crop_dialog, self.hf_dialog, self.optimize_dialog, self.images_dialog) = (None, None, None, None);
            (self.watermark_dialog, self.flatten_dialog) = (None, None);
            return;
        };
        let mut reqs = Vec::new();
        if let Some(d) = &mut self.crop_dialog {
            let (req, open) = crop_dialog(ctx, d, tab);
            reqs.extend(req);
            if !open {
                self.crop_dialog = None;
            }
        }
        if let Some(d) = &mut self.hf_dialog {
            let (req, open) = header_footer_dialog(ctx, d, tab, &mut self.settings.recent_colors);
            reqs.extend(req);
            if !open {
                self.hf_dialog = None;
            }
        }
        if let Some(d) = &mut self.optimize_dialog {
            let (req, open) = optimize_dialog(ctx, d, tab);
            reqs.extend(req);
            if !open {
                self.optimize_dialog = None;
            }
        }
        if let Some(d) = &mut self.watermark_dialog {
            let (req, open) = watermark_dialog(ctx, d, tab, &mut self.settings.recent_colors);
            reqs.extend(req);
            if !open {
                self.watermark_dialog = None;
            }
        }
        if let Some(d) = &mut self.flatten_dialog {
            let (req, open) = flatten_dialog(ctx, d, tab);
            reqs.extend(req);
            if !open {
                self.flatten_dialog = None;
            }
        }
        if let Some(d) = &mut self.images_dialog {
            let (req, open) = images_dialog(ctx, d, tab);
            reqs.extend(req);
            if !open {
                self.images_dialog = None;
            }
        }
        tab.outbox.extend(reqs);
    }

    fn split_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut every) = self.split_every else { return };
        let Some(tab) = self.tabs.get(self.active) else {
            self.split_every = None;
            return;
        };
        let (doc, count) = (tab.id, tab.page_count());
        let stem = tab.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let dir = tab.path.parent().map(Path::to_path_buf);
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(t("Split document"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t("Pages per file"));
                    ui.add(egui::DragValue::new(&mut every).range(1..=count.max(1)));
                });
                let files = count.div_ceil(every.max(1));
                ui.weak(tf(
                    "Creates {files} files named “{stem}-01.pdf”, “{stem}-02.pdf”, …",
                    &[("files", &files), ("stem", &stem)],
                ));
                ui.horizontal(|ui| {
                    go = ui.button(t("Choose folder…")).clicked();
                    cancel = ui.button(t("Cancel")).clicked();
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
                self.error = Some(tf("Could not open Windows Settings: {e}", &[("e", &e)]));
            }
            return;
        }
        let result =
            std::process::Command::new("xdg-mime").args(["default", "magic-pdf.desktop", "application/pdf"]).status();
        match result {
            Ok(s) if s.success() => self.notice(t("magic-pdf is now the default PDF viewer")),
            Ok(_) | Err(_) => {
                self.error = Some(
                    t("Could not set the default PDF viewer. Install magic-pdf with packaging/linux/install.sh first.")
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
        if let Some(doc) = self.tabs.get(self.active).map(|t| t.id) {
            self.commit_edits(doc);
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.info().is_none() {
            return;
        }
        if cfg!(windows) {
            let req = Request::PrintNative { doc: tab.id, title: tab.name.clone(), current_page: tab.current_page };
            self.send(req);
            return;
        }
        let doc = tab.id;
        match crate::print::list_printers() {
            Ok(p) if p.names.is_empty() => self.error = Some(t("No printers are set up.").into()),
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

    fn combine_ui(&mut self, ctx: &egui::Context) {
        if self.combining {
            busy(ctx, t("Combining…"));
        }
        let Some(dialog) = &mut self.combine_dialog else { return };
        let unsaved: Vec<PathBuf> = self.tabs.iter().filter(|t| t.dirty).map(|t| t.path.clone()).collect();
        let (request, open) = dialog.show(ctx, &unsaved);
        if !open {
            self.combine_dialog = None;
        }
        if let Some(r) = request {
            self.combining = true;
            self.send(r);
        }
    }

    /// The digital signature tool: drag a box, then the signing dialog.
    fn start_digital_signature(&mut self) {
        self.tool = Tool::DigitalSign;
        self.notice(t("Drag a box where the signature goes"));
    }

    fn sign_ui(&mut self, ctx: &egui::Context) {
        while let Ok((doc, out, result)) = self.sign_results.1.try_recv() {
            self.signing = false;
            match result {
                Ok(()) => {
                    self.notice(&tf("Signed and saved as {file}", &[("file", &out.display())]));
                    self.show_signed(doc, &out);
                }
                Err(e) => self.error = Some(tf("Signing failed: {e}", &[("e", &e)])),
            }
        }
        if std::mem::take(&mut self.stamps.start_digital_signature) {
            self.start_digital_signature();
        }
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Some((page, rect)) = tab.sign_box.take()
        {
            self.tool = Tool::Hand;
            self.sign_dialog = Some(crate::sign_dialog::SignDialog::new(tab.id, page, rect));
        }
        if self.signing {
            busy(ctx, t("Signing…"));
        }
        let Some(dialog) = &mut self.sign_dialog else { return };
        let Some(tab) = self.tabs.iter().find(|t| t.id == dialog.doc) else {
            self.sign_dialog = None;
            return;
        };
        let (file, unsaved) = (tab.path.clone(), tab.dirty);
        let action = dialog.show(ctx, &mut self.settings, &mut self.identities, &file, unsaved);
        match action {
            crate::sign_dialog::SignAction::Keep => {}
            crate::sign_dialog::SignAction::Close => {
                self.sign_dialog = None;
                self.settings.save();
            }
            crate::sign_dialog::SignAction::Sign { request, identity, out } => {
                let doc = dialog.doc;
                self.sign_dialog = None;
                self.settings.save();
                self.signing = true;
                let tx = self.sign_results.0.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let result = crate::sign_dialog::sign_file(&file, &out, &request, &identity);
                    let _ = tx.send((doc, out, result));
                    ctx.request_repaint();
                });
            }
        }
    }

    /// After signing: reloads the tab when the signed file replaced it, else opens
    /// the signed copy.
    fn show_signed(&mut self, doc: DocId, out: &Path) {
        let out = simplify_path(std::fs::canonicalize(out).unwrap_or_else(|_| out.to_path_buf()));
        match self.tabs.iter().position(|t| t.id == doc && t.path == out) {
            Some(index) => {
                let page = self.tabs[index].current_page;
                self.close_tab_now(index);
                self.open(&out);
                let last = self.tabs.len() - 1;
                let tab = self.tabs.remove(last);
                self.tabs.insert(index.min(self.tabs.len()), tab);
                self.active = index.min(self.tabs.len() - 1);
                self.tabs[self.active].pending_goto = Some(page);
            }
            None => self.open(&out),
        }
        self.signatures_on_load = true;
    }

    fn print_dialog_ui(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.print_results.1.try_recv() {
            match result {
                Ok(msg) if msg.is_empty() => self.notice(t("Sent to the printer")),
                Ok(msg) => self.notice(&tf("Sent to the printer ({msg})", &[("msg", &msg)])),
                Err(e) => self.error = Some(tf("Printing failed: {e}", &[("e", &e)])),
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
        egui::Window::new(t("Print"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("print_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(t("Printer"));
                    egui::ComboBox::from_id_salt("printer")
                        .selected_text(dlg.printer.clone().unwrap_or_default())
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            for p in &dlg.printers {
                                ui.selectable_value(&mut dlg.printer, Some(p.clone()), p);
                            }
                        });
                    ui.end_row();
                    ui.label(t("Copies"));
                    ui.add(egui::DragValue::new(&mut dlg.copies).range(1..=99));
                    ui.end_row();
                    ui.label(t("Pages"));
                    ui.vertical(|ui| {
                        ui.radio_value(&mut dlg.pages, PageChoice::All, tf("All ({count})", &[("count", &count)]));
                        ui.radio_value(
                            &mut dlg.pages,
                            PageChoice::Current,
                            tf("Current page ({page})", &[("page", &(current + 1))]),
                        );
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut dlg.pages, PageChoice::Range, t("Pages:"));
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
                    ui.label(t("Options"));
                    ui.vertical(|ui| {
                        ui.checkbox(&mut dlg.two_sided, t("Print on both sides"));
                        ui.checkbox(&mut dlg.fit_to_page, t("Fit to paper"));
                    });
                    ui.end_row();
                });
                if let Some(e) = &dlg.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t("Print")).clicked() {
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
                    if ui.button(t("Cancel")).clicked() {
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
        if self.search_all
            && let Some(active) = self.tabs.get(self.active)
        {
            // Every open document searches for the active tab's query.
            let (q, case, word) = (active.search.query.clone(), active.search.match_case, active.search.whole_word);
            let current = self.active;
            for i in 0..self.tabs.len() {
                if i == current || matches!(self.tabs[i].state, LoadState::Failed(_)) {
                    continue;
                }
                let t = &mut self.tabs[i];
                (t.search.query, t.search.match_case, t.search.whole_word) = (q.clone(), case, word);
                self.active = i;
                self.start_one_search();
            }
            self.active = current;
        }
        self.start_one_search();
    }

    fn start_one_search(&mut self) {
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
            query: SearchQuery { text: query, match_case: tab.search.match_case, whole_word: tab.search.whole_word },
        };
        self.sidebar_tab = SidebarTab::Search;
        self.send(req);
    }

    fn clear_search(&mut self) {
        let tabs: Vec<usize> = if self.search_all { (0..self.tabs.len()).collect() } else { vec![self.active] };
        for i in tabs {
            let Some(tab) = self.tabs.get_mut(i) else { continue };
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
            let partner = self.compare.as_ref().is_some_and(|c| c.a == tab.id || c.b == tab.id);
            if i != self.active && !partner {
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
                engine.send(Request::Retain { doc: tab.id, keep: wanted.clone() });
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
            let req = Request::DeleteAnnot { doc: tab.id, page: sel.page, index: sel.index };
            self.send(req);
        }
    }

    /// Menu, toolbar, tab bar, notice bars and status bar.
    fn panels(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if Tool::ANNOTATION_TOOLS.contains(&self.tool) {
            egui::Panel::top("tool_options").show(ui, |ui| self.tool_options(ui));
        }
        if !self.tabs.is_empty() {
            egui::Panel::top("tabs").show(ui, |ui| self.tab_bar(ui));
        }
        if self.compare_panes().is_some() {
            egui::Panel::top("compare").show(ui, |ui| self.compare_bar(ui));
        }
        if self.tabs.get(self.active).is_some_and(|t| self.einvoices.contains_key(&t.id)) {
            egui::Panel::top("einvoice").show(ui, |ui| self.einvoice_bar(ui));
        }
        if let Some(tab) = self.tabs.get(self.active)
            && let Some(info) = tab.info().filter(|i| !i.signatures.is_empty())
        {
            let doc = tab.id;
            let sigs = info.signatures.clone();
            egui::Panel::top("signatures").show(ui, |ui| {
                if crate::view::signatures::bar(ui, &sigs) {
                    self.show_signatures = Some(doc);
                }
            });
        }
        if self.tabs.get(self.active).is_some_and(|t| self.tool == Tool::Redact || !t.redactions.is_empty()) {
            egui::Panel::top("redaction").show(ui, |ui| self.redaction_bar(ui));
        }

        if let Some(tab) = self.tabs.get(self.active) {
            let status = match &tab.state {
                LoadState::Ready(info) => format!(
                    "{}   ·   {}   ·   {:.0}%{}{}",
                    tf(
                        "Page {page} of {pages}",
                        &[("page", &(tab.current_page + 1)), ("pages", &info.page_sizes.len())]
                    ),
                    tab.mode.label(),
                    tab.zoom_percent(),
                    info.title.as_ref().map(|t| format!("   ·   {t}")).unwrap_or_default(),
                    if tab.dirty { format!("   ·   {}", t("unsaved changes")) } else { String::new() }
                ),
                LoadState::Loading => t("Loading…").into(),
                LoadState::NeedsPassword { .. } => t("Password required").into(),
                LoadState::Failed(_) => t("Failed to open").into(),
            };
            let now = ctx.input(|i| i.time);
            let notice = self.notice.as_ref().filter(|(_, t)| now - t < 6.0).map(|(n, _)| n.clone());
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
    }

    /// "Esc ends the presentation", shown for a few seconds after it starts.
    fn presentation_hint(&self, ctx: &egui::Context) {
        let Some(p) = &self.presenting else { return };
        let age = ctx.input(|i| i.time) - p.since;
        if age < 3.0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            egui::Area::new(egui::Id::unique("presentation_hint"))
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(t("Presentation: arrows, space or click for the next page; Esc ends"));
                    });
                });
        }
    }

    fn start_presentation(&mut self, ctx: &egui::Context) {
        let tool = self.tool;
        let sidebar = self.settings.show_sidebar;
        let Some(tab) = self.tabs.get_mut(self.active).filter(|t| t.info().is_some()) else { return };
        self.presenting = Some(Presentation {
            doc: tab.id,
            mode: tab.mode,
            fit: tab.viewport.fit,
            zoom: tab.viewport.zoom,
            sidebar,
            tool,
            since: ctx.input(|i| i.time),
        });
        tab.cancel_interaction();
        tab.set_mode(LayoutMode::SinglePage);
        tab.viewport.fit = crate::view::viewport::FitMode::Page;
        self.tool = Tool::Hand;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
    }

    fn end_presentation(&mut self, ctx: &egui::Context) {
        let Some(p) = self.presenting.take() else { return };
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        self.tool = p.tool;
        self.settings.show_sidebar = p.sidebar;
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == p.doc) {
            let page = tab.current_page;
            tab.set_mode(p.mode);
            tab.viewport.fit = p.fit;
            tab.viewport.zoom = p.zoom;
            tab.goto_page(page);
        }
    }

    /// Keys and clicks during a presentation. True if presenting (other shortcuts off).
    fn presentation_input(&mut self, ctx: &egui::Context) -> bool {
        let Some(p) = &self.presenting else { return false };
        let Some(tab) = self.tabs.get_mut(self.active).filter(|t| t.id == p.doc) else {
            self.end_presentation(ctx);
            return false;
        };
        let mut end = false;
        ctx.input_mut(|i| {
            for key in [Key::ArrowRight, Key::ArrowDown, Key::PageDown, Key::Space, Key::Enter] {
                while i.consume_key(Modifiers::NONE, key) {
                    tab.step_page(true);
                }
            }
            for key in [Key::ArrowLeft, Key::ArrowUp, Key::PageUp, Key::Backspace] {
                while i.consume_key(Modifiers::NONE, key) {
                    tab.step_page(false);
                }
            }
            if i.consume_key(Modifiers::NONE, Key::Home) {
                tab.goto_page(0);
            }
            if i.consume_key(Modifiers::NONE, Key::End) {
                tab.goto_page(usize::MAX);
            }
            if i.pointer.button_clicked(egui::PointerButton::Primary) {
                tab.step_page(true);
            }
            if i.pointer.button_clicked(egui::PointerButton::Secondary) {
                tab.step_page(false);
            }
            end = i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::NONE, Key::F5);
        });
        if end {
            self.end_presentation(ctx);
        }
        true
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let shortcut = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        if self.present_on_load && self.tabs.get(self.active).is_some_and(|t| t.info().is_some()) {
            self.present_on_load = false;
            self.start_presentation(ctx);
        }
        if self.presentation_input(ctx) {
            return;
        }
        if shortcut(Modifiers::NONE, Key::F5) {
            self.start_presentation(ctx);
            return;
        }

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
        if shortcut(cmd, Key::D) {
            self.props_dialog = self.tabs.get(self.active).and_then(crate::view::doc_info::PropertiesDialog::new);
        }
        if shortcut(cmd, Key::Comma) {
            self.show_settings = true;
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
                (Key::T, Tool::TextBox),
                (Key::L, Tool::Line),
                (Key::A, Tool::Arrow),
                (Key::P, Tool::Polygon),
                (Key::C, Tool::Cloud),
                (Key::K, Tool::Stamp),
                (Key::G, Tool::Signature),
                (Key::M, Tool::Distance),
                (Key::X, Tool::Redact),
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
        let pasted = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(t) => Some(t.clone()),
                _ => None,
            })
        });
        let Some(tab) = self.tabs.get_mut(self.active) else { return };

        if !typing
            && copy
            && let Some(text) = tab.selected_text()
        {
            ctx.copy_text(text);
            self.annot_clip_text = None;
        } else if !typing
            && copy
            && tab.form_focus.is_none()
            && let Some(sel) = &tab.selected_annot
        {
            // Copy the annotation. The clipboard gets a text too (its comment), and
            // pasting that same text pastes the annotation.
            tab.outbox.push(Request::CopyAnnot { doc: tab.id, page: sel.page, index: sel.index });
            let kind = tab
                .annots
                .get(&sel.page)
                .and_then(|v| v.iter().find(|a| a.index == sel.index))
                .map(|a| a.kind.label())
                .unwrap_or("Annotation");
            let text = if sel.committed.trim().is_empty() { format!("[{kind}]") } else { sel.committed.clone() };
            ctx.copy_text(text.clone());
            self.annot_clip_text = Some(text);
            self.notice = Some((
                tf("{kind} copied (Ctrl+V pastes it under the mouse)", &[("kind", &kind)]),
                ctx.input(|i| i.time),
            ));
        }
        if !typing
            && tab.form_focus.is_none()
            && let Some(text) = pasted
            && self.annot_clip_text.as_deref().map(|t| t.replace("\r\n", "\n")) == Some(text)
        {
            let (page, at) = match tab.hover_pdf {
                Some((page, at)) => (page, Some(at)),
                None => (tab.current_page, None),
            };
            tab.outbox.push(Request::PasteAnnot { doc: tab.id, page, at });
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
        for (key, mode) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5].into_iter().zip(LayoutMode::ALL) {
            if shortcut(cmd, key) {
                tab.set_mode(mode);
            }
        }
        if typing {
            return;
        }
        // Arrow keys move a selected (movable) annotation: 1 pt, 10 pt with Shift.
        let can_nudge = tab
            .selected_annot
            .as_ref()
            .is_some_and(|sel| !sel.editing && tab.annot(sel.page, sel.index).is_some_and(|a| a.kind.movable()));
        if can_nudge {
            ctx.input_mut(|i| {
                for (key, dir) in [
                    (Key::ArrowLeft, egui::vec2(-1.0, 0.0)),
                    (Key::ArrowRight, egui::vec2(1.0, 0.0)),
                    (Key::ArrowUp, egui::vec2(0.0, -1.0)),
                    (Key::ArrowDown, egui::vec2(0.0, 1.0)),
                ] {
                    for (mods, step) in [(Modifiers::NONE, 1.0), (Modifiers::SHIFT, 10.0)] {
                        while i.consume_key(mods, key) {
                            tab.nudge += dir * step;
                        }
                    }
                }
            });
        }
        // Grid overview: arrow keys move between pages, Enter opens the one marked.
        if tab.mode == LayoutMode::Grid && !can_nudge && matches!(self.tool, Tool::Hand | Tool::SelectText) {
            ctx.input_mut(|i| {
                for (key, dx, dy) in
                    [(Key::ArrowLeft, -1, 0), (Key::ArrowRight, 1, 0), (Key::ArrowUp, 0, -1), (Key::ArrowDown, 0, 1)]
                {
                    while i.consume_key(Modifiers::NONE, key) {
                        tab.grid_move(dx, dy);
                    }
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) {
                    tab.open_from_grid(tab.grid_focus.unwrap_or(tab.current_page));
                }
            });
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
            ui.add(egui::Image::new(&self.app_icon).fit_to_exact_size(egui::vec2(18.0, 18.0)));
            ui.add_space(4.0);
            let has_tab = self.tabs.get(self.active).is_some_and(|t| t.info().is_some());
            ui.menu_button(t("File"), |ui| {
                if ui.button(kb(t("Open…\tCtrl+O"))).clicked() {
                    self.open_dialog();
                }
                let any_recent = !self.settings.recent.is_empty() || !self.settings.pinned.is_empty();
                ui.add_enabled_ui(any_recent, |ui| {
                    ui.menu_button(t("Open recent"), |ui| {
                        if let Some(a) = recent_list(ui, &self.settings, true) {
                            self.recent_action(a);
                            ui.close();
                        }
                    });
                });
                ui.separator();
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Save\tCtrl+S")))).clicked() {
                    self.save_active(false);
                }
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Save as…\tCtrl+Shift+S")))).clicked() {
                    self.save_active(true);
                }
                ui.add_enabled_ui(has_tab, |ui| {
                    ui.menu_button(t("Compare with"), |ui| {
                        let others: Vec<(DocId, String)> = self
                            .tabs
                            .iter()
                            .enumerate()
                            .filter(|(i, t)| *i != self.active && t.info().is_some())
                            .map(|(_, t)| (t.id, t.title()))
                            .collect();
                        for (id, title) in others {
                            if ui.button(title).clicked() {
                                self.start_compare(Some(id));
                                ui.close();
                            }
                        }
                        if ui.button(t("Another file…")).clicked() {
                            self.start_compare(None);
                            ui.close();
                        }
                    });
                });
                if ui.add_enabled(has_tab, egui::Button::new(t("Open containing folder"))).clicked()
                    && let Some(tab) = self.tabs.get(self.active)
                {
                    crate::reveal::show_in_folder(&tab.path);
                }
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Document properties…\tCtrl+D")))).clicked() {
                    self.props_dialog =
                        self.tabs.get(self.active).and_then(crate::view::doc_info::PropertiesDialog::new);
                }
                if ui.add_enabled(has_tab, egui::Button::new(t("Save optimized copy…"))).clicked() {
                    self.optimize_dialog = Some(Default::default());
                }
                if ui.add_enabled(has_tab, egui::Button::new(t("Export pages as pictures…"))).clicked() {
                    self.images_dialog = Some(Default::default());
                }
                if ui.button(t("Combine files…")).clicked() {
                    let first = self.tabs.get(self.active).map(|t| t.path.clone());
                    self.combine_dialog = Some(crate::view::combine::CombineDialog::new(first));
                }
                if ui.add_enabled(has_tab, egui::Button::new(t("Sign with certificate…"))).clicked() {
                    self.start_digital_signature();
                }
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Print…\tCtrl+P")))).clicked() {
                    self.print();
                }
                ui.separator();
                if ui.add_enabled(!self.tabs.is_empty(), egui::Button::new(kb(t("Close tab\tCtrl+W")))).clicked()
                    && let Some(t) = self.tabs.get(self.active)
                {
                    self.request_close(vec![t.id], false);
                }
                if ui.button(kb(t("Quit\tCtrl+Q"))).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button(t("Edit"), |ui| {
                let (can_undo, can_redo, has_sel) = self
                    .tabs
                    .get(self.active)
                    .map(|t| (t.can_undo, t.can_redo, t.selected_annot.is_some()))
                    .unwrap_or_default();
                if ui.add_enabled(can_undo, egui::Button::new(kb(t("Undo\tCtrl+Z")))).clicked() {
                    self.undo(false);
                }
                if ui.add_enabled(can_redo, egui::Button::new(kb(t("Redo\tCtrl+Y")))).clicked() {
                    self.undo(true);
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new(t("Delete annotation\tDel"))).clicked() {
                    self.delete_selected_annot();
                }
                if ui.add_enabled(has_sel, egui::Button::new(kb(t("Copy annotation\tCtrl+C")))).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestCopy);
                }
                if ui
                    .add_enabled(
                        self.annot_clip_text.is_some() && has_tab,
                        egui::Button::new(kb(t("Paste annotation\tCtrl+V"))),
                    )
                    .clicked()
                    && let Some(tab) = self.tabs.get_mut(self.active)
                {
                    // From the menu: next to the original (or at the same place on this page).
                    let page = tab.current_page;
                    tab.outbox.push(Request::PasteAnnot { doc: tab.id, page, at: None });
                }
                ui.separator();
                if ui.button(kb(t("Settings…\tCtrl+,"))).clicked() {
                    self.show_settings = true;
                }
            });
            ui.menu_button(t("Pages"), |ui| {
                let (n, total) = self
                    .tabs
                    .get(self.active)
                    .filter(|t| t.info().is_some())
                    .map(|t| (t.target_pages().len(), t.page_count()))
                    .unwrap_or((0, 0));
                ui.add_enabled_ui(total > 0, |ui| {
                    ui.weak(if n == 1 {
                        t("Acts on the selected (or current) page").to_string()
                    } else {
                        tf("Acts on {n} selected pages", &[("n", &n)])
                    });
                    if let Some(a) = sidebar::page_menu(ui, n, total) {
                        self.page_action(a);
                    }
                    ui.separator();
                    if ui.button(t("Recognise text (OCR)…")).clicked()
                        && let Some(t) = self.tabs.get(self.active)
                    {
                        self.ocr.open(t.id, t.page_selection.len() > 1);
                    }
                    ui.separator();
                    if ui.button(t("Crop pages…")).clicked() {
                        self.crop_dialog = Some(Default::default());
                    }
                    if ui.button(t("Header and footer…")).clicked() {
                        self.hf_dialog = Some(Default::default());
                    }
                    if ui
                        .button(t("Remove header and footer"))
                        .on_hover_text(t("Removes what magic-pdf added, on all pages"))
                        .clicked()
                        && let Some(tab) = self.tabs.get_mut(self.active)
                    {
                        let pages = (0..tab.page_count()).collect();
                        tab.outbox.push(Request::EditPages { doc: tab.id, op: PageOp::RemoveHeaderFooter { pages } });
                    }
                    if ui.button(t("Watermark…")).clicked() {
                        self.watermark_dialog = Some(Default::default());
                    }
                    if ui
                        .button(t("Remove watermark"))
                        .on_hover_text(t("Removes what magic-pdf added, on all pages"))
                        .clicked()
                        && let Some(tab) = self.tabs.get_mut(self.active)
                    {
                        let pages = (0..tab.page_count()).collect();
                        tab.outbox.push(Request::EditPages { doc: tab.id, op: PageOp::RemoveWatermark { pages } });
                    }
                    if ui.button(t("Flatten annotations and form fields…")).clicked() {
                        self.flatten_dialog = Some(Default::default());
                    }
                    ui.separator();
                    if ui.button(t("Redact (mark areas)")).clicked() {
                        self.tool = Tool::Redact;
                    }
                    let (hits, marks) =
                        self.tabs.get(self.active).map_or((0, 0), |t| (t.search.hits.len(), t.redactions.len()));
                    if ui.add_enabled(hits > 0, egui::Button::new(t("Mark search matches for redaction"))).clicked()
                        && let Some(tab) = self.tabs.get_mut(self.active)
                    {
                        tab.mark_search_hits();
                    }
                    if ui.add_enabled(marks > 0, egui::Button::new(t("Apply redactions…"))).clicked() {
                        self.confirm_redact = self.tabs.get(self.active).map(|t| t.id);
                    }
                });
            });
            ui.menu_button(t("View"), |ui| {
                for (i, mode) in LayoutMode::ALL.into_iter().enumerate() {
                    let selected = self.tabs.get(self.active).is_some_and(|t| t.mode == mode);
                    if ui
                        .add_enabled(
                            has_tab,
                            egui::Button::selectable(selected, kb(&format!("{}\tCtrl+{}", mode.label(), i + 1))),
                        )
                        .clicked()
                        && let Some(t) = self.active_tab()
                    {
                        t.set_mode(mode);
                    }
                }
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Presentation\tF5")))).clicked() {
                    let ctx = ui.ctx().clone();
                    self.start_presentation(&ctx);
                }
                ui.separator();
                for fit in [FitMode::Width, FitMode::Height, FitMode::Page] {
                    if ui.add_enabled(has_tab, egui::Button::new(fit.label())).clicked()
                        && let Some(t) = self.active_tab()
                    {
                        t.viewport.fit = fit;
                    }
                }
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Actual size\tCtrl+0")))).clicked()
                    && let Some(t) = self.active_tab()
                {
                    t.set_zoom(ACTUAL_SIZE);
                }
                ui.separator();
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Rotate clockwise\tCtrl+R")))).clicked()
                    && let Some(t) = self.active_tab()
                {
                    t.rotate(true);
                }
                if ui.add_enabled(has_tab, egui::Button::new(kb(t("Rotate counter-clockwise\tCtrl+Shift+R")))).clicked()
                    && let Some(t) = self.active_tab()
                {
                    t.rotate(false);
                }
                ui.separator();
                ui.checkbox(&mut self.settings.show_sidebar, t("Sidebar\tF9"));
                let mut night = self.settings.night_mode;
                if ui.checkbox(&mut night, t("Night mode (invert pages)")).changed() {
                    self.set_night_mode(night);
                }
                let mut dark = self.settings.dark_ui;
                if ui.checkbox(&mut dark, t("Dark interface")).changed() {
                    self.settings.dark_ui = dark;
                    ui.ctx().set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
                    self.settings.save();
                }
            });
            ui.menu_button(t("Help"), |ui| {
                if cfg!(any(windows, target_os = "linux"))
                    && ui.button(t("Make magic-pdf the default PDF viewer…")).clicked()
                {
                    self.make_default_viewer();
                }
                if ui.button(t("Check for updates…")).clicked() {
                    self.check_for_updates(true);
                    ui.close();
                }
                ui.separator();
                if ui.button(t("Keyboard shortcuts")).clicked() {
                    self.show_shortcuts = true;
                }
                if ui.button(t("About magic-pdf")).clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        // In a narrow window the buttons wrap onto a second row, so all stay reachable.
        ui.horizontal_wrapped(|ui| self.toolbar_buttons(ui));
    }

    /// The toolbar's buttons (all but search).
    fn toolbar_buttons(&mut self, ui: &mut Ui) {
        {
            ui.toggle_value(&mut self.settings.show_sidebar, icons::text(icons::SIDEBAR))
                .on_hover_text(t("Show or hide the sidebar (F9)"));
            ui.separator();
            if ui.button(icons::text(icons::FOLDER_OPEN)).on_hover_text(kb(t("Open a PDF (Ctrl+O)"))).clicked() {
                self.open_dialog();
            }
            let (has_doc, dirty, can_undo, can_redo) = self
                .tabs
                .get(self.active)
                .map(|t| (t.info().is_some(), t.dirty, t.can_undo, t.can_redo))
                .unwrap_or_default();
            if ui
                .add_enabled(has_doc && dirty, egui::Button::new(icons::text(icons::FLOPPY_DISK)))
                .on_hover_text(kb(t("Save (Ctrl+S)")))
                .clicked()
            {
                self.save_active(false);
            }
            if ui
                .add_enabled(has_doc, egui::Button::new(icons::text(icons::PRINTER)))
                .on_hover_text(kb(t("Print (Ctrl+P)")))
                .clicked()
            {
                self.print();
            }
            if ui
                .add_enabled(can_undo, egui::Button::new(icons::text(icons::UNDO)))
                .on_hover_text(kb(t("Undo (Ctrl+Z)")))
                .clicked()
            {
                self.undo(false);
            }
            if ui
                .add_enabled(can_redo, egui::Button::new(icons::text(icons::REDO)))
                .on_hover_text(kb(t("Redo (Ctrl+Y)")))
                .clicked()
            {
                self.undo(true);
            }
            ui.separator();
            // The tools and page controls need a document: greyed out on the start
            // screen (the rest of the row).
            if !has_doc {
                ui.disable();
            }
            for tool in [Tool::Hand, Tool::SelectText] {
                ui.selectable_value(&mut self.tool, tool, tool.icon()).on_hover_text(tool.label());
            }
            ui.separator();
            const SHAPES: [Tool; 8] = [
                Tool::Rect,
                Tool::Ellipse,
                Tool::Line,
                Tool::Arrow,
                Tool::Polygon,
                Tool::Cloud,
                Tool::Distance,
                Tool::Area,
            ];
            if SHAPES.contains(&self.tool) {
                self.last_shape = self.tool;
            }
            for tool in Tool::ANNOTATION_TOOLS {
                if SHAPES.contains(&tool) && tool != self.last_shape {
                    continue;
                }
                ui.selectable_value(&mut self.tool, tool, tool.icon()).on_hover_text(tool.label());
                if tool == self.last_shape {
                    // The other shapes in a drop-down next to the current one.
                    let menu = ui.menu_button(icons::text(icons::CARET_DOWN).size(11.0), |ui| {
                        for shape in SHAPES {
                            let picked = ui
                                .horizontal(|ui| {
                                    let icon = ui.selectable_label(self.tool == shape, shape.icon());
                                    let text = ui.selectable_label(self.tool == shape, shape.label());
                                    icon.clicked() || text.clicked()
                                })
                                .inner;
                            if picked {
                                self.tool = shape;
                                ui.close();
                            }
                        }
                    });
                    menu.response.on_hover_text(t("More shapes"));
                }
            }
            ui.separator();
            ui.selectable_value(&mut self.tool, Tool::Redact, Tool::Redact.icon()).on_hover_text(Tool::Redact.label());
            ui.separator();

            let Some(tab) = self.tabs.get_mut(self.active) else { return };
            let ready = tab.info().is_some();
            // Greyed out while a document loads. Not a nested group: that would wrap
            // only within the rest of the row in a narrow window.
            {
                if !ready {
                    ui.disable();
                }
                let n = tab.page_count();
                if ui.button(icons::text(icons::CARET_LEFT)).on_hover_text(t("Previous page (PgUp)")).clicked() {
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
                if ui.button(icons::text(icons::CARET_RIGHT)).on_hover_text(t("Next page (PgDn)")).clicked() {
                    tab.step_page(true);
                }
                ui.separator();

                if ui.button(icons::text(icons::ZOOM_OUT)).on_hover_text(kb(t("Zoom out (Ctrl+-)"))).clicked() {
                    tab.zoom_step(false);
                }
                let label = match tab.viewport.fit {
                    FitMode::None => format!("{:.0}%", tab.zoom_percent()),
                    fit => format!("{} ({:.0}%)", fit.label(), tab.zoom_percent()),
                };
                egui::ComboBox::from_id_salt("zoom").selected_text(label).width(130.0).show_ui(ui, |ui| {
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
                if ui.button(icons::text(icons::ZOOM_IN)).on_hover_text(kb(t("Zoom in (Ctrl++)"))).clicked() {
                    tab.zoom_step(true);
                }
                ui.separator();

                // Page layout: one button showing the current layout, the others in its menu.
                let layouts = LayoutMode::ALL.into_iter().zip([
                    icons::COLUMNS,
                    icons::ROWS,
                    icons::FILE,
                    icons::BOOK_OPEN,
                    icons::SQUARES_FOUR,
                ]);
                let current = layouts.clone().find(|(m, _)| *m == tab.mode).map_or(icons::COLUMNS, |(_, i)| i);
                ui.menu_button(icons::text(current), |ui| {
                    for (i, (mode, icon)) in layouts.enumerate() {
                        let picked = ui
                            .horizontal(|ui| {
                                let a = ui.selectable_label(tab.mode == mode, icons::text(icon));
                                let b = ui.selectable_label(
                                    tab.mode == mode,
                                    kb(&format!("{}  Ctrl+{}", mode.label(), i + 1)),
                                );
                                a.clicked() || b.clicked()
                            })
                            .inner;
                        if picked {
                            tab.set_mode(mode);
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text(tf("Page layout: {layout}", &[("layout", &tab.mode.label())]));
            }
        }
    }

    /// The search box with its options and match count, right-aligned.
    fn search_bar(&mut self, ui: &mut Ui) {
        // Search, right-aligned.
        let mut submit = false;
        let mut clear = false;
        let used = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let Some(tab) = self.tabs.get_mut(self.active) else { return 0.0 };
            if ui.button(icons::text(icons::CARET_DOWN)).on_hover_text(t("Next match (F3)")).clicked() {
                tab.search_step(true);
            }
            if ui.button(icons::text(icons::CARET_UP)).on_hover_text(t("Previous match (Shift+F3)")).clicked() {
                tab.search_step(false);
            }
            let all_before = self.search_all;
            let s = &mut tab.search;
            let before = (s.match_case, s.whole_word);
            // Search options in one menu (keeps the toolbar narrow); highlighted when
            // any is on.
            let any = s.match_case || s.whole_word || self.search_all;
            let icon = icons::text(icons::TEXT_AA);
            let icon = if any { icon.color(ui.visuals().selection.stroke.color) } else { icon };
            ui.menu_button(icon, |ui| {
                ui.checkbox(&mut s.match_case, t("Match case"));
                ui.checkbox(&mut s.whole_word, t("Whole words"));
                ui.checkbox(&mut self.search_all, t("All open tabs"));
            })
            .response
            .on_hover_text(t("Search options: match case, whole words, all open tabs"));
            if (before != (s.match_case, s.whole_word) || all_before != self.search_all) && s.submitted.is_some() {
                submit = true;
            }
            if s.submitted.is_some() {
                let status = match (s.current, s.hits.len()) {
                    (_, 0) if s.running => "…".to_string(),
                    (_, 0) => t("No matches").to_string(),
                    (Some(c), n) => format!("{}/{}{}", c + 1, n, if s.running { "+" } else { "" }),
                    (None, n) => format!("{n}"),
                };
                ui.weak(status);
            }
            let id = egui::Id::unique(SEARCH_ID);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut s.query)
                    .id(id)
                    .hint_text(kb(t("Search (Ctrl+F)")))
                    .desired_width(130.0),
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
            ui.min_rect().width()
        });
        if used.inner > 0.0 {
            self.search_width = used.inner;
        }
        if submit {
            self.start_search();
        }
        if clear {
            self.clear_search();
        }
    }

    /// Settings for new annotations of the active tool, shown in a bar under the
    /// toolbar while an annotation tool is selected.
    fn tool_options(&mut self, ui: &mut Ui) {
        let tool = self.tool;
        if matches!(tool, Tool::Stamp | Tool::Signature) {
            use crate::view::stamps;
            let mut changed = false;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(tool.label()).strong());
                ui.separator();
                changed |= if tool == Tool::Stamp {
                    stamps::stamp_options(ui, &mut self.stamps, &mut self.settings)
                } else {
                    stamps::signature_options(ui, &mut self.stamps, &mut self.settings)
                };
                let recent = &mut self.settings.recent_colors;
                if let Some(slot) = tool.color(&mut self.settings.annot_style) {
                    ui.label(t("Colour"));
                    if let Some(Some(c)) =
                        color_picker::color_button(ui, ("tool", tool.label()), Some(*slot), false, recent)
                    {
                        *slot = c;
                        changed = true;
                    }
                }
                if let Some(e) = self.stamps.image_error(&self.settings) {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
            });
            if changed {
                self.settings.save();
            }
            return;
        }
        let style = &mut self.settings.annot_style;
        let recent = &mut self.settings.recent_colors;
        let mut changed = false;
        let width = |ui: &mut Ui, w: &mut f32| ui.add(egui::Slider::new(w, 0.5..=12.0).text("pt")).changed();
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(tool.label()).strong());
            ui.separator();
            if let Some(slot) = tool.color(style) {
                ui.label(if tool == Tool::TextBox { t("Border") } else { t("Colour") });
                if let Some(Some(c)) =
                    color_picker::color_button(ui, ("tool", tool.label()), Some(*slot), false, recent)
                {
                    *slot = c;
                    changed = true;
                }
            }
            match tool {
                Tool::Ink => {
                    ui.label(t("Width"));
                    changed |= width(ui, &mut style.ink_width);
                }
                Tool::Line | Tool::Arrow => {
                    ui.label(t("Width"));
                    changed |= width(ui, &mut style.shape_width);
                }
                Tool::Distance | Tool::Area => {
                    ui.label(t("Width"));
                    changed |= width(ui, &mut style.shape_width);
                    if tool == Tool::Area {
                        ui.label(t("Fill"));
                        if let Some(f) =
                            color_picker::color_button(ui, "tool_area_fill", style.shape_fill, true, recent)
                        {
                            style.shape_fill = f;
                            changed = true;
                        }
                    }
                    ui.separator();
                    ui.label(t("Scale 1 :"));
                    changed |= ui
                        .add(egui::DragValue::new(&mut style.measure_ratio).range(0.001..=1_000_000.0).speed(1.0))
                        .on_hover_text(t("1 for full size; 100 for a drawing at 1:100"))
                        .changed();
                    egui::ComboBox::from_id_salt("measure_unit")
                        .selected_text(style.measure_unit.clone())
                        .width(50.0)
                        .show_ui(ui, |ui| {
                            for u in ["mm", "cm", "m", "km", "in", "ft"] {
                                changed |= ui.selectable_value(&mut style.measure_unit, u.to_string(), u).changed();
                            }
                        });
                }
                Tool::Rect | Tool::Ellipse | Tool::Polygon | Tool::Cloud => {
                    ui.label(t("Width"));
                    changed |= width(ui, &mut style.shape_width);
                    ui.label(t("Fill"));
                    if let Some(f) = color_picker::color_button(ui, "tool_shape_fill", style.shape_fill, true, recent) {
                        style.shape_fill = f;
                        changed = true;
                    }
                }
                Tool::TextBox => {
                    ui.label(t("Width"));
                    changed |=
                        ui.add(egui::Slider::new(&mut style.textbox_border_width, 0.0..=6.0).text("pt")).changed();
                    ui.label(t("Fill"));
                    if let Some(f) = color_picker::color_button(ui, "tool_box_fill", style.textbox_fill, true, recent) {
                        style.textbox_fill = f;
                        changed = true;
                    }
                    ui.label(t("Text"));
                    if let Some(Some(c)) =
                        color_picker::color_button(ui, "tool_box_text", Some(style.textbox_text), false, recent)
                    {
                        style.textbox_text = c;
                        changed = true;
                    }
                    ui.label(t("Size"));
                    changed |= ui
                        .add(egui::DragValue::new(&mut style.textbox_font_size).range(6.0..=72.0).suffix(" pt"))
                        .changed();
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
        let mut move_to = None;
        let mut tab_rects = Vec::new();
        // Search sits at the right end of the tab row (it searches the active tab);
        // when the window is too narrow to leave room for the tabs, it gets a row of its own.
        let gap = ui.spacing().item_spacing.x;
        let beside = ui.available_width() - self.search_width - gap >= 240.0;
        let tabs_width = if beside { ui.available_width() - self.search_width - gap } else { ui.available_width() };
        ui.horizontal(|ui| {
            egui::ScrollArea::horizontal().id_salt("tabbar").max_width(tabs_width).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let resp = ui
                            .add(
                                egui::Button::selectable(i == self.active, tab.title())
                                    .sense(egui::Sense::click_and_drag()),
                            )
                            .on_hover_text(format!(
                                "{}\n{}",
                                tab.path.display(),
                                t("Drag it down onto the page or out of the window to open it in its own window")
                            ));
                        tab_rects.push(resp.rect);
                        if resp.clicked() {
                            self.active = i;
                        }
                        if resp.middle_clicked() {
                            close = Some(tab.id);
                        }
                        if resp.drag_started_by(egui::PointerButton::Primary) {
                            self.tab_drag = Some(i);
                            self.active = i;
                        }
                        if resp.secondary_clicked() {
                            // Listed once when the menu opens (asks the other windows).
                            self.tab_menu_peers = crate::ipc::peers()
                                .into_iter()
                                .filter_map(|p| p.window().map(|w| (p, w.title)))
                                .collect();
                        }
                        let (doc, peers) = (tab.id, &self.tab_menu_peers);
                        let path = tab.path.clone();
                        resp.context_menu(|ui| {
                            if ui.button(t("Open containing folder")).clicked() {
                                crate::reveal::show_in_folder(&path);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button(t("Move to new window")).clicked() {
                                move_to = Some((doc, MoveTarget::NewWindow(None)));
                                ui.close();
                            }
                            ui.add_enabled_ui(!peers.is_empty(), |ui| {
                                ui.menu_button(t("Move to window"), |ui| {
                                    for (peer, title) in peers {
                                        if ui.button(title).clicked() {
                                            move_to = Some((doc, MoveTarget::Peer(peer.clone())));
                                            ui.close();
                                        }
                                    }
                                });
                            });
                            ui.separator();
                            if ui.button(t("Close")).clicked() {
                                close = Some(doc);
                                ui.close();
                            }
                        });
                        if ui
                            .small_button(icons::sized(icons::X, 12.0))
                            .on_hover_text(kb(t("Close (Ctrl+W)")))
                            .clicked()
                        {
                            close = Some(tab.id);
                        }
                        ui.add_space(6.0);
                    }
                });
            });
            if beside {
                self.search_bar(ui);
            }
        });
        if !beside {
            ui.horizontal(|ui| self.search_bar(ui));
        }
        self.tab_drag_feedback(ui.ctx(), &tab_rects, &mut move_to);
        if let Some((doc, target)) = move_to {
            self.move_tab(doc, target);
        }
        if let Some(id) = close {
            self.request_close(vec![id], false);
        }
    }

    /// Shows the dragged tab under the pointer and handles the drop.
    fn tab_drag_feedback(
        &mut self,
        ctx: &egui::Context,
        tab_rects: &[egui::Rect],
        move_to: &mut Option<(DocId, MoveTarget)>,
    ) {
        let Some(i) = self.tab_drag else { return };
        let Some(tab) = self.tabs.get(i) else {
            self.tab_drag = None;
            return;
        };
        let (pointer, down, window, origin) = ctx.input(|inp| {
            (
                inp.pointer.latest_pos(),
                inp.pointer.primary_down(),
                inp.viewport_rect(),
                inp.viewport().inner_rect.map(|r| r.min),
            )
        });
        #[cfg(windows)]
        let (pointer, down) = match (os_pointer(), origin) {
            (Some((screen, os_down)), Some(o)) => {
                let ppp = ctx.pixels_per_point();
                (Some(egui::pos2(screen.x / ppp, screen.y / ppp) - o.to_vec2()), os_down)
            }
            _ => (pointer, down),
        };
        if down {
            // Keep following the mouse even when no window events arrive.
            ctx.request_repaint();
            if let Some(p) = pointer {
                let outside = matches!(decide_tab_drop(Some(p), window, origin, tab_rects, &[]), TabDrop::NewWindow(_));
                egui::Area::new(egui::Id::unique("tab_drag"))
                    .fixed_pos(p + egui::vec2(12.0, 8.0))
                    .order(egui::Order::Tooltip)
                    .interactable(false)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(tab.title());
                            if outside {
                                ui.weak(t("Release to move it to its own window"));
                            }
                        });
                    });
            }
            return;
        }
        self.tab_drag = None;
        let pointer_outside = pointer.is_none_or(|p| !window.contains(p));
        let peers = if pointer_outside && origin.is_some() { crate::ipc::peers() } else { Vec::new() };
        let peer_rects: Vec<Option<[f32; 4]>> = peers.iter().map(|p| p.window().and_then(|w| w.rect)).collect();
        match decide_tab_drop(pointer, window, origin, tab_rects, &peer_rects) {
            TabDrop::Stay => {}
            TabDrop::Reorder(j) if j != i => {
                let tab = self.tabs.remove(i);
                self.tabs.insert(j, tab);
                self.active = j;
            }
            TabDrop::Reorder(_) => {}
            TabDrop::Peer(p) => *move_to = Some((tab.id, MoveTarget::Peer(peers[p].clone()))),
            TabDrop::NewWindow(at) => *move_to = Some((tab.id, MoveTarget::NewWindow(at))),
        }
    }

    fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let LoadState::NeedsPassword { input, wrong } = &mut tab.state else { return };
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new(t("Password required"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(tf("“{file}” is protected by a password.", &[("file", &tab.name)]));
                if *wrong {
                    ui.colored_label(ui.visuals().error_fg_color, t("Incorrect password, try again."));
                }
                let resp = ui.add(egui::TextEdit::singleline(input).password(true));
                resp.request_focus();
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                ui.horizontal(|ui| {
                    submit |= ui.button(t("Open")).clicked();
                    cancel = ui.button(t("Cancel")).clicked();
                });
            });
        if submit {
            let req = Request::Open { doc: tab.id, path: tab.path.clone(), password: Some(std::mem::take(input)) };
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
        egui::Window::new(t("Unsaved changes"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                if dirty.len() == 1 {
                    ui.label(tf("Save changes to “{file}” before closing?", &[("file", &dirty[0].1)]));
                } else {
                    ui.label(t("These documents have unsaved changes:"));
                    for (_, name) in &dirty {
                        ui.label(format!("  • {name}"));
                    }
                }
                ui.add_space(8.0);
                if waiting {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t("Saving…"));
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    if ui.button(t("Save")).clicked() {
                        choice = Some(0);
                    }
                    if ui.button(t("Don't save")).clicked() {
                        choice = Some(1);
                    }
                    if ui.button(t("Cancel")).clicked() {
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
                        self.send(Request::Save { doc: *doc, path: t.path.clone() });
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
            Some(2) => {
                self.close_intent = None;
                self.pending_installer = None;
            }
            _ => {}
        }
    }

    fn error_dialog(&mut self, ctx: &egui::Context) {
        let Some(message) = self.error.clone() else { return };
        let mut ok = false;
        egui::Window::new(t("Error"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(message);
                ok = ui.button(t("OK")).clicked();
            });
        if ok {
            self.error = None;
        }
    }

    fn info_windows(&mut self, ctx: &egui::Context) {
        self.settings_window(ctx);
        egui::Window::new(t("Keyboard shortcuts")).open(&mut self.show_shortcuts).resizable(false).show(ctx, |ui| {
            egui::Grid::new("shortcuts").striped(true).show(ui, |ui| {
                for (k, v) in [
                    ("Ctrl+O / Ctrl+S / Ctrl+Shift+S", "Open, save, save as"),
                    ("Ctrl+P", "Print"),
                    ("Tab / Esc in a form field", "Next field / stop editing"),
                    ("Ctrl+W / Ctrl+Tab", "Close / switch tab"),
                    ("Ctrl+Q", "Quit"),
                    ("Ctrl+Z / Ctrl+Y, Ctrl+Shift+Z", "Undo / redo"),
                    ("Wheel", "Scroll (horizontally in horizontal layout)"),
                    ("Shift+Wheel", "Scroll along the other axis"),
                    ("Ctrl+Wheel, Ctrl+± , Ctrl+0", "Zoom, actual size"),
                    ("PgUp / PgDn, Home / End", "Previous / next, first / last page"),
                    ("Space / Shift+Space", "Scroll one screen"),
                    ("Ctrl+1 … Ctrl+5", "Horizontal, vertical, single, facing, grid"),
                    ("Double-click a page in the grid", "Open it in the previous layout"),
                    ("Arrows, Enter in the grid", "Move between pages, open the marked one"),
                    ("Ctrl+R / Ctrl+Shift+R", "Rotate view"),
                    ("H / S", "Hand tool / text selection tool"),
                    ("U / N / T / D / R / E", "Highlight, note, text box, draw, rectangle, ellipse"),
                    ("L / A / P / C", "Line, arrow, polygon, cloud (Enter or double-click finishes)"),
                    ("K / G", "Stamp, signature"),
                    ("M", "Measure distance (area: in the shapes menu)"),
                    ("X", "Redact: mark areas or words, then apply"),
                    ("Ctrl+C / Ctrl+V on an annotation", "Copy it, paste it under the mouse"),
                    ("Del", "Delete the selected annotation or thumbnail pages"),
                    ("Drag / handles / arrows", "Move, resize, nudge (Shift: 10 pt) annotations"),
                    ("Ctrl / Shift + click thumbnail", "Select several pages"),
                    ("Drag thumbnails", "Reorder pages"),
                    ("Ctrl+C / Ctrl+A", "Copy selection / select page text"),
                    ("Ctrl+F, Enter, F3 / Shift+F3", "Search, next / previous match"),
                    ("F9", "Toggle sidebar"),
                    ("F5, Esc", "Start / end the presentation (arrows, space, click: next page)"),
                    ("Ctrl+,", "Settings"),
                    ("Ctrl+D", "Document properties"),
                    ("Middle-drag", "Pan with any tool"),
                ] {
                    ui.strong(kb(t(k)));
                    ui.label(t(v));
                    ui.end_row();
                }
            });
        });
        egui::Window::new(t("About magic-pdf")).open(&mut self.show_about).resizable(false).show(ctx, |ui| {
            ui.heading(format!("magic-pdf {}", env!("CARGO_PKG_VERSION")));
            ui.label(t("A lightweight PDF viewer built for wide screens."));
            ui.label(t("Rendering by PDFium; UI by egui. Licensed under the AGPL-3.0."));
        });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
        });
        if dropped.is_empty() {
            return;
        }
        // Dropped on the Combine files window: added to its list.
        if let Some(d) = &mut self.combine_dialog {
            d.add(dropped);
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
        let ids: Vec<DocId> = self.tabs.iter().map(|t| t.id).collect();
        let mut pending = false;
        for d in ids {
            pending |= self.commit_edits(d);
        }
        if pending || self.tabs.iter().any(|t| t.dirty) {
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

    /// One frame of the benchmark (MAGIC_PDF_BENCH).
    fn bench_step(&mut self, ctx: &egui::Context) {
        let Some(b) = &mut self.bench else { return };
        let now = std::time::Instant::now();
        let elapsed = b.start.elapsed().as_secs_f64();
        let frame = now.duration_since(b.last).as_secs_f64() * 1000.0;
        b.last = now;
        ctx.request_repaint();
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let count = tab.page_count();
        match b.phase {
            BenchPhase::Opening => {
                if tab.info().is_some() {
                    b.opened = elapsed;
                    b.phase = BenchPhase::FirstRender;
                }
            }
            BenchPhase::FirstRender => {
                if self.cache.usage().0 > 0 {
                    b.first_render = elapsed;
                    b.phase = BenchPhase::Paging(0);
                    b.phase_start = now;
                }
            }
            BenchPhase::Paging(page) => {
                b.frames.push(frame);
                if page + 1 < count {
                    tab.goto_page(page + 1);
                    b.phase = BenchPhase::Paging(page + 1);
                } else {
                    b.paging = b.phase_start.elapsed().as_secs_f64();
                    b.phase = BenchPhase::Settling;
                    b.phase_start = now;
                }
            }
            BenchPhase::Settling => {
                b.settle_frames.push(frame);
                // Rendering the last page and the thumbnails around it.
                if b.phase_start.elapsed().as_secs_f64() > 2.0 {
                    b.memory_after_paging = rss_mb();
                    b.textures = self.cache.usage();
                    tab.search.query = std::env::var("MAGIC_PDF_BENCH_QUERY").unwrap_or_else(|_| "the".into());
                    b.phase = BenchPhase::Searching;
                    b.phase_start = now;
                    self.start_search();
                }
            }
            BenchPhase::Searching => {
                b.search_frames.push(frame);
                if !tab.search.running && tab.search.submitted.is_some() {
                    b.search = b.phase_start.elapsed().as_secs_f64();
                    b.hits = tab.search.hits.len();
                    let report = b.report(count);
                    if let Err(e) = std::fs::write(&b.out, report) {
                        log::error!("bench: {e}");
                    }
                    self.bench = None;
                    self.allow_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn selftest(&mut self, ctx: &egui::Context) {
        let Some(path) = self.selftest_to.clone() else { return };
        let time = ctx.input(|i| i.time);
        let loaded = self.tabs.iter().all(|t| !matches!(t.state, LoadState::Loading));
        // Files from Finder arrive shortly after launch: wait a moment for them.
        if !(time > 3.0 && !self.tabs.is_empty() && loaded) && time < 30.0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }
        let mut report = String::new();
        for t in &self.tabs {
            let state = match &t.state {
                LoadState::Ready(info) => format!("{} pages", info.page_sizes.len()),
                LoadState::Failed(e) => tf("failed: {e}", &[("e", &e)]),
                LoadState::Loading => "still loading".into(),
                LoadState::NeedsPassword { .. } => "password required".into(),
            };
            report += &format!("{}\t{state}\n", t.path.display());
        }
        if self.tabs.is_empty() {
            report += "no documents\n";
        }
        if let Err(e) = std::fs::write(&path, report) {
            log::error!("selftest: {e}");
        }
        self.selftest_to = None;
        self.allow_quit = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn recent_action(&mut self, action: RecentAction) {
        match action {
            RecentAction::Open(path) => {
                if path.exists() {
                    self.open(&path);
                } else {
                    self.error = Some(tf("{file} no longer exists.", &[("file", &path.display())]));
                    self.settings.forget_recent(&path);
                }
            }
            RecentAction::TogglePin(path) => self.settings.toggle_pin(&path),
            RecentAction::Forget(path) => self.settings.forget_recent(&path),
        }
        self.settings.save();
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let s = &mut self.settings;
        let (mut dark, mut night) = (s.dark_ui, s.night_mode);
        let mut changed = false;
        let mut check_now = false;
        egui::Window::new(t("Settings")).open(&mut open).resizable(false).collapsible(false).show(ctx, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                ui.label(t("Language"));
                let current = crate::i18n::CHOICES.iter().find(|c| c.0 == s.language).map_or("System", |c| c.1);
                egui::ComboBox::from_id_salt("language")
                    .selected_text(if current == "System" { t("System") } else { current })
                    .show_ui(ui, |ui| {
                        for (code, name) in crate::i18n::CHOICES {
                            let name = if code == "auto" { t(name) } else { name };
                            if ui.selectable_label(s.language == code, name).clicked() && s.language != code {
                                s.language = code.to_string();
                                crate::i18n::set_language(code);
                                changed = true;
                            }
                        }
                    });
                ui.end_row();

                ui.label(t("Author name"));
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut s.author).desired_width(200.0))
                    .on_hover_text(t("Stored with new annotations and comments"))
                    .changed();
                ui.end_row();

                ui.label(t("Layout for new files"));
                egui::ComboBox::from_id_salt("default_layout").selected_text(s.default_mode.label()).show_ui(
                    ui,
                    |ui| {
                        for mode in LayoutMode::ALL {
                            changed |= ui.selectable_value(&mut s.default_mode, mode, mode.label()).changed();
                        }
                    },
                );
                ui.end_row();

                ui.label(t("Appearance"));
                ui.vertical(|ui| {
                    ui.checkbox(&mut dark, t("Dark interface"));
                    ui.checkbox(&mut night, t("Night mode (invert pages)"));
                    changed |= ui.checkbox(&mut s.show_sidebar, kb(t("Show the sidebar (F9)"))).changed();
                });
                ui.end_row();

                ui.label(t("Thumbnail size"));
                changed |= ui
                    .add(egui::Slider::new(&mut s.thumb_width, 60.0..=600.0).logarithmic(true).suffix(" pt"))
                    .changed();
                ui.end_row();

                ui.label(t("Starting"));
                changed |= ui
                    .checkbox(&mut s.restore_session, t("Reopen the files from last time"))
                    .on_hover_text(t(
                        "When magic-pdf starts without a file, it opens the tabs of the last window you closed",
                    ))
                    .changed();
                ui.end_row();

                ui.label(t("Updates"));
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut s.check_updates, t("Check daily")).changed();
                    check_now = ui.button(t("Check now")).clicked();
                });
                ui.end_row();

                ui.label(t("Recent files"));
                ui.horizontal(|ui| {
                    let n = s.recent.len() + s.pinned.len();
                    ui.weak(tf("{n} in the list ({pinned} pinned)", &[("n", &n), ("pinned", &s.pinned.len())]));
                    if ui
                        .add_enabled(!s.recent.is_empty(), egui::Button::new(t("Clear")))
                        .on_hover_text(t("Pinned files stay"))
                        .clicked()
                    {
                        s.recent.clear();
                        changed = true;
                    }
                });
                ui.end_row();
            });
        });
        self.show_settings = open;
        if dark != self.settings.dark_ui {
            self.settings.dark_ui = dark;
            ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
            changed = true;
        }
        if night != self.settings.night_mode {
            self.set_night_mode(night);
        }
        if changed {
            self.settings.save();
        }
        if check_now {
            self.check_for_updates(true);
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        // The central panel has no frame (pages draw their own background).
        ui.painter().rect_filled(ui.max_rect(), 0.0, ui.visuals().panel_fill);
        let logo = self
            .logo
            .get_or_insert_with(|| {
                let image = image::load_from_memory(include_bytes!("../assets/icons/magic-pdf-256.png"))
                    .map(|i| i.to_rgba8())
                    .unwrap_or_default();
                let size = [image.width() as usize, image.height() as usize];
                ui.ctx().load_texture(
                    "logo",
                    egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
                    egui::TextureOptions {
                        mipmap_mode: Some(egui::TextureFilter::Linear),
                        ..egui::TextureOptions::LINEAR
                    },
                )
            })
            .clone();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.18);
            ui.add(egui::Image::new(&logo).fit_to_exact_size(egui::vec2(72.0, 72.0)));
            ui.add_space(6.0);
            ui.heading(t("magic-pdf"));
            ui.add_space(14.0);
            if let Err(e) = &self.engine {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            let open = egui::Button::new(
                egui::RichText::new(t("Open a PDF…")).size(15.0).color(egui::Color32::WHITE).strong(),
            )
            .fill(crate::theme::ACCENT)
            .min_size(egui::vec2(160.0, 34.0));
            if ui.add(open).clicked() {
                self.open_dialog();
            }
            ui.add_space(2.0);
            ui.weak(t("or drop files here"));
            if !self.settings.recent.is_empty() || !self.settings.pinned.is_empty() {
                ui.add_space(28.0);
                let width = 560.0_f32.min(ui.available_width() - 32.0);
                ui.allocate_ui_with_layout(egui::vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(width);
                    ui.strong(t("Recent files"));
                    ui.add_space(4.0);
                    if let Some(a) = recent_list(ui, &self.settings, false) {
                        self.recent_action(a);
                    }
                });
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.cache.begin_frame();
        self.process_responses();
        self.handle_ipc(&ctx);
        self.handle_close_request(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        let presenting = self.presenting.is_some();
        if presenting {
            self.presentation_hint(&ctx);
        } else {
            self.panels(ui, &ctx);
        }
        let mut page_actions = Vec::new();
        if !presenting && self.settings.show_sidebar && self.tabs.get(self.active).is_some_and(|t| t.info().is_some()) {
            let thumb_width = &mut self.settings.thumb_width;
            let author = &self.settings.author;
            let all = self.search_all && self.sidebar_tab == SidebarTab::Search && self.tabs.len() > 1;
            let panel = egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(self.settings.sidebar_width)
                .size_range(sidebar::MIN_WIDTH..=900.0)
                .show(ui, |ui| {
                    if all {
                        sidebar::tab_row(ui, &self.tabs[self.active], &mut self.sidebar_tab);
                        ui.separator();
                        sidebar::search_all_results(ui, &mut self.tabs, &mut self.active);
                    } else if self.sidebar_tab == SidebarTab::Attachments {
                        let tab = &self.tabs[self.active];
                        sidebar::tab_row(ui, tab, &mut self.sidebar_tab);
                        ui.separator();
                        let inv = self.einvoices.get(&tab.id);
                        if inv.is_some() && ui.button(t("Show invoice data")).clicked() {
                            self.show_einvoice = Some(tab.id);
                        }
                        if let Some((index, action)) = crate::view::doc_info::attachments_list(ui, tab, inv) {
                            let doc = tab.id;
                            self.attachment_actions.insert((doc, index), action);
                            self.tabs[self.active].outbox.push(Request::GetAttachment { doc, index });
                        }
                    } else {
                        let tab = &mut self.tabs[self.active];
                        sidebar::show(
                            ui,
                            tab,
                            &mut self.sidebar_tab,
                            &mut self.cache,
                            &mut page_actions,
                            thumb_width,
                            author,
                        )
                    }
                });
            // Remembered for the next start (dragging the edge resizes it).
            self.settings.sidebar_width = panel.response.rect.width();
        }

        let recent_before = self.settings.recent_colors.clone();
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.tabs.is_empty() {
                self.welcome(ui);
                return;
            }
            if matches!(self.tool, Tool::Stamp | Tool::Signature) {
                self.stamps.prepare(&self.settings);
            }
            let full = ui.available_rect_before_wrap();
            let panes: Vec<(usize, egui::Rect)> = match self.compare_panes() {
                Some((left, right)) => {
                    let mid = full.center().x;
                    let l = egui::Rect::from_min_max(full.min, egui::pos2(mid - 2.0, full.max.y));
                    let r = egui::Rect::from_min_max(egui::pos2(mid + 2.0, full.min.y), full.max);
                    ui.painter().rect_filled(
                        egui::Rect::from_x_y_ranges(mid - 2.0..=mid + 2.0, full.y_range()),
                        0.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    );
                    vec![(left, l), (right, r)]
                }
                None => vec![(self.active, full)],
            };
            for (i, rect) in panes {
                let tab = &mut self.tabs[i];
                let signature = self.settings.signatures.get(self.settings.annot_style.signature);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("pane", i)));
                document_view::show(
                    &mut child,
                    tab,
                    ViewContext {
                        cache: &mut self.cache,
                        recent_colors: &mut self.settings.recent_colors,
                        tool: self.tool,
                        night: self.settings.night_mode,
                        style: &self.settings.annot_style,
                        author: &self.settings.author,
                        stamp: self.stamps.stamp(&self.settings.annot_style),
                        signature,
                        signature_image: self
                            .stamps
                            .signature_image(&self.settings.annot_style, &self.settings.signatures),
                        presenting,
                    },
                );
                if let Some(e) = tab.error_message.take() {
                    self.error = Some(e);
                }
            }
            self.sync_comparison();
        });
        if crate::view::stamps::signatures_window(&ctx, &mut self.stamps, &mut self.settings) {
            self.settings.save();
        }
        self.sign_ui(&ctx);
        self.combine_ui(&ctx);

        if self.settings.recent_colors != recent_before {
            self.settings.save();
        }
        for a in page_actions {
            self.page_action(a);
        }
        self.split_dialog(&ctx);
        self.page_tool_dialogs(&ctx);
        self.doc_info_windows(&ctx);
        if self.signatures_on_load
            && let Some(t) = self.tabs.get(self.active).filter(|t| t.info().is_some())
        {
            self.signatures_on_load = false;
            self.show_signatures = Some(t.id);
        }
        if let Some(doc) = self.show_signatures {
            let sigs = self.tabs.iter().find(|t| t.id == doc).and_then(|t| t.info()).map(|i| i.signatures.clone());
            if !sigs.is_some_and(|s| crate::view::signatures::window(&ctx, &s)) {
                self.show_signatures = None;
            }
        }
        for r in self.ocr.show(&ctx, self.tabs.get(self.active)) {
            self.send(r);
        }
        self.redaction_dialog(&ctx);
        self.password_dialog(&ctx);
        self.unsaved_dialog(&ctx);
        self.print_dialog_ui(&ctx);
        self.update_ui(&ctx);
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
        self.selftest(&ctx);
        self.bench_step(&ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        #[cfg(target_os = "linux")]
        if let Some(factor) = self.pinch.as_ref().and_then(|p| p.take_zoom()) {
            raw_input.events.push(egui::Event::Zoom(factor));
        }
        self.route_form_keys(ctx, &mut raw_input.events);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Before the window (and its Wayland connection) goes away.
        #[cfg(target_os = "linux")]
        if let Some(mut pinch) = self.pinch.take() {
            pinch.stop();
        }
        for tab in &self.tabs {
            if tab.info().is_some() {
                self.settings.set_file_state(&tab.path, tab.file_state());
            }
        }
        self.settings.save();
        if let Some(installer) = self.pending_installer.take()
            && let Err(e) = crate::update::run_installer(&installer)
        {
            log::error!("{e}");
        }
    }
}

/// Shows a release's "What's new" text: bold headings, bullets and **bold** words.
/// Lines wrap at `width` (the window sizes itself to its content, so the available
/// width can't be used).
fn release_notes(ui: &mut Ui, blocks: &[crate::update::NoteBlock], width: f32) {
    use crate::update::NoteBlock;
    let rich = |ui: &Ui, text: &str, width: f32| {
        let size = ui.style().text_styles[&egui::TextStyle::Body].size;
        let mut job = egui::text::LayoutJob::default();
        for (part, bold) in crate::update::bold_parts(text) {
            let format = egui::TextFormat {
                font_id: egui::FontId::new(size, crate::fonts::family(bold, false)),
                color: if bold { ui.visuals().strong_text_color() } else { ui.visuals().text_color() },
                ..Default::default()
            };
            job.append(&part, 0.0, format);
        }
        job.wrap.max_width = width;
        job
    };
    // Labels wrap at the width they may use, so limit it.
    ui.set_max_width(width);
    for (i, block) in blocks.iter().enumerate() {
        match block {
            NoteBlock::Heading(text) => {
                if i > 0 {
                    ui.add_space(6.0);
                }
                ui.add(egui::Label::new(rich(ui, &format!("**{text}**"), width)).wrap());
            }
            NoteBlock::Bullet(text) => {
                ui.horizontal_top(|ui| {
                    ui.set_max_width(width);
                    let bullet = ui.label("•").rect.width() + ui.spacing().item_spacing.x;
                    ui.add(egui::Label::new(rich(ui, text, width - bullet)).wrap());
                });
            }
            NoteBlock::Paragraph(text) => {
                ui.add(egui::Label::new(rich(ui, text, width)).wrap());
            }
        }
    }
}

/// On Windows, `canonicalize` returns `\\?\C:\…` and `\\?\UNC\…` paths; show and
/// store the familiar `C:\…` and `\\server\share\…` forms instead (Explorer and other
/// programs do not all accept the verbatim ones).
fn simplify_path(path: PathBuf) -> PathBuf {
    match path.to_str().filter(|_| cfg!(windows)).and_then(plain_windows_path) {
        Some(plain) => PathBuf::from(plain),
        None => path,
    }
}

/// `\\?\C:\x\a.pdf` as `C:\x\a.pdf`, `\\?\UNC\server\share\a.pdf` as
/// `\\server\share\a.pdf`. None for other paths, and for paths too long for the plain
/// form (260 characters), which Windows opens only in the verbatim form.
fn plain_windows_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let plain = match rest.strip_prefix(r"UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => rest.to_string(),
    };
    (plain.chars().count() < 260).then_some(plain)
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
                reqs.push(Request::FormKey { doc, key: FormKey::Char(c as u32), modifiers: 0 });
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
        egui::Event::Key { key, pressed: true, modifiers, .. } => {
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
            let mods =
                (if modifiers.shift { form_mods::SHIFT } else { 0 }) | (if modifiers.alt { form_mods::ALT } else { 0 });
            reqs.push(Request::FormKey { doc, key: FormKey::Down(vk), modifiers: mods });
            // Keys that produce characters PDFium expects as WM_CHAR-style input.
            let ch = match key {
                Key::Backspace => Some(8),
                Key::Enter => Some(13),
                _ => None,
            };
            if let Some(c) = ch {
                reqs.push(Request::FormKey { doc, key: FormKey::Char(c), modifiers: mods });
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

/// A comparison of two open documents.
struct Comparison {
    a: DocId,
    b: DocId,
    /// Pages with differences (None: still comparing).
    diffs: Option<Vec<crate::engine::compare::PageDiff>>,
    /// The change shown last (index into `diffs`).
    current: Option<usize>,
    /// Pages and zoom of both sides last frame, to follow scrolling and zooming.
    last: Option<((usize, f32), (usize, f32))>,
}

/// What was clicked in a recent-files list.
enum RecentAction {
    Open(PathBuf),
    TogglePin(PathBuf),
    Forget(PathBuf),
}

/// Pinned files first, then recent ones: file name with its folder; a pin button
/// and a context menu to pin or remove an entry. `menu` is the compact form for
/// the File menu.
fn recent_list(ui: &mut Ui, settings: &crate::settings::Settings, menu: bool) -> Option<RecentAction> {
    let mut action = None;
    let entries = settings.pinned.iter().map(|p| (p, true)).chain(settings.recent.iter().map(|p| (p, false)));
    let mut was_pinned = !settings.pinned.is_empty();
    for (path, pinned) in entries {
        if was_pinned && !pinned {
            ui.separator();
        }
        was_pinned = pinned;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let folder = path.parent().map(short_folder).unwrap_or_default();
        ui.horizontal(|ui| {
            let pin = icons::text(icons::PUSH_PIN).size(14.0);
            let pin = if pinned { pin.color(ui.visuals().selection.stroke.color) } else { pin.weak() };
            if ui
                .add(egui::Button::new(pin).frame(false))
                .on_hover_text(if pinned { t("Unpin") } else { t("Pin to the top of the list") })
                .clicked()
            {
                action = Some(RecentAction::TogglePin(path.clone()));
            }
            let mut text = egui::text::LayoutJob::default();
            text.append(
                &name,
                0.0,
                egui::TextFormat::simple(egui::FontId::proportional(14.0), ui.visuals().strong_text_color()),
            );
            text.append(
                &format!("   {folder}"),
                0.0,
                egui::TextFormat::simple(egui::FontId::proportional(12.0), ui.visuals().weak_text_color()),
            );
            // One line within the list's width: a long folder ends in "…" (the full
            // path shows on hover).
            text.wrap = egui::text::TextWrapping {
                max_width: if menu { 520.0 } else { ui.available_width() - 8.0 },
                max_rows: 1,
                break_anywhere: true,
                overflow_character: Some('…'),
            };
            let button = egui::Button::new(text).frame(menu).truncate();
            let button = if menu { button } else { button.frame(false) };
            let response = ui.add(button).on_hover_text(path.display().to_string());
            if response.clicked() {
                action = Some(RecentAction::Open(path.clone()));
            }
            response.context_menu(|ui| {
                if ui.button(if pinned { t("Unpin") } else { t("Pin") }).clicked() {
                    action = Some(RecentAction::TogglePin(path.clone()));
                    ui.close();
                }
                if ui.button(t("Remove from list")).clicked() {
                    action = Some(RecentAction::Forget(path.clone()));
                    ui.close();
                }
            });
        });
    }
    action
}

/// The last two folders of a path, e.g. `…\datasheets\out` (whole if short).
fn short_folder(dir: &Path) -> String {
    let parts: Vec<_> = dir.components().collect();
    if parts.len() <= 3 {
        return dir.display().to_string();
    }
    let tail: PathBuf = parts[parts.len() - 2..].iter().collect();
    format!("…{}{}", std::path::MAIN_SEPARATOR, tail.display())
}

/// The notice after pages were written to other files.
fn exported_text(e: &crate::engine::Exported) -> String {
    use crate::engine::Exported;
    let mb = |n: usize| format!("{:.1} MB", n as f64 / 1_048_576.0);
    match e {
        Exported::Pictures { count, dir } => {
            tf("Saved {n} pictures to {dir}", &[("n", count), ("dir", &dir.display())])
        }
        Exported::Pages { path } => tf("Saved pages to {file}", &[("file", &path.display())]),
        Exported::Split { files, dir } => tf("Split into {n} files in {dir}", &[("n", files), ("dir", &dir.display())]),
        Exported::Optimized { name, before, after, images } => {
            let saved = tf(
                "Saved {file}: {before} → {after}",
                &[("file", name), ("before", &mb(*before)), ("after", &mb(*after))],
            );
            match images {
                0 => saved,
                1 => format!("{saved} ({})", t("1 picture scaled down")),
                n => format!("{saved} ({})", tf("{n} pictures scaled down", &[("n", n)])),
            }
        }
    }
}

/// A spinner with a short text in the middle of the window, for work in the background.
fn busy(ctx: &egui::Context, text: &str) {
    egui::Area::new(egui::Id::unique("busy")).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0)).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(text);
                });
            });
        },
    );
}

/// The benchmark run with MAGIC_PDF_BENCH (debug aid, not shown to users).
struct Bench {
    out: PathBuf,
    phase: BenchPhase,
    start: std::time::Instant,
    last: std::time::Instant,
    phase_start: std::time::Instant,
    opened: f64,
    first_render: f64,
    paging: f64,
    search: f64,
    hits: usize,
    /// Frame times (ms) while paging, while the last pages render, while searching.
    frames: Vec<f64>,
    settle_frames: Vec<f64>,
    search_frames: Vec<f64>,
    memory_after_paging: f64,
    textures: (usize, usize),
}

#[derive(Clone, Copy, PartialEq)]
enum BenchPhase {
    Opening,
    FirstRender,
    /// Showing this page; the next frame goes to the next one.
    Paging(usize),
    Settling,
    Searching,
}

impl Bench {
    fn new(out: PathBuf) -> Bench {
        let now = std::time::Instant::now();
        Bench {
            out,
            phase: BenchPhase::Opening,
            start: now,
            last: now,
            phase_start: now,
            opened: 0.0,
            first_render: 0.0,
            paging: 0.0,
            search: 0.0,
            hits: 0,
            frames: Vec::new(),
            settle_frames: Vec::new(),
            search_frames: Vec::new(),
            memory_after_paging: 0.0,
            textures: (0, 0),
        }
    }

    fn report(&self, pages: usize) -> String {
        let stats = |v: &[f64]| {
            let mut s = v.to_vec();
            s.sort_by(f64::total_cmp);
            let pct = |p: f64| s.get(((s.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
            format!(
                "{} frames, median {:.1} ms, 95% {:.1} ms, max {:.1} ms",
                s.len(),
                pct(0.5),
                pct(0.95),
                s.last().copied().unwrap_or(0.0)
            )
        };
        format!(
            "pages\t{pages}\nopened\t{:.2} s\nfirst page drawn\t{:.2} s\npaging through all pages\t{:.2} s ({})\n\
             after paging\t{}\nsearch\t{:.2} s, {} hits ({})\ntextures after paging\t{} ({:.0} MB)\n\
             memory after paging\t{:.0} MB\npeak memory\t{:.0} MB\n",
            self.opened,
            self.first_render,
            self.paging,
            stats(&self.frames),
            stats(&self.settle_frames),
            self.search,
            self.hits,
            stats(&self.search_frames),
            self.textures.0,
            self.textures.1 as f64 / 1_048_576.0,
            self.memory_after_paging,
            peak_rss_mb(),
        )
    }
}

/// Resident memory of this process in MB (Linux only; 0 elsewhere).
fn rss_mb() -> f64 {
    proc_status("VmRSS:")
}

fn peak_rss_mb() -> f64 {
    proc_status("VmHWM:")
}

fn proc_status(key: &str) -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .map_or(0.0, |kb: f64| kb / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    fn chars(reqs: &[Request]) -> String {
        reqs.iter()
            .filter_map(|r| match r {
                Request::FormKey { key: FormKey::Char(c), .. } => char::from_u32(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dropping_a_dragged_tab() {
        use egui::{Rect, pos2, vec2};
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0));
        let tabs = [
            Rect::from_min_size(pos2(10.0, 50.0), vec2(100.0, 20.0)),
            Rect::from_min_size(pos2(120.0, 50.0), vec2(100.0, 20.0)),
            Rect::from_min_size(pos2(230.0, 50.0), vec2(100.0, 20.0)),
        ];
        let origin = Some(pos2(500.0, 300.0));
        let other = [Some([1600.0, 300.0, 800.0, 600.0])];
        // In the tab bar: reorder.
        assert_eq!(decide_tab_drop(Some(pos2(130.0, 60.0)), window, origin, &tabs, &other), TabDrop::Reorder(1));
        assert_eq!(decide_tab_drop(Some(pos2(900.0, 60.0)), window, origin, &tabs, &other), TabDrop::Reorder(2));
        // Just below the tab bar: nothing happens.
        assert_eq!(decide_tab_drop(Some(pos2(500.0, 120.0)), window, origin, &tabs, &other), TabDrop::Stay);
        // Pulled well down onto the page (e.g. in a maximized window): new window there.
        assert_eq!(
            decide_tab_drop(Some(pos2(500.0, 500.0)), window, origin, &tabs, &other),
            TabDrop::NewWindow(Some([1000.0, 800.0]))
        );
        assert_eq!(decide_tab_drop(Some(pos2(500.0, 500.0)), window, None, &tabs, &other), TabDrop::NewWindow(None));
        // Over the other window (screen x = 500 + 1200 = 1700).
        assert_eq!(decide_tab_drop(Some(pos2(1200.0, 100.0)), window, origin, &tabs, &other), TabDrop::Peer(0));
        // Elsewhere outside: new window at the drop point.
        assert_eq!(
            decide_tab_drop(Some(pos2(-200.0, 100.0)), window, origin, &tabs, &other),
            TabDrop::NewWindow(Some([300.0, 400.0]))
        );
        // Window position unknown (Wayland): always a new window.
        assert_eq!(decide_tab_drop(Some(pos2(1200.0, 100.0)), window, None, &tabs, &other), TabDrop::NewWindow(None));
    }

    #[test]
    fn simplify_path_only_touches_windows_verbatim_paths() {
        let p = PathBuf::from("/home/u/a.pdf");
        assert_eq!(simplify_path(p.clone()), p);
        assert_eq!(plain_windows_path(r"\\?\C:\x\a.pdf").as_deref(), Some(r"C:\x\a.pdf"));
        assert_eq!(plain_windows_path(r"\\?\UNC\server\share\a.pdf").as_deref(), Some(r"\\server\share\a.pdf"));
        assert_eq!(plain_windows_path(r"C:\x\a.pdf"), None);
        let long = format!(r"\\?\C:\{}\a.pdf", "d".repeat(300));
        assert_eq!(plain_windows_path(&long), None, "too long for the plain form");
        if cfg!(windows) {
            assert_eq!(simplify_path(PathBuf::from(r"\\?\C:\x\a.pdf")), PathBuf::from(r"C:\x\a.pdf"));
            let unc = PathBuf::from(r"\\?\UNC\server\share\a.pdf");
            assert_eq!(simplify_path(unc), PathBuf::from(r"\\server\share\a.pdf"));
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
        let input = |events| egui::RawInput { events, ..Default::default() };
        let frame = |ctx: &egui::Context, events| {
            let mut output = ctx.run_ui(input(events), |ui| {
                let _ = ui.button(t("File"));
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
