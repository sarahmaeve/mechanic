// Text rendering: glyph rasterization via cosmic-text / swash and a GPU glyph
// atlas that caches the results.

use std::collections::HashMap;

use cosmic_text::{Attrs, Buffer, CacheKey, FontSystem, Metrics, Shaping, SwashCache};
use mechanic_config::font::FontConfig;

// ── Atlas constants ───────────────────────────────────────────────────────────

/// Number of slots per row in the atlas.
const ATLAS_COLS: u32 = 16;
/// Initial number of rows in the atlas (grows on demand).
const ATLAS_INITIAL_ROWS: u32 = 8;

/// Compute the atlas slot size from the cell dimensions.
///
/// Slot must fit the tallest/widest glyph plus italic overhang and
/// descender margin. 1.5× the max cell dimension is a comfortable
/// headroom; next_power_of_two for texture-friendly alignment;
/// floor at 32 so tiny fonts still have a reasonable slot.
pub fn compute_slot_size(cell_width: f32, cell_height: f32) -> u32 {
    let max_dim = cell_width.max(cell_height);
    let padded = (max_dim * 1.5).ceil() as u32;
    padded.next_power_of_two().max(32)
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Real font metrics extracted from cosmic-text after shaping.
///
/// These replace the rough `font_size * 0.6 / 1.3` estimates used previously.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    /// Advance width of a monospace cell in physical pixels (from the space
    /// glyph's `x_advance`).
    pub cell_width: f32,
    /// Line height in physical pixels (from `Metrics::line_height`).
    pub cell_height: f32,
    /// Distance from the top of the cell to the baseline in physical pixels
    /// (from `LayoutRun::max_ascent`).
    pub ascent: f32,
}

/// Location and metrics of a rasterized glyph in the GPU atlas.
#[derive(Debug, Clone, Copy)]
pub struct GlyphInfo {
    /// UV rectangle in the atlas texture covering *only* the glyph bitmap:
    /// `(u_min, v_min, u_max, v_max)`.
    pub atlas_uv: [f32; 4],
    /// Horizontal offset in pixels from the cell left edge to the glyph's
    /// left edge (bearing X).
    pub offset_x: f32,
    /// Vertical offset in pixels from the cell top to the glyph's top edge
    /// (`ascent - placement.top`).
    pub offset_y: f32,
    /// Width of the rasterized bitmap in pixels.
    pub glyph_width: f32,
    /// Height of the rasterized bitmap in pixels.
    pub glyph_height: f32,
}

// ── TextRenderer ──────────────────────────────────────────────────────────────

/// Key for the fast-path glyph cache, avoiding cosmic-text shaping on every
/// frame for characters we have already rasterized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CharStyleKey {
    ch: char,
    bold: bool,
    italic: bool,
}

/// Manages font shaping and GPU glyph atlas upload.
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// The atlas texture lives on the GPU.
    pub atlas_texture: wgpu::Texture,
    /// A view into `atlas_texture`, kept alive alongside the texture.
    pub atlas_view: wgpu::TextureView,
    /// Map from cosmic-text `CacheKey` to cached `GlyphInfo`.
    atlas_map: HashMap<CacheKey, GlyphInfo>,
    /// Fast-path cache: `(char, bold, italic)` → `GlyphInfo`.
    ///
    /// This lets us skip cosmic-text shaping entirely for characters that
    /// have already been rasterized.  The shaping step (Buffer + shape) is
    /// the most expensive part of the per-character path.
    char_cache: HashMap<CharStyleKey, Option<GlyphInfo>>,
    /// Next free slot index.
    atlas_next_slot: u32,
    /// Total number of slots currently allocated.
    atlas_capacity_slots: u32,
    /// Width and height of each glyph slot in the atlas (pixels).
    /// Computed from cell metrics so large fonts aren't cropped.
    slot_size: u32,
    /// Monotonically increasing counter; incremented whenever the atlas
    /// texture is recreated (i.e., when it grows).  Consumers can compare
    /// against a stored value to know when to rebuild bind groups.
    atlas_generation: u64,
    /// Font metrics (size, line height).
    metrics: Metrics,
    /// Real cell metrics derived from a shaped test character.
    cell_metrics: CellMetrics,
}

