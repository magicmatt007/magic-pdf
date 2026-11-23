//! Pan/zoom state mapping document space to screen space.

use eframe::egui::{Pos2, Rect, Vec2, vec2};
use serde::{Deserialize, Serialize};

pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 32.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FitMode {
    #[default]
    None,
    Width,
    Height,
    Page,
}

impl FitMode {
    pub fn label(self) -> &'static str {
        crate::i18n::t(match self {
            FitMode::None => "Custom",
            FitMode::Width => "Fit width",
            FitMode::Height => "Fit height",
            FitMode::Page => "Fit page",
        })
    }
}

#[derive(Clone, Debug)]
pub struct Viewport {
    /// Screen points per document point.
    pub zoom: f32,
    /// Document-space position shown at the top-left of the view.
    pub offset: Vec2,
    pub fit: FitMode,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            offset: Vec2::ZERO,
            fit: FitMode::Height,
        }
    }
}

impl Viewport {
    pub fn to_screen(&self, doc: Pos2, view: Rect) -> Pos2 {
        view.min + (doc.to_vec2() - self.offset) * self.zoom
    }

    pub fn rect_to_screen(&self, doc: Rect, view: Rect) -> Rect {
        Rect::from_min_max(self.to_screen(doc.min, view), self.to_screen(doc.max, view))
    }

    pub fn to_doc(&self, screen: Pos2, view: Rect) -> Pos2 {
        ((screen - view.min) / self.zoom + self.offset).to_pos2()
    }

    /// The visible document-space area.
    pub fn visible_doc_rect(&self, view: Rect) -> Rect {
        Rect::from_min_size(self.offset.to_pos2(), view.size() / self.zoom)
    }

    /// Keeps the content in view; content smaller than the view is centred.
    pub fn clamp(&mut self, content: Vec2, view: Vec2) {
        let visible = view / self.zoom;
        for axis in 0..2 {
            self.offset[axis] = if content[axis] <= visible[axis] {
                -(visible[axis] - content[axis]) / 2.0
            } else {
                self.offset[axis].clamp(0.0, content[axis] - visible[axis])
            };
        }
    }

    /// Zooms while keeping the document point under `anchor` (screen) fixed.
    pub fn zoom_at(&mut self, new_zoom: f32, anchor: Pos2, view: Rect) {
        let new_zoom = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let doc = self.to_doc(anchor, view);
        self.zoom = new_zoom;
        self.offset = doc.to_vec2() - (anchor - view.min) / new_zoom;
    }

    /// Zoom that fits `target` (document space) into `view` according to `fit`.
    pub fn fit_zoom(fit: FitMode, target: Vec2, view: Vec2, margin: f32) -> Option<f32> {
        let avail = (view - vec2(2.0 * margin, 2.0 * margin)).max(vec2(1.0, 1.0));
        let zx = avail.x / target.x.max(1.0);
        let zy = avail.y / target.y.max(1.0);
        let z = match fit {
            FitMode::None => return None,
            FitMode::Width => zx,
            FitMode::Height => zy,
            FitMode::Page => zx.min(zy),
        };
        Some(z.clamp(MIN_ZOOM, MAX_ZOOM))
    }

    /// Scrolls so that the document point `doc` is at the start of the view along the
    /// scroll axis (x for horizontal layouts, y otherwise), `margin` points before it.
    pub fn scroll_to(&mut self, doc: Pos2, horizontal: bool, margin: f32) {
        if horizontal {
            self.offset.x = doc.x - margin;
        } else {
            self.offset.y = doc.y - margin;
        }
    }

    /// Scrolls the minimum amount needed to make `target` visible, centring it if it is
    /// off-screen.
    pub fn reveal(&mut self, target: Rect, view: Rect) {
        let visible = self.visible_doc_rect(view);
        if visible.contains_rect(target) {
            return;
        }
        let center = target.center().to_vec2() - visible.size() / 2.0;
        if target.min.x < visible.min.x || target.max.x > visible.max.x {
            self.offset.x = center.x;
        }
        if target.min.y < visible.min.y || target.max.y > visible.max.y {
            self.offset.y = center.y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    fn view() -> Rect {
        Rect::from_min_size(pos2(100.0, 50.0), vec2(800.0, 600.0))
    }

    #[test]
    fn screen_doc_roundtrip() {
        let vp = Viewport {
            zoom: 2.0,
            offset: vec2(30.0, 40.0),
            fit: FitMode::None,
        };
        let p = pos2(123.0, 456.0);
        let back = vp.to_doc(vp.to_screen(p, view()), view());
        assert!((back - p).length() < 1e-4);
        assert_eq!(vp.to_screen(pos2(30.0, 40.0), view()), view().min);
    }

    #[test]
    fn zoom_at_keeps_anchor_fixed() {
        let mut vp = Viewport::default();
        let anchor = pos2(400.0, 300.0);
        let before = vp.to_doc(anchor, view());
        vp.zoom_at(3.0, anchor, view());
        let after = vp.to_doc(anchor, view());
        assert!((before - after).length() < 1e-3);
        vp.zoom_at(1000.0, anchor, view());
        assert_eq!(vp.zoom, MAX_ZOOM);
    }

    #[test]
    fn fit_height_and_page() {
        let z = Viewport::fit_zoom(FitMode::Height, vec2(595.0, 842.0), vec2(800.0, 862.0), 10.0);
        assert!((z.unwrap() - 1.0).abs() < 1e-4);
        let z = Viewport::fit_zoom(FitMode::Page, vec2(1000.0, 100.0), vec2(520.0, 900.0), 10.0);
        assert!((z.unwrap() - 0.5).abs() < 1e-4);
        assert!(Viewport::fit_zoom(FitMode::None, vec2(1.0, 1.0), vec2(1.0, 1.0), 0.0).is_none());
    }

    #[test]
    fn clamp_centres_small_content_and_bounds_large() {
        let mut vp = Viewport {
            zoom: 1.0,
            offset: vec2(-500.0, 5000.0),
            fit: FitMode::None,
        };
        vp.clamp(vec2(400.0, 2000.0), vec2(800.0, 600.0));
        assert_eq!(vp.offset.x, -200.0);
        assert_eq!(vp.offset.y, 1400.0);
    }

    #[test]
    fn reveal_offscreen_rect() {
        let mut vp = Viewport {
            zoom: 1.0,
            offset: Vec2::ZERO,
            fit: FitMode::None,
        };
        let target = Rect::from_min_size(pos2(2000.0, 100.0), vec2(50.0, 20.0));
        vp.reveal(target, view());
        assert!(vp.visible_doc_rect(view()).contains_rect(target));
        assert_eq!(vp.offset.y, 0.0, "already visible vertically, so y is untouched");
    }
}
