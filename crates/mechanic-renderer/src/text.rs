use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher, RandomState};
use std::sync::{Arc, Mutex};

use cosmic_text::{
    Attrs, Buffer, CacheKey, Fallback, FontSystem, LayoutGlyph, Metrics, PlatformFallback, Shaping,
    SwashCache, SwashContent, Wrap,
};
use mechanic_config::font::FontConfig;

use crate::grid::{CellFlags, RenderCell};

/// Number of slots per row in the atlas.
const ATLAS_COLS: u32 = 16;
/// Initial number of rows in the atlas (grows on demand).
const ATLAS_INITIAL_ROWS: u32 = 8;
const GLYPH_GUTTER: u32 = 1;
const SHAPE_CACHE_ROWS: usize = 256;
const SHAPE_CACHE_BYTES: usize = 1024 * 1024;
static INTERNED_FAMILIES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

struct ConfiguredFallback {
    common: Vec<&'static str>,
    scripts: HashMap<unicode_script::Script, Vec<&'static str>>,
}

impl Fallback for ConfiguredFallback {
    fn common_fallback(&self) -> &[&'static str] {
        &self.common
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        PlatformFallback.forbidden_fallback()
    }

    fn script_fallback(&self, script: unicode_script::Script, locale: &str) -> &[&'static str] {
        self.scripts
            .get(&script)
            .map_or_else(|| PlatformFallback.script_fallback(script, locale), Vec::as_slice)
    }
}

fn configured_font_system(config: &FontConfig) -> FontSystem {
    let system = FontSystem::new();
    let mut fallback = configured_fallback(config, &system);
    let available: HashSet<_> = system
        .db()
        .faces()
        .flat_map(|face| face.families.iter().map(|(name, _)| name.as_str()))
        .collect();
    fallback.retain_available(&available);
    FontSystem::new_with_locale_and_db_and_fallback(
        system.locale().to_owned(),
        system.db().clone(),
        fallback,
    )
}

impl ConfiguredFallback {
    fn retain_available(&mut self, available: &HashSet<&str>) {
        // Match Cosmic's exact family-name comparison and preserve preference order.
        for names in std::iter::once(&mut self.common).chain(self.scripts.values_mut()) {
            let mut seen = HashSet::new();
            names.retain(|name| available.contains(name) && seen.insert(*name));
        }
    }
}

fn configured_fallback(config: &FontConfig, system: &FontSystem) -> ConfiguredFallback {
    let mut common = Vec::new();
    let mut interned = INTERNED_FAMILIES.lock().unwrap_or_else(|error| error.into_inner());
    for family in config.fallback_families.iter().take(32) {
        if family.len() > 256 {
            log::warn!("ignoring font fallback name longer than 256 bytes");
            continue;
        }
        let name = match interned.iter().find(|name| **name == family) {
            Some(name) => *name,
            None if interned.len() < 128 => {
                // Cosmic's Fallback interface requires static family names.
                // Intern once across renderer recreation, with a fixed bound.
                let name: &'static str = Box::leak(family.clone().into_boxed_str());
                interned.push(name);
                name
            }
            None => {
                log::warn!("font fallback name intern table is full");
                continue;
            }
        };
        common.push(name);
    }
    drop(interned);
    let mut scripts = HashMap::new();
    for script in [
        unicode_script::Script::Latin,
        unicode_script::Script::Cyrillic,
        unicode_script::Script::Arabic,
        unicode_script::Script::Han,
        unicode_script::Script::Hiragana,
        unicode_script::Script::Katakana,
        unicode_script::Script::Common,
        unicode_script::Script::Inherited,
    ] {
        let mut names = common.clone();
        names.extend_from_slice(PlatformFallback.script_fallback(script, system.locale()));
        scripts.insert(script, names);
    }
    common.extend_from_slice(PlatformFallback.common_fallback());
    ConfiguredFallback { common, scripts }
}

/// Compute the atlas slot size from the cell dimensions.
pub fn compute_slot_size(cell_width: f32, cell_height: f32) -> u32 {
    let max_dim = cell_width.max(cell_height);
    let padded = (max_dim * 1.5).ceil() as u32;
    padded.checked_next_power_of_two().unwrap_or(u32::MAX).max(32)
}

/// Shaped font metrics in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    /// Advance width of a monospace cell in physical pixels (from the space glyph's `x_advance`).
    pub cell_width: f32,
    /// Line height in physical pixels (from `Metrics::line_height`).
    pub cell_height: f32,
    /// Distance from cell top to baseline, in physical pixels.
    pub ascent: f32,
}

/// Location and metrics of a rasterized glyph in the GPU atlas.
#[derive(Debug, Clone, Copy)]
pub struct GlyphInfo {
    /// UV rectangle in the atlas texture covering *only* the glyph bitmap: `(u_min, v_min, u_max, v_max)`.
    pub atlas_uv: [f32; 4],
    /// Horizontal offset in pixels from the cell left edge to the glyph's left edge (bearing X).
    pub offset_x: f32,
    /// Vertical bearing from the shaped glyph origin (`-placement.top`).
    pub offset_y: f32,
    /// Width of the rasterized bitmap in pixels.
    pub glyph_width: f32,
    /// Height of the rasterized bitmap in pixels.
    pub glyph_height: f32,
    /// Scale already applied while rasterizing the outline; geometry should
    /// only apply the remaining requested-scale / raster-scale correction.
    pub raster_scale_x: f32,
}

/// Raster cache identity includes its horizontal outline transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub base: CacheKey,
    horizontal_scale: u32,
}

impl GlyphKey {
    fn new(base: CacheKey, scale_x: f32) -> Self {
        Self { base, horizontal_scale: (scale_x * 1024.0).round().max(1.0) as u32 }
    }

    fn scale_x(self) -> f32 {
        self.horizontal_scale as f32 / 1024.0
    }
}

impl std::ops::Deref for GlyphKey {
    type Target = CacheKey;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

/// Character and style key for cached glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CharStyleKey {
    ch: char,
    bold: bool,
    italic: bool,
}

/// One glyph from a shaped terminal row. A cluster can contain several glyphs.
#[derive(Debug, Clone)]
pub struct ShapedGlyph {
    pub cache_key: GlyphKey,
    pub source_col: usize,
    /// Physical origin in pixels from the visual row's left/top edges.
    pub x: f32,
    pub y: f32,
    /// Horizontal scale shared by a cluster or a complete RTL word.
    pub scale_x: f32,
    /// Visual cells sharing this geometry group, excluding glyph overhangs.
    pub cluster_start: usize,
    pub cluster_end: usize,
}

/// Shaping and terminal hit-test data share the same visual cell permutation.
#[derive(Debug)]
pub struct ShapedRow {
    pub glyphs: Vec<ShapedGlyph>,
    /// `visual_cols[logical_col]` is the displayed terminal column.
    pub visual_cols: Vec<usize>,
    /// Logical cells in right-to-left clusters invert their selection side.
    pub rtl: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RowCellKey {
    character: char,
    marks: String,
    flags: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RowKey(Vec<RowCellKey>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ParagraphKey {
    rows: Vec<RowKey>,
    prefix: String,
    suffix: String,
}

const SHAPING_FLAGS: CellFlags = CellFlags::BOLD
    .union(CellFlags::ITALIC)
    .union(CellFlags::WIDE_CHAR)
    .union(CellFlags::WIDE_CHAR_SPACER)
    .union(CellFlags::LEADING_WIDE_CHAR_SPACER)
    .union(CellFlags::HIDDEN);

// Borrowed keys must hash exactly like their owned counterparts.
struct RowRef<'a>(&'a [RenderCell]);

impl RowRef<'_> {
    fn matches(&self, key: &RowKey) -> bool {
        self.0.len() == key.0.len()
            && self.0.iter().zip(&key.0).all(|(cell, key)| {
                cell.character == key.character
                    && cell.zerowidth == key.marks
                    && (cell.flags & SHAPING_FLAGS).bits() == key.flags
            })
    }
}

impl Hash for RowRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        for cell in self.0 {
            cell.character.hash(state);
            cell.zerowidth.hash(state);
            (cell.flags & SHAPING_FLAGS).bits().hash(state);
        }
    }
}

impl hashbrown::Equivalent<Arc<RowKey>> for RowRef<'_> {
    fn equivalent(&self, key: &Arc<RowKey>) -> bool {
        self.matches(key)
    }
}

struct ParagraphRef<'a> {
    rows: &'a [&'a [RenderCell]],
    prefix: &'a str,
    suffix: &'a str,
}

impl Hash for ParagraphRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.rows.len().hash(state);
        for row in self.rows {
            RowRef(row).hash(state);
        }
        self.prefix.hash(state);
        self.suffix.hash(state);
    }
}

impl hashbrown::Equivalent<Arc<ParagraphKey>> for ParagraphRef<'_> {
    fn equivalent(&self, key: &Arc<ParagraphKey>) -> bool {
        self.prefix == key.prefix
            && self.suffix == key.suffix
            && self.rows.len() == key.rows.len()
            && self.rows.iter().zip(&key.rows).all(|(cells, key)| RowRef(cells).matches(key))
    }
}

impl RowKey {
    fn new(cells: &[RenderCell]) -> Self {
        Self(
            cells
                .iter()
                .map(|cell| RowCellKey {
                    character: cell.character,
                    marks: cell.zerowidth.clone(),
                    flags: (cell.flags & SHAPING_FLAGS).bits(),
                })
                .collect(),
        )
    }

    fn bytes(&self) -> usize {
        self.0.len() * std::mem::size_of::<RowCellKey>()
            + self.0.iter().map(|cell| cell.marks.len()).sum::<usize>()
    }
}

/// Failure is explicit: atlas uploads never crop a bitmap or exceed device bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtlasError {
    GlyphTooLarge { width: u32, height: u32, limit: u32 },
    FrameTooLarge { glyphs: usize, capacity: usize },
    InvalidBitmap,
}

impl std::fmt::Display for AtlasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "glyph atlas preparation failed: {self:?}")
    }
}

impl std::error::Error for AtlasError {}

struct RasterGlyph {
    key: GlyphKey,
    width: u32,
    height: u32,
    left: i32,
    top: i32,
    coverage: Vec<u8>,
    raster_scale_x: f32,
}

#[derive(Debug, Clone, Copy)]
struct AtlasLayout {
    columns: u32,
    rows: u32,
    slot_size: u32,
}

impl AtlasLayout {
    fn new(slot_size: u32, glyphs: usize, limit: u32) -> Result<Self, AtlasError> {
        let axis = limit / slot_size.max(1);
        if axis == 0 {
            return Err(AtlasError::GlyphTooLarge { width: slot_size, height: slot_size, limit });
        }
        let capacity = u64::from(axis) * u64::from(axis);
        if glyphs as u64 > capacity {
            return Err(AtlasError::FrameTooLarge { glyphs, capacity: capacity as usize });
        }
        let glyphs = glyphs.max(1) as u32;
        let columns = ATLAS_COLS.min(axis).max(glyphs.div_ceil(axis));
        let rows = glyphs.div_ceil(columns);
        Ok(Self { columns, rows, slot_size })
    }

    fn capacity(self) -> u32 {
        self.columns * self.rows
    }

