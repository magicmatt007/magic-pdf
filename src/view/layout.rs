//! Pure page-layout engine: turns page sizes and a layout mode into page rectangles in
//! document space (PDF points at zoom 1, origin top-left, y down).

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::engine::{PageGeom, PdfBox, Rotation};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    /// All pages in one row, scrolled left/right. Ideal for wide monitors.
    #[default]
    Horizontal,
    /// All pages in one column.
    Vertical,
    /// One page at a time.
    SinglePage,
    /// Two pages side by side per row (cover page alone), scrolled vertically.
    Facing,
    /// As many pages per row as fit the window width.
    Grid,
}

impl LayoutMode {
    pub const ALL: [LayoutMode; 5] = [
        LayoutMode::Horizontal,
        LayoutMode::Vertical,
        LayoutMode::SinglePage,
        LayoutMode::Facing,
        LayoutMode::Grid,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayoutMode::Horizontal => "Horizontal",
            LayoutMode::Vertical => "Vertical",
            LayoutMode::SinglePage => "Single page",
            LayoutMode::Facing => "Facing",
            LayoutMode::Grid => "Grid",
        }
    }

    /// Whether scrolling advances along the x axis.
    pub fn is_horizontal(self) -> bool {
        self == LayoutMode::Horizontal
    }
}

/// Gap between pages and around the content, in points.
pub const GAP: f32 = 12.0;

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutParams {
    pub mode: LayoutMode,
    pub rotation: Rotation,
    /// Width available in document units; only used by [`LayoutMode::Grid`].
    pub available_width: f32,
    /// Page shown in [`LayoutMode::SinglePage`].
    pub single_page: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Rectangle of each page, `None` if the page is not part of the layout.
    pub pages: Vec<Option<Rect>>,
    /// Pages per row, in order.
    pub rows: Vec<Vec<usize>>,
    /// Total content size including outer margins.
    pub size: Vec2,
}

impl Layout {
    pub fn page_rect(&self, page: usize) -> Option<Rect> {
        self.pages.get(page).copied().flatten()
    }

    pub fn row_rect(&self, page: usize) -> Option<Rect> {
        let row = self.rows.iter().find(|r| r.contains(&page))?;
        row.iter()
            .filter_map(|&p| self.page_rect(p))
            .reduce(|a, b| a.union(b))
    }

    /// Pages whose rectangle intersects `area` (document space).
    pub fn pages_in(&self, area: Rect) -> Vec<usize> {
        self.pages
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.filter(|r| r.intersects(area)).map(|_| i))
            .collect()
    }

    pub fn page_at(&self, point: Pos2) -> Option<usize> {
        self.pages
            .iter()
            .position(|r| r.is_some_and(|r| r.contains(point)))
    }
}

/// Page size after applying the view rotation.
pub fn rotated_size(size: (f32, f32), rotation: Rotation) -> Vec2 {
    if rotation.swaps_axes() {
        vec2(size.1, size.0)
    } else {
        vec2(size.0, size.1)
    }
}

