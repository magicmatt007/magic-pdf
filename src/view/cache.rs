//! GPU texture cache for rendered pages with a memory budget and LRU eviction.

use std::collections::HashMap;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

use crate::engine::{DocId, RenderKey, Rotation};

/// Largest bitmap we ask PDFium for, in pixels (~64 MiB RGBA).
const MAX_PIXELS: f32 = 16_000_000.0;

/// Rounds a desired pixel width up to one of a geometric series of buckets
/// (factor 2^(1/4) ≈ 1.19), so small zoom changes reuse existing renders.
pub fn width_bucket(px: f32, aspect_h_over_w: f32) -> u32 {
    let px = px.max(16.0);
    let k = (px.log2() * 4.0).ceil();
    let mut w = 2f32.powf(k / 4.0).round();
    let max_w = (MAX_PIXELS / aspect_h_over_w.max(0.01)).sqrt();
    if w > max_w {
        w = max_w.floor();
    }
    w.max(16.0) as u32
}

struct Entry {
    texture: TextureHandle,
    bytes: usize,
    last_used: u64,
}

pub struct TextureCache {
    entries: HashMap<RenderKey, Entry>,
    total_bytes: usize,
    budget_bytes: usize,
    frame: u64,
    pub invert: bool,
}

impl TextureCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            budget_bytes,
            frame: 0,
            invert: false,
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    pub fn contains(&self, key: &RenderKey) -> bool {
        self.entries.contains_key(key)
    }

    pub fn insert(&mut self, ctx: &egui::Context, key: RenderKey, size: [usize; 2], mut rgba: Vec<u8>) {
        if self.invert {
            for px in rgba.as_chunks_mut::<4>().0 {
                px[0] = 255 - px[0];
                px[1] = 255 - px[1];
                px[2] = 255 - px[2];
            }
        }
        let image = ColorImage::from_rgba_unmultiplied(size, &rgba);
        let name = format!("page-{}-{}-{}-v{}", key.doc, key.page, key.width_px, key.version);
        let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
        let bytes = size[0] * size[1] * 4;
        if let Some(old) = self.entries.insert(
            key,
            Entry {
                texture,
                bytes,
                last_used: self.frame,
            },
        ) {
            self.total_bytes -= old.bytes;
        }
        self.total_bytes += bytes;
    }

    /// Best available texture for a page: prefers the current content `version`, then
    /// the smallest render at least `want_width` wide, otherwise the largest available
    /// (shown scaled until the crisp one arrives). Stale versions are used as
    /// placeholders to avoid flicker after edits.
    pub fn best(
        &mut self,
        doc: DocId,
        page: usize,
        rotation: Rotation,
        want_width: u32,
        version: u32,
    ) -> Option<(TextureHandle, RenderKey)> {
        let mut best: Option<RenderKey> = None;
        for key in self.entries.keys() {
            if key.doc != doc || key.page != page || key.rotation != rotation {
                continue;
            }
            best = match best {
                None => Some(*key),
                Some(b) => {
                    let (kv, bv) = (key.version == version, b.version == version);
                    let better = if kv != bv {
                        kv
                    } else if b.width_px >= want_width {
                        key.width_px >= want_width && key.width_px < b.width_px
                    } else {
                        key.width_px > b.width_px
                    };
                    Some(if better { *key } else { b })
                }
            };
        }
        let key = best?;
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = self.frame;
        Some((entry.texture.clone(), key))
    }

    pub fn touch(&mut self, key: &RenderKey) {
        if let Some(e) = self.entries.get_mut(key) {
            e.last_used = self.frame;
        }
    }

    pub fn remove_doc(&mut self, doc: DocId) {
        self.entries.retain(|k, e| {
            let keep = k.doc != doc;
            if !keep {
                self.total_bytes -= e.bytes;
            }
            keep
        });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    /// Evicts least recently used textures not used in the current frame until the
    /// cache fits its budget.
    pub fn evict(&mut self) {
        if self.total_bytes <= self.budget_bytes {
            return;
        }
        let mut candidates: Vec<(RenderKey, u64)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.last_used < self.frame)
            .map(|(k, e)| (*k, e.last_used))
            .collect();
        candidates.sort_by_key(|c| c.1);
        for (key, _) in candidates {
            if self.total_bytes <= self.budget_bytes {
                break;
            }
            if let Some(e) = self.entries.remove(&key) {
                self.total_bytes -= e.bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::width_bucket;

    #[test]
    fn buckets_are_monotonic_and_cover_request() {
        let mut last = 0;
        for px in (20..3000).step_by(7) {
            let b = width_bucket(px as f32, 1.414);
            assert!(b >= px, "{b} < {px}");
            assert!(b >= last);
            assert!((b as f32) < px as f32 * 1.2 + 2.0);
            last = b;
        }
    }

    #[test]
    fn buckets_respect_pixel_cap() {
        let b = width_bucket(100_000.0, 1.414);
        assert!((b as f32) * (b as f32) * 1.414 <= 16_000_001.0);
    }
}