    fn uv(self, slot: u32, width: u32, height: u32) -> [f32; 4] {
        let x = (slot % self.columns) * self.slot_size + GLYPH_GUTTER;
        let y = (slot / self.columns) * self.slot_size + GLYPH_GUTTER;
        let atlas_width = (self.columns * self.slot_size) as f32;
        let atlas_height = (self.rows * self.slot_size) as f32;
        [
            x as f32 / atlas_width,
            y as f32 / atlas_height,
            (x + width) as f32 / atlas_width,
            (y + height) as f32 / atlas_height,
        ]
    }
}

/// Manages font shaping and GPU glyph atlas upload.
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    raster_context: swash::scale::ScaleContext,
    /// The atlas texture lives on the GPU.
    pub atlas_texture: wgpu::Texture,
    /// A view into `atlas_texture`, kept alive alongside the texture.
    pub atlas_view: wgpu::TextureView,
    /// Map from cosmic-text `CacheKey` to cached `GlyphInfo`.
    atlas_map: HashMap<GlyphKey, GlyphInfo>,
    empty_glyphs: HashSet<GlyphKey>,
    ascii_shapes: HashMap<CharStyleKey, Vec<LayoutGlyph>>,
    shape_cache: hashbrown::HashMap<Arc<RowKey>, Arc<ShapedRow>, RandomState>,
    shape_order: VecDeque<(Arc<RowKey>, usize)>,
    shape_cache_bytes: usize,
    paragraph_cache: hashbrown::HashMap<Arc<ParagraphKey>, Vec<Arc<ShapedRow>>, RandomState>,
    paragraph_order: VecDeque<(Arc<ParagraphKey>, usize)>,
    paragraph_cache_bytes: usize,
    /// Strong references keep identity comparisons valid; retain only the
    /// currently prepared frame, independent of the bounded shaping caches.
    prepared_rows: Vec<Arc<ShapedRow>>,
    prepared_generation: Option<u64>,
    shape_buffer: Buffer,
    /// Next free slot index.
    atlas_next_slot: u32,
    /// Total number of slots currently allocated.
    atlas_capacity_slots: u32,
    /// Glyph slot width and height in pixels.
    slot_size: u32,
    atlas_layout: AtlasLayout,
    /// Incremented on texture replacement; invalidates cached bind groups and UVs.
    atlas_generation: u64,
    /// Real cell metrics derived from a shaped test character.
    cell_metrics: CellMetrics,
}

impl TextRenderer {
    /// Construct a new `TextRenderer`, loading fonts from `config`.
    pub fn new(
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        config: &FontConfig,
        scale_factor: f32,
    ) -> Self {
        let mut font_system = configured_font_system(config);

        let px_size = config.size * scale_factor;
        let line_height = px_size * 1.3; // initial estimate; overridden by real metrics below
        let metrics = Metrics::new(px_size, line_height);

        let cell_metrics = {
            let mut cell_width = px_size * 0.6; // fallback
            let mut cell_height = line_height; // fallback
            let mut ascent = px_size * 0.8; // fallback

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

        let limit = device.limits().max_texture_dimension_2d;
        let slot_size =
            compute_slot_size(cell_metrics.cell_width, cell_metrics.cell_height).min(limit);

        let swash_cache = SwashCache::new();

        let max_slots = (limit / slot_size).pow(2);
        let atlas_layout = AtlasLayout::new(
            slot_size,
            (ATLAS_COLS * ATLAS_INITIAL_ROWS).min(max_slots) as usize,
            limit,
        )
        .expect("initial atlas fits device bounds");
        let capacity_slots = atlas_layout.capacity();
        let atlas_texture = Self::create_atlas_texture(device, atlas_layout);
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let shape_buffer = Buffer::new(&mut font_system, metrics);
        Self {
            font_system,
            swash_cache,
            raster_context: swash::scale::ScaleContext::new(),
            atlas_texture,
            atlas_view,
            atlas_map: HashMap::new(),
            empty_glyphs: HashSet::new(),
            ascii_shapes: HashMap::new(),
            shape_cache: hashbrown::HashMap::default(),
            shape_order: VecDeque::new(),
            shape_cache_bytes: 0,
            paragraph_cache: hashbrown::HashMap::default(),
            paragraph_order: VecDeque::new(),
            paragraph_cache_bytes: 0,
            prepared_rows: Vec::new(),
            prepared_generation: None,
            shape_buffer,
            atlas_next_slot: 0,
            atlas_capacity_slots: capacity_slots,
            slot_size,
            atlas_layout,
            atlas_generation: 0,
            cell_metrics,
        }
    }

    /// Return the real cell metrics extracted from the font.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell_metrics
    }

    /// Return the current atlas generation counter.
    pub fn atlas_generation(&self) -> u64 {
        self.atlas_generation
    }

    fn create_atlas_texture(device: &wgpu::Device, layout: AtlasLayout) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph_atlas"),
            size: wgpu::Extent3d {
                width: layout.columns * layout.slot_size,
                height: layout.rows * layout.slot_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// Shape complete rows with cell spans independent of proportional fallback
    /// advances. Cached shaping is independent of colors, selection and atlas UVs.
    pub fn shape_row(&mut self, cells: &[RenderCell], config: &FontConfig) -> Arc<ShapedRow> {
        if let Some(row) = self.shape_cache.get(&RowRef(cells)) {
            return Arc::clone(row);
        }
        let key = Arc::new(RowKey::new(cells));
        let row = Arc::new(
            if cells.iter().all(|cell| {
                cell.character.is_ascii()
                    && !cell.character.is_control()
                    && cell.zerowidth.is_empty()
                    && !cell.flags.intersects(
                        CellFlags::WIDE_CHAR
                            | CellFlags::WIDE_CHAR_SPACER
                            | CellFlags::LEADING_WIDE_CHAR_SPACER,
                    )
            }) {
                self.shape_ascii(cells, config)
            } else {
                shape_contextual_row(
                    &mut self.font_system,
                    &mut self.shape_buffer,
                    cells,
                    config,
                    self.cell_metrics,
                )
            },
        );
        let bytes = key.bytes()
            + row.glyphs.len() * std::mem::size_of::<ShapedGlyph>()
            + row.visual_cols.len() * (std::mem::size_of::<usize>() + 1);
        if bytes <= SHAPE_CACHE_BYTES {
            while self.shape_cache.len() >= SHAPE_CACHE_ROWS
                || self.shape_cache_bytes + bytes > SHAPE_CACHE_BYTES
            {
                if let Some((old, old_bytes)) = self.shape_order.pop_front() {
                    self.shape_cache.remove(&old);
                    self.shape_cache_bytes -= old_bytes;
                } else {
                    break;
                }
            }
            self.shape_order.push_back((key.clone(), bytes));
            self.shape_cache.insert(key, Arc::clone(&row));
            self.shape_cache_bytes += bytes;
        }
        row
    }

    /// Soft-wrapped rows share bidi context and Arabic joining forms while
    /// ligatures remain within separately presented terminal rows.
    pub fn shape_grid(
        &mut self,
        grid: &crate::grid::RenderGrid,
        config: &FontConfig,
    ) -> Vec<Arc<ShapedRow>> {
        let mut output = Vec::with_capacity(grid.rows);
        let mut first = 0;
        while first < grid.rows {
            let mut end = first + 1;
            while end < grid.rows && grid.wrapped.get(end - 1).copied().unwrap_or(false) {
                end += 1;
            }
            let prefix = if first == 0 { grid.bidi_prefix.as_str() } else { "" };
            let suffix = if end == grid.rows { grid.bidi_suffix.as_str() } else { "" };
            if prefix.is_empty()
                && suffix.is_empty()
                && (end == first + 1
                    || grid.cells[first * grid.cols..end * grid.cols]
                        .iter()
                        .all(|cell| cell.character.is_ascii() && cell.zerowidth.is_empty()))
            {
                output.extend((first..end).map(|row| {
                    self.shape_row(&grid.cells[row * grid.cols..(row + 1) * grid.cols], config)
                }));
            } else {
                let rows: Vec<_> = (first..end)
                    .map(|row| &grid.cells[row * grid.cols..(row + 1) * grid.cols])
                    .collect();
                if let Some(cached) =
                    self.paragraph_cache.get(&ParagraphRef { rows: &rows, prefix, suffix })
                {
                    output.extend(cached.iter().cloned());
                } else {
                    let key = Arc::new(ParagraphKey {
                        rows: rows.iter().map(|row| RowKey::new(row)).collect(),
                        prefix: prefix.to_owned(),
                        suffix: suffix.to_owned(),
                    });
                    let shaped: Vec<_> = shape_contextual_paragraph(
                        &mut self.font_system,
                        &mut self.shape_buffer,
                        &rows,
                        config,
                        self.cell_metrics,
                        prefix,
                        suffix,
                    )
                    .into_iter()
                    .map(Arc::new)
                    .collect();
                    let bytes = key.rows.iter().map(RowKey::bytes).sum::<usize>()
                        + prefix.len()
                        + suffix.len()
                        + shaped
                            .iter()
                            .map(|row| {
                                row.glyphs.len() * std::mem::size_of::<ShapedGlyph>()
                                    + row.visual_cols.len() * (std::mem::size_of::<usize>() + 1)
                            })
                            .sum::<usize>();
                    if bytes <= SHAPE_CACHE_BYTES {
                        while self.paragraph_cache.len() >= 64
                            || self.paragraph_cache_bytes + bytes > SHAPE_CACHE_BYTES
                        {
                            if let Some((old, old_bytes)) = self.paragraph_order.pop_front() {
                                self.paragraph_cache.remove(&old);
                                self.paragraph_cache_bytes -= old_bytes;
                            } else {
                                break;
                            }
                        }
                        self.paragraph_order.push_back((key.clone(), bytes));
                        self.paragraph_cache.insert(key, shaped.clone());
                        self.paragraph_cache_bytes += bytes;
                    }
                    output.extend(shaped);
                }
            }
            first = end;
        }
        output
    }

    fn shape_ascii(&mut self, cells: &[RenderCell], config: &FontConfig) -> ShapedRow {
        let mut shaped = ShapedRow {
            glyphs: Vec::with_capacity(cells.len()),
            visual_cols: (0..cells.len()).collect(),
            rtl: vec![false; cells.len()],
        };
        for (col, cell) in cells.iter().enumerate() {
            if cell.character == ' ' || cell.flags.contains(CellFlags::HIDDEN) {
                continue;
            }
            let key = CharStyleKey {
                ch: cell.character,
                bold: cell.flags.contains(CellFlags::BOLD),
                italic: cell.flags.contains(CellFlags::ITALIC),
            };
            let glyphs = self.ascii_shapes.entry(key).or_insert_with(|| {
                let text = cell.character.to_string();
                let attrs = cell_attrs(cell, config);
                let mut buffer = self.shape_buffer.borrow_with(&mut self.font_system);
                buffer.set_wrap(Wrap::None);
                buffer.set_text(&text, &attrs, Shaping::Advanced, None);
                buffer.shape_until_scroll(false);
                buffer.layout_runs().flat_map(|run| run.glyphs.to_vec()).collect()
            });
            for glyph in glyphs.iter() {
                let physical = glyph.physical(
                    (col as f32 * self.cell_metrics.cell_width, self.cell_metrics.ascent),
                    1.0,
                );
                shaped.glyphs.push(ShapedGlyph {
                    cache_key: GlyphKey::new(physical.cache_key, 1.0),
                    source_col: col,
                    x: physical.x as f32,
                    y: physical.y as f32,
                    scale_x: 1.0,
                    cluster_start: col,
                    cluster_end: col + 1,
                });
            }
        }
        shaped
    }

    fn raster_glyph(&mut self, key: GlyphKey) -> Result<Option<RasterGlyph>, AtlasError> {
        let scale_x = key.scale_x();
        let transformed = if key.horizontal_scale != 1024 {
            transformed_swash_image(&mut self.font_system, &mut self.raster_context, key)
        } else {
            None
        };
        let (image, raster_scale_x) = if let Some(image) = transformed {
            (Some(image), scale_x)
        } else {
            // Bitmap-only fonts have no outline to transform. Preserve their
            // correct coverage and let the remaining geometry scale handle it.
            (self.swash_cache.get_image_uncached(&mut self.font_system, key.base), 1.0)
        };
        let Some(image) = image else {
            return Ok(None);
        };
        let placement = image.placement;
        if placement.width == 0 || placement.height == 0 {
            return Ok(None);
        }
        let coverage =
            bitmap_coverage(image.content, placement.width, placement.height, image.data)?;
        Ok(Some(RasterGlyph {
            key,
            width: placement.width,
            height: placement.height,
            left: placement.left,
            top: placement.top,
            coverage,
            raster_scale_x,
        }))
    }

    /// Reserve/upload every frame glyph before immutable instance-building
    /// lookups. Rebuilding discards older rows only after all images validate.
    pub fn prepare_glyphs<I>(
        &mut self,
        keys: I,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), AtlasError>
    where
        I: IntoIterator<Item = GlyphKey>,
    {
        let keys: HashSet<_> = keys.into_iter().collect();
        let mut images = Vec::new();
        let mut empty = Vec::new();
        for &key in &keys {
            if self.atlas_map.contains_key(&key) || self.empty_glyphs.contains(&key) {
                continue;
            }
            match self.raster_glyph(key)? {
                Some(image) => images.push(image),
                None => empty.push(key),
            }
        }
        let max_width = images
            .iter()
            .map(|image| image.width)
            .chain(
                keys.iter()
                    .filter_map(|key| self.atlas_map.get(key).map(|info| info.glyph_width as u32)),
            )
            .max()
            .unwrap_or(1);
        let max_height = images
            .iter()
            .map(|image| image.height)
            .chain(
                keys.iter()
                    .filter_map(|key| self.atlas_map.get(key).map(|info| info.glyph_height as u32)),
            )
            .max()
            .unwrap_or(1);
        let limit = device.limits().max_texture_dimension_2d;
        let padded = max_width.max(max_height).saturating_add(GLYPH_GUTTER * 2);
        if padded > limit {
            return Err(AtlasError::GlyphTooLarge { width: max_width, height: max_height, limit });
        }
        let needed_size = padded.checked_next_power_of_two().unwrap_or(limit);
        let slot_size = self.slot_size.max(needed_size.min(limit));
        let rebuild = slot_size != self.slot_size
            || self.atlas_next_slot as usize + images.len() > self.atlas_capacity_slots as usize;
        if rebuild {
            let active_count =
                images.len() + keys.iter().filter(|key| self.atlas_map.contains_key(key)).count();
            let axis = limit / slot_size;
            let max_capacity = (u64::from(axis) * u64::from(axis)) as usize;
            let desired =
                (self.atlas_capacity_slots as usize * 2).max(active_count).min(max_capacity);
            // Validate the active frame independently of our growth preference.
            if active_count > max_capacity {
                return Err(AtlasError::FrameTooLarge {
                    glyphs: active_count,
                    capacity: max_capacity,
                });
            }
            let layout = AtlasLayout::new(slot_size, desired, limit)?;
            for &key in &keys {
                if self.atlas_map.contains_key(&key) {
                    if let Some(image) = self.raster_glyph(key)? {
                        images.push(image);
                    } else {
                        empty.push(key);
                    }
                }
            }
            let texture = Self::create_atlas_texture(device, layout);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.atlas_texture = texture;
            self.atlas_view = view;
            self.atlas_map.clear();
            self.empty_glyphs.retain(|key| keys.contains(key));
            self.atlas_next_slot = 0;
            self.atlas_capacity_slots = layout.capacity();
            self.slot_size = slot_size;
            self.atlas_layout = layout;
            self.atlas_generation += 1;
        }
        for image in images {
            let slot = self.atlas_next_slot;
            self.atlas_next_slot += 1;
            let x = (slot % self.atlas_layout.columns) * self.slot_size + GLYPH_GUTTER;
            let y = (slot / self.atlas_layout.columns) * self.slot_size + GLYPH_GUTTER;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &image.coverage,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(image.width),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: image.width,
                    height: image.height,
                    depth_or_array_layers: 1,
                },
            );
            self.atlas_map.insert(
                image.key,
                GlyphInfo {
                    atlas_uv: self.atlas_layout.uv(slot, image.width, image.height),
                    offset_x: image.left as f32,
                    offset_y: -(image.top as f32),
                    glyph_width: image.width as f32,
                    glyph_height: image.height as f32,
                    raster_scale_x: image.raster_scale_x,
                },
            );
        }
        if self.empty_glyphs.len() + empty.len() > 1024 {
            self.empty_glyphs.clear();
        }
        let remaining = 1024 - self.empty_glyphs.len();
        self.empty_glyphs.extend(empty.into_iter().take(remaining));
        Ok(())
    }

    /// Unchanged row identities already have valid UVs at this generation.
    /// Changed rows need only residency checks until a missing glyph requires
    /// the complete frame preflight that makes atlas growth atomic.
    pub fn prepare_frame(
        &mut self,
        rows: &[Arc<ShapedRow>],
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), AtlasError> {
        let all_resident = self.prepared_generation == Some(self.atlas_generation)
            && rows.iter().enumerate().all(|(index, row)| {
                self.prepared_rows.get(index).is_some_and(|old| Arc::ptr_eq(old, row))
                    || row.glyphs.iter().all(|glyph| {
                        self.atlas_map.contains_key(&glyph.cache_key)
                            || self.empty_glyphs.contains(&glyph.cache_key)
                    })
            });
        if !all_resident {
            self.prepare_glyphs(
                rows.iter().flat_map(|row| row.glyphs.iter().map(|glyph| glyph.cache_key)),
                device,
                queue,
            )?;
        }
        self.prepared_rows.clear();
        self.prepared_rows.extend(rows.iter().cloned());
        self.prepared_generation = Some(self.atlas_generation);
        Ok(())
    }

    /// Lookup only: it cannot grow the atlas or invalidate another glyph's UVs.
    pub fn glyph_info(&self, key: GlyphKey) -> Option<GlyphInfo> {
        self.atlas_map.get(&key).copied()
    }
}