pub fn compute(page_sizes: &[(f32, f32)], p: &LayoutParams) -> Layout {
    let n = page_sizes.len();
    let sizes: Vec<Vec2> = page_sizes
        .iter()
        .map(|&s| rotated_size(s, p.rotation))
        .collect();

    let rows: Vec<Vec<usize>> = match p.mode {
        LayoutMode::Horizontal => {
            if n == 0 {
                vec![]
            } else {
                vec![(0..n).collect()]
            }
        }
        LayoutMode::Vertical => (0..n).map(|i| vec![i]).collect(),
        LayoutMode::SinglePage => {
            if n == 0 {
                vec![]
            } else {
                vec![vec![p.single_page.min(n - 1)]]
            }
        }
        LayoutMode::Facing => {
            // Page 1 is the cover and stands alone; then (2,3), (4,5), ...
            let mut rows = Vec::new();
            if n > 0 {
                rows.push(vec![0]);
            }
            let mut i = 1;
            while i < n {
                rows.push((i..(i + 2).min(n)).collect());
                i += 2;
            }
            rows
        }
        LayoutMode::Grid => {
            let mut rows: Vec<Vec<usize>> = Vec::new();
            let mut row_w = GAP;
            for (i, s) in sizes.iter().enumerate() {
                let w = s.x + GAP;
                match rows.last_mut() {
                    Some(row) if row_w + w <= p.available_width.max(1.0) => {
                        row.push(i);
                        row_w += w;
                    }
                    _ => {
                        rows.push(vec![i]);
                        row_w = GAP + w;
                    }
                }
            }
            rows
        }
    };

    let row_sizes: Vec<Vec2> = rows
        .iter()
        .map(|row| {
            let w: f32 = row.iter().map(|&i| sizes[i].x).sum::<f32>()
                + GAP * (row.len().saturating_sub(1)) as f32;
            let h = row.iter().map(|&i| sizes[i].y).fold(0.0, f32::max);
            vec2(w, h)
        })
        .collect();
    let content_w = row_sizes.iter().map(|s| s.x).fold(0.0, f32::max);

    let mut pages = vec![None; n];
    let mut y = GAP;
    for (row, rs) in rows.iter().zip(&row_sizes) {
        // Facing rows with one page: keep the cover on the right-hand side like a book.
        let mut x = GAP + (content_w - rs.x) / 2.0;
        if p.mode == LayoutMode::Facing && row.len() == 1 && row[0] == 0 && n > 1 {
            let pair_w = sizes[0].x * 2.0 + GAP;
            x = GAP + (content_w - pair_w).max(0.0) / 2.0 + sizes[0].x + GAP;
        }
        for &i in row {
            let s = sizes[i];
            let top = y + (rs.y - s.y) / 2.0;
            pages[i] = Some(Rect::from_min_size(pos2(x, top), s));
            x += s.x + GAP;
        }
        y += rs.y + GAP;
    }

    let max_right = pages
        .iter()
        .flatten()
        .map(|r| r.right())
        .fold(0.0, f32::max);
    let size = vec2(max_right.max(content_w + GAP) + GAP, y);
    Layout { pages, rows, size }
}

/// PDF page space (origin bottom-left) -> rotated page-local space (origin top-left).
pub fn pdf_point_to_local(p: Pos2, (w, h): (f32, f32), rotation: Rotation) -> Pos2 {
    let (x, y) = (p.x, h - p.y);
    match rotation {
        Rotation::R0 => pos2(x, y),
        Rotation::R90 => pos2(h - y, x),
        Rotation::R180 => pos2(w - x, h - y),
        Rotation::R270 => pos2(y, w - x),
    }
}

/// Inverse of [`pdf_point_to_local`].
pub fn local_point_to_pdf(v: Pos2, (w, h): (f32, f32), rotation: Rotation) -> Pos2 {
    let (x, y) = match rotation {
        Rotation::R0 => (v.x, v.y),
        Rotation::R90 => (v.y, h - v.x),
        Rotation::R180 => (w - v.x, h - v.y),
        Rotation::R270 => (w - v.y, v.x),
    };
    pos2(x, h - y)
}

/// PDF page space -> page-local view space, honouring the page's crop box origin, its
/// own /Rotate and the view rotation.
pub fn geom_pdf_to_local(p: Pos2, geom: &PageGeom, view: Rotation) -> Pos2 {
    let q = pos2(p.x - geom.origin.0, p.y - geom.origin.1);
    pdf_point_to_local(q, geom.size, geom.rotation.then(view))
}

/// Inverse of [`geom_pdf_to_local`].
pub fn geom_local_to_pdf(v: Pos2, geom: &PageGeom, view: Rotation) -> Pos2 {
    let q = local_point_to_pdf(v, geom.size, geom.rotation.then(view));
    pos2(q.x + geom.origin.0, q.y + geom.origin.1)
}

