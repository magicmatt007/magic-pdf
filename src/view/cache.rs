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

/// Ordering key for [`TextureCache::best`] (higher is better).
///
/// Versions only grow, so among sharp renders the newest content wins (the current
/// version if it has arrived, otherwise the latest stale one, never an older one,
/// which would make the page jump back while typing). Ties are broken by size.
fn rank(k: &RenderKey, want_width: u32, version: u32) -> (bool, i64, i64) {
    let sharp = k.width_px as f32 >= want_width as f32 * 0.9;
    let age = -(version.saturating_sub(k.version) as i64);
    if sharp {
        (true, age, -(k.width_px as i64))
    } else {
        (false, k.width_px as i64, age)
    }
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

    /// Best available texture for a page. Sharpness comes first: among renders that
    /// are (nearly) wide enough, the current content `version` wins, then the smallest.
    /// If none is wide enough, the largest is used, scaled, until the crisp one arrives.
    /// So after an edit (e.g. typing in a form field) the previous sharp render stays
    /// on screen instead of a blurry thumbnail of the new version.
    pub fn best(
        &mut self,
        doc: DocId,
        page: usize,
        rotation: Rotation,
        want_width: u32,
        version: u32,
    ) -> Option<(TextureHandle, RenderKey)> {
        let key = *self
            .entries
            .keys()
            .filter(|k| k.doc == doc && k.page == page && k.rotation == rotation)
            .max_by_key(|k| rank(k, want_width, version))?;
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
    use super::*;

    fn key(width_px: u32, version: u32) -> RenderKey {
        RenderKey {
            doc: 1,
            page: 0,
            width_px,
            rotation: Rotation::R0,
            version,
        }
    }

    fn insert(cache: &mut TextureCache, ctx: &egui::Context, k: RenderKey) {
        cache.insert(ctx, k, [1, 1], vec![255; 4]);
    }

    fn best(cache: &mut TextureCache, want: u32, version: u32) -> RenderKey {
        cache.best(1, 0, Rotation::R0, want, version).unwrap().1
    }

    #[test]
    fn keeps_sharp_old_render_over_blurry_new_thumbnail() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(usize::MAX);
        insert(&mut cache, &ctx, key(1000, 0));
        // After an edit only the new thumbnail has arrived: keep showing the sharp one.
        insert(&mut cache, &ctx, key(130, 1));
        assert_eq!(best(&mut cache, 1000, 1), key(1000, 0));
        // Once the sharp new version arrives it wins.
        insert(&mut cache, &ctx, key(1000, 1));
        assert_eq!(best(&mut cache, 1000, 1), key(1000, 1));
        // The thumbnail view itself gets the small current render.
        assert_eq!(best(&mut cache, 130, 1), key(130, 1));
    }

    #[test]
    fn newest_sharp_render_wins_while_typing() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(usize::MAX);
        // Renders from earlier keystrokes, all equally sharp; version 6 is still pending.
        for v in [0, 3, 5, 1, 4, 2] {
            insert(&mut cache, &ctx, key(1000, v));
        }
        for _ in 0..20 {
            assert_eq!(best(&mut cache, 1000, 6), key(1000, 5));
        }
    }

    #[test]
    fn prefers_smallest_sharp_then_largest_blurry() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(usize::MAX);
        insert(&mut cache, &ctx, key(200, 0));
        insert(&mut cache, &ctx, key(800, 0));
        insert(&mut cache, &ctx, key(1600, 0));
        assert_eq!(best(&mut cache, 700, 0), key(800, 0));
        assert_eq!(best(&mut cache, 3000, 0), key(1600, 0));
    }

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