fn cell_attrs<'a>(cell: &RenderCell, config: &'a FontConfig) -> Attrs<'a> {
    let mut attrs = Attrs::new().family(cosmic_text::Family::Name(&config.family));
    if cell.flags.contains(CellFlags::BOLD) {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
    if cell.flags.contains(CellFlags::ITALIC) {
        attrs = attrs.style(cosmic_text::Style::Italic);
    }
    attrs
}

struct CellRange {
    start: usize,
    end: usize,
    col: usize,
    span: usize,
    row: usize,
}

struct GlyphCluster {
    first: usize,
    end: usize,
    min_x: f32,
    glyphs: Vec<LayoutGlyph>,
}

fn joining_type(character: char) -> swash::text::JoiningType {
    use swash::text::Codepoint as _;
    character.joining_type()
}

fn joins_across(
    left: Option<swash::text::JoiningType>,
    right: Option<swash::text::JoiningType>,
) -> bool {
    use swash::text::JoiningType::{D, L, R};
    matches!(left, Some(D | L)) && matches!(right, Some(D | R))
}

fn first_joining_type(text: &str) -> Option<swash::text::JoiningType> {
    text.chars().map(joining_type).find(|kind| *kind != swash::text::JoiningType::T)
}

fn cell_first_joining_type(cell: &RenderCell) -> Option<swash::text::JoiningType> {
    if cell.flags.intersects(
        CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER,
    ) {
        return Some(swash::text::JoiningType::U);
    }
    std::iter::once(cell.character)
        .chain(cell.zerowidth.chars())
        .map(joining_type)
        .find(|kind| *kind != swash::text::JoiningType::T)
}

fn shaping_boundary<'a>(
    text: &mut String,
    spans: &mut Vec<(std::ops::Range<usize>, Attrs<'a>)>,
    left: &Attrs<'a>,
    right: &Attrs<'a>,
    joined: bool,
) {
    let start = text.len();
    if joined {
        // ZWJs retain contextual forms; ZWNJ blocks cross-row ligatures.
        // ZWSP starts a grapheme so the right ZWJ receives its own font style.
        text.push_str("\u{200d}\u{200c}\u{200b}");
        spans.push((start..text.len(), left.clone()));
        let start = text.len();
        text.push('\u{200d}');
        spans.push((start..text.len(), right.clone()));
    } else {
        text.push('\u{200c}');
        spans.push((start..text.len(), left.clone()));
    }
}

fn shape_contextual_row(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    cells: &[RenderCell],
    config: &FontConfig,
    metrics: CellMetrics,
) -> ShapedRow {
    shape_contextual_paragraph(font_system, buffer, &[cells], config, metrics, "", "").remove(0)
}

