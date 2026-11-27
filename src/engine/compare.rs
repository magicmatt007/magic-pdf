//! Comparing the text of two documents, page by page (pages matched by number).

use similar::{Algorithm, DiffTag, capture_diff_slices};

use super::commands::{PageText, PdfBox};

/// A word on a page and where it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: PdfBox,
}

/// Splits page text into words with their bounding boxes.
pub fn words(text: &PageText) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    let mut current: Option<Word> = None;
    for c in &text.chars {
        match (c.ch.is_whitespace(), c.rect) {
            (false, Some(r)) => match &mut current {
                Some(w) => {
                    w.text.push(c.ch);
                    w.rect = w.rect.union(r);
                }
                None => current = Some(Word { text: c.ch.to_string(), rect: r }),
            },
            _ => out.extend(current.take()),
        }
    }
    out.extend(current);
    out
}

/// The differences on one page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageDiff {
    pub page: usize,
    /// Words only in the first document (removed) and only in the second (added).
    pub removed: Vec<PdfBox>,
    pub added: Vec<PdfBox>,
    /// Short descriptions of the changes, e.g. "−old words +new words".
    pub summary: Vec<String>,
}

impl PageDiff {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }
}

fn joined(words: &[Word]) -> String {
    let s = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
    if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s }
}

/// Word differences between page `page` of two documents.
pub fn diff_page(page: usize, a: &[Word], b: &[Word]) -> PageDiff {
    let ta: Vec<&str> = a.iter().map(|w| w.text.as_str()).collect();
    let tb: Vec<&str> = b.iter().map(|w| w.text.as_str()).collect();
    let mut d = PageDiff { page, ..Default::default() };
    for op in capture_diff_slices(Algorithm::Myers, &ta, &tb) {
        let (tag, ra, rb) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        d.removed.extend(a[ra.clone()].iter().map(|w| w.rect));
        d.added.extend(b[rb.clone()].iter().map(|w| w.rect));
        let line = match tag {
            DiffTag::Delete => format!("− {}", joined(&a[ra])),
            DiffTag::Insert => format!("+ {}", joined(&b[rb])),
            _ => format!("{} → {}", joined(&a[ra]), joined(&b[rb])),
        };
        d.summary.push(line);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::TextChar;

    fn text(s: &str) -> PageText {
        PageText {
            chars: s
                .chars()
                .enumerate()
                .map(|(i, ch)| TextChar {
                    ch,
                    rect: Some(PdfBox { left: i as f32 * 10.0, bottom: 0.0, right: i as f32 * 10.0 + 8.0, top: 10.0 }),
                })
                .collect(),
            links: Vec::new(),
        }
    }

    #[test]
    fn words_and_their_boxes() {
        let w = words(&text("ab  cd"));
        assert_eq!(w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["ab", "cd"]);
        assert_eq!((w[1].rect.left, w[1].rect.right), (40.0, 58.0));
    }

    #[test]
    fn changed_words_are_marked_on_both_sides() {
        let a = words(&text("the price is 100 francs today"));
        let b = words(&text("the price is 120 francs"));
        let d = diff_page(3, &a, &b);
        assert_eq!(d.page, 3);
        assert_eq!(d.removed.len(), 2, "100 and today");
        assert_eq!(d.added.len(), 1, "120");
        assert_eq!(d.summary, ["100 → 120", "− today"]);
        assert!(diff_page(0, &a, &a).is_empty());
    }
}