impl TextRenderer {
    /// Width of the atlas texture in pixels.
    fn atlas_width(slot_size: u32) -> u32 {
        slot_size * ATLAS_COLS
    }

    /// Current height of the atlas texture in pixels.
    fn atlas_height(slot_size: u32, capacity_slots: u32) -> u32 {
        let rows = capacity_slots.div_ceil(ATLAS_COLS);
        rows * slot_size
    }

    /// Construct a new `TextRenderer`, loading fonts from `config`.
    ///
    /// `scale_factor` is the window's DPI scale (e.g. 2.0 on Retina Macs).
    /// Glyph rasterization uses `font_size * scale_factor` so glyphs are
    /// sharp at the display's native resolution.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        config: &FontConfig,
        scale_factor: f32,
    ) -> Self {
        let mut font_system = FontSystem::new();

        // Font size in physical pixels — scale the point size by the display
        // factor so glyphs are rendered at native resolution.
        let px_size = config.size * scale_factor;
        let line_height = px_size * 1.3; // initial estimate; overridden by real metrics below
        let metrics = Metrics::new(px_size, line_height);

        // ── Extract real cell metrics ─────────────────────────────────────────
        //
        // Shape a space character to get the monospace advance width and the
        // true line metrics (ascent, line_height).
        let cell_metrics = {
            let mut cell_width = px_size * 0.6; // fallback
            let mut cell_height = line_height; // fallback
            let mut ascent = px_size * 0.8; // fallback

            // Shape a single space in a nested scope so the cosmic-text
            // buffer/borrow release their mutable lease on font_system
            // before we query fontdb for the resolved face name below.
            let resolved_font_id: Option<cosmic_text::fontdb::ID> = {
                let mut buffer = Buffer::new(&mut font_system, metrics);
                let mut borrow = buffer.borrow_with(&mut font_system);
                let attrs = Attrs::new().family(cosmic_text::Family::Name(&config.family));
                borrow.set_text(" ", &attrs, Shaping::Advanced, None);
                borrow.shape_until_scroll(false);

                let mut found_id = None;
                if let Some(run) = borrow.layout_runs().next() {
                    cell_height = run.line_height;
                    ascent = run.line_y - run.line_top;
                    if let Some(glyph) = run.glyphs.first() {
                        cell_width = glyph.w;
                        found_id = Some(glyph.font_id);
                    }
                }
                found_id
            };

            // Log which font cosmic-text actually resolved — the requested
            // family may not be installed, in which case it silently falls
            // back to another face.  Users see this at RUST_LOG=info.
            if let Some(id) = resolved_font_id {
                match font_system.db().face(id) {
                    Some(face) => {
                        let resolved_name =
                            face.families.first().map(|(n, _)| n.as_str()).unwrap_or("<unknown>");
                        if resolved_name.eq_ignore_ascii_case(&config.family) {
                            log::info!("font resolved: '{resolved_name}' (matches request)");
                        } else {
                            log::warn!(
                                "font '{}' not found — fell back to '{resolved_name}'",
                                config.family
                            );
                        }
                    }
                    None => {
                        log::warn!(
                            "font resolution returned id {id:?} but fontdb has no matching face"
                        );
                    }
                }
            } else {
                log::warn!("no glyph produced for test character — font loading may have failed");
            }

            CellMetrics { cell_width, cell_height, ascent }
        };

        // Slot must fit the tallest/widest glyph plus italic overhang and
        // descender margin. 1.5× the max cell dimension is a comfortable
        // headroom; next_power_of_two for texture-friendly alignment;
        // floor at 32 so tiny fonts still have a reasonable slot.
        let slot_size = compute_slot_size(cell_metrics.cell_width, cell_metrics.cell_height);

        let swash_cache = SwashCache::new();

        let capacity_slots = ATLAS_COLS * ATLAS_INITIAL_ROWS;
        let atlas_texture = Self::create_atlas_texture(device, slot_size, capacity_slots);
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut renderer = Self {
            font_system,
            swash_cache,
            atlas_texture,
            atlas_view,
            atlas_map: HashMap::new(),
            char_cache: HashMap::new(),
            atlas_next_slot: 0,
            atlas_capacity_slots: capacity_slots,
            slot_size,
            atlas_generation: 0,
            metrics,
            cell_metrics,
        };

        // Pre-rasterize the printable ASCII range on startup.
        renderer.rasterize_ascii_range(device, queue, config);

        renderer
    }

    /// Return the real cell metrics extracted from the font.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell_metrics
    }

    /// Return the current atlas generation counter.
    ///
    /// This is incremented each time the atlas texture is recreated (grows).
    /// Consumers can compare against a stored value to know when to rebuild
    /// bind groups.
    pub fn atlas_generation(&self) -> u64 {
        self.atlas_generation
    }

    // ── Atlas texture management ──────────────────────────────────────────────

    fn create_atlas_texture(device: &wgpu::Device, slot_size: u32, capacity_slots: u32) -> wgpu::Texture {
        let width = Self::atlas_width(slot_size);
        let height = Self::atlas_height(slot_size, capacity_slots);
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph_atlas"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// Allocate a new atlas slot, growing the texture if necessary.
    ///
    /// Returns the slot index.
    fn alloc_slot(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> u32 {
        if self.atlas_next_slot >= self.atlas_capacity_slots {
            // Double the capacity.
            let new_capacity = self.atlas_capacity_slots * 2;
            let new_texture = Self::create_atlas_texture(device, self.slot_size, new_capacity);
            let new_view = new_texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.atlas_texture = new_texture;
            self.atlas_view = new_view;
            self.atlas_capacity_slots = new_capacity;
            self.atlas_generation += 1;
            log::debug!(
                "Glyph atlas grown to {} slots ({} rows), generation {}",
                new_capacity,
                new_capacity / ATLAS_COLS,
                self.atlas_generation
            );
            // Re-upload all cached glyphs to the new texture.
            // In a real renderer you might keep a CPU-side copy; for now
            // we simply clear both caches and let glyphs be re-rasterized lazily.
            self.atlas_map.clear();
            self.char_cache.clear();
            self.atlas_next_slot = 0;
            let _ = queue; // queue not needed for clear
        }

        let slot = self.atlas_next_slot;
        self.atlas_next_slot += 1;
        slot
    }

    /// Convert a slot index to `(col, row)` within the atlas grid.
    fn slot_to_grid(slot: u32) -> (u32, u32) {
        (slot % ATLAS_COLS, slot / ATLAS_COLS)
    }

    /// UV rectangle for a given slot covering only the actual glyph bitmap
    /// (not the full slot).
    fn glyph_uv(slot: u32, glyph_w: u32, glyph_h: u32, slot_size: u32, capacity_slots: u32) -> [f32; 4] {
        let (col, row) = Self::slot_to_grid(slot);
        let atlas_w = Self::atlas_width(slot_size) as f32;
        let atlas_h = Self::atlas_height(slot_size, capacity_slots) as f32;
        let x0 = (col * slot_size) as f32 / atlas_w;
        let y0 = (row * slot_size) as f32 / atlas_h;
        let x1 = x0 + glyph_w as f32 / atlas_w;
        let y1 = y0 + glyph_h as f32 / atlas_h;
        [x0, y0, x1, y1]
    }

    // ── Rasterization ─────────────────────────────────────────────────────────

    /// Pre-rasterize printable ASCII glyphs (U+0020 – U+007E).
    fn rasterize_ascii_range(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        config: &FontConfig,
    ) {
        for cp in 0x20u32..=0x7E {
            let ch = char::from_u32(cp).unwrap_or(' ');
            self.rasterize_char(ch, false, false, device, queue, config);
        }
    }

    /// Rasterize `ch` with the given style flags and upload it to the atlas.
    ///
    /// Returns `None` for whitespace or characters with no glyph (e.g. control
    /// characters).
    pub fn rasterize_char(
        &mut self,
        ch: char,
        bold: bool,
        italic: bool,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        config: &FontConfig,
    ) -> Option<GlyphInfo> {
        // Whitespace has no glyph to draw.
        if ch == ' ' || ch == '\t' || ch == '\n' {
            return None;
        }

        // ── Fast-path: return cached result without shaping ──────────────
        let style_key = CharStyleKey { ch, bold, italic };
        if let Some(cached) = self.char_cache.get(&style_key) {
            return *cached;
        }

        // Build a one-character buffer to let cosmic-text shape it and give us
        // the CacheKey.
        //
        // The buffer and borrow are scoped so they release `self.font_system`
        // before we need `self` again below.
        let cache_key = {
            let mut buffer = Buffer::new(&mut self.font_system, self.metrics);
            let mut borrow = buffer.borrow_with(&mut self.font_system);

            let mut attrs = Attrs::new().family(cosmic_text::Family::Name(&config.family));
            if bold {
                attrs = attrs.weight(cosmic_text::Weight::BOLD);
            }
            if italic {
                attrs = attrs.style(cosmic_text::Style::Italic);
            }

            let text = ch.to_string();
            borrow.set_text(&text, &attrs, Shaping::Advanced, None);
            borrow.shape_until_scroll(false);

            // Find the glyph in the shaped layout.
            // `LayoutGlyph` does not directly expose a CacheKey; we call
            // `.physical()` to get a `PhysicalGlyph` which carries one.
            borrow.layout_runs().find_map(|run| {
                run.glyphs.iter().next().map(|glyph| glyph.physical((0.0, 0.0), 1.0).cache_key)
            })
        };
        let Some(cache_key) = cache_key else {
            // No glyph found for this character — cache the miss.
            self.char_cache.insert(style_key, None);
            return None;
        };

        // Return cached result if already rasterized.
        if let Some(&info) = self.atlas_map.get(&cache_key) {
            self.char_cache.insert(style_key, Some(info));
            return Some(info);
        }

        // Rasterize via swash.
        let image = self.swash_cache.get_image_uncached(&mut self.font_system, cache_key)?;

        let glyph_w = image.placement.width;
        let glyph_h = image.placement.height;

        if glyph_w == 0 || glyph_h == 0 {
            return None;
        }

        // Compute glyph placement within the cell.
        // offset_x = bearing X (pixels from cell left to glyph left edge).
        // offset_y = ascent - placement.top (pixels from cell top to glyph top).
        let offset_x = image.placement.left as f32;
        let offset_y = self.cell_metrics.ascent - image.placement.top as f32;

        // Allocate an atlas slot.
        let slot = self.alloc_slot(device, queue);
        let slot_size = self.slot_size;

        let (col, row) = Self::slot_to_grid(slot);
        let dst_x = col * slot_size;
        let dst_y = row * slot_size;

        // Upload the glyph bitmap.  The data from swash for a Mask glyph is
        // one byte per pixel (alpha).
        let upload_w = glyph_w.min(slot_size);
        let upload_h = glyph_h.min(slot_size);

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: dst_x, y: dst_y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &image.data[..(upload_w * upload_h) as usize],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(upload_w),
                rows_per_image: None,
            },
            wgpu::Extent3d { width: upload_w, height: upload_h, depth_or_array_layers: 1 },
        );

        let uv = Self::glyph_uv(slot, upload_w, upload_h, slot_size, self.atlas_capacity_slots);
        let info = GlyphInfo {
            atlas_uv: uv,
            offset_x,
            offset_y,
            glyph_width: upload_w as f32,
            glyph_height: upload_h as f32,
        };
        self.atlas_map.insert(cache_key, info);
        self.char_cache.insert(style_key, Some(info));
        Some(info)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::compute_slot_size;

    #[test]
    fn slot_size_basic() {
        // A typical 8×16 cell (narrow/tall monospace): slot must be at least 16.
        assert!(compute_slot_size(8.0, 16.0) >= 16);
    }

    #[test]
    fn slot_size_floor_at_32() {
        // Even for a small cell, the slot size must be at least 32.
        assert!(compute_slot_size(8.0, 16.0) >= 32);
    }

    #[test]
    fn slot_size_large_font_fits() {
        // 72pt at 2× = 100px cell_height → slot must accommodate 1.5×.
        // compute_slot_size(50.0, 100.0): max=100, padded=150 → next_pow2=256.
        assert!(compute_slot_size(50.0, 100.0) >= 150);
    }

    #[test]
    fn slot_size_is_power_of_two() {
        for &(w, h) in &[(8.0f32, 16.0f32), (10.0, 20.0), (50.0, 100.0), (1.0, 1.0)] {
            let s = compute_slot_size(w, h);
            assert_eq!(s & (s - 1), 0, "slot_size({w}, {h}) = {s} is not a power of two");
        }
    }
}