fn shape_contextual_paragraph(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    rows: &[&[RenderCell]],
    config: &FontConfig,
    metrics: CellMetrics,
    prefix: &str,
    suffix: &str,
) -> Vec<ShapedRow> {
    let mut shaped: Vec<_> = rows
        .iter()
        .map(|cells| ShapedRow {
            glyphs: Vec::new(),
            visual_cols: (0..cells.len()).collect(),
            rtl: vec![false; cells.len()],
        })
        .collect();
    let mut text = String::new();
    let mut ranges = Vec::<CellRange>::new();
    let mut spans = Vec::new();
    let mut row_bytes = Vec::new();
    let mut occupied_ranges = Vec::new();
    let mut row_cell_indices = Vec::new();
    let default_attrs = Attrs::new().family(cosmic_text::Family::Name(&config.family));
    let mut previous_attrs = default_attrs.clone();
    let mut previous_joining =
        prefix.chars().rev().map(joining_type).find(|kind| *kind != swash::text::JoiningType::T);
    if !prefix.is_empty() {
        text.push_str(prefix);
        spans.push((0..text.len(), default_attrs.clone()));
    }
    let occupied = |cell: &RenderCell| {
        !cell.flags.intersects(
            CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER,
        ) && (cell.character != ' ' || !cell.zerowidth.is_empty())
    };
    for (row, cells) in rows.iter().enumerate() {
        if row > 0 || !prefix.is_empty() {
            let right = cells.iter().find_map(cell_first_joining_type);
            let right_attrs = cells
                .first()
                .map_or_else(|| default_attrs.clone(), |cell| cell_attrs(cell, config));
            shaping_boundary(
                &mut text,
                &mut spans,
                &previous_attrs,
                &right_attrs,
                joins_across(previous_joining, right),
            );
        }
        let first = cells.iter().position(occupied);
        let last = cells.iter().rposition(occupied);
        let occupied_range = match (first, last) {
            (Some(first), Some(last)) => {
                let span = if cells[last].flags.contains(CellFlags::WIDE_CHAR)
                    && cells
                        .get(last + 1)
                        .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
                {
                    2
                } else {
                    1
                };
                first..last + span
            }
            _ => 0..0,
        };
        occupied_ranges.push(occupied_range);
        let row_start = text.len();
        let mut cell_indices = vec![0; cells.len()];
        let mut col = 0;
        while col < cells.len() {
            let cell = &cells[col];
            let attrs = cell_attrs(cell, config);
            if col > 0
                && !previous_attrs.compatible(&attrs)
                && joins_across(previous_joining, cell_first_joining_type(cell))
            {
                shaping_boundary(&mut text, &mut spans, &previous_attrs, &attrs, true);
            }
            let start = text.len();
            let hidden = cell.flags.intersects(
                CellFlags::HIDDEN
                    | CellFlags::WIDE_CHAR_SPACER
                    | CellFlags::LEADING_WIDE_CHAR_SPACER,
            );
            text.push(if hidden { ' ' } else { cell.character });
            if !hidden {
                text.push_str(&cell.zerowidth);
            }
            let end = text.len();
            for character in text[start..end].chars() {
                let kind = joining_type(character);
                if kind != swash::text::JoiningType::T {
                    previous_joining = Some(kind);
                }
            }
            let span = if cell.flags.contains(CellFlags::WIDE_CHAR)
                && cells
                    .get(col + 1)
                    .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
            {
                2
            } else {
                1
            };
            for index in &mut cell_indices[col..col + span] {
                *index = ranges.len();
            }
            ranges.push(CellRange { start, end, col, span, row });
            spans.push((start..end, attrs.clone()));
            previous_attrs = attrs;
            col += span;
        }
        row_bytes.push(row_start..text.len());
        row_cell_indices.push(cell_indices);
    }
    if !suffix.is_empty() {
        shaping_boundary(
            &mut text,
            &mut spans,
            &previous_attrs,
            &default_attrs,
            joins_across(previous_joining, first_joining_type(suffix)),
        );
        let start = text.len();
        text.push_str(suffix);
        spans.push((
            start..text.len(),
            Attrs::new().family(cosmic_text::Family::Name(&config.family)),
        ));
    }
    if text.is_empty() {
        return shaped;
    }
    let bidi = unicode_bidi::BidiInfo::new(&text, None);
    let attrs = Attrs::new().family(cosmic_text::Family::Name(&config.family));
    let mut borrow = buffer.borrow_with(font_system);
    // Cosmic 0.19's no-wrap layout drops an initial opposite-direction span.
    // Unbounded word layout keeps that span without introducing line breaks.
    borrow.set_wrap(Wrap::Word);
    borrow.set_size(None, None);
    borrow.set_monospace_width(Some(metrics.cell_width));
    borrow.set_rich_text(
        spans.iter().map(|(range, attrs)| (&text[range.clone()], attrs.clone())),
        &attrs,
        Shaping::Advanced,
        Some(cosmic_text::Align::Left),
    );
    borrow.shape_until_scroll(false);
    let mut row_clusters: Vec<Vec<GlyphCluster>> = (0..rows.len()).map(|_| Vec::new()).collect();
    let mut cluster_indices: Vec<HashMap<(usize, usize), usize>> =
        (0..rows.len()).map(|_| HashMap::new()).collect();
    for run in borrow.layout_runs() {
        for glyph in run.glyphs {
            if ranges.is_empty()
                || glyph.end <= ranges[0].start
                || glyph.start >= ranges[ranges.len() - 1].end
            {
                continue;
            }
            let first_index = ranges.partition_point(|range| range.end <= glyph.start);
            let end_index = ranges.partition_point(|range| range.start < glyph.end);
            if first_index >= end_index {
                continue;
            }
            let source = &ranges[first_index];
            let tail = &ranges[end_index - 1];
            // The joining bridge prevents cross-row ligatures. Keep any
            // invisible boundary-only glyph out of the terminal-cell mapping.
            debug_assert_eq!(source.row, tail.row, "glyph crossed a protected row boundary");
            if source.row != tail.row {
                continue;
            }
            let row = source.row;
            if !occupied_ranges[row].contains(&source.col) {
                continue;
            }
            let first_col = source.col;
            let last_col = tail.col + tail.span;
            let clusters = &mut row_clusters[row];
            if let Some(index) = cluster_indices[row].get(&(first_col, last_col)) {
                let cluster = &mut clusters[*index];
                cluster.min_x = cluster.min_x.min(glyph.x);
                cluster.glyphs.push(glyph.clone());
            } else {
                cluster_indices[row].insert((first_col, last_col), clusters.len());
                clusters.push(GlyphCluster {
                    first: first_col,
                    end: last_col,
                    min_x: glyph.x,
                    glyphs: vec![glyph.clone()],
                });
            }
        }
    }
    for (row, mut clusters) in row_clusters.into_iter().enumerate() {
        // Default-ignorable characters may produce no glyph. They still own
        // their terminal cells, so reserve empty clusters before reordering.
        let mut covered = vec![false; rows[row].len()];
        for cluster in &clusters {
            covered[cluster.first..cluster.end].fill(true);
        }
        let mut col = occupied_ranges[row].start;
        while col < occupied_ranges[row].end {
            let range = &ranges[row_cell_indices[row][col]];
            if !covered[col] {
                clusters.push(GlyphCluster {
                    first: col,
                    end: col + range.span,
                    min_x: 0.0,
                    glyphs: Vec::new(),
                });
            }
            col += range.span;
        }
        clusters.sort_by_key(|cluster| cluster.first);
        let mut merged = Vec::<GlyphCluster>::new();
        for cluster in clusters {
            if let Some(previous) = merged.last_mut()
                && cluster.first < previous.end
            {
                previous.end = previous.end.max(cluster.end);
                previous.min_x = previous.min_x.min(cluster.min_x);
                previous.glyphs.extend(cluster.glyphs);
            } else {
                merged.push(cluster);
            }
        }
        let Some(paragraph) = bidi.paragraphs.iter().find(|paragraph| {
            paragraph.range.start <= row_bytes[row].start
                && row_bytes[row].start < paragraph.range.end
        }) else {
            continue;
        };
        let line_levels = bidi.reordered_levels(paragraph, row_bytes[row].clone());
        let levels: Vec<_> = merged
            .iter()
            .map(|cluster| {
                let byte = ranges[row_cell_indices[row][cluster.first]].start;
                line_levels[byte]
            })
            .collect();
        let order = unicode_bidi::BidiInfo::reorder_visual(&levels);
        let mut visual_col = occupied_ranges[row].start;
        let mut visual_spans = vec![(0, 0); merged.len()];
        for &index in &order {
            let cluster = &merged[index];
            let rtl = levels[index].is_rtl();
            let cluster_visual = visual_col;
            let mut units = Vec::new();
            let mut col = cluster.first;
            while col < cluster.end {
                let range = &ranges[row_cell_indices[row][col]];
                units.push(range);
                col += range.span;
            }
            if rtl {
                units.reverse();
            }
            for unit in units {
                for offset in 0..unit.span {
                    shaped[row].visual_cols[unit.col + offset] = visual_col + offset;
                    shaped[row].rtl[unit.col + offset] = rtl;
                }
                visual_col += unit.span;
            }
            visual_spans[index] = (cluster_visual, visual_col);
        }
        let is_rtl_word = |index: usize| {
            levels[index].is_rtl()
                && rows[row][merged[index].first..merged[index].end]
                    .iter()
                    .all(|cell| !cell.character.is_whitespace())
        };
        let mut first = 0;
        while first < order.len() {
            let mut end = first + 1;
            if is_rtl_word(order[first]) {
                while end < order.len()
                    && levels[order[end]] == levels[order[first]]
                    && is_rtl_word(order[end])
                {
                    end += 1;
                }
            }
            let group = &order[first..end];
            let group_start = visual_spans[group[0]].0;
            let group_end = visual_spans[group[group.len() - 1]].1;
            // Scale a connected RTL word as one geometry group. Independent
            // cell/cluster scaling deforms joining strokes and stacks ligature
            // parts; one affine transform preserves all natural glyph offsets.
            let natural_start = group
                .iter()
                .flat_map(|index| &merged[*index].glyphs)
                .map(|glyph| glyph.x)
                .fold(f32::INFINITY, f32::min);
            let natural_end = group
                .iter()
                .flat_map(|index| &merged[*index].glyphs)
                .map(|glyph| glyph.x + glyph.w)
                .fold(f32::NEG_INFINITY, f32::max);
            let natural_width = natural_end - natural_start;
            let reserved_width = (group_end - group_start) as f32 * metrics.cell_width;
            let scale_x = if natural_width > 0.0 { reserved_width / natural_width } else { 1.0 };
            for (cluster, glyph) in group.iter().flat_map(|index| {
                let cluster = &merged[*index];
                cluster.glyphs.iter().map(move |glyph| (cluster, glyph))
            }) {
                let index = ranges.partition_point(|range| range.end <= glyph.start);
                let source_col = ranges
                    .get(index)
                    .filter(|range| range.row == row && range.start <= glyph.start)
                    .map_or(cluster.first, |range| range.col);
                let physical = glyph.physical((-natural_start, metrics.ascent), 1.0);
                shaped[row].glyphs.push(ShapedGlyph {
                    cache_key: GlyphKey::new(physical.cache_key, scale_x),
                    source_col,
                    x: group_start as f32 * metrics.cell_width + physical.x as f32 * scale_x,
                    y: physical.y as f32,
                    scale_x,
                    cluster_start: group_start,
                    cluster_end: group_end,
                });
            }
            first = end;
        }
    }
    shaped
}

fn transformed_swash_image(
    font_system: &mut FontSystem,
    context: &mut swash::scale::ScaleContext,
    key: GlyphKey,
) -> Option<cosmic_text::SwashImage> {
    use swash::scale::{Render, Source};
    use swash::zeno::{Angle, Format, Transform, Vector};

    let font = font_system.get_font(key.font_id, key.font_weight)?;
    let variable_weight =
        font.as_swash().variations().find_by_tag(swash::Tag::from_be_bytes(*b"wght"));
    let mut scaler = context
        .builder(font.as_swash())
        .size(f32::from_bits(key.font_size_bits))
        .hint(!key.flags.contains(cosmic_text::CacheKeyFlags::DISABLE_HINTING));
    if let Some(axis) = variable_weight {
        scaler = scaler.normalized_coords(font.as_swash().variations().normalized_coords([(
            swash::Tag::from_be_bytes(*b"wght"),
            f32::from(key.font_weight.0).clamp(axis.min_value(), axis.max_value()),
        )]));
    }
    let mut scaler = scaler.build();
    let scale_x = key.scale_x();
    let transform = if key.flags.contains(cosmic_text::CacheKeyFlags::FAKE_ITALIC) {
        Transform::skew(Angle::from_degrees(14.0), Angle::from_degrees(0.0))
            .then_scale(scale_x, 1.0)
    } else {
        Transform::scale(scale_x, 1.0)
    };
    let pixel_font = key.flags.contains(cosmic_text::CacheKeyFlags::PIXEL_FONT);
    let x = key.x_bin.as_float();
    let y = key.y_bin.as_float();
    Render::new(&[Source::ColorOutline(0), Source::Outline])
        .format(Format::Alpha)
        .offset(Vector::new(
            if pixel_font { x.round() * scale_x } else { x * scale_x },
            if pixel_font { y.round() } else { y },
        ))
        .transform(Some(transform))
        .render(&mut scaler, key.glyph_id)
}

