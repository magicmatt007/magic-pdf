//! The look of the magic apps: warm neutral surfaces instead of grey, one accent per
//! app taken from its icon (orange for magic-pdf), and the yellow sparkle the icons
//! share. The same colours are used in magic-image, so the apps look like a family.
//!
//! Rules: the accent marks what is on or selected (a light tint with dark accent text,
//! or a solid fill for the one main button of a dialog); grey stays for what is off.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Theme};

/// The magic-pdf accent (its icon's orange), for solid fills with white text.
pub const ACCENT: Color32 = Color32::from_rgb(0xEA, 0x58, 0x0C);
/// A deeper shade of the accent for small white text on it (page-number badge).
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0xC2, 0x41, 0x0C);
/// The sparkle yellow every magic app's icon carries.
#[allow(dead_code)]
pub const SPARKLE: Color32 = Color32::from_rgb(0xFA, 0xCC, 0x15);

struct Palette {
    /// Behind everything: around dialogs, empty areas.
    bg: Color32,
    /// Menu bar, toolbar, sidebar, status bar.
    chrome: Color32,
    /// Windows and popups.
    surface: Color32,
    /// Text fields: set off from windows and from the toolbar, so an empty field
    /// (without a hint text) can still be seen.
    text_field: Color32,
    /// Buttons at rest, and their hovered and pressed shades.
    field: Color32,
    field_hover: Color32,
    field_active: Color32,
    border: Color32,
    text: Color32,
    /// Selected items: a tint of the accent with accent-coloured text.
    tint: Color32,
    on_tint: Color32,
    link: Color32,
    /// Behind the pages.
    canvas: Color32,
}

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xFA, 0xF8, 0xF5),
    chrome: Color32::from_rgb(0xF3, 0xEF, 0xEA),
    surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    text_field: Color32::from_rgb(0xF6, 0xF2, 0xEC),
    field: Color32::from_rgb(0xE9, 0xE4, 0xDD),
    field_hover: Color32::from_rgb(0xE1, 0xDA, 0xD0),
    field_active: Color32::from_rgb(0xD6, 0xCC, 0xBF),
    border: Color32::from_rgb(0xE1, 0xDA, 0xD0),
    text: Color32::from_rgb(0x1F, 0x1B, 0x16),
    tint: Color32::from_rgb(0xFE, 0xD7, 0xAA),
    on_tint: Color32::from_rgb(0x9A, 0x34, 0x12),
    link: Color32::from_rgb(0xC2, 0x41, 0x0C),
    canvas: Color32::from_rgb(0xE4, 0xDE, 0xD6),
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1A, 0x17, 0x14),
    chrome: Color32::from_rgb(0x21, 0x1D, 0x19),
    surface: Color32::from_rgb(0x2B, 0x26, 0x20),
    text_field: Color32::from_rgb(0x17, 0x14, 0x11),
    // Lighter than `surface`, so buttons and empty checkboxes show inside windows.
    field: Color32::from_rgb(0x3A, 0x33, 0x2B),
    field_hover: Color32::from_rgb(0x45, 0x3D, 0x34),
    field_active: Color32::from_rgb(0x52, 0x48, 0x3D),
    border: Color32::from_rgb(0x3A, 0x34, 0x2D),
    text: Color32::from_rgb(0xF2, 0xEE, 0xE8),
    tint: Color32::from_rgb(0x4A, 0x2A, 0x14),
    on_tint: Color32::from_rgb(0xFD, 0xBA, 0x74),
    link: Color32::from_rgb(0xFB, 0x92, 0x3C),
    canvas: Color32::from_rgb(0x12, 0x10, 0x0D),
};

fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// Applies the light and dark looks; `ctx.set_theme` then switches between them.
pub fn install(ctx: &egui::Context) {
    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            apply(&mut style.visuals, palette(theme == Theme::Dark));
            // Room between a window's edge and its contents.
            style.spacing.window_margin = egui::Margin::same(12);
        });
    }
}

fn apply(v: &mut egui::Visuals, p: &Palette) {
    let small = CornerRadius::same(8);
    v.panel_fill = p.chrome;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.faint_bg_color = p.bg;
    v.extreme_bg_color = p.surface;
    v.text_edit_bg_color = Some(p.text_field);
    v.code_bg_color = p.field;
    v.hyperlink_color = p.link;
    v.selection.bg_fill = p.tint;
    v.selection.stroke = Stroke::new(1.0, p.on_tint);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.chrome;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = small;
    for (state, fill) in [
        (&mut w.inactive, p.field),
        (&mut w.hovered, p.field_hover),
        (&mut w.active, p.field_active),
        (&mut w.open, p.field),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.corner_radius = small;
        state.fg_stroke = Stroke::new(state.fg_stroke.width, p.text);
    }
    w.inactive.bg_stroke = Stroke::NONE;
    w.hovered.bg_stroke = Stroke::new(1.0, p.field_active);
    w.active.bg_stroke = Stroke::new(1.0, p.field_active);
}

/// The colour behind the pages for the current look.
pub fn canvas(dark: bool) -> Color32 {
    palette(dark).canvas
}

/// The one main action of a dialog (Install, Apply, …): filled with the accent.
pub fn primary_button<'a>(text: impl Into<egui::WidgetText>) -> egui::Button<'a> {
    egui::Button::new(text.into().color(Color32::WHITE).strong()).fill(ACCENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio of two colours.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let lum = |c: Color32| {
            let ch = |x: u8| {
                let s = x as f32 / 255.0;
                if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
        };
        let (x, y) = (lum(a) + 0.05, lum(b) + 0.05);
        x.max(y) / x.min(y)
    }

    #[test]
    fn text_is_readable_in_both_looks() {
        for p in [&LIGHT, &DARK] {
            for bg in [p.bg, p.chrome, p.surface, p.field, p.field_hover] {
                assert!(contrast(p.text, bg) >= 7.0, "text on {bg:?}");
            }
            assert!(contrast(p.on_tint, p.tint) >= 4.5, "selected text on its tint");
            assert!(contrast(p.link, p.surface) >= 4.5, "links");
        }
        assert!(contrast(Color32::WHITE, ACCENT) >= 3.0, "main button (large, bold text)");
        assert!(contrast(Color32::WHITE, ACCENT_DEEP) >= 4.5, "page-number badge");
    }
}
