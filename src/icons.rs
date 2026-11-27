//! Toolbar icons from the Phosphor icon font (regular weight, v2.1.2, MIT licence:
//! assets/fonts/phosphor/LICENSE). Only the glyphs used here are named; their code
//! points come from Phosphor's official stylesheet.

use eframe::egui;

pub const FOLDER_OPEN: &str = "\u{e256}";
pub const FLOPPY_DISK: &str = "\u{e248}";
pub const UNDO: &str = "\u{e08a}"; // arrow-u-up-left
pub const REDO: &str = "\u{e08c}"; // arrow-u-up-right
pub const HAND: &str = "\u{e298}";
pub const CURSOR_TEXT: &str = "\u{e7d8}";
pub const HIGHLIGHTER: &str = "\u{ec76}";
pub const TEXT_UNDERLINE: &str = "\u{e5c4}";
pub const TEXT_STRIKETHROUGH: &str = "\u{e5c2}";
pub const NOTE_PENCIL: &str = "\u{e34c}";
pub const SCRIBBLE: &str = "\u{e806}";
pub const RECTANGLE: &str = "\u{e3f0}";
pub const TEXTBOX: &str = "\u{eb0a}";
pub const CIRCLE: &str = "\u{e18a}";
pub const CARET_LEFT: &str = "\u{e138}";
pub const CARET_RIGHT: &str = "\u{e13a}";
pub const CARET_UP: &str = "\u{e13c}";
pub const CARET_DOWN: &str = "\u{e136}";
pub const ZOOM_OUT: &str = "\u{e30e}"; // magnifying-glass-minus
pub const ZOOM_IN: &str = "\u{e310}"; // magnifying-glass-plus
pub const MAGNIFYING_GLASS: &str = "\u{e30c}";
pub const COLUMNS: &str = "\u{e546}";
pub const ROWS: &str = "\u{e5a2}";
pub const FILE: &str = "\u{e230}";
pub const BOOK_OPEN: &str = "\u{e0e6}";
pub const SQUARES_FOUR: &str = "\u{e464}";
pub const X: &str = "\u{e4f6}";
pub const TEXT_AA: &str = "\u{e6ee}";
pub const COPY: &str = "\u{e1ca}";
pub const TRASH: &str = "\u{e4a6}";
pub const PRINTER: &str = "\u{e3dc}";
pub const TEXT_B: &str = "\u{e5be}";
pub const TEXT_ITALIC: &str = "\u{e5c0}";
pub const ALIGN_LEFT: &str = "\u{e484}";
pub const ALIGN_CENTER: &str = "\u{e480}";
pub const ALIGN_RIGHT: &str = "\u{e486}";
pub const SIDEBAR: &str = "\u{ec24}";
pub const PUSH_PIN: &str = "\u{e3e2}";
pub const LINE: &str = "\u{e32a}";
pub const ARROW: &str = "\u{e092}";
pub const POLYGON: &str = "\u{ec7e}";
pub const CLOUD: &str = "\u{e1aa}";
pub const STAMP: &str = "\u{ea48}";
pub const SIGNATURE: &str = "\u{e3ac}"; // pen-nib
pub const FUNNEL: &str = "\u{e266}";
pub const RULER: &str = "\u{e6b8}";
pub const AREA: &str = "\u{e69a}"; // selection
pub const EYE_SLASH: &str = "\u{e224}";
pub const FILES: &str = "\u{e710}";
pub const LIST_BULLETS: &str = "\u{e2f2}";
pub const CHAT_TEXT: &str = "\u{e17a}";
pub const PAPERCLIP: &str = "\u{e39a}";
pub const CERTIFICATE: &str = "\u{e766}";

/// Size of toolbar icons in points.
pub const SIZE: f32 = 17.0;

/// Font family holding only the icon font. Using a family of its own (rather than a
/// fallback) guarantees these private-use code points always render as Phosphor
/// glyphs, whatever other fonts are installed.
fn family() -> egui::FontFamily {
    egui::FontFamily::Name("phosphor".into())
}

pub fn install(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    ctx.add_font(FontInsert::new(
        "phosphor",
        egui::FontData::from_static(include_bytes!("../assets/fonts/phosphor/Phosphor-Regular.ttf")),
        vec![InsertFontFamily { family: family(), priority: FontPriority::Highest }],
    ));
}

/// An icon at toolbar size.
pub fn text(icon: &str) -> egui::RichText {
    egui::RichText::new(icon).family(family()).size(SIZE)
}

/// An icon at a custom size.
pub fn sized(icon: &str, size: f32) -> egui::RichText {
    egui::RichText::new(icon).family(family()).size(size)
}