fn bitmap_coverage(
    content: SwashContent,
    width: u32,
    height: u32,
    data: Vec<u8>,
) -> Result<Vec<u8>, AtlasError> {
    let pixels = (width as usize).checked_mul(height as usize).ok_or(AtlasError::InvalidBitmap)?;
    let channels = if content == SwashContent::Mask { 1 } else { 4 };
    if data.len() != pixels.checked_mul(channels).ok_or(AtlasError::InvalidBitmap)? {
        return Err(AtlasError::InvalidBitmap);
    }
    match content {
        SwashContent::Mask => Ok(data),
        // The terminal shader colors glyph coverage; retain alpha from color
        // bitmaps rather than interpreting interleaved RGBA bytes as rows.
        SwashContent::Color => Ok(data.as_chunks::<4>().0.iter().map(|rgba| rgba[3]).collect()),
        SwashContent::SubpixelMask => Ok(data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|rgba| ((u16::from(rgba[0]) + u16::from(rgba[1]) + u16::from(rgba[2])) / 3) as u8)
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(text: &str) -> Vec<RenderCell> {
        text.chars().map(|character| RenderCell { character, ..RenderCell::default() }).collect()
    }

    #[test]
    fn borrowed_shape_keys_preserve_text_styles_and_paragraph_boundaries() {
        use std::hash::BuildHasher;

        let mut source = cells("س日本語 Русский Україна café Straße español português italiano");
        source[0].zerowidth = "\u{64e}\u{651}".into();
        let mut rows = hashbrown::HashMap::with_hasher(RandomState::new());
        for flags in [CellFlags::empty(), SHAPING_FLAGS, CellFlags::all()] {
            source[0].flags = flags;
            let key = Arc::new(RowKey::new(&source));
            assert_eq!(rows.hasher().hash_one(&key), rows.hasher().hash_one(RowRef(&source)));
            rows.insert(key, 42);
            assert_eq!(rows.get(&RowRef(&source)), Some(&42));
            let mut overlay = source.clone();
            overlay[0].fg = mechanic_config::theme::palette::BLACK;
            overlay[0].flags.toggle(CellFlags::UNDERLINE);
            assert_eq!(rows.get(&RowRef(&overlay)), Some(&42));
            for flags in [
                CellFlags::BOLD,
                CellFlags::ITALIC,
                CellFlags::HIDDEN,
                CellFlags::WIDE_CHAR,
                CellFlags::WIDE_CHAR_SPACER,
                CellFlags::LEADING_WIDE_CHAR_SPACER,
            ] {
                let mut changed = source.clone();
                changed[0].flags.toggle(flags);
                assert!(rows.get(&RowRef(&changed)).is_none());
            }
            let mut changed = source.clone();
            changed[0].zerowidth.push('\u{301}');
            assert!(rows.get(&RowRef(&changed)).is_none());
            assert!(rows.get(&RowRef(&source[1..])).is_none());
            rows.clear();
        }

        let slices = [&source[..3], &source[3..], &[]];
        let key = Arc::new(ParagraphKey {
            rows: slices.iter().map(|row| RowKey::new(row)).collect(),
            prefix: "قبل".into(),
            suffix: "بعد".into(),
        });
        let mut paragraphs = hashbrown::HashMap::with_hasher(RandomState::new());
        let borrowed = ParagraphRef { rows: &slices, prefix: "قبل", suffix: "بعد" };
        assert_eq!(paragraphs.hasher().hash_one(&key), paragraphs.hasher().hash_one(&borrowed));
        paragraphs.insert(key, 42);
        assert_eq!(paragraphs.get(&borrowed), Some(&42));
        assert!(paragraphs.get(&ParagraphRef { prefix: "", ..borrowed }).is_none());
        assert!(paragraphs.get(&ParagraphRef { suffix: "", ..borrowed }).is_none());
        let moved_boundary = [&source[..4], &source[4..], &[]];
        assert!(paragraphs.get(&ParagraphRef { rows: &moved_boundary, ..borrowed }).is_none());
        let reordered = [slices[1], slices[0], slices[2]];
        assert!(paragraphs.get(&ParagraphRef { rows: &reordered, ..borrowed }).is_none());
    }

    fn cpu_shaper() -> (FontSystem, Buffer, FontConfig, CellMetrics) {
        let config = FontConfig { family: "Menlo".into(), ..FontConfig::default() };
        let mut fonts = configured_font_system(&config);
        let buffer = Buffer::new(&mut fonts, Metrics::new(16.0, 24.0));
        (fonts, buffer, config, CellMetrics { cell_width: 10.0, cell_height: 24.0, ascent: 18.0 })
    }

    #[test]
    fn numeric_prefix_before_rtl_text_keeps_visible_glyphs() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        for text in [
            "0001/0030 تحتفظ اللغة العربية بسياق الفقر ",
            "123 العربية",
            "العربية 123",
            "123 עברית",
            " 12 345 (67) العربية 89  ",
            "123 \u{2067}العربية\u{2069} 456",
            "123 English 日本語 456  ",
        ] {
            let input = cells(text);
            let shaped = shape_contextual_row(&mut fonts, &mut buffer, &input, &config, metrics);
            assert_permutation(&shaped);
            for (col, cell) in input.iter().enumerate() {
                if cell.character.is_ascii_digit() {
                    assert!(
                        shaped.glyphs.iter().any(|glyph| glyph.source_col == col),
                        "lost digit at {col} in {text:?}: {shaped:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn fallback_pruning_preserves_exact_names_and_priority() {
        let mut fallback = ConfiguredFallback {
            common: vec!["missing", "Alias", "Primary", "Alias", "primary", "Primary"],
            scripts: HashMap::from([(
                unicode_script::Script::Arabic,
                vec!["Alias", "missing", "Alias", "Primary"],
            )]),
        };
        fallback.retain_available(&HashSet::from(["Primary", "Alias"]));
        assert_eq!(fallback.common, ["Alias", "Primary"]);
        assert_eq!(
            fallback.script_fallback(unicode_script::Script::Arabic, "en-US"),
            ["Alias", "Primary"]
        );
        assert_eq!(
            fallback.script_fallback(unicode_script::Script::Thai, "th-TH"),
            PlatformFallback.script_fallback(unicode_script::Script::Thai, "th-TH")
        );
        assert_eq!(fallback.forbidden_fallback(), PlatformFallback.forbidden_fallback());
    }

    #[test]
    fn pruned_fallback_preserves_multilingual_glyphs_and_geometry() {
        let source = FontSystem::new();
        let available: HashSet<_> = source
            .db()
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.as_str()))
            .collect();
        assert!(!available.is_empty(), "differential shaping needs installed fonts");
        let alias =
            source.db().faces().find_map(|face| face.families.get(1)).map(|(name, _)| name.clone());
        let metrics = CellMetrics { cell_width: 10.0, cell_height: 24.0, ascent: 18.0 };
        for family in ["Menlo", "Geeza Pro", "Missing Mechanic Test Font"] {
            let mut config = FontConfig { family: family.into(), ..FontConfig::default() };
            config.fallback_families.insert(0, "Missing Mechanic Test Font".into());
            config.fallback_families.extend(["Menlo".into(), "menlo".into(), "Geeza Pro".into()]);
            if let Some(alias) = &alias {
                config.fallback_families.insert(0, alias.clone());
            }
            let mut pruned = configured_fallback(&config, &source);
            pruned.retain_available(&available);
            let mut optimized = FontSystem::new_with_locale_and_db_and_fallback(
                source.locale().to_owned(),
                source.db().clone(),
                pruned,
            );
            let mut reference = FontSystem::new_with_locale_and_db_and_fallback(
                source.locale().to_owned(),
                source.db().clone(),
                configured_fallback(&config, &source),
            );
            let mut optimized_buffer = Buffer::new(&mut optimized, Metrics::new(16.0, 24.0));
            let mut reference_buffer = Buffer::new(&mut reference, Metrics::new(16.0, 24.0));
            for text in [
                "office café cafe\u{301} Straße español português città français",
                "Русский Українська ї є ґ Й и\u{306}",
                "中文 日本語 ひらがな カタカナ 한국어",
                "العربية: السَّلام عليكم 2026 (Paris) لا الله ب\u{200d}ب ب\u{200c}ب",
                "abc \u{2067}العربية 12 \u{2066}東京\u{2069}\u{2069} \u{202e}xyz\u{202c}",
                "עברית हिन्दी ภาษาไทย \u{10ffff}",
            ] {
                let mut input = Vec::<RenderCell>::new();
                for character in text.chars() {
                    let width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
                    if width == 0 && !input.is_empty() {
                        let base = input
                            .iter_mut()
                            .rev()
                            .find(|cell| !cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
                            .unwrap();
                        base.zerowidth.push(character);
                    } else {
                        input.push(RenderCell {
                            character,
                            flags: if width == 2 {
                                CellFlags::WIDE_CHAR
                            } else {
                                CellFlags::empty()
                            },
                            ..Default::default()
                        });
                        if width == 2 {
                            input.push(RenderCell {
                                flags: CellFlags::WIDE_CHAR_SPACER,
                                ..Default::default()
                            });
                        }
                    }
                }
                for styled in [false, true] {
                    if styled {
                        for (index, cell) in input.iter_mut().enumerate() {
                            cell.flags |= match index % 3 {
                                0 => CellFlags::BOLD,
                                1 => CellFlags::ITALIC,
                                _ => CellFlags::BOLD | CellFlags::ITALIC,
                            };
                        }
                    }
                    for width in [7, 19, 120] {
                        // Keep wide cells with their spacers at each wrap.
                        let mut slices = Vec::new();
                        let mut remaining = input.as_slice();
                        while !remaining.is_empty() {
                            let mut end = width.min(remaining.len());
                            if remaining[end - 1].flags.contains(CellFlags::WIDE_CHAR) {
                                end -= 1;
                            }
                            slices.push(&remaining[..end]);
                            remaining = &remaining[end..];
                        }
                        for (prefix, suffix) in [("", ""), ("قبل \u{2066}abc ", " xyz\u{2069} بعد")]
                        {
                            let expected = shape_contextual_paragraph(
                                &mut reference,
                                &mut reference_buffer,
                                &slices,
                                &config,
                                metrics,
                                prefix,
                                suffix,
                            );
                            let actual = shape_contextual_paragraph(
                                &mut optimized,
                                &mut optimized_buffer,
                                &slices,
                                &config,
                                metrics,
                                prefix,
                                suffix,
                            );
                            assert_eq!(actual.len(), expected.len());
                            for (actual, expected) in actual.iter().zip(&expected) {
                                assert_permutation(actual);
                                assert_eq!(
                                    actual.visual_cols, expected.visual_cols,
                                    "{family}: {text}"
                                );
                                assert_eq!(actual.rtl, expected.rtl, "{family}: {text}");
                                let signature = |row: &ShapedRow| {
                                    row.glyphs
                                        .iter()
                                        .map(|glyph| {
                                            (
                                                glyph.cache_key,
                                                glyph.source_col,
                                                glyph.cluster_start,
                                                glyph.cluster_end,
                                                glyph.x.to_bits(),
                                                glyph.y.to_bits(),
                                                glyph.scale_x.to_bits(),
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                };
                                assert_eq!(
                                    signature(actual),
                                    signature(expected),
                                    "{family}: {text}, width={width}, styled={styled}, prefix={prefix:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn assert_permutation(row: &ShapedRow) {
        let mut columns = row.visual_cols.clone();
        columns.sort_unstable();
        assert_eq!(columns, (0..columns.len()).collect::<Vec<_>>());
        assert!(row.glyphs.iter().all(|glyph| glyph.source_col < columns.len()
            && glyph.x.is_finite()
            && glyph.y.is_finite()));
    }

    fn ink_ids(fonts: &mut FontSystem, row: &ShapedRow) -> Vec<u16> {
        row.glyphs
            .iter()
            .filter_map(|glyph| {
                let font =
                    fonts.get_font(glyph.cache_key.font_id, glyph.cache_key.font_weight).unwrap();
                (glyph.cache_key.glyph_id != font.as_swash().charmap().map(' '))
                    .then_some(glyph.cache_key.glyph_id)
            })
            .collect()
    }

    #[test]
    fn joining_boundaries_follow_unicode_controls_and_direction() {
        let boundary = |left: &str, right: &str| {
            joins_across(
                left.chars()
                    .rev()
                    .map(joining_type)
                    .find(|kind| *kind != swash::text::JoiningType::T),
                first_joining_type(right),
            )
        };
        assert!(boundary("ب\u{64e}", "ب"));
        assert!(boundary("ل", "ا"));
        assert!(boundary("ب", "ـ"));
        assert!(boundary("ـ", "ب"));
        assert!(boundary("ب\u{200d}", "ب"));
        assert!(!boundary("ا", "ب"));
        assert!(!boundary("ب\u{200c}", "ب"));
        assert!(!boundary("ب", "\u{200c}ب"));
        assert!(!boundary("ب ", "ب"));
    }

    #[test]
    #[ignore = "manual CPU shaping benchmark"]
    fn wrapped_shaping_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        let mut csv = String::from(
            "workload,sample,ms_per_shape,glyphs,columns,rows,warmup_iterations,iterations\n",
        );
        for (name, phrase, prefix, suffix) in [
            ("arabic", "سلام عليكم المسؤول الله لا سلام ", "", ""),
            ("mixed", "قال المسؤول: «سنبدأ عام 2026». Latin / Русский ", "", ""),
            ("viewport", "سلام عليكم المسؤول الله لا سلام ", "المسؤول سلا", "م عليكم"),
            ("contextual_ascii", "ordinary terminal text with digits 12345 and words ", "", ""),
        ] {
            let input: Vec<_> = phrase.chars().cycle().take(80 * 24).collect();
            let mut rows: Vec<_> =
                input.chunks(80).map(|chars| cells(&chars.iter().collect::<String>())).collect();
            let mut samples = Vec::new();
            let mut glyphs = 0;
            for iteration in 0..20 {
                rows[12][20].character = char::from(b'0' + iteration % 10);
                let slices: Vec<_> = rows.iter().map(Vec::as_slice).collect();
                black_box(shape_contextual_paragraph(
                    &mut fonts,
                    &mut buffer,
                    &slices,
                    &config,
                    metrics,
                    prefix,
                    suffix,
                ));
            }
            for sample in 0..7 {
                let started = Instant::now();
                for iteration in 0..20 {
                    rows[12][20].character = char::from(b'0' + iteration % 10);
                    let slices: Vec<_> = rows.iter().map(Vec::as_slice).collect();
                    let output = shape_contextual_paragraph(
                        &mut fonts,
                        &mut buffer,
                        &slices,
                        &config,
                        metrics,
                        prefix,
                        suffix,
                    );
                    glyphs = output.iter().map(|row| row.glyphs.len()).sum::<usize>();
                    black_box(output);
                }
                let millis = started.elapsed().as_secs_f64() * 1000. / 20.;
                csv.push_str(&format!("{name},{sample},{millis:.6},{glyphs},80,24,20,20\n"));
                samples.push(millis);
            }
            samples.sort_by(f64::total_cmp);
            println!("{name}: {:.3} ms/shape, {glyphs} glyphs, 24x80 cells", samples[3]);
        }
        if let Ok(path) = std::env::var("MECHANIC_SHAPING_CSV") {
            std::fs::write(path, csv).unwrap();
        }
    }

    #[test]
    fn language_rows_preserve_glyphs_marks_and_terminal_spans() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        for text in [
            "Français déjà Noël — Deutsch Grüße Straße",
            "Español canción — Português ação — Italiano città",
            "Русский текст — Українська мова ї є ґ",
            "قال المسؤول: «سنبدأ عام 2026».",
        ] {
            let input = cells(text);
            let shaped = shape_contextual_row(&mut fonts, &mut buffer, &input, &config, metrics);
            assert_permutation(&shaped);
            assert!(!shaped.glyphs.is_empty(), "missing language glyphs for {text}");
            assert!(
                shaped.glyphs.iter().all(|glyph| glyph.cache_key.glyph_id != 0),
                "font fallback failed for {text}"
            );
        }
        let mut marked = cells("a");
        marked[0].zerowidth = "\u{315}\u{323}".into();
        let shaped = shape_contextual_row(&mut fonts, &mut buffer, &marked, &config, metrics);
        assert!(shaped.glyphs.len() >= 2, "multiple combining glyphs were discarded");
        assert!(shaped.glyphs.iter().all(|glyph| glyph.source_col == 0));
        assert_permutation(&shaped);

        let mut japanese = Vec::new();
        for character in "日本語".chars() {
            japanese.push(RenderCell {
                character,
                flags: CellFlags::WIDE_CHAR,
                ..RenderCell::default()
            });
            japanese
                .push(RenderCell { flags: CellFlags::WIDE_CHAR_SPACER, ..RenderCell::default() });
        }
        let shaped = shape_contextual_row(&mut fonts, &mut buffer, &japanese, &config, metrics);
        assert_permutation(&shaped);
        assert_eq!(shaped.visual_cols, vec![0, 1, 2, 3, 4, 5]);
        for col in [0, 2, 4] {
            assert!(
                shaped
                    .glyphs
                    .iter()
                    .any(|glyph| glyph.source_col == col && glyph.cache_key.glyph_id != 0)
            );
        }
    }

    #[test]
    fn german_letters_quotes_and_diaeresis_keep_cells_across_styles_and_wraps() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        let text = "ÄÖÜäöüßẞ „Grüße, Straße!“ A\u{308}O\u{308}U\u{308}a\u{308}o\u{308}u\u{308}";
        let mut input = Vec::<RenderCell>::new();
        for character in text.chars() {
            if character == '\u{308}' {
                input.last_mut().unwrap().zerowidth.push(character);
            } else {
                input.push(RenderCell { character, ..Default::default() });
            }
        }
        assert_eq!(input.iter().filter(|cell| !cell.zerowidth.is_empty()).count(), 6);
        for style in [
            CellFlags::empty(),
            CellFlags::BOLD,
            CellFlags::ITALIC,
            CellFlags::BOLD | CellFlags::ITALIC,
        ] {
            for cell in &mut input {
                cell.flags = style;
            }
            for width in [1, 3, 7, input.len()] {
                let rows: Vec<_> = input.chunks(width).collect();
                let before = input.clone();
                let shaped = shape_contextual_paragraph(
                    &mut fonts,
                    &mut buffer,
                    &rows,
                    &config,
                    metrics,
                    "",
                    "",
                );
                assert_eq!(shaped.len(), rows.len());
                for (row, source) in shaped.iter().zip(&rows) {
                    assert_permutation(row);
                    assert_eq!(row.visual_cols, (0..source.len()).collect::<Vec<_>>());
                    assert!(row.rtl.iter().all(|rtl| !rtl));
                    assert!(
                        row.glyphs.iter().all(|glyph| glyph.cache_key.glyph_id != 0),
                        "German glyph fallback failed: style {style:?}, width {width}"
                    );
                    for (col, cell) in source.iter().enumerate() {
                        if cell.character == ' ' {
                            continue;
                        }
                        assert!(
                            row.glyphs.iter().any(|glyph| glyph.source_col == col
                                && glyph.cluster_start <= col
                                && col < glyph.cluster_end),
                            "German cell lost glyph/selection geometry: {:?}{:?}, style {style:?}, width {width}, column {col}",
                            cell.character,
                            cell.zerowidth
                        );
                    }
                }
                // Shaping may reorder display geometry but must never normalize
                // the cells later used by clipboard copying or terminal input.
                assert_eq!(input, before);
            }
        }
    }

    #[test]
    fn german_composed_and_decomposed_diaeresis_shape_equally_without_losing_marks() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        let signature = |row: &ShapedRow| {
            row.glyphs
                .iter()
                .map(|glyph| {
                    (
                        glyph.cache_key,
                        glyph.x.to_bits(),
                        glyph.y.to_bits(),
                        glyph.source_col,
                        glyph.cluster_start,
                        glyph.cluster_end,
                    )
                })
                .collect::<Vec<_>>()
        };
        for style in [
            CellFlags::empty(),
            CellFlags::BOLD,
            CellFlags::ITALIC,
            CellFlags::BOLD | CellFlags::ITALIC,
        ] {
            for (base, composed) in
                [('A', 'Ä'), ('O', 'Ö'), ('U', 'Ü'), ('a', 'ä'), ('o', 'ö'), ('u', 'ü')]
            {
                let composed_cell =
                    RenderCell { character: composed, flags: style, ..Default::default() };
                let base_cell = RenderCell { character: base, flags: style, ..Default::default() };
                let marked_cell = RenderCell { zerowidth: "\u{308}".into(), ..base_cell.clone() };
                let composed_row = shape_contextual_row(
                    &mut fonts,
                    &mut buffer,
                    &[composed_cell],
                    &config,
                    metrics,
                );
                let marked_row =
                    shape_contextual_row(&mut fonts, &mut buffer, &[marked_cell], &config, metrics);
                let base_row =
                    shape_contextual_row(&mut fonts, &mut buffer, &[base_cell], &config, metrics);
                for row in [&composed_row, &marked_row] {
                    assert_permutation(row);
                    assert_eq!(row.visual_cols, vec![0]);
                    assert!(!row.glyphs.is_empty());
                    assert!(row.glyphs.iter().all(|glyph| glyph.cache_key.glyph_id != 0
                        && glyph.source_col == 0
                        && glyph.cluster_start == 0
                        && glyph.cluster_end == 1));
                }
                assert_eq!(
                    signature(&marked_row),
                    signature(&composed_row),
                    "canonical German spelling changed display: {base}/{composed}, {style:?}"
                );
                assert_ne!(
                    signature(&marked_row),
                    signature(&base_row),
                    "diaeresis was discarded: {base}, {style:?}"
                );
            }
        }
    }

    #[test]
    fn arabic_joining_and_prose_punctuation_follow_paragraph_direction() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        let input = cells("سلام");
        let row = shape_contextual_row(&mut fonts, &mut buffer, &input, &config, metrics);
        assert_permutation(&row);
        assert_eq!(row.visual_cols, vec![3, 2, 1, 0]);
        let joined: Vec<_> = row.glyphs.iter().map(|glyph| glyph.cache_key.glyph_id).collect();
        let isolated: Vec<_> = input
            .iter()
            .flat_map(|cell| {
                shape_contextual_row(
                    &mut fonts,
                    &mut buffer,
                    std::slice::from_ref(cell),
                    &config,
                    metrics,
                )
                .glyphs
            })
            .map(|glyph| glyph.cache_key.glyph_id)
            .collect();
        assert_ne!(joined, isolated, "Arabic was shaped character by character");

        let mut prose = cells("قال المسؤول: «سنبدأ عام 2026».");
        let end = prose.len();
        prose.extend(cells("      "));
        let row = shape_contextual_row(&mut fonts, &mut buffer, &prose, &config, metrics);
        assert_permutation(&row);
        assert_eq!(
            row.visual_cols[end - 1],
            0,
            "final neutral period must follow RTL paragraph direction"
        );
        assert_eq!(&row.visual_cols[end..], &(end..prose.len()).collect::<Vec<_>>());
    }

    #[test]
    fn soft_wrapped_arabic_uses_joined_forms_and_keeps_row_local_ligatures() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        for word in ["بب", "بلا", "العربية", "الله"] {
            let input = cells(word);
            let slices: Vec<_> = input.iter().map(std::slice::from_ref).collect();
            let rows = shape_contextual_paragraph(
                &mut fonts,
                &mut buffer,
                &slices,
                &config,
                metrics,
                "",
                "",
            );
            for row in &rows {
                assert_permutation(row);
                assert!(!ink_ids(&mut fonts, row).is_empty());
                assert!(row.glyphs.iter().all(|glyph| glyph.source_col == 0
                    && glyph.cluster_start == 0
                    && glyph.cluster_end == 1));
            }
        }
        let beh = cells("ب");
        let joined = shape_contextual_paragraph(
            &mut fonts,
            &mut buffer,
            &[&beh, &beh],
            &config,
            metrics,
            "",
            "",
        );
        let isolated = shape_contextual_row(&mut fonts, &mut buffer, &beh, &config, metrics);
        let isolated_ids = ink_ids(&mut fonts, &isolated);
        assert_ne!(ink_ids(&mut fonts, &joined[0]), isolated_ids);
        assert_ne!(ink_ids(&mut fonts, &joined[1]), isolated_ids);
        let mut blocked = beh.clone();
        blocked[0].zerowidth.push('\u{200c}');
        let rows = shape_contextual_paragraph(
            &mut fonts,
            &mut buffer,
            &[&blocked, &beh],
            &config,
            metrics,
            "",
            "",
        );
        assert_eq!(ink_ids(&mut fonts, &rows[0]), isolated_ids);
        assert_eq!(ink_ids(&mut fonts, &rows[1]), isolated_ids);
        let nonjoining = cells("ا");
        let rows = shape_contextual_paragraph(
            &mut fonts,
            &mut buffer,
            &[&nonjoining, &beh],
            &config,
            metrics,
            "",
            "",
        );
        assert_eq!(ink_ids(&mut fonts, &rows[1]), isolated_ids);
    }

    #[test]
    fn wrapped_marks_styles_and_viewport_context_keep_glyph_geometry() {
        let (_, _, _, metrics) = cpu_shaper();
        for family in ["Menlo", "Geeza Pro", "Noto Sans Arabic"] {
            let config = FontConfig { family: family.into(), ..FontConfig::default() };
            let mut fonts = configured_font_system(&config);
            if !fonts.db().faces().any(|face| face.families.iter().any(|(name, _)| name == family))
            {
                continue;
            }
            let mut buffer = Buffer::new(&mut fonts, Metrics::new(16.0, 24.0));
            for word in ["بلا", "العربية", "الله"] {
                let mut input = cells(word);
                input[0].zerowidth = "\u{64e}\u{651}".into();
                input[1].flags = CellFlags::BOLD;
                let slices: Vec<_> = input.iter().map(std::slice::from_ref).collect();
                let complete = shape_contextual_paragraph(
                    &mut fonts,
                    &mut buffer,
                    &slices,
                    &config,
                    metrics,
                    "",
                    "",
                );
                for row in 0..input.len() {
                    let text = |cells: &[RenderCell]| {
                        cells
                            .iter()
                            .map(|cell| format!("{}{}", cell.character, cell.zerowidth))
                            .collect::<String>()
                    };
                    let viewport = shape_contextual_paragraph(
                        &mut fonts,
                        &mut buffer,
                        &[slices[row]],
                        &config,
                        metrics,
                        &text(&input[..row]),
                        &text(&input[row + 1..]),
                    );
                    assert_permutation(&viewport[0]);
                    assert_eq!(viewport[0].visual_cols, complete[row].visual_cols);
                    assert_eq!(viewport[0].rtl, complete[row].rtl);
                    assert!(!ink_ids(&mut fonts, &viewport[0]).is_empty());
                    let signature = |shaped: &ShapedRow| {
                        shaped
                            .glyphs
                            .iter()
                            .map(|glyph| {
                                (
                                    glyph.cache_key,
                                    glyph.x.to_bits(),
                                    glyph.y.to_bits(),
                                    glyph.scale_x.to_bits(),
                                    glyph.source_col,
                                )
                            })
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        signature(&viewport[0]),
                        signature(&complete[row]),
                        "viewport changed {family} glyph geometry for {word} row {row}"
                    );
                }
            }
        }
    }

    #[test]
    fn multicell_arabic_wraps_preserve_forms_and_viewport_geometry() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        for parts in [["سل", "ام"], ["السلا", "م عليكم"], ["المس", "ؤول 2026"]]
        {
            let input: Vec<_> = parts.iter().map(|text| cells(text)).collect();
            let slices: Vec<_> = input.iter().map(Vec::as_slice).collect();
            let complete = shape_contextual_paragraph(
                &mut fonts,
                &mut buffer,
                &slices,
                &config,
                metrics,
                "",
                "",
            );
            for index in 0..2 {
                assert_permutation(&complete[index]);
                assert!(
                    complete[index]
                        .glyphs
                        .iter()
                        .all(|glyph| glyph.cluster_end <= input[index].len())
                );
                let viewport = shape_contextual_paragraph(
                    &mut fonts,
                    &mut buffer,
                    &[slices[index]],
                    &config,
                    metrics,
                    if index == 1 { parts[0] } else { "" },
                    if index == 0 { parts[1] } else { "" },
                );
                let signature = |row: &ShapedRow| {
                    row.glyphs
                        .iter()
                        .map(|glyph| {
                            (
                                glyph.cache_key,
                                glyph.x.to_bits(),
                                glyph.y.to_bits(),
                                glyph.scale_x.to_bits(),
                                glyph.source_col,
                                glyph.cluster_start,
                                glyph.cluster_end,
                            )
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    signature(&complete[index]),
                    signature(&viewport[0]),
                    "viewport changed geometry for {parts:?} row {index}"
                );
                assert_eq!(complete[index].visual_cols, viewport[0].visual_cols);
                assert_eq!(complete[index].rtl, viewport[0].rtl);
            }
        }
    }

    #[test]
    fn proportional_arabic_advances_scale_to_contiguous_terminal_clusters() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        for word in ["سلام", "الله", "المسؤول"] {
            let input = cells(word);
            let row = shape_contextual_row(&mut fonts, &mut buffer, &input, &config, metrics);
            assert_permutation(&row);
            let natural: Vec<_> =
                buffer.layout_runs().flat_map(|run| run.glyphs.to_vec()).collect();
            let scale = row.glyphs[0].scale_x;
            let min_x = natural.iter().map(|glyph| glyph.x).fold(f32::INFINITY, f32::min);
            let max_x =
                natural.iter().map(|glyph| glyph.x + glyph.w).fold(f32::NEG_INFINITY, f32::max);
            assert!(
                ((max_x - min_x) * scale - input.len() as f32 * metrics.cell_width).abs() < 0.001
            );
            let mut origins = Vec::new();
            for glyph in &row.glyphs {
                assert_eq!(glyph.cluster_start, 0);
                assert_eq!(glyph.cluster_end, input.len());
                assert_eq!(glyph.scale_x, scale, "joining letters need one shared word transform");
                let source = natural
                    .iter()
                    .find(|source| {
                        word[..source.start].chars().count() == glyph.source_col
                            && source.physical((-min_x, metrics.ascent), 1.0).cache_key
                                == glyph.cache_key.base
                    })
                    .unwrap();
                origins.push((glyph.x, source.physical((-min_x, metrics.ascent), 1.0).x as f32));
            }
            for pair in origins.windows(2) {
                let transformed = pair[1].0 - pair[0].0;
                let expected = (pair[1].1 - pair[0].1) * scale;
                assert!(
                    (transformed - expected).abs() < 0.001,
                    "word glyph origins lost their natural joining geometry"
                );
            }
        }
    }

    #[test]
    fn default_ignorables_keep_unique_cell_reservations() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        for control in ['\u{200e}', '\u{200f}', '\u{202a}', '\u{202c}', '\u{2066}', '\u{2069}'] {
            let text = format!("س{control}لام 2026");
            let row =
                shape_contextual_row(&mut fonts, &mut buffer, &cells(&text), &config, metrics);
            assert_permutation(&row);
            let mut attached = cells("سلام 2026");
            attached[1].zerowidth.push(control);
            let row = shape_contextual_row(&mut fonts, &mut buffer, &attached, &config, metrics);
            assert_permutation(&row);
        }
    }

    #[test]
    fn scaled_outline_rasterization_has_distinct_keys_and_native_resolution() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        let word = shape_contextual_row(&mut fonts, &mut buffer, &cells("الله"), &config, metrics);
        let base = word.glyphs[0].cache_key.base;
        let unit = GlyphKey::new(base, 1.0);
        let doubled = GlyphKey::new(base, 2.0);
        assert_ne!(unit, doubled);
        assert_eq!(
            GlyphKey::new(base, 2.0001),
            doubled,
            "subquantum changes should reuse raster images"
        );
        let face = fonts.db().face(base.font_id).unwrap();
        println!("Arabic fallback resolved to {:?}", face.families);
        let original = SwashCache::new().get_image_uncached(&mut fonts, base).unwrap();
        let scaled =
            transformed_swash_image(&mut fonts, &mut swash::scale::ScaleContext::new(), doubled)
                .unwrap();
        assert_eq!(scaled.content, SwashContent::Mask);
        assert!(
            scaled.placement.width >= original.placement.width.saturating_mul(2).saturating_sub(2)
        );
        assert!(scaled.placement.width > original.placement.width);
        assert!(
            (i64::from(scaled.placement.height) - i64::from(original.placement.height)).abs() <= 1
        );
        assert_eq!(scaled.data.len(), (scaled.placement.width * scaled.placement.height) as usize);
        assert!(scaled.data.iter().any(|alpha| *alpha > 0));
    }

    #[test]
    fn soft_wrapped_arabic_keeps_numeric_and_neutral_continuation_context() {
        let (mut fonts, mut buffer, config, metrics) = cpu_shaper();
        let first = cells("قال المسؤول في ");
        let second = cells("2026)،");
        let third = cells(".");
        let rows = shape_contextual_paragraph(
            &mut fonts,
            &mut buffer,
            &[&first, &second, &third],
            &config,
            metrics,
            "",
            "",
        );
        for row in &rows {
            assert_permutation(row);
        }
        assert_eq!(rows[1].visual_cols, vec![2, 3, 4, 5, 1, 0]);
        assert!(rows[1].rtl[4] && rows[1].rtl[5]);
        assert!(rows[2].rtl[0], "punctuation-only continuation lost paragraph direction");
        let separate = shape_contextual_row(&mut fonts, &mut buffer, &third, &config, metrics);
        assert!(!separate.rtl[0], "a hard break should start a new paragraph");
        let viewport = shape_contextual_paragraph(
            &mut fonts,
            &mut buffer,
            &[&second],
            &config,
            metrics,
            "قال المسؤول في ",
            ".",
        );
        assert_eq!(viewport[0].visual_cols, rows[1].visual_cols);
        assert_eq!(viewport[0].rtl, rows[1].rtl);

        let lam = cells("ل");
        let alef = cells("ا");
        let split = shape_contextual_paragraph(
            &mut fonts,
            &mut buffer,
            &[&lam, &alef],
            &config,
            metrics,
            "",
            "",
        );
        assert!(
            split.iter().all(|row| !row.glyphs.is_empty()
                && row.glyphs.iter().all(|glyph| glyph.source_col == 0))
        );
    }

    #[test]
    fn full_bitmap_coverage_preserves_rgba_rows_without_clipping() {
        assert_eq!(
            bitmap_coverage(SwashContent::Mask, 3, 2, vec![1, 2, 3, 4, 5, 6]).unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );
        let rgba = vec![9, 8, 7, 1, 6, 5, 4, 2, 3, 2, 1, 3, 7, 8, 9, 4, 1, 2, 3, 5, 4, 5, 6, 6];
        assert_eq!(
            bitmap_coverage(SwashContent::Color, 3, 2, rgba).unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            bitmap_coverage(SwashContent::SubpixelMask, 1, 1, vec![30, 60, 90, 0]).unwrap(),
            vec![60]
        );
        assert_eq!(
            bitmap_coverage(SwashContent::Mask, 3, 2, vec![1; 5]),
            Err(AtlasError::InvalidBitmap)
        );
        assert_eq!(
            bitmap_coverage(SwashContent::Color, 3, 2, vec![1; 6]),
            Err(AtlasError::InvalidBitmap)
        );
    }

    #[test]
    fn atlas_layout_respects_device_limits_and_full_bitmap_extents() {
        let layout = AtlasLayout::new(64, 400, 2048).unwrap();
        assert!(layout.columns * layout.slot_size <= 2048);
        assert!(layout.rows * layout.slot_size <= 2048);
        assert!(layout.capacity() >= 400);
        let uv = layout.uv(399, 62, 60);
        assert!(uv.iter().all(|value| (0.0..=1.0).contains(value)));
        assert!(matches!(AtlasLayout::new(2048, 1, 1024), Err(AtlasError::GlyphTooLarge { .. })));
        assert!(matches!(AtlasLayout::new(64, 257, 1024), Err(AtlasError::FrameTooLarge { .. })));
    }

    #[test]
    fn slot_size_basic() {
        assert!(compute_slot_size(8.0, 16.0) >= 16);
    }

    #[test]
    fn configured_fallback_is_used_before_platform_defaults() {
        let config = FontConfig {
            family: "Mechanic deliberately missing primary font".into(),
            fallback_families: vec!["Georgia".into()],
            ..FontConfig::default()
        };
        let mut fonts = configured_font_system(&config);
        let mut buffer = Buffer::new(&mut fonts, Metrics::new(16.0, 24.0));
        let shaped = shape_contextual_row(
            &mut fonts,
            &mut buffer,
            &cells("é"),
            &config,
            CellMetrics { cell_width: 10.0, cell_height: 24.0, ascent: 18.0 },
        );
        let face = fonts.db().face(shaped.glyphs[0].cache_key.font_id).unwrap();
        assert!(
            face.families.iter().any(|(family, _)| family == "Georgia"),
            "configured fallback not honored: {:?}",
            face.families
        );
    }

    #[test]
    #[ignore = "requires a Metal device; run explicitly on macOS"]
    fn frame_preflight_growth_keeps_every_uv_valid_and_shapes_bounded() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let config = FontConfig { family: "Menlo".into(), ..FontConfig::default() };
        let mut renderer = TextRenderer::new(&device, &queue, &config, 1.0);
        let input: Vec<_> = (0x21..0x180)
            .chain(0x410..0x460)
            .filter_map(char::from_u32)
            .filter(|character| {
                !character.is_control()
                    && unicode_script::UnicodeScript::script(character)
                        != unicode_script::Script::Inherited
            })
            .map(|character| RenderCell { character, ..RenderCell::default() })
            .collect();
        let shaped = renderer.shape_row(&input, &config);
        let keys: Vec<_> = shaped.glyphs.iter().map(|glyph| glyph.cache_key).collect();
        let initial_generation = renderer.atlas_generation();
        renderer.prepare_glyphs(keys.iter().copied(), &device, &queue).unwrap();
        assert!(renderer.atlas_generation() > initial_generation);
        let infos: Vec<_> = keys
            .iter()
            .filter_map(|key| renderer.glyph_info(*key).map(|info| (*key, info)))
            .collect();
        assert!(infos.len() > 128);
        let generation = renderer.atlas_generation();
        for (_, info) in &infos {
            assert!(info.atlas_uv.iter().all(|value| (0.0..=1.0).contains(value)));
            assert!(
                info.glyph_width + (GLYPH_GUTTER * 2) as f32 <= renderer.slot_size as f32
                    && info.glyph_height + (GLYPH_GUTTER * 2) as f32 <= renderer.slot_size as f32
            );
        }
        renderer.prepare_glyphs(keys.iter().copied(), &device, &queue).unwrap();
        assert_eq!(renderer.atlas_generation(), generation);
        for (key, info) in &infos {
            assert_eq!(renderer.glyph_info(*key).unwrap().atlas_uv, info.atlas_uv);
        }
        renderer.prepare_frame(std::slice::from_ref(&shaped), &device, &queue).unwrap();
        renderer.prepare_frame(std::slice::from_ref(&shaped), &device, &queue).unwrap();
        assert_eq!(renderer.atlas_generation(), generation);
        assert!(Arc::ptr_eq(&renderer.prepared_rows[0], &shaped));
        // A different row identity containing only resident glyphs needs no
        // atlas rebuild, and replaces the retained current-frame reference.
        let changed = Arc::new(ShapedRow {
            glyphs: shaped.glyphs.clone(),
            visual_cols: shaped.visual_cols.clone(),
            rtl: shaped.rtl.clone(),
        });
        renderer.prepare_frame(std::slice::from_ref(&changed), &device, &queue).unwrap();
        assert_eq!(renderer.atlas_generation(), generation);
        assert_eq!(renderer.prepared_rows.len(), 1);
        assert!(Arc::ptr_eq(&renderer.prepared_rows[0], &changed));

        let new_row = renderer.shape_row(&cells("中"), &config);
        assert!(new_row.glyphs.iter().any(|glyph| renderer.glyph_info(glyph.cache_key).is_none()));
        // Simulate the atlas reaching its slot capacity before the new glyph.
        // Growth must preflight ALL active rows, including unchanged ones.
        renderer.atlas_capacity_slots = renderer.atlas_next_slot;
        let frame = [Arc::clone(&shaped), Arc::clone(&new_row)];
        renderer.prepare_frame(&frame, &device, &queue).unwrap();
        assert!(renderer.atlas_generation() > generation);
        assert_eq!(renderer.prepared_rows.len(), 2);
        assert!(infos.iter().all(|(key, _)| renderer.glyph_info(*key).is_some()));
        assert!(new_row.glyphs.iter().all(|glyph| renderer.glyph_info(glyph.cache_key).is_some()));

        // A separate atlas rebuild invalidates prior frame-generation proof.
        // Even identical row Arcs must be checked/prepared again afterward.
        let other = renderer.shape_row(&cells("語"), &config);
        renderer.atlas_capacity_slots = renderer.atlas_next_slot;
        renderer
            .prepare_glyphs(other.glyphs.iter().map(|glyph| glyph.cache_key), &device, &queue)
            .unwrap();
        assert_ne!(renderer.prepared_generation, Some(renderer.atlas_generation()));
        renderer.prepare_frame(&frame, &device, &queue).unwrap();
        assert_eq!(renderer.prepared_generation, Some(renderer.atlas_generation()));
        assert!(infos.iter().all(|(key, _)| renderer.glyph_info(*key).is_some()));
        assert!(new_row.glyphs.iter().all(|glyph| renderer.glyph_info(glyph.cache_key).is_some()));
        let mut recolored = input.clone();
        recolored[0].fg = mechanic_config::theme::palette::BLACK;
        assert!(
            Arc::ptr_eq(&renderer.shape_row(&recolored, &config), &shaped),
            "overlay colors should reuse shaping"
        );
        for index in 0..SHAPE_CACHE_ROWS + 32 {
            renderer.shape_row(&cells(&format!("frame {index}")), &config);
        }
        assert!(renderer.shape_cache.len() <= SHAPE_CACHE_ROWS);
        assert!(renderer.shape_cache_bytes <= SHAPE_CACHE_BYTES);

        let mut grid = crate::grid::RenderGrid::new(8, 2);
        for (cell, character) in grid.cells.iter_mut().zip("مرحبا بكم日本語éאבج".chars())
        {
            cell.character = character;
        }
        grid.wrapped[0] = true;
        let first = renderer.shape_grid(&grid, &config);
        let repeated = renderer.shape_grid(&grid, &config);
        assert!(first.iter().zip(&repeated).all(|(a, b)| Arc::ptr_eq(a, b)));

        for change in 0..8 {
            match change {
                0 => grid.cells[0].zerowidth.push('\u{64e}'),
                1 => grid.cells[1].flags.insert(CellFlags::BOLD),
                2 => grid.bidi_prefix = "قبل ".into(),
                3 => grid.bidi_suffix = " بعد".into(),
                4 => grid.wrapped[0] = false,
                5 => grid.cells[2].flags.insert(CellFlags::HIDDEN),
                6 => grid.cells[0].fg = mechanic_config::theme::palette::BLACK,
                _ => grid.cells[0].character = 'ش',
            }
            let cached = renderer.shape_grid(&grid, &config);
            renderer.shape_cache.clear();
            renderer.shape_order.clear();
            renderer.shape_cache_bytes = 0;
            renderer.paragraph_cache.clear();
            renderer.paragraph_order.clear();
            renderer.paragraph_cache_bytes = 0;
            let fresh = renderer.shape_grid(&grid, &config);
            for (cached, fresh) in cached.iter().zip(&fresh) {
                assert_eq!(cached.visual_cols, fresh.visual_cols);
                assert_eq!(cached.rtl, fresh.rtl);
                assert_eq!(cached.glyphs.len(), fresh.glyphs.len());
                for (a, b) in cached.glyphs.iter().zip(&fresh.glyphs) {
                    assert_eq!(
                        (
                            a.cache_key,
                            a.source_col,
                            a.x,
                            a.y,
                            a.scale_x,
                            a.cluster_start,
                            a.cluster_end
                        ),
                        (
                            b.cache_key,
                            b.source_col,
                            b.x,
                            b.y,
                            b.scale_x,
                            b.cluster_start,
                            b.cluster_end
                        ),
                        "cached geometry diverged after change {change}",
                    );
                }
            }
        }
        grid.wrapped[0] = true;
        for index in 0..80 {
            grid.bidi_prefix = format!("paragraph {index} ");
            renderer.shape_grid(&grid, &config);
        }
        // Large contexts exercise the byte limit independently of the entry limit.
        for index in 0..20 {
            grid.bidi_prefix = format!("{index} {}", "x".repeat(64 * 1024));
            renderer.shape_grid(&grid, &config);
        }
        assert!(renderer.paragraph_cache.len() < 20);
        assert_eq!(renderer.paragraph_cache.len(), renderer.paragraph_order.len());
        assert_eq!(
            renderer.paragraph_cache_bytes,
            renderer.paragraph_order.iter().map(|(_, bytes)| bytes).sum::<usize>()
        );
        assert!(renderer.paragraph_cache_bytes <= SHAPE_CACHE_BYTES);
        for (key, _) in &renderer.paragraph_order {
            let (stored, _) = renderer.paragraph_cache.get_key_value(key).unwrap();
            assert!(Arc::ptr_eq(key, stored));
        }
    }

    #[test]
    fn slot_size_floor_at_32() {
        assert!(compute_slot_size(8.0, 16.0) >= 32);
    }

    #[test]
    fn slot_size_large_font_fits() {
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