pub fn geom_box_to_local(b: &PdfBox, geom: &PageGeom, view: Rotation) -> Rect {
    Rect::from_two_pos(
        geom_pdf_to_local(pos2(b.left, b.top), geom, view),
        geom_pdf_to_local(pos2(b.right, b.bottom), geom, view),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(mode: LayoutMode) -> LayoutParams {
        LayoutParams {
            mode,
            rotation: Rotation::R0,
            available_width: 1000.0,
            single_page: 0,
        }
    }

    const A4: (f32, f32) = (595.0, 842.0);

    #[test]
    fn horizontal_places_pages_in_one_row() {
        let l = compute(&[A4, A4, A4], &params(LayoutMode::Horizontal));
        assert_eq!(l.rows.len(), 1);
        let r: Vec<Rect> = l.pages.iter().map(|r| r.unwrap()).collect();
        assert_eq!(r[0].min, pos2(GAP, GAP));
        assert_eq!(r[1].min.x, GAP + 595.0 + GAP);
        assert!(r.iter().all(|p| p.min.y == GAP));
        assert_eq!(l.size, vec2(3.0 * 595.0 + 4.0 * GAP, 842.0 + 2.0 * GAP));
    }

    #[test]
    fn horizontal_centers_smaller_pages_vertically() {
        let l = compute(&[A4, (595.0, 442.0)], &params(LayoutMode::Horizontal));
        let small = l.page_rect(1).unwrap();
        assert_eq!(small.center().y, l.page_rect(0).unwrap().center().y);
    }

    #[test]
    fn vertical_stacks_pages() {
        let l = compute(&[A4, A4], &params(LayoutMode::Vertical));
        assert_eq!(l.rows.len(), 2);
        assert_eq!(l.page_rect(1).unwrap().min.y, GAP + 842.0 + GAP);
    }

    #[test]
    fn facing_has_cover_then_pairs() {
        let l = compute(&[A4; 5], &params(LayoutMode::Facing));
        assert_eq!(l.rows, vec![vec![0], vec![1, 2], vec![3, 4]]);
        // Cover sits in the right column, aligned with odd pages.
        assert_eq!(l.page_rect(0).unwrap().min.x, l.page_rect(2).unwrap().min.x);
    }

    #[test]
    fn grid_wraps_by_available_width() {
        let mut p = params(LayoutMode::Grid);
        p.available_width = 2.0 * 595.0 + 3.0 * GAP + 1.0;
        let l = compute(&[A4; 5], &p);
        assert_eq!(l.rows, vec![vec![0, 1], vec![2, 3], vec![4]]);
    }

    #[test]
    fn single_page_only_lays_out_current() {
        let mut p = params(LayoutMode::SinglePage);
        p.single_page = 2;
        let l = compute(&[A4; 4], &p);
        assert!(l.page_rect(0).is_none());
        assert!(l.page_rect(2).is_some());
        p.single_page = 99;
        assert!(compute(&[A4; 4], &p).page_rect(3).is_some());
    }

    #[test]
    fn rotation_swaps_page_axes() {
        let mut p = params(LayoutMode::Horizontal);
        p.rotation = Rotation::R90;
        let l = compute(&[A4], &p);
        assert_eq!(l.page_rect(0).unwrap().size(), vec2(842.0, 595.0));
    }

    #[test]
    fn empty_document() {
        for mode in LayoutMode::ALL {
            let l = compute(&[], &params(mode));
            assert!(l.pages.is_empty());
        }
    }

    #[test]
    fn point_transform_roundtrip() {
        for rot in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
            let p = pos2(100.0, 700.0);
            let local = pdf_point_to_local(p, A4, rot);
            let back = local_point_to_pdf(local, A4, rot);
            assert!((back - p).length() < 1e-3, "{rot:?}");
        }
        // Top-left corner of an unrotated page maps to the local origin.
        assert_eq!(pdf_point_to_local(pos2(0.0, 842.0), A4, Rotation::R0), pos2(0.0, 0.0));
        // Rotated 90° clockwise, the top-left corner ends up top-right.
        assert_eq!(pdf_point_to_local(pos2(0.0, 842.0), A4, Rotation::R90), pos2(842.0, 0.0));
    }

    #[test]
    fn geom_transform_handles_origin_and_page_rotation() {
        // A page with a crop box starting at (10, 20) and /Rotate 90.
        let geom = PageGeom {
            size: (595.0, 842.0),
            origin: (10.0, 20.0),
            rotation: Rotation::R90,
        };
        // Top-left of the unrotated crop box ends up at the top-right of the display.
        let p = geom_pdf_to_local(pos2(10.0, 862.0), &geom, Rotation::R0);
        assert_eq!(p, pos2(842.0, 0.0));
        // Page rotation 90 + view rotation 270 = upright again.
        let p = geom_pdf_to_local(pos2(10.0, 862.0), &geom, Rotation::R270);
        assert_eq!(p, pos2(0.0, 0.0));
        let back = geom_local_to_pdf(pos2(100.0, 200.0), &geom, Rotation::R180);
        let again = geom_pdf_to_local(back, &geom, Rotation::R180);
        assert!((again - pos2(100.0, 200.0)).length() < 1e-3);
    }

    #[test]
    fn page_lookup() {
        let l = compute(&[A4, A4], &params(LayoutMode::Horizontal));
        let r1 = l.page_rect(1).unwrap();
        assert_eq!(l.page_at(r1.center()), Some(1));
        assert_eq!(l.page_at(pos2(1.0, 1.0)), None);
        assert_eq!(l.pages_in(r1.shrink(5.0)), vec![1]);
    }
}
