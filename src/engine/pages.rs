//! Page operations (delete, move, rotate, insert) and how they renumber pages.

use std::sync::Arc;

/// A structural edit of the document's pages.
#[derive(Clone, Debug, PartialEq)]
pub enum PageOp {
    Delete {
        pages: Vec<usize>,
    },
    /// Moves `pages` (in ascending order, as a block) so the block starts at index `to`
    /// of the resulting document.
    Move {
        pages: Vec<usize>,
        to: usize,
    },
    /// Adds `quarter_turns` clockwise to each page's own rotation.
    Rotate {
        pages: Vec<usize>,
        quarter_turns: i32,
    },
    InsertBlank {
        at: usize,
        size: (f32, f32),
    },
    /// Inserts pages of another PDF (`pages`: all if None).
    InsertFile {
        at: usize,
        source: Arc<Vec<u8>>,
        name: String,
        pages: Option<Vec<usize>>,
        /// Number of pages inserted; filled in by the engine.
        count: usize,
    },
    /// Adds recognised words (OCR) as invisible text to pages. `replace` first
    /// removes invisible text the pages already have (an earlier recognition).
    TextLayer {
        pages: Vec<(usize, Vec<super::ocr::OcrWord>)>,
        replace: bool,
    },
    /// Trims the pages' visible area by margins (points, as the page is displayed).
    Crop {
        pages: Vec<usize>,
        margins: Margins,
    },
    /// Writes header and footer texts (replacing ones magic-pdf added before).
    HeaderFooter {
        pages: Vec<usize>,
        spec: HeaderFooter,
    },
    /// Removes headers and footers magic-pdf added.
    RemoveHeaderFooter {
        pages: Vec<usize>,
    },
    /// Replaces the bookmarks.
    SetOutline {
        outline: Vec<super::commands::OutlineItem>,
    },
    /// Removes everything under the areas (PDF page space) for good: text, image
    /// pixels, shapes and annotations, and paints the areas black.
    Redact {
        areas: Vec<(usize, Vec<super::commands::PdfBox>)>,
    },
    /// Changes title, author, subject and keywords.
    SetProperties {
        properties: super::commands::EditableProperties,
    },
}

/// Margins in points: top, right, bottom, left (as the page is displayed).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Margins {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Margins {
    pub fn as_array(self) -> [f32; 4] {
        [self.top, self.right, self.bottom, self.left]
    }
}

/// Header and footer texts: left, centre and right part of each. `{page}` and
/// `{pages}` are replaced by the page number and the page count.
#[derive(Clone, Debug, PartialEq)]
pub struct HeaderFooter {
    pub header: [String; 3],
    pub footer: [String; 3],
    pub size: f32,
    pub color: super::commands::Rgb,
    /// Distance from the page edge (points).
    pub margin: f32,
    /// Number of the first page in the document.
    pub first_number: i64,
}

impl HeaderFooter {
    /// The text for page `index` of a `count`-page document.
    pub fn resolve(text: &str, index: usize, count: usize, first_number: i64) -> String {
        text.replace("{page}", &(first_number + index as i64).to_string())
            .replace("{pages}", &(first_number + count as i64 - 1).to_string())
    }
}

/// How page numbers change with an operation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageMapping {
    /// Old page index -> new page index (None: the page was removed).
    pub map: Vec<Option<usize>>,
    pub new_count: usize,
    /// New page indices whose content or geometry changed (e.g. rotated).
    pub changed: Vec<usize>,
}

impl PageMapping {
    /// The mapping that undoes this one.
    pub fn inverse(&self) -> PageMapping {
        let mut map = vec![None; self.new_count];
        for (old, new) in self.map.iter().enumerate() {
            if let Some(n) = new {
                map[*n] = Some(old);
            }
        }
        let changed = self.changed.iter().filter_map(|n| map[*n]).collect();
        PageMapping {
            map,
            new_count: self.map.len(),
            changed,
        }
    }
}

fn sorted_unique(pages: &[usize]) -> Vec<usize> {
    let mut p = pages.to_vec();
    p.sort_unstable();
    p.dedup();
    p
}

