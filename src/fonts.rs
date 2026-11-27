//! Noto Sans (SIL Open Font License, assets/fonts/noto-sans/OFL.txt): the text box
//! font. The same bytes draw text boxes into PDFs (engine) and show them while they
//! are edited (egui), so editing looks like the result.

use eframe::egui;

pub const REGULAR: &[u8] = include_bytes!("../assets/fonts/noto-sans/NotoSans-Regular.ttf");
pub const BOLD: &[u8] = include_bytes!("../assets/fonts/noto-sans/NotoSans-Bold.ttf");
pub const ITALIC: &[u8] = include_bytes!("../assets/fonts/noto-sans/NotoSans-Italic.ttf");
pub const BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/noto-sans/NotoSans-BoldItalic.ttf");

/// The egui font family for a text box face.
pub fn family(bold: bool, italic: bool) -> egui::FontFamily {
    egui::FontFamily::Name(
        match (bold, italic) {
            (false, false) => "noto-sans",
            (true, false) => "noto-sans-bold",
            (false, true) => "noto-sans-italic",
            (true, true) => "noto-sans-bold-italic",
        }
        .into(),
    )
}

/// Makes the four faces available to egui.
pub fn install(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    for (bold, italic, bytes) in
        [(false, false, REGULAR), (true, false, BOLD), (false, true, ITALIC), (true, true, BOLD_ITALIC)]
    {
        let family = family(bold, italic);
        let egui::FontFamily::Name(name) = &family else { continue };
        ctx.add_font(FontInsert::new(
            name,
            egui::FontData::from_static(bytes),
            vec![InsertFontFamily { family: family.clone(), priority: FontPriority::Highest }],
        ));
    }
}

/// Makes the interface sharper on ordinary (1x) screens: Noto Sans Regular as the
/// interface font instead of egui's thin Ubuntu Light, hinted onto the pixel grid in
/// both directions, and glyphs placed on whole pixels (sub-pixel placement blurs them).
pub fn install_ui(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, HintingTarget, InsertFontFamily, SmoothHinting};
    let mut data = egui::FontData::from_static(REGULAR);
    data.tweak.hinting_target = HintingTarget::Smooth(SmoothHinting {
        light: false,
        symmetric_rendering: false,
        preserve_linear_metrics: false,
    });
    ctx.add_font(FontInsert::new(
        "noto-sans-ui",
        data,
        vec![InsertFontFamily { family: egui::FontFamily::Proportional, priority: FontPriority::Highest }],
    ));
    ctx.all_styles_mut(|style| style.visuals.text_options.subpixel_binning = false);
}
