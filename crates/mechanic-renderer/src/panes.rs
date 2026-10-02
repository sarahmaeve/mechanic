use std::{collections::HashSet, mem, sync::Arc, time::Instant};

use bytemuck::Zeroable;
use mechanic_config::{font::FontConfig, theme::Rgb};

use super::{
    CURSOR_USE_ATLAS, FrameUniforms, Globals, GpuInstance, RenderProfile, RenderState,
    build_instances_for_rows, instance_cache::InstanceCache, rgb_to_f32,
};
use crate::{
    grid::RenderGrid,
    text::{SHAPING_FLAGS, ShapedRow, TextRenderer},
};

/// A terminal pane's content bounds, in physical window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaneRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PaneRect {
    fn scissor(self, window_size: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
        let width = self.width.min(window_size.0.saturating_sub(self.x));
        let height = self.height.min(window_size.1.saturating_sub(self.y));
        (width > 0 && height > 0).then_some((self.x, self.y, width, height))
    }
}

/// A visible pane. IDs must be unique and remain stable when panes move or resize.
#[derive(Debug, Clone, Copy)]
pub struct RenderPane<'a> {
    pub id: u64,
    pub rect: PaneRect,
    pub grid: &'a RenderGrid,
    pub active: bool,
}

/// A visible resize divider occupying its complete gap between terminal panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderDivider {
    pub rect: PaneRect,
    /// True while the pointer hovers over or drags this divider.
    pub highlighted: bool,
}

/// A pane's reserved drag-header bounds, in physical window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderPaneHandle {
    pub rect: PaneRect,
    /// True while the pointer hovers over or drags this pane handle.
    pub highlighted: bool,
}

/// A title drawn in reserved header bounds. The caller reserves space for any
/// drag grip; glyphs are clipped to this rectangle and centered vertically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderPaneHeader {
    pub rect: PaneRect,
    pub title: String,
    /// `None` uses the theme foreground.
    pub color: Option<Rgb>,
}

struct HeaderLabel {
    grid: RenderGrid,
    rows: Vec<Arc<ShapedRow>>,
}

#[derive(Default)]
pub(super) struct HeaderState {
    requested: Vec<RenderPaneHeader>,
    labels: Vec<HeaderLabel>,
    color: Option<Rgb>,
    metrics: Option<(f32, f32)>,
    generation: Option<u64>,
    instances: Vec<GpuInstance>,
    ranges: Vec<(PaneRect, std::ops::Range<u32>)>,
    buffer: Option<wgpu::Buffer>,
    capacity: usize,
    dirty: bool,
}