impl PageOp {
    pub fn label(&self) -> &'static str {
        match self {
            PageOp::Delete { .. } => "Delete pages",
            PageOp::Move { .. } => "Move pages",
            PageOp::Rotate { .. } => "Rotate pages",
            PageOp::InsertBlank { .. } => "Insert blank page",
            PageOp::InsertFile { .. } => "Insert pages",
            PageOp::TextLayer { .. } => "Recognise text",
            PageOp::Crop { .. } => "Crop pages",
            PageOp::HeaderFooter { .. } => "Header and footer",
            PageOp::RemoveHeaderFooter { .. } => "Remove header and footer",
            PageOp::SetOutline { .. } => "Edit bookmarks",
            PageOp::SetProperties { .. } => "Document properties",
            PageOp::Redact { .. } => "Redact",
        }
    }

    /// Checks the operation against a document with `count` pages.
    pub fn validate(&self, count: usize) -> Result<(), String> {
        let in_range = |pages: &[usize]| {
            if pages.is_empty() {
                Err("no pages selected".to_string())
            } else if pages.iter().any(|&p| p >= count) {
                Err("page does not exist".to_string())
            } else {
                Ok(())
            }
        };
        match self {
            PageOp::Delete { pages } => {
                in_range(pages)?;
                if sorted_unique(pages).len() >= count {
                    return Err("a document needs at least one page".into());
                }
                Ok(())
            }
            PageOp::Move { pages, to } => {
                in_range(pages)?;
                if *to + sorted_unique(pages).len() > count {
                    return Err("target position is out of range".into());
                }
                Ok(())
            }
            PageOp::Rotate { pages, .. }
            | PageOp::Crop { pages, .. }
            | PageOp::HeaderFooter { pages, .. }
            | PageOp::RemoveHeaderFooter { pages } => in_range(pages),
            PageOp::SetOutline { .. } | PageOp::SetProperties { .. } => Ok(()),
            PageOp::TextLayer { pages, .. } => in_range(&pages.iter().map(|(p, _)| *p).collect::<Vec<_>>()),
            PageOp::Redact { areas } => in_range(&areas.iter().map(|(p, _)| *p).collect::<Vec<_>>()),
            PageOp::InsertBlank { at, .. } | PageOp::InsertFile { at, .. } => {
                if *at > count {
                    Err("insert position is out of range".into())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Page renumbering for a document with `count` pages (assumes `validate` passed).
    pub fn mapping(&self, count: usize) -> PageMapping {
        match self {
            PageOp::Delete { pages } => {
                let del = sorted_unique(pages);
                let mut next = 0;
                let map = (0..count)
                    .map(|p| {
                        if del.binary_search(&p).is_ok() {
                            None
                        } else {
                            next += 1;
                            Some(next - 1)
                        }
                    })
                    .collect();
                PageMapping {
                    map,
                    new_count: count - del.len(),
                    changed: Vec::new(),
                }
            }
            PageOp::Move { pages, to } => {
                let moved = sorted_unique(pages);
                let mut order: Vec<usize> = (0..count).filter(|p| moved.binary_search(p).is_err()).collect();
                let at = (*to).min(order.len());
                order.splice(at..at, moved.iter().copied());
                let mut map = vec![None; count];
                for (new, old) in order.iter().enumerate() {
                    map[*old] = Some(new);
                }
                PageMapping {
                    map,
                    new_count: count,
                    changed: Vec::new(),
                }
            }
            PageOp::Rotate { pages, .. }
            | PageOp::Crop { pages, .. }
            | PageOp::HeaderFooter { pages, .. }
            | PageOp::RemoveHeaderFooter { pages } => PageMapping {
                map: (0..count).map(Some).collect(),
                new_count: count,
                changed: sorted_unique(pages),
            },
            PageOp::TextLayer { pages, .. } => PageMapping {
                map: (0..count).map(Some).collect(),
                new_count: count,
                changed: sorted_unique(&pages.iter().map(|(p, _)| *p).collect::<Vec<_>>()),
            },
            PageOp::Redact { areas } => PageMapping {
                map: (0..count).map(Some).collect(),
                new_count: count,
                changed: sorted_unique(&areas.iter().map(|(p, _)| *p).collect::<Vec<_>>()),
            },
            PageOp::SetOutline { .. } | PageOp::SetProperties { .. } => PageMapping {
                map: (0..count).map(Some).collect(),
                new_count: count,
                changed: Vec::new(),
            },
            PageOp::InsertBlank { at, .. } => insert_mapping(count, *at, 1),
            PageOp::InsertFile { at, count: n, .. } => insert_mapping(count, *at, *n),
        }
    }
}

fn insert_mapping(count: usize, at: usize, n: usize) -> PageMapping {
    PageMapping {
        map: (0..count).map(|p| Some(if p < at { p } else { p + n })).collect(),
        new_count: count + n,
        changed: Vec::new(),
    }
}

/// `pages` as a block moved to sit before original page `gap` (0..=count): the
/// `to` index for [`PageOp::Move`]. Returns None if nothing would move.
pub fn move_target(pages: &[usize], gap: usize, count: usize) -> Option<usize> {
    let moved = sorted_unique(pages);
    let to = gap - moved.iter().filter(|&&p| p < gap).count();
    let op = PageOp::Move {
        pages: moved.clone(),
        to,
    };
    let m = op.mapping(count);
    let identity = m.map.iter().enumerate().all(|(o, n)| *n == Some(o));
    (!identity).then_some(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(m: &PageMapping) -> Vec<usize> {
        // New page -> old page, to read results as "the new document order".
        let inv = m.inverse();
        inv.map.iter().map(|o| o.unwrap_or(usize::MAX)).collect()
    }

    #[test]
    fn delete_renumbers() {
        let m = PageOp::Delete { pages: vec![3, 1] }.mapping(5);
        assert_eq!(m.map, vec![Some(0), None, Some(1), None, Some(2)]);
        assert_eq!(m.new_count, 3);
        assert!(PageOp::Delete { pages: vec![0, 1] }.validate(2).is_err());
    }

    #[test]
    fn move_block() {
        // Move pages 1 and 2 to the end of a 5-page document.
        let m = PageOp::Move { pages: vec![2, 1], to: 3 }.mapping(5);
        assert_eq!(order(&m), vec![0, 3, 4, 1, 2]);
        // Move page 4 to the front.
        let m = PageOp::Move { pages: vec![4], to: 0 }.mapping(5);
        assert_eq!(order(&m), vec![4, 0, 1, 2, 3]);
        assert!(PageOp::Move { pages: vec![0, 1], to: 4 }.validate(5).is_err());
    }

    #[test]
    fn move_target_from_drop_gap() {
        // Drag page 0 to after page 2 (gap 3) in a 5-page document.
        let to = move_target(&[0], 3, 5).unwrap();
        assert_eq!(order(&PageOp::Move { pages: vec![0], to }.mapping(5)), vec![1, 2, 0, 3, 4]);
        // Dropping right next to itself does nothing.
        assert_eq!(move_target(&[2], 2, 5), None);
        assert_eq!(move_target(&[2], 3, 5), None);
        // Several pages dropped at the very end.
        let to = move_target(&[0, 2], 5, 5).unwrap();
        assert_eq!(order(&PageOp::Move { pages: vec![0, 2], to }.mapping(5)), vec![1, 3, 4, 0, 2]);
    }

    #[test]
    fn insert_and_inverse() {
        let m = PageOp::InsertBlank {
            at: 1,
            size: (595.0, 842.0),
        }
        .mapping(3);
        assert_eq!(m.map, vec![Some(0), Some(2), Some(3)]);
        let inv = m.inverse();
        assert_eq!(inv.map, vec![Some(0), None, Some(1), Some(2)]);
        assert_eq!(inv.new_count, 3);
    }

    #[test]
    fn rotate_marks_pages_changed() {
        let m = PageOp::Rotate {
            pages: vec![2, 0],
            quarter_turns: 1,
        }
        .mapping(3);
        assert_eq!(m.changed, vec![0, 2]);
        assert_eq!(m.inverse().changed, vec![0, 2]);
    }
}
