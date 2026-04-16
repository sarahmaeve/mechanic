// Text rendering: glyph rasterization via cosmic-text / swash and a GPU glyph
// atlas that caches the results.

use std::collections::HashMap;

use cosmic_text::{Attrs, Buffer, CacheKey, FontSystem, Metrics, Shaping, SwashCache};
use mechanic_config::font::FontConfig;

// ── Atlas constants ───────────────────────────────────────────────────────────

/// Width / height of each glyph slot in the atlas texture (pixels).
const ATLAS_SLOT_SIZE: u32 = 64;
/// Number of slots per row in the atlas.
const ATLAS_COLS: u32 = 16;
/// Initial number of rows in the atlas (grows on demand).
const ATLAS_INITIAL_ROWS: u32 = 8;

// ── Public types ──────────────────────────────────────────────────────────────

/// Location and metrics of a rasterized glyph in the GPU atlas.
#[derive(Debug, Clone, Copy)]
pub struct GlyphInfo {
    /// UV rectangle in the atlas texture: `(u_min, v_min, u_max, v_max)`.
    pub atlas_uv: [f32; 4],
    /// Horizontal offset in pixels from the cell origin to the glyph's left
    /// edge.
    pub bearing_x: i32,
    /// Vertical offset in pixels from the cell baseline to the glyph's top
    /// edge (positive = up).
    pub bearing_y: i32,
    /// Width of the rasterized bitmap in pixels.
    pub width: u32,
    /// Height of the rasterized bitmap in pixels.
    pub height: u32,
}

// ── TextRenderer ──────────────────────────────────────────────────────────────

/// Manages font shaping and GPU glyph atlas upload.
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// The atlas texture lives on the GPU.
    pub atlas_texture: wgpu::Texture,
    /// A view into `atlas_texture`, kept alive alongside the texture.
    pub atlas_view: wgpu::TextureView,
    /// Map from cosmic-text `CacheKey` to atlas slot index.
    atlas_map: HashMap<CacheKey, u32>,
    /// Next free slot index.
    atlas_next_slot: u32,
    /// Total number of slots currently allocated.
    atlas_capacity_slots: u32,
    /// Font metrics (size, line height).
    metrics: Metrics,
}

impl TextRenderer {
    /// Width of the atlas texture in pixels.
    pub fn atlas_width() -> u32 {
        ATLAS_SLOT_SIZE * ATLAS_COLS
    }

    /// Current height of the atlas texture in pixels.
    fn atlas_height(capacity_slots: u32) -> u32 {
        let rows = capacity_slots.div_ceil(ATLAS_COLS);
        rows * ATLAS_SLOT_SIZE
    }

    /// Construct a new `TextRenderer`, loading fonts from `config`.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, config: &FontConfig) -> Self {
        let font_system = FontSystem::new();

        // Set the primary font family preference.  cosmic-text's FontSystem
        // discovers system fonts automatically; we steer it with Attrs.
        let _ = &config.family; // config family will be used below in rasterize_ascii_range

        let swash_cache = SwashCache::new();

        // Font size in points → metrics.
        let px_size = config.size;
        let line_height = px_size * 1.3; // a reasonable default
        let metrics = Metrics::new(px_size, line_height);

        let capacity_slots = ATLAS_COLS * ATLAS_INITIAL_ROWS;
        let atlas_texture = Self::create_atlas_texture(device, capacity_slots);
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut renderer = Self {
            font_system,
            swash_cache,
            atlas_texture,
            atlas_view,
            atlas_map: HashMap::new(),
            atlas_next_slot: 0,
            atlas_capacity_slots: capacity_slots,
            metrics,
        };

        // Pre-rasterize the printable ASCII range on startup.
        renderer.rasterize_ascii_range(device, queue, config);

        renderer
    }

    // ── Atlas texture management ──────────────────────────────────────────────

    fn create_atlas_texture(device: &wgpu::Device, capacity_slots: u32) -> wgpu::Texture {
        let width = Self::atlas_width();
        let height = Self::atlas_height(capacity_slots);
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
            let new_texture = Self::create_atlas_texture(device, new_capacity);
            let new_view = new_texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.atlas_texture = new_texture;
            self.atlas_view = new_view;
            self.atlas_capacity_slots = new_capacity;
            log::debug!(
                "Glyph atlas grown to {} slots ({} rows)",
                new_capacity,
                new_capacity / ATLAS_COLS
            );
            // Re-upload all cached glyphs to the new texture.
            // In a real renderer you might keep a CPU-side copy; for Phase 1
            // we simply clear the cache and let glyphs be re-rasterized lazily.
            self.atlas_map.clear();
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

    /// UV rectangle for a given slot.
    fn slot_uv(slot: u32, capacity_slots: u32) -> [f32; 4] {
        let (col, row) = Self::slot_to_grid(slot);
        let atlas_w = Self::atlas_width() as f32;
        let atlas_h = Self::atlas_height(capacity_slots) as f32;
        let x0 = (col * ATLAS_SLOT_SIZE) as f32 / atlas_w;
        let y0 = (row * ATLAS_SLOT_SIZE) as f32 / atlas_h;
        let x1 = x0 + ATLAS_SLOT_SIZE as f32 / atlas_w;
        let y1 = y0 + ATLAS_SLOT_SIZE as f32 / atlas_h;
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
        let cache_key = cache_key?;

        // Return cached result if already rasterized.
        if let Some(&slot) = self.atlas_map.get(&cache_key) {
            let uv = Self::slot_uv(slot, self.atlas_capacity_slots);
            return Some(GlyphInfo {
                atlas_uv: uv,
                bearing_x: 0,
                bearing_y: 0,
                width: ATLAS_SLOT_SIZE,
                height: ATLAS_SLOT_SIZE,
            });
        }

        // Rasterize via swash.
        let image = self.swash_cache.get_image_uncached(&mut self.font_system, cache_key)?;

        let glyph_w = image.placement.width;
        let glyph_h = image.placement.height;

        if glyph_w == 0 || glyph_h == 0 {
            return None;
        }

        // Allocate an atlas slot.
        let slot = self.alloc_slot(device, queue);
        self.atlas_map.insert(cache_key, slot);

        let (col, row) = Self::slot_to_grid(slot);
        let dst_x = col * ATLAS_SLOT_SIZE;
        let dst_y = row * ATLAS_SLOT_SIZE;

        // Upload the glyph bitmap.  The data from swash for a Mask glyph is
        // one byte per pixel (alpha).
        let upload_w = glyph_w.min(ATLAS_SLOT_SIZE);
        let upload_h = glyph_h.min(ATLAS_SLOT_SIZE);

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

        let uv = Self::slot_uv(slot, self.atlas_capacity_slots);
        Some(GlyphInfo {
            atlas_uv: uv,
            bearing_x: image.placement.left,
            bearing_y: image.placement.top,
            width: upload_w,
            height: upload_h,
        })
    }
}