impl HeaderState {
    fn set(&mut self, headers: &[RenderPaneHeader]) {
        if self.requested != headers {
            self.requested = headers.to_vec();
            self.invalidate();
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.labels.clear();
        self.dirty = true;
    }

    fn prepare_layouts(&mut self, text: &mut TextRenderer, config: &FontConfig) {
        let metrics = text.cell_metrics();
        let cell_size = (metrics.cell_width, metrics.cell_height);
        if self.metrics != Some(cell_size) {
            self.metrics = Some(cell_size);
            self.invalidate();
        }
        if self.labels.len() == self.requested.len() {
            return;
        }
        self.labels = self
            .requested
            .iter()
            .map(|header| {
                let grid = header_grid(header, self.color.unwrap_or(Rgb::new(173, 255, 255)));
                let rows = text.shape_grid(&grid, config);
                HeaderLabel { grid, rows }
            })
            .collect();
    }

    fn update(&mut self, text: &TextRenderer, device: &wgpu::Device, queue: &wgpu::Queue) -> usize {
        if !self.dirty && self.generation == Some(text.atlas_generation()) {
            return 0;
        }
        let Some(cell_size) = self.metrics else { return 0 };
        self.instances.clear();
        self.ranges.clear();
        for (header, label) in self.requested.iter().zip(&self.labels) {
            let first = self.instances.len() as u32;
            if header.rect.width > 0 && header.rect.height > 0 {
                let (instances, backgrounds) = build_instances_for_rows(
                    &label.grid,
                    &label.rows,
                    text,
                    cell_size,
                    false,
                    0..1,
                    Vec::new(),
                );
                let scale = (header.rect.height as f32 / cell_size.1).min(1.0);
                let y =
                    header.rect.y as f32 + (header.rect.height as f32 - cell_size.1 * scale) * 0.5;
                self.instances.extend(instances.into_iter().skip(backgrounds as usize).map(
                    |mut instance| {
                        instance.glyph_offset = [
                            header.rect.x as f32
                                + (instance.cell_pos[0] as f32 * cell_size.0
                                    + instance.glyph_offset[0])
                                    * scale,
                            y + (instance.cell_pos[1] as f32 * cell_size.1
                                + instance.glyph_offset[1])
                                * scale,
                        ];
                        instance.glyph_size =
                            [instance.glyph_size[0] * scale, instance.glyph_size[1] * scale];
                        instance.cell_pos = [0, 0];
                        instance
                    },
                ));
            }
            self.ranges.push((header.rect, first..self.instances.len() as u32));
        }
        if self.instances.len() > self.capacity {
            self.capacity = self.instances.len().next_power_of_two().max(4);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pane_header_instances"),
                size: (self.capacity * mem::size_of::<GpuInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let bytes = bytemuck::cast_slice::<GpuInstance, u8>(&self.instances);
        if !bytes.is_empty() {
            queue.write_buffer(self.buffer.as_ref().unwrap(), 0, bytes);
        }
        self.generation = Some(text.atlas_generation());
        self.dirty = false;
        bytes.len()
    }
}

fn header_grid(header: &RenderPaneHeader, fallback: Rgb) -> RenderGrid {
    use swash::text::{Category, Codepoint};
    // Bound shaping even if an external title producer sends an enormous title.
    let mut cells: Vec<crate::grid::RenderCell> = Vec::new();
    for ch in header.title.chars().take(4096).filter(|ch| !ch.is_control()) {
        if matches!(ch.category(), Category::NonspacingMark | Category::EnclosingMark)
            && let Some(cell) = cells.last_mut()
        {
            cell.zerowidth.push(ch);
        } else {
            cells.push(crate::grid::RenderCell {
                character: ch,
                fg: header.color.unwrap_or(fallback),
                ..Default::default()
            });
        }
    }
    let mut grid = RenderGrid::new(cells.len().max(1), 1);
    if !cells.is_empty() {
        grid.cells = cells;
    }
    grid.cursor_visible = false;
    grid
}

/// One retained buffer for static pane dividers, drag grips and drop outlines.
#[derive(Default)]
pub(super) struct DividerState {
    requested: Vec<RenderDivider>,
    handles: Vec<RenderPaneHandle>,
    drop_preview: Option<PaneRect>,
    instances: Vec<GpuInstance>,
    instance_buf: Option<wgpu::Buffer>,
    capacity: usize,
    colors: Option<(Rgb, Rgb)>,
    dirty: bool,
}

impl DividerState {
    fn set(&mut self, dividers: &[RenderDivider]) {
        if self.requested == dividers {
            return;
        }
        self.requested.clear();
        self.requested.extend_from_slice(dividers);
        self.dirty = true;
    }

    fn set_handles(&mut self, handles: &[RenderPaneHandle]) {
        if self.handles == handles {
            return;
        }
        self.handles.clear();
        self.handles.extend_from_slice(handles);
        self.dirty = true;
    }

    fn set_drop_preview(&mut self, preview: Option<PaneRect>) {
        if self.drop_preview != preview {
            self.drop_preview = preview;
            self.dirty = true;
        }
    }

    fn update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        mut profile: Option<&mut RenderProfile>,
    ) -> usize {
        if !self.dirty {
            if let Some(profile) = profile {
                profile.instance_count += self.instances.len();
            }
            return 0;
        }
        let instances_started = profile.as_ref().map(|_| Instant::now());
        let (idle, active) = self.colors.unwrap_or((Rgb::new(29, 81, 89), Rgb::new(173, 255, 255)));
        self.instances.clear();
        self.instances.extend(
            self.requested
                .iter()
                .filter(|divider| divider.rect.width > 0 && divider.rect.height > 0)
                .map(|divider| GpuInstance {
                    bg_color: rgb_to_f32(if divider.highlighted { active } else { idle }),
                    glyph_offset: [divider.rect.x as f32, divider.rect.y as f32],
                    glyph_size: [divider.rect.width as f32, divider.rect.height as f32],
                    use_atlas: CURSOR_USE_ATLAS,
                    ..GpuInstance::zeroed()
                }),
        );
        for handle in &self.handles {
            if handle.rect.width > 0 && handle.rect.height > 0 {
                self.instances.extend(handle_instances(
                    *handle,
                    if handle.highlighted { active } else { idle },
                ));
            }
        }
        if let Some(rect) = self.drop_preview
            && rect.width > 0
            && rect.height > 0
        {
            // The app reserves an 18-logical-pixel header; its physical height
            // gives the preview the same scale as the grip without a new uniform.
            let scale = self.handles.first().map_or(1.0, |handle| handle.rect.height as f32 / 18.0);
            self.instances.extend(drop_preview_instances(rect, active, scale));
        }
        if let (Some(profile), Some(started)) = (profile.as_deref_mut(), instances_started) {
            profile.instances_ns += started.elapsed().as_nanos();
            profile.instance_count += self.instances.len();
        }
        let upload_started = profile.as_ref().map(|_| Instant::now());
        let count = self.instances.len();
        if count > self.capacity {
            self.capacity = count.next_power_of_two().max(4);
            self.instance_buf = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pane_decoration_instances"),
                size: (self.capacity * mem::size_of::<GpuInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let bytes = bytemuck::cast_slice::<GpuInstance, u8>(&self.instances);
        if !bytes.is_empty() {
            queue.write_buffer(self.instance_buf.as_ref().unwrap(), 0, bytes);
        }
        self.dirty = false;
        if let (Some(profile), Some(started)) = (profile, upload_started) {
            profile.upload_ns += started.elapsed().as_nanos();
            profile.upload_bytes += bytes.len();
        }
        bytes.len()
    }
}

fn handle_instances(handle: RenderPaneHandle, color: Rgb) -> [GpuInstance; 3] {
    let width = handle.rect.width as f32;
    let height = handle.rect.height as f32;
    let bar_width = (height * 0.5).min(width);
    let bar_height = (height / 18.0).round().max(1.0).min(height / 6.0);
    let x = handle.rect.x as f32 + (width - bar_width) * 0.5;
    let y = handle.rect.y as f32 + (height - bar_height * 6.0) * 0.5;
    std::array::from_fn(|index| GpuInstance {
        bg_color: rgb_to_f32(color),
        glyph_offset: [x, y + index as f32 * bar_height * 2.5],
        glyph_size: [bar_width, bar_height],
        use_atlas: CURSOR_USE_ATLAS,
        ..GpuInstance::zeroed()
    })
}

fn drop_preview_instances(rect: PaneRect, color: Rgb, scale: f32) -> [GpuInstance; 4] {
    let width = rect.width as f32;
    let height = rect.height as f32;
    let thickness = (scale * 2.0).round().max(2.0).min(width * 0.5).min(height * 0.5);
    let x = rect.x as f32;
    let y = rect.y as f32;
    let base = GpuInstance {
        bg_color: rgb_to_f32(color),
        use_atlas: CURSOR_USE_ATLAS,
        ..GpuInstance::zeroed()
    };
    [
        GpuInstance { glyph_offset: [x, y], glyph_size: [width, thickness], ..base },
        GpuInstance {
            glyph_offset: [x, y + height - thickness],
            glyph_size: [width, thickness],
            ..base
        },
        GpuInstance { glyph_offset: [x, y], glyph_size: [thickness, height], ..base },
        GpuInstance {
            glyph_offset: [x + width - thickness, y],
            glyph_size: [thickness, height],
            ..base
        },
    ]
}

/// Retain the actual shaped rows even if the shared text cache evicts them.
/// Appearance and cursor updates do not change this key.
#[derive(Default)]
struct PaneLayout {
    key: Option<LayoutKey>,
    rows: Vec<Arc<ShapedRow>>,
}

struct LayoutKey {
    cols: usize,
    rows: usize,
    cells: Vec<(char, String, crate::grid::CellFlags)>,
    wrapped: Vec<bool>,
    prefix: String,
    suffix: String,
}

impl LayoutKey {
    fn matches(&self, grid: &RenderGrid) -> bool {
        self.cols == grid.cols
            && self.rows == grid.rows
            && self.wrapped == grid.wrapped
            && self.prefix == grid.bidi_prefix
            && self.suffix == grid.bidi_suffix
            && self.cells.len() == grid.cells.len()
            && self.cells.iter().zip(&grid.cells).all(|((ch, marks, flags), cell)| {
                *ch == cell.character
                    && *marks == cell.zerowidth
                    && *flags == cell.flags & SHAPING_FLAGS
            })
    }

    fn new(grid: &RenderGrid) -> Self {
        Self {
            cols: grid.cols,
            rows: grid.rows,
            cells: grid
                .cells
                .iter()
                .map(|cell| (cell.character, cell.zerowidth.clone(), cell.flags & SHAPING_FLAGS))
                .collect(),
            wrapped: grid.wrapped.clone(),
            prefix: grid.bidi_prefix.clone(),
            suffix: grid.bidi_suffix.clone(),
        }
    }
}

impl PaneLayout {
    fn prepare(&mut self, grid: &RenderGrid, text: &mut TextRenderer, config: &FontConfig) {
        if self.key.as_ref().is_some_and(|key| key.matches(grid)) {
            return;
        }
        self.rows = text.shape_grid(grid, config);
        self.key = Some(LayoutKey::new(grid));
    }

    fn visual_column(&self, col: usize, row: usize) -> usize {
        self.rows.get(row).and_then(|row| row.visual_cols.get(col)).copied().unwrap_or(col)
    }

    fn logical_column(&self, col: usize, row: usize) -> (usize, bool) {
        self.rows
            .get(row)
            .and_then(|row| {
                row.visual_cols
                    .iter()
                    .position(|visual| *visual == col)
                    .map(|logical| (logical, row.rtl[logical]))
            })
            .unwrap_or((col, false))
    }
}

pub(super) struct PaneState {
    layout: PaneLayout,
    cache: InstanceCache,
    globals_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    rect: PaneRect,
    active: bool,
    instance_count: u32,
    border_count: u32,
    border_key: Option<BorderKey>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct BorderKey {
    rect: PaneRect,
    active: bool,
    visible: bool,
    instance_count: usize,
    color: Rgb,
}

struct PaneUploadContext<'a> {
    cell_size: (f32, f32),
    focused: bool,
    borders: bool,
    colors: (Rgb, Rgb),
    outline_colors: &'a std::collections::HashMap<u64, Rgb>,
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
}

impl PaneState {
    fn new(state: &RenderState, atlas: &wgpu::TextureView) -> Self {
        let globals_buf = state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pane_globals"),
            size: mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = RenderState::make_bind_group(
            &state.device,
            &state.bind_group_layout,
            &globals_buf,
            atlas,
            &state.sampler,
            &state.logo.view,
        );
        let instance_capacity = 256;
        let instance_buf = state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pane_instances"),
            size: (instance_capacity * mem::size_of::<GpuInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            layout: PaneLayout::default(),
            cache: InstanceCache::default(),
            globals_buf,
            bind_group,
            instance_buf,
            instance_capacity,
            rect: PaneRect::default(),
            active: false,
            instance_count: 0,
            border_count: 0,
            border_key: None,
        }
    }

    pub(super) fn update_bind_group(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        atlas: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        logo: &wgpu::TextureView,
    ) {
        self.bind_group =
            RenderState::make_bind_group(device, layout, &self.globals_buf, atlas, sampler, logo);
    }

    pub(super) fn invalidate_layout(&mut self) {
        self.layout = PaneLayout::default();
        self.cache.invalidate();
    }

    fn update_instances(
        &mut self,
        pane: &RenderPane<'_>,
        text: &TextRenderer,
        context: &PaneUploadContext<'_>,
        mut profile: Option<&mut RenderProfile>,
    ) -> usize {
        let PaneUploadContext {
            cell_size,
            focused,
            borders,
            colors,
            outline_colors,
            device,
            queue,
        } = *context;
        let override_color = outline_colors.get(&pane.id).copied();
        let borders = borders || override_color.is_some();
        let color = override_color.unwrap_or(if pane.active { colors.0 } else { colors.1 });
        let focused = focused && pane.active;
        let instances_started = profile.as_ref().map(|_| Instant::now());
        let mut uploads = self.cache.update(
            pane.grid,
            &self.layout.rows,
            (text.atlas_generation(), cell_size),
            focused,
            |row, reusable| {
                build_instances_for_rows(
                    pane.grid,
                    &self.layout.rows,
                    text,
                    cell_size,
                    focused,
                    row..row + 1,
                    reusable,
                )
                .0
            },
        );
        if let (Some(profile), Some(started)) = (profile.as_deref_mut(), instances_started) {
            profile.instances_ns += started.elapsed().as_nanos();
        }
        let upload_started = profile.as_ref().map(|_| Instant::now());
        let count = self.cache.instances.len();
        let required = count + 4;
        if required > self.instance_capacity {
            self.instance_capacity = required.next_power_of_two();
            self.instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pane_instances"),
                size: (self.instance_capacity * mem::size_of::<GpuInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            uploads.clear();
            uploads.push(0..count);
            self.border_key = None;
        }
        let mut uploaded = 0;
        for range in uploads {
            let bytes =
                bytemuck::cast_slice::<GpuInstance, u8>(&self.cache.instances[range.clone()]);
            if !bytes.is_empty() {
                queue.write_buffer(
                    &self.instance_buf,
                    (range.start * mem::size_of::<GpuInstance>()) as u64,
                    bytes,
                );
                uploaded += bytes.len();
            }
        }
        let key = BorderKey {
            rect: pane.rect,
            active: pane.active,
            visible: borders,
            instance_count: count,
            color,
        };
        if self.border_key != Some(key) {
            let border = border_instances(pane.rect, color);
            self.border_count =
                if borders && pane.rect.width > 0 && pane.rect.height > 0 { 4 } else { 0 };
            if self.border_count > 0 {
                let bytes = bytemuck::cast_slice::<GpuInstance, u8>(&border);
                queue.write_buffer(
                    &self.instance_buf,
                    (count * mem::size_of::<GpuInstance>()) as u64,
                    bytes,
                );
                uploaded += bytes.len();
            }
            self.border_key = Some(key);
        }
        self.rect = pane.rect;
        self.active = pane.active;
        self.instance_count = count as u32;
        if let (Some(profile), Some(started)) = (profile, upload_started) {
            profile.upload_ns += started.elapsed().as_nanos();
            profile.upload_bytes += uploaded;
            profile.instance_count += count + self.border_count as usize;
        }
        uploaded
    }
}

fn border_instances(rect: PaneRect, color: Rgb) -> [GpuInstance; 4] {
    let width = rect.width as f32;
    let height = rect.height as f32;
    let thickness = 1.0f32.min(width).min(height);
    let base = GpuInstance {
        bg_color: rgb_to_f32(color),
        use_atlas: CURSOR_USE_ATLAS,
        ..GpuInstance::zeroed()
    };
    [
        GpuInstance { glyph_size: [width, thickness], ..base },
        GpuInstance {
            glyph_offset: [0.0, height - thickness],
            glyph_size: [width, thickness],
            ..base
        },
        GpuInstance { glyph_size: [thickness, height], ..base },
        GpuInstance {
            glyph_offset: [width - thickness, 0.0],
            glyph_size: [thickness, height],
            ..base
        },
    ]
}

impl RenderState {
    pub(crate) fn copy_pane_settings_from(&mut self, previous: &RenderState) {
        let waker = previous.device_lost_waker.lock().unwrap().clone();
        if let Some(waker) = waker {
            self.set_device_lost_waker(waker);
        }
        self.pane_colors = previous.pane_colors;
        self.pane_outline_colors = previous.pane_outline_colors.clone();
        self.dividers.colors = previous.dividers.colors;
        self.dividers.set(&previous.dividers.requested);
        self.dividers.set_handles(&previous.dividers.handles);
        self.dividers.set_drop_preview(previous.dividers.drop_preview);
        self.headers.color = previous.headers.color;
        self.headers.set(&previous.headers.requested);
    }

    pub fn set_pane_headers(&mut self, headers: &[RenderPaneHeader]) {
        if self.headers.requested != headers {
            self.pane_frame_cached = false;
        }
        self.headers.set(headers);
    }

    pub fn set_pane_header_color(&mut self, color: Rgb) {
        if self.headers.color != Some(color) {
            self.headers.color = Some(color);
            self.headers.invalidate();
            self.pane_frame_cached = false;
        }
    }

    pub fn set_pane_outline_colors(&mut self, colors: &[(u64, Option<Rgb>)]) {
        let colors: std::collections::HashMap<_, _> =
            colors.iter().filter_map(|(id, color)| color.map(|color| (*id, color))).collect();
        if self.pane_outline_colors != colors {
            self.pane_outline_colors = colors;
            self.pane_frame_cached = false;
        }
    }

    pub fn set_pane_dividers(&mut self, dividers: &[RenderDivider]) {
        self.dividers.set(dividers);
    }

    pub fn set_pane_handles(&mut self, handles: &[RenderPaneHandle]) {
        self.dividers.set_handles(handles);
    }

    pub fn set_pane_drop_preview(&mut self, preview: Option<PaneRect>) {
        self.dividers.set_drop_preview(preview);
    }

    pub fn set_divider_colors(&mut self, foreground: Rgb, background: Rgb, active: Rgb) {
        let muted = |fg: u8, bg: u8| ((u16::from(fg) * 35 + u16::from(bg) * 65) / 100) as u8;
        let idle = Rgb::new(
            muted(foreground.r, background.r),
            muted(foreground.g, background.g),
            muted(foreground.b, background.b),
        );
        if self.dividers.colors != Some((idle, active)) {
            self.dividers.colors = Some((idle, active));
            self.dividers.dirty = true;
        }
    }

    pub fn set_pane_colors(&mut self, active: Rgb, inactive: Rgb) {
        self.pane_colors = (active, inactive);
    }

    pub fn prepare_pane_layout(
        &mut self,
        id: u64,
        grid: &RenderGrid,
        text: &mut TextRenderer,
        config: &FontConfig,
    ) {
        if !self.panes.contains_key(&id) {
            let pane = PaneState::new(self, &text.atlas_view);
            self.panes.insert(id, pane);
        }
        self.panes.get_mut(&id).unwrap().layout.prepare(grid, text, config);
    }

    pub fn pane_visual_column(&self, id: u64, col: usize, row: usize) -> usize {
        self.panes.get(&id).map_or(col, |pane| pane.layout.visual_column(col, row))
    }

    pub fn pane_logical_column(&self, id: u64, col: usize, row: usize) -> (usize, bool) {
        self.panes.get(&id).map_or((col, false), |pane| pane.layout.logical_column(col, row))
    }

    /// Shared atlas preflight happens before any pane creates instances with UVs.
    #[cfg(test)]
    fn prepare_panes_frame(
        &mut self,
        panes: &[RenderPane<'_>],
        text: &mut TextRenderer,
        config: &FontConfig,
        uniforms: FrameUniforms,
    ) -> Option<usize> {
        self.prepare_panes_frame_profiled(panes, text, config, uniforms, None)
    }

    fn prepare_panes_frame_profiled(
        &mut self,
        panes: &[RenderPane<'_>],
        text: &mut TextRenderer,
        config: &FontConfig,
        uniforms: FrameUniforms,
        mut profile: Option<&mut RenderProfile>,
    ) -> Option<usize> {
        let visible: HashSet<_> = panes.iter().map(|pane| pane.id).collect();
        if visible.len() != panes.len() {
            log::error!("duplicate pane IDs in render scene");
            return None;
        }
        self.panes.retain(|id, _| visible.contains(id));
        self.pane_order.clear();
        // Allocate newly visible pane resources before timing atlas/shaping work.
        // Warm frames retain these resources; this setup is independent of glyphs.
        for pane in panes {
            if !self.panes.contains_key(&pane.id) {
                let state = PaneState::new(self, &text.atlas_view);
                self.panes.insert(pane.id, state);
            }
        }
        let atlas_started = profile.as_ref().map(|_| Instant::now());
        for pane in panes {
            self.prepare_pane_layout(pane.id, pane.grid, text, config);
            self.pane_order.push(pane.id);
        }
        self.headers.prepare_layouts(text, config);
        let mut rows: Vec<_> = self
            .pane_order
            .iter()
            .flat_map(|id| self.panes[id].layout.rows.iter().cloned())
            .collect();
        rows.extend(self.headers.labels.iter().flat_map(|label| label.rows.iter().cloned()));
        let atlas_failed = match text.prepare_frame(&rows, &self.device, &self.queue) {
            Ok(()) => false,
            Err(error) => {
                log::error!("pane glyph atlas preparation failed: {error}");
                true
            }
        };
        let generation = text.atlas_generation();
        let atlas_changed = generation != self.last_atlas_generation;
        if atlas_changed {
            self.update_atlas_bind_group(&text.atlas_view);
            self.last_atlas_generation = generation;
        }
        if let (Some(profile), Some(started)) = (profile.as_deref_mut(), atlas_started) {
            profile.atlas_ns += started.elapsed().as_nanos();
            profile.atlas_changed = atlas_changed;
        }
        let mut uploaded = 0;
        let context = PaneUploadContext {
            cell_size: self.cell_size,
            focused: uniforms.window_focused,
            borders: panes.len() > 1,
            colors: self.pane_colors,
            outline_colors: &self.pane_outline_colors,
            device: &self.device,
            queue: &self.queue,
        };
        for pane in panes {
            let state = self.panes.get_mut(&pane.id).unwrap();
            if atlas_failed {
                state.cache.invalidate();
            }
            uploaded += state.update_instances(pane, text, &context, profile.as_deref_mut());
            if atlas_failed {
                state.cache.invalidate();
            }
        }
        if atlas_failed {
            self.headers.dirty = true;
        }
        let header_bytes = self.headers.update(text, &self.device, &self.queue);
        uploaded += header_bytes;
        if let Some(profile) = profile {
            profile.upload_bytes += header_bytes;
            profile.instance_count += self.headers.instances.len();
        }
        if atlas_failed {
            self.headers.dirty = true;
        }
        Some(uploaded)
    }

    pub fn render_panes(
        &mut self,
        panes: &[RenderPane<'_>],
        text: &mut TextRenderer,
        config: &FontConfig,
        uniforms: FrameUniforms,
    ) -> bool {
        self.pane_frame_cached = false;
        self.last_instance_count = 0;
        if self.device_lost.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let profiling = log::log_enabled!(target: "mechanic_render_profile", log::Level::Trace);
        let mut profile = profiling.then(RenderProfile::default);
        if self
            .prepare_panes_frame_profiled(panes, text, config, uniforms, profile.as_mut())
            .is_none()
        {
            if let Some(profile) = profile {
                profile.log(false);
            }
            return false;
        }
        let presented = self.present_pane_frame(uniforms, profile.as_mut());
        self.pane_frame_cached = presented;
        if let Some(profile) = profile {
            profile.log(presented);
        }
        presented
    }

    pub(super) fn render_pane_animation(&mut self, uniforms: FrameUniforms) -> bool {
        self.present_pane_frame(uniforms, None)
    }

    fn present_pane_frame(
        &mut self,
        uniforms: FrameUniforms,
        mut profile: Option<&mut RenderProfile>,
    ) -> bool {
        if self.device_lost.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        self.dividers.update(&self.device, &self.queue, profile.as_deref_mut());
        let upload_started = profile.as_ref().map(|_| Instant::now());
        self.write_pane_uniforms(uniforms);
        if let (Some(profile), Some(started)) = (profile.as_deref_mut(), upload_started) {
            profile.upload_ns += started.elapsed().as_nanos();
            profile.upload_bytes += (self.pane_order.len()
                + usize::from(
                    !self.dividers.instances.is_empty() || !self.headers.instances.is_empty(),
                ))
                * mem::size_of::<Globals>();
        }
        let surface_started = profile.as_ref().map(|_| Instant::now());
        let surface = self.acquire_surface_texture();
        if let (Some(profile), Some(started)) = (profile.as_deref_mut(), surface_started) {
            profile.surface_ns = started.elapsed().as_nanos();
        }
        let Some(surface) = surface else {
            return false;
        };
        let submit_present_started = profile.as_ref().map(|_| Instant::now());
        let view = surface.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pane_frame_encoder"),
        });
        self.draw_panes(&mut encoder, &view, uniforms);
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(surface);
        if let (Some(profile), Some(started)) = (profile, submit_present_started) {
            profile.submit_present_ns = started.elapsed().as_nanos();
        }
        true
    }

    fn pane_globals(&self, rect: PaneRect, uniforms: FrameUniforms) -> Globals {
        Globals {
            viewport_size: [self.size.0 as f32, self.size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time: uniforms.time,
            content_opacity: uniforms.content_opacity,
            animation_flags: uniforms.animation_flags(),
            text_opacity: uniforms.text_opacity,
            bloom_progress: uniforms.bloom_progress,
            bloom_peak_multiplier: uniforms.bloom_peak_multiplier,
            logo_size: f32::from(uniforms.logo_size),
            logo_style: self.logo.style as u32,
            pane_origin: [rect.x as f32, rect.y as f32],
            _padding: [0.0; 2],
        }
    }

    fn write_pane_uniforms(&self, uniforms: FrameUniforms) {
        for id in &self.pane_order {
            let pane = &self.panes[id];
            let globals = self.pane_globals(pane.rect, uniforms);
            self.queue.write_buffer(&pane.globals_buf, 0, bytemuck::bytes_of(&globals));
        }
        if !self.dividers.instances.is_empty() || !self.headers.instances.is_empty() {
            let globals = self.pane_globals(PaneRect::default(), uniforms);
            self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
        }
    }

    fn draw_panes(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        uniforms: FrameUniforms,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pane_cell_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        a: uniforms.content_opacity as f64,
                        ..self.clear_color
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        for id in &self.pane_order {
            let pane = &self.panes[id];
            let Some((x, y, width, height)) = pane.rect.scissor(self.size) else {
                continue;
            };
            pass.set_scissor_rect(x, y, width, height);
            pass.set_bind_group(0, &pane.bind_group, &[]);
            pass.set_vertex_buffer(0, pane.instance_buf.slice(..));
            pass.set_pipeline(&self.pipeline);
            pass.draw(0..6, 0..pane.cache.background_count);
            pass.set_pipeline(&self.foreground_pipeline);
            pass.draw(0..6, pane.cache.background_count..pane.instance_count + pane.border_count);
        }
        if let Some(buffer) = &self.dividers.instance_buf
            && !self.dividers.instances.is_empty()
        {
            pass.set_scissor_rect(0, 0, self.size.0, self.size.1);
            pass.set_pipeline(&self.foreground_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..6, 0..self.dividers.instances.len() as u32);
        }
        if let Some(buffer) = &self.headers.buffer {
            pass.set_pipeline(&self.foreground_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..));
            for (rect, range) in &self.headers.ranges {
                if let Some((x, y, width, height)) = rect.scissor(self.size) {
                    pass.set_scissor_rect(x, y, width, height);
                    pass.draw(0..6, range.clone());
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "panes_gpu_tests.rs"]
mod gpu_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_title_bounds_controls_and_combining_marks() {
        let header = RenderPaneHeader {
            rect: PaneRect::default(),
            title: "e\u{301}\n\u{0}x".into(),
            color: Some(Rgb::new(1, 2, 3)),
        };
        let grid = header_grid(&header, Rgb::new(9, 8, 7));
        assert_eq!(grid.cols, 2);
        assert_eq!(grid.cells[0].character, 'e');
        assert_eq!(grid.cells[0].zerowidth, "\u{301}");
        assert!(grid.cells.iter().all(|cell| cell.fg == Rgb::new(1, 2, 3)));
        assert!(!grid.cursor_visible);
        let bounded = header_grid(
            &RenderPaneHeader { title: "x".repeat(10000), ..header },
            Rgb::new(0, 0, 0),
        );
        assert_eq!(bounded.cols, 4096);
    }

    #[test]
    fn pane_scissors_are_clamped_to_window() {
        assert_eq!(
            PaneRect { x: 5, y: 7, width: 20, height: 20 }.scissor((16, 16)),
            Some((5, 7, 11, 9))
        );
        assert_eq!(PaneRect { x: u32::MAX, y: 0, width: 10, height: 10 }.scissor((16, 16)), None);
        assert_eq!(PaneRect { width: 0, height: 10, ..Default::default() }.scissor((16, 16)), None);
    }

    #[test]
    fn layout_key_tracks_shaping_and_paragraph_context() {
        let mut grid = RenderGrid::new(4, 2);
        let key = LayoutKey::new(&grid);
        grid.cursor_position = (2, 1);
        grid.cells[0].fg = Rgb::new(1, 2, 3);
        grid.cells[0].flags |= crate::grid::CellFlags::UNDERLINE;
        assert!(key.matches(&grid));
        grid.cells[0].zerowidth.push('\u{301}');
        assert!(!key.matches(&grid));
        grid.cells[0].zerowidth.clear();
        grid.wrapped[0] = true;
        assert!(!key.matches(&grid));
        grid.wrapped[0] = false;
        grid.bidi_prefix = "عربي".into();
        assert!(!key.matches(&grid));
    }

    #[test]
    fn unchanged_divider_input_preserves_geometry_cache() {
        let mut state = DividerState::default();
        let divider = RenderDivider {
            rect: PaneRect { x: 20, y: 0, width: 12, height: 40 },
            highlighted: false,
        };
        state.set(&[divider]);
        assert!(state.dirty);
        state.dirty = false;
        state.set(&[divider]);
        assert!(!state.dirty);
        state.set(&[RenderDivider { highlighted: true, ..divider }]);
        assert!(state.dirty);
        state.dirty = false;
        state.set(&[]);
        assert!(state.dirty);
        assert!(state.requested.is_empty());
    }

    #[test]
    fn grip_and_preview_inputs_only_dirty_changed_decorations() {
        let mut state = DividerState::default();
        let handle = RenderPaneHandle {
            rect: PaneRect { width: 100, height: 36, ..Default::default() },
            highlighted: false,
        };
        state.set_handles(&[handle]);
        assert!(state.dirty);
        state.dirty = false;
        state.set_handles(&[handle]);
        state.set_drop_preview(None);
        assert!(!state.dirty);
        let preview = PaneRect { x: 10, y: 20, width: 40, height: 50 };
        state.set_drop_preview(Some(preview));
        assert!(state.dirty);
        state.dirty = false;
        state.set_drop_preview(Some(preview));
        assert!(!state.dirty);
        state.set_handles(&[RenderPaneHandle { highlighted: true, ..handle }]);
        assert!(state.dirty);
    }
}
