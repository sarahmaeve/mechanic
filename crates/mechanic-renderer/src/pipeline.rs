use std::mem;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use wgpu::util::DeviceExt as _;

use crate::{
    background,
    grid::RenderGrid,
    logo::Logo,
    text::{CellMetrics, TextRenderer},
};
use mechanic_config::theme::Rgb;

#[path = "instance_cache.rs"]
mod instance_cache;
use instance_cache::InstanceCache;

#[path = "panes.rs"]
mod panes;
pub use panes::{PaneRect, RenderDivider, RenderPane, RenderPaneHandle};

/// Instanced vertex data. Field offsets must match the shader attributes.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct GpuInstance {
    /// Grid position: `(col, row)`.
    pub cell_pos: [u32; 2],
    /// Atlas UV rect covering the actual glyph bitmap: `(u_min, v_min, u_max, v_max)`.
    pub atlas_uv: [f32; 4],
    /// Foreground color (r, g, b, a) in [0, 1].
    pub fg_color: [f32; 4],
    /// Background color (r, g, b, a) in [0, 1].
    pub bg_color: [f32; 4],
    /// Pixel offset from cell origin to glyph quad origin.
    pub glyph_offset: [f32; 2],
    /// Pixel size of the glyph quad (0, 0 for background instances).
    pub glyph_size: [f32; 2],
    /// 1 → sample atlas; 0 → solid background.
    pub use_atlas: u32,
    /// Reserved vertex attributes; the Rust struct has four-byte alignment.
    pub _pad: [u32; 3],
}

fn rgb_to_f32(c: Rgb) -> [f32; 4] {
    [f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0, 1.0]
}

fn effective_fg(cell: &crate::grid::RenderCell) -> Rgb {
    if cell.flags.contains(crate::grid::CellFlags::INVERSE) { cell.bg } else { cell.fg }
}

fn clip_glyph(
    left: f32,
    width: f32,
    mut uv: [f32; 4],
    clip_left: f32,
    clip_right: f32,
) -> Option<(f32, f32, [f32; 4])> {
    let x = left.max(clip_left);
    let right = (left + width).min(clip_right);
    if width <= 0.0 || right <= x {
        return None;
    }
    let du = uv[2] - uv[0];
    uv[2] = uv[0] + du * (right - left) / width;
    uv[0] += du * (x - left) / width;
    Some((x, right - x, uv))
}

/// Opt-in host-side stage timings. Surface acquisition and submit/present can
/// include driver waits; these numbers do not measure completed GPU execution.
#[derive(Default)]
struct RenderProfile {
    atlas_ns: u128,
    instances_ns: u128,
    upload_ns: u128,
    surface_ns: u128,
    submit_present_ns: u128,
    instance_count: usize,
    upload_bytes: usize,
    atlas_changed: bool,
}

impl RenderProfile {
    fn log(&self, presented: bool) {
        log::trace!(target: "mechanic_render_profile",
            "render-profile atlas_ns={} instances_ns={} upload_ns={} surface_ns={} submit_present_ns={} instance_count={} upload_bytes={} atlas_changed={} presented={}",
            self.atlas_ns,
            self.instances_ns,
            self.upload_ns,
            self.surface_ns,
            self.submit_present_ns,
            self.instance_count,
            self.upload_bytes,
            u8::from(self.atlas_changed),
            u8::from(presented),
        );
    }
}

/// Solid background/cursor shader branch.
const SOLID_USE_ATLAS: u32 = 0;

/// Glyph shader branch.
const GLYPH_USE_ATLAS: u32 = 1;

/// Hollow block cursor for unfocused windows.
const HOLLOW_BLOCK_USE_ATLAS: u32 = 2;

const CURSOR_USE_ATLAS: u32 = 3;

/// Cursor border in physical pixels; must match cell.wgsl.
#[expect(dead_code, reason = "used in the shader; recorded here so both sides drift together")]
const HOLLOW_CURSOR_BORDER_PX: f32 = 1.5;

/// Per-frame shader inputs.
#[derive(Debug, Clone, Copy)]
pub struct FrameUniforms {
    /// Logo square size in physical pixels; zero hides it.
    pub logo_size: u16,
    /// Surface opacity, from 0 (transparent) to 1 (opaque).
    pub content_opacity: f32,
    /// Glyph coverage multiplier; does not affect backgrounds.
    pub text_opacity: f32,
    /// Seconds since window creation.
    pub time: f32,
    pub animate_background: bool,
    pub animate_logo: bool,
    /// OS keyboard focus, independent of the animation flag.
    pub window_focused: bool,
    /// Focus bloom progress in [0, 1]; zero when inactive.
    pub bloom_progress: f32,
    /// Logo opacity multiplier at the bloom's midpoint.
    pub bloom_peak_multiplier: f32,
}

impl FrameUniforms {
    fn animation_flags(self) -> u32 {
        u32::from(self.animate_background) | (u32::from(self.animate_logo) << 1)
    }
}

/// GPU-side mirror of [`FrameUniforms`], laid out to match the `Globals` struct in `cell.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Globals {
    viewport_size: [f32; 2],
    cell_size: [f32; 2],
    time: f32,
    content_opacity: f32,
    /// Bit 0: background; bit 1: logo.
    animation_flags: u32,
    /// Glyph-coverage multiplier for the text path.  1.0 for focused windows; configurable idle value for blurred windows.  See [`FrameUniforms::text_opacity`].
    text_opacity: f32,
    /// Progress through the focus-gain bloom in `[0, 1]`.  See [`FrameUniforms::bloom_progress`].
    bloom_progress: f32,
    /// Peak multiplier applied to logo opacity at bloom midpoint. See [`FrameUniforms::bloom_peak_multiplier`].
    bloom_peak_multiplier: f32,
    logo_size: f32,
    logo_style: u32,
    /// Physical origin of this pane in the window's shared viewport.
    pane_origin: [f32; 2],
    _padding: [f32; 2],
}

/// Intermediate result of `init_surface`: device/queue/surface ready, but no pipeline or atlas yet.
pub struct SurfaceInit {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    surface_factory: SurfaceFactory,
    adapter: wgpu::Adapter,
    device_lost: Arc<AtomicBool>,
}

type SurfaceFactory =
    Box<dyn Fn() -> Result<wgpu::Surface<'static>, wgpu::CreateSurfaceError> + Send + Sync>;

const SURFACE_RECOVERY_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Default)]
struct SurfaceRecovery {
    retry_at: Option<Instant>,
}

impl SurfaceRecovery {
    fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    fn failed(&mut self, now: Instant) {
        self.retry_at = Some(now + SURFACE_RECOVERY_BACKOFF);
    }
}

/// Holds all wgpu objects needed to render terminal frames.
pub struct RenderState {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    surface: Option<wgpu::Surface<'static>>,
    surface_factory: SurfaceFactory,
    adapter: wgpu::Adapter,
    device_lost: Arc<AtomicBool>,
    surface_recovery: SurfaceRecovery,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    foreground_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    sampler: wgpu::Sampler,
    /// Rasterized corner logo.  Kept here so its texture view stays alive for the lifetime of the bind group.
    logo: Logo,
    /// Cell dimensions in pixels (from real font metrics).
    pub cell_size: (f32, f32),
    /// Current surface size in pixels.
    pub size: (u32, u32),
    /// Background clear color.
    pub clear_color: wgpu::Color,
    /// Atlas generation at the time the bind group was last built. When this diverges from `TextRenderer::atlas_generation()` the bind group is rebuilt to point at the new atlas texture.
    last_atlas_generation: u64,
    /// Count of instances uploaded by the most recent full [`Self::render`] call.  Zero before the first full render.
    last_instance_count: u32,
    last_background_count: u32,
    shaped_rows: Vec<std::sync::Arc<crate::text::ShapedRow>>,
    instance_cache: InstanceCache,
    panes: std::collections::HashMap<u64, panes::PaneState>,
    pane_order: Vec<u64>,
    pane_frame_cached: bool,
    pane_colors: (Rgb, Rgb),
    dividers: panes::DividerState,
}

/// Initialise the wgpu instance, adapter, device, queue, and configured surface — without building any pipelines or textures.
pub async fn init_surface<W>(
    window: W,
    size: (u32, u32),
) -> Result<SurfaceInit, Box<dyn std::error::Error>>
where
    W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
{
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let window = Arc::new(window);
    let surface_factory: SurfaceFactory = Box::new({
        let instance = instance.clone();
        move || instance.create_surface(Arc::clone(&window))
    });
    let surface = surface_factory()?;

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|e| format!("no adapter found: {e}"))?;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("mechanic_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::default(),
        })
        .await?;

    let device_lost = Arc::new(AtomicBool::new(false));
    device.set_device_lost_callback({
        let device_lost = Arc::clone(&device_lost);
        move |reason, message| {
            device_lost.store(true, Ordering::Release);
            log::error!(
                "GPU device lost ({reason:?}): {message}; device recreation is unsupported"
            );
        }
    });

    let caps = surface.get_capabilities(&adapter);
    let surface_format =
        caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);

    // The shader emits straight alpha; the surface must use post-multiplied alpha.
    log::info!("surface alpha modes available: {:?}", caps.alpha_modes);
    let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
        wgpu::CompositeAlphaMode::PostMultiplied
    } else if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        caps.alpha_modes[0]
    };
    log::info!("selected surface alpha mode: {alpha_mode:?}");

    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: size.0,
        height: size.1,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    surface.configure(&device, &surface_config);

    Ok(SurfaceInit {
        device,
        queue,
        surface,
        surface_config,
        surface_factory,
        adapter,
        device_lost,
    })
}

impl RenderState {
    pub fn prepare_layout(
        &mut self,
        grid: &RenderGrid,
        text: &mut TextRenderer,
        config: &mechanic_config::font::FontConfig,
    ) {
        self.shaped_rows = text.shape_grid(grid, config);
    }
    pub fn visual_column(&self, col: usize, row: usize) -> usize {
        self.shaped_rows.get(row).and_then(|r| r.visual_cols.get(col)).copied().unwrap_or(col)
    }

    pub fn logical_column(&self, col: usize, row: usize) -> (usize, bool) {
        self.shaped_rows
            .get(row)
            .and_then(|r| {
                r.visual_cols
                    .iter()
                    .position(|visual| *visual == col)
                    .map(|logical| (logical, r.rtl[logical]))
            })
            .unwrap_or((col, false))
    }
    /// Build the pipeline and bind the text renderer's atlas.
    pub fn new_with_atlas(
        SurfaceInit {
            device,
            queue,
            surface,
            surface_config,
            surface_factory,
            adapter,
            device_lost,
        }: SurfaceInit,
        atlas_view: &wgpu::TextureView,
        atlas_generation: u64,
        cell_metrics: CellMetrics,
        bg: Rgb,
        logo_style: mechanic_config::theme::LogoStyle,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let size = (surface_config.width, surface_config.height);
        let surface_format = surface_config.format;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
        });

        let bind_group_layout = create_cell_bind_group_layout(&device);

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cell_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline =
            create_cell_pipeline(&device, &shader, Some(&pipeline_layout), surface_format, false);
        let foreground_pipeline =
            create_cell_pipeline(&device, &shader, Some(&pipeline_layout), surface_format, true);

        let cell_size = (cell_metrics.cell_width, cell_metrics.cell_height);
        let globals = Globals {
            viewport_size: [size.0 as f32, size.1 as f32],
            cell_size: [cell_size.0, cell_size.1],
            time: 0.0,
            content_opacity: 1.0,
            animation_flags: 0,
            text_opacity: 1.0,
            bloom_progress: 0.0,
            bloom_peak_multiplier: 1.0,
            logo_size: f32::from(mechanic_config::theme::DEFAULT_LOGO_SIZE),
            logo_style: logo_style as u32,
            pane_origin: [0.0; 2],
            _padding: [0.0; 2],
        };

        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals_buf"),
            contents: bytemuck::bytes_of(&globals),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        const INITIAL_CAPACITY: usize = 256;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instance_buf"),
            size: (INITIAL_CAPACITY * mem::size_of::<GpuInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let logo = Logo::new(&device, &queue, logo_style);

        let bind_group = Self::make_bind_group(
            &device,
            &bind_group_layout,
            &globals_buf,
            atlas_view,
            &sampler,
            &logo.view,
        );

        Ok(Self {
            device,
            queue,
            surface: Some(surface),
            surface_factory,
            adapter,
            device_lost,
            surface_recovery: SurfaceRecovery::default(),
            surface_config,
            pipeline,
            foreground_pipeline,
            bind_group_layout,
            bind_group,
            globals_buf,
            instance_buf,
            instance_capacity: INITIAL_CAPACITY,
            sampler,
            logo,
            cell_size,
            size,
            clear_color: background::clear_color(bg),
            last_atlas_generation: atlas_generation,
            last_instance_count: 0,
            last_background_count: 0,
            shaped_rows: Vec::new(),
            instance_cache: InstanceCache::default(),
            panes: std::collections::HashMap::new(),
            pane_order: Vec::new(),
            pane_frame_cached: false,
            pane_colors: (Rgb::new(173, 255, 255), Rgb::new(26, 58, 64)),
            dividers: panes::DividerState::default(),
        })
    }

    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        globals_buf: &wgpu::Buffer,
        atlas_view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        logo_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cell_bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(logo_view),
                },
            ],
        })
    }

    /// Rebuild the bind group to point at the current atlas texture view.
    pub fn update_atlas_bind_group(&mut self, atlas_view: &wgpu::TextureView) {
        self.bind_group = Self::make_bind_group(
            &self.device,
            &self.bind_group_layout,
            &self.globals_buf,
            atlas_view,
            &self.sampler,
            &self.logo.view,
        );
        for pane in self.panes.values_mut() {
            pane.update_bind_group(
                &self.device,
                &self.bind_group_layout,
                atlas_view,
                &self.sampler,
                &self.logo.view,
            );
        }
    }

    /// Sync the stored atlas generation to `gen`.
    pub fn sync_atlas_generation(&mut self, generation: u64) {
        self.last_atlas_generation = generation;
    }

    /// Reconfigure the surface after a window resize.
    pub fn resize(&mut self, new_size: (u32, u32)) {
        if new_size.0 == 0 || new_size.1 == 0 {
            return;
        }
        self.size = new_size;
        self.surface_config.width = new_size.0;
        self.surface_config.height = new_size.1;
        if let Some(surface) = &self.surface
            && !self.configure_surface(surface)
        {
            self.surface = None;
            self.surface_recovery.failed(Instant::now());
        }

        let globals = Globals {
            viewport_size: [new_size.0 as f32, new_size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time: 0.0,
            content_opacity: 1.0,
            animation_flags: 0,
            text_opacity: 1.0,
            bloom_progress: 0.0,
            bloom_peak_multiplier: 1.0,
            logo_size: f32::from(mechanic_config::theme::DEFAULT_LOGO_SIZE),
            logo_style: self.logo.style as u32,
            pane_origin: [0.0; 2],
            _padding: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        self.last_instance_count = 0;
        self.shaped_rows.clear();
        self.instance_cache.invalidate();
        self.pane_frame_cached = false;
    }

    fn configure_surface(&self, surface: &wgpu::Surface<'static>) -> bool {
        if self.device_lost.load(Ordering::Acquire) {
            return false;
        }
        let caps = surface.get_capabilities(&self.adapter);
        if !surface_config_supported(&self.surface_config, &caps)
            || self.surface_config.width > self.device.limits().max_texture_dimension_2d
            || self.surface_config.height > self.device.limits().max_texture_dimension_2d
        {
            log::error!("replacement surface does not support the existing GPU configuration");
            return false;
        }
        // Device loss or a driver error during configure must not become an
        // uncaptured validation panic. GPU resource/device recreation is separate.
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let internal = self.device.push_error_scope(wgpu::ErrorFilter::Internal);
        let memory = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        surface.configure(&self.device, &self.surface_config);
        let errors = [
            pollster::block_on(memory.pop()),
            pollster::block_on(internal.pop()),
            pollster::block_on(validation.pop()),
        ];
        for error in errors.iter().flatten() {
            log::error!("surface configuration failed: {error}");
        }
        errors.iter().all(Option::is_none) && !self.device_lost.load(Ordering::Acquire)
    }

    /// Replace a lost native surface, preserving the device, atlas, and cached frame.
    /// Failed attempts back off; this does not recover a lost GPU device.
    /// Call on the window event-loop thread, as Metal surface creation requires.
    pub fn recreate_surface(&mut self) -> bool {
        let now = Instant::now();
        if self.device_lost.load(Ordering::Acquire) || !self.surface_recovery.ready(now) {
            return false;
        }
        // Release the old swapchain before creating another for the same window.
        self.surface = None;
        match (self.surface_factory)() {
            Ok(surface) if self.configure_surface(&surface) => {
                self.surface = Some(surface);
                self.surface_recovery.retry_at = None;
                true
            }
            Ok(_) => {
                self.surface_recovery.failed(now);
                false
            }
            Err(error) => {
                log::error!("surface recreation failed: {error}");
                self.surface_recovery.failed(now);
                false
            }
        }
    }

    fn acquire_surface_texture(&mut self) -> Option<wgpu::SurfaceTexture> {
        if self.device_lost.load(Ordering::Acquire) {
            return None;
        }
        if self.surface.is_none() && !self.recreate_surface() {
            return None;
        }
        let acquisition = self.surface.as_ref()?.get_current_texture();
        surface_texture_for_render(acquisition, |action| match action {
            SurfaceRecoveryAction::Recreate => {
                self.recreate_surface();
            }
            SurfaceRecoveryAction::Reconfigure => {
                let configured =
                    self.surface.as_ref().is_some_and(|surface| self.configure_surface(surface));
                if !configured {
                    self.surface = None;
                    self.surface_recovery.failed(Instant::now());
                }
            }
        })
    }

    /// Update the cell size used by the pipeline's globals uniform.
    pub fn set_cell_size(&mut self, cell_size: (f32, f32)) {
        self.cell_size = cell_size;
        self.last_instance_count = 0;
        self.shaped_rows.clear();
        self.instance_cache.invalidate();
        self.pane_frame_cached = false;
        for pane in self.panes.values_mut() {
            pane.invalidate_layout();
        }
    }

    /// Render a single frame; true only after submitting and presenting it.
    pub fn render(
        &mut self,
        grid: &RenderGrid,
        text_renderer: &mut TextRenderer,
        font_config: &mechanic_config::font::FontConfig,
        uniforms: FrameUniforms,
    ) -> bool {
        self.pane_frame_cached = false;
        if self.device_lost.load(Ordering::Acquire) {
            return false;
        }
        // Full rendering replaces the cached buffer before surface acquisition.
        self.last_instance_count = 0;
        let profiling = log::log_enabled!(target: "mechanic_render_profile", log::Level::Trace);
        let mut profile = profiling.then(RenderProfile::default);
        let globals = Globals {
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
            pane_origin: [0.0; 2],
            _padding: [0.0; 2],
        };
        let uniform_upload_started = profiling.then(Instant::now);
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
        if let (Some(profile), Some(started)) = (&mut profile, uniform_upload_started) {
            profile.upload_ns = started.elapsed().as_nanos();
            profile.upload_bytes = mem::size_of::<Globals>();
        }

        let atlas_started = profiling.then(Instant::now);
        self.prepare_layout(grid, text_renderer, font_config);
        let atlas_result =
            text_renderer.prepare_frame(&self.shaped_rows, &self.device, &self.queue);
        if let Err(error) = &atlas_result {
            log::error!("glyph atlas preparation failed: {error}");
            self.instance_cache.invalidate();
        }
        if let (Some(profile), Some(started)) = (&mut profile, atlas_started) {
            profile.atlas_ns = started.elapsed().as_nanos();
        }

        let instances_started = profiling.then(Instant::now);
        let current_gen = text_renderer.atlas_generation();
        let mut uploads = self.instance_cache.update(
            grid,
            &self.shaped_rows,
            (current_gen, self.cell_size),
            uniforms.window_focused,
            |row, reusable| {
                build_instances_for_rows(
                    grid,
                    &self.shaped_rows,
                    text_renderer,
                    self.cell_size,
                    uniforms.window_focused,
                    row..row + 1,
                    reusable,
                )
                .0
            },
        );
        if atlas_result.is_err() {
            self.instance_cache.invalidate();
        }
        let instances = &self.instance_cache.instances;
        let background_count = self.instance_cache.background_count;
        let instance_count = instances.len();

        if let (Some(profile), Some(started)) = (&mut profile, instances_started) {
            profile.instances_ns = started.elapsed().as_nanos();
            profile.instance_count = instances.len();
        }

        let atlas_binding_started = profiling.then(Instant::now);
        let atlas_changed = current_gen != self.last_atlas_generation;
        if atlas_changed {
            self.update_atlas_bind_group(&text_renderer.atlas_view);
            self.last_atlas_generation = current_gen;
        }
        if let (Some(profile), Some(started)) = (&mut profile, atlas_binding_started) {
            profile.atlas_ns += started.elapsed().as_nanos();
            profile.atlas_changed = atlas_changed;
        }

        let instance_upload_started = profiling.then(Instant::now);

        if instance_count > self.instance_capacity {
            let new_cap = instance_count.next_power_of_two();
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instance_buf"),
                size: (new_cap * mem::size_of::<GpuInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
            uploads.clear();
            uploads.push(0..instance_count);
        }

        let mut uploaded_bytes = 0;
        for range in uploads {
            let bytes = bytemuck::cast_slice::<GpuInstance, u8>(
                &self.instance_cache.instances[range.clone()],
            );
            self.queue.write_buffer(
                &self.instance_buf,
                (range.start * mem::size_of::<GpuInstance>()) as u64,
                bytes,
            );
            uploaded_bytes += bytes.len();
        }
        if let (Some(profile), Some(started)) = (&mut profile, instance_upload_started) {
            profile.upload_ns += started.elapsed().as_nanos();
            profile.upload_bytes += uploaded_bytes;
        }

        let surface_started = profiling.then(Instant::now);
        let surface_texture = self.acquire_surface_texture();
        if let (Some(profile), Some(started)) = (&mut profile, surface_started) {
            profile.surface_ns = started.elapsed().as_nanos();
        }
        let Some(surface_texture) = surface_texture else {
            if let Some(profile) = profile {
                profile.log(false);
            }
            return false;
        };

        let submit_present_started = profiling.then(Instant::now);
        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame_encoder"),
        });

        let clear_color = wgpu::Color {
            r: self.clear_color.r,
            g: self.clear_color.g,
            b: self.clear_color.b,
            a: uniforms.content_opacity as f64,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cell_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instance_buf.slice(..));
            pass.draw(0..6, 0..background_count);
            pass.set_pipeline(&self.foreground_pipeline);
            pass.draw(0..6, background_count..instance_count as u32);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(surface_texture);
        if let (Some(profile), Some(started)) = (&mut profile, submit_present_started) {
            profile.submit_present_ns = started.elapsed().as_nanos();
            profile.log(true);
        }

        self.last_instance_count = instance_count as u32;
        self.last_background_count = background_count;
        true
    }

    /// Draw cached instances with new uniforms; true only after presentation.
    pub fn render_animation(&mut self, uniforms: FrameUniforms) -> bool {
        if self.pane_frame_cached {
            return self.render_pane_animation(uniforms);
        }
        if self.last_instance_count == 0 || self.device_lost.load(Ordering::Acquire) {
            return false;
        }

        let globals = Globals {
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
            pane_origin: [0.0; 2],
            _padding: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let Some(surface_texture) = self.acquire_surface_texture() else {
            return false;
        };

        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("anim_frame_encoder"),
        });

        let clear_color = wgpu::Color {
            r: self.clear_color.r,
            g: self.clear_color.g,
            b: self.clear_color.b,
            a: uniforms.content_opacity as f64,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("anim_cell_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instance_buf.slice(..));
            pass.draw(0..6, 0..self.last_background_count);
            pass.set_pipeline(&self.foreground_pipeline);
            pass.draw(0..6, self.last_background_count..self.last_instance_count);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(surface_texture);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceRecoveryAction {
    Reconfigure,
    Recreate,
}

fn surface_config_supported(
    config: &wgpu::SurfaceConfiguration,
    caps: &wgpu::SurfaceCapabilities,
) -> bool {
    config.width > 0
        && config.height > 0
        && caps.formats.contains(&config.format)
        && caps.present_modes.contains(&config.present_mode)
        && caps.alpha_modes.contains(&config.alpha_mode)
        && caps.usages.contains(config.usage)
        && config.color_space == wgpu::SurfaceColorSpace::Auto
}

fn surface_texture_for_render(
    acquisition: wgpu::CurrentSurfaceTexture,
    recover: impl FnOnce(SurfaceRecoveryAction),
) -> Option<wgpu::SurfaceTexture> {
    match acquisition {
        wgpu::CurrentSurfaceTexture::Success(texture)
        | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => Some(texture),
        wgpu::CurrentSurfaceTexture::Outdated => {
            recover(SurfaceRecoveryAction::Reconfigure);
            None
        }
        wgpu::CurrentSurfaceTexture::Lost => {
            recover(SurfaceRecoveryAction::Recreate);
            None
        }
        wgpu::CurrentSurfaceTexture::Timeout
        | wgpu::CurrentSurfaceTexture::Occluded
        | wgpu::CurrentSurfaceTexture::Validation => None,
    }
}

fn create_cell_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: Option<&wgpu::PipelineLayout>,
    format: wgpu::TextureFormat,
    foreground: bool,
) -> wgpu::RenderPipeline {
    let attributes = wgpu::vertex_attr_array![
        0 => Uint32x2, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4,
        4 => Float32x2, 5 => Float32x2, 6 => Uint32,
    ];
    let instance_layout = wgpu::VertexBufferLayout {
        array_stride: mem::size_of::<GpuInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &attributes,
    };
    // Preserve the surface opacity while blending glyph coverage over cells.
    let blend = foreground.then_some(wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("cell_pipeline"),
        layout,
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(instance_layout)],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_surface_acquisitions_never_supply_a_presentable_frame() {
        for acquisition in [
            wgpu::CurrentSurfaceTexture::Timeout,
            wgpu::CurrentSurfaceTexture::Occluded,
            wgpu::CurrentSurfaceTexture::Validation,
        ] {
            assert!(
                surface_texture_for_render(acquisition, |_| {
                    panic!("only an outdated surface can be recovered by reconfiguration")
                })
                .is_none()
            );
        }
    }

    #[test]
    fn outdated_surface_reconfigures_but_still_requires_another_acquisition() {
        let mut reconfigurations = 0;
        let frame = surface_texture_for_render(wgpu::CurrentSurfaceTexture::Outdated, |action| {
            assert_eq!(action, SurfaceRecoveryAction::Reconfigure);
            reconfigurations += 1;
        });
        assert!(frame.is_none());
        assert_eq!(reconfigurations, 1);
    }

    #[test]
    fn lost_surface_recreates_once_and_does_not_claim_presentation() {
        let mut recreations = 0;
        let frame = surface_texture_for_render(wgpu::CurrentSurfaceTexture::Lost, |action| {
            assert_eq!(action, SurfaceRecoveryAction::Recreate);
            recreations += 1;
        });
        assert!(frame.is_none());
        assert_eq!(recreations, 1);
    }

    #[test]
    fn failed_surface_recovery_defers_repeated_native_creation() {
        let now = Instant::now();
        let mut recovery = SurfaceRecovery::default();
        assert!(recovery.ready(now));
        recovery.failed(now);
        assert!(!recovery.ready(now));
        assert!(!recovery.ready(now + SURFACE_RECOVERY_BACKOFF - Duration::from_millis(1)));
        assert!(recovery.ready(now + SURFACE_RECOVERY_BACKOFF));
        recovery.retry_at = None;
        assert!(recovery.ready(now));
    }

    #[test]
    fn replacement_surface_must_keep_existing_pipeline_configuration() {
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            width: 320,
            height: 200,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::PostMultiplied,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let mut caps = wgpu::SurfaceCapabilities {
            formats: vec![config.format],
            present_modes: vec![config.present_mode],
            alpha_modes: vec![config.alpha_mode],
            usages: config.usage,
            ..Default::default()
        };
        assert!(surface_config_supported(&config, &caps));
        caps.formats = vec![wgpu::TextureFormat::Rgba8Unorm];
        assert!(!surface_config_supported(&config, &caps));
        caps.formats = vec![config.format];
        caps.alpha_modes.clear();
        assert!(!surface_config_supported(&config, &caps));
        caps.alpha_modes = vec![config.alpha_mode];
        caps.present_modes.clear();
        assert!(!surface_config_supported(&config, &caps));
        caps.present_modes = vec![config.present_mode];
        caps.usages = wgpu::TextureUsages::COPY_SRC;
        assert!(!surface_config_supported(&config, &caps));
        caps.usages = config.usage;
        let mut minimized = config.clone();
        minimized.width = 0;
        assert!(!surface_config_supported(&minimized, &caps));
    }

    #[test]
    fn ligature_clips_uvs_at_selection_boundary() {
        let uv = [0.2, 0.1, 0.6, 0.9];
        let (x, w, clipped) = clip_glyph(2.0, 20.0, uv, 10.0, 20.0).unwrap();
        assert_eq!((x, w), (10.0, 10.0));
        assert!((clipped[0] - 0.36).abs() < 0.0001);
        assert!((clipped[2] - 0.56).abs() < 0.0001);
        assert_eq!((clipped[1], clipped[3]), (uv[1], uv[3]));
        assert!(clip_glyph(2.0, 20.0, uv, 22.0, 30.0).is_none());
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "pipeline_gpu_tests.rs"]
mod gpu_tests;

#[cfg(test)]
fn build_instances(
    grid: &RenderGrid,
    shaped_rows: &[std::sync::Arc<crate::text::ShapedRow>],
    text_renderer: &TextRenderer,
    cell_size: (f32, f32),
    focused: bool,
) -> (Vec<GpuInstance>, u32) {
    build_instances_for_rows(
        grid,
        shaped_rows,
        text_renderer,
        cell_size,
        focused,
        0..grid.rows,
        Vec::new(),
    )
}

fn build_instances_for_rows(
    grid: &RenderGrid,
    shaped_rows: &[std::sync::Arc<crate::text::ShapedRow>],
    text_renderer: &TextRenderer,
    cell_size: (f32, f32),
    focused: bool,
    rows: std::ops::Range<usize>,
    mut instances: Vec<GpuInstance>,
) -> (Vec<GpuInstance>, u32) {
    let total_cells = grid.cols * rows.len();
    let include_cursor = rows.end == grid.rows;
    instances.clear();
    instances.reserve(total_cells * 2);

    for (row, shaped) in shaped_rows.iter().enumerate().take(rows.end).skip(rows.start) {
        for col in 0..grid.cols {
            let Some(cell) = grid.get(col, row) else {
                continue;
            };

            let mut fg = cell.fg;
            let mut bg = cell.bg;

            if cell.flags.contains(crate::grid::CellFlags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }

            let visual_col = shaped.visual_cols[col];
            instances.push(GpuInstance {
                cell_pos: [visual_col as u32, row as u32],
                atlas_uv: [0.0; 4],
                fg_color: [0.0; 4],
                bg_color: rgb_to_f32(bg),
                glyph_offset: [0.0; 2],
                glyph_size: [0.0; 2],
                use_atlas: SOLID_USE_ATLAS,
                _pad: [0; 3],
            });
        }
    }
    let background_count = instances.len() as u32;

    // Paint every background first so wide glyphs and combining marks survive.
    for (row, shaped) in shaped_rows.iter().enumerate().take(rows.end).skip(rows.start) {
        let mut logical_cols = vec![0; grid.cols];
        for (logical, &visual) in shaped.visual_cols.iter().enumerate() {
            logical_cols[visual] = logical;
        }
        for glyph in &shaped.glyphs {
            let Some(info) = text_renderer.glyph_info(glyph.cache_key) else { continue };
            let scale_x = glyph.scale_x / info.raster_scale_x;
            let left = glyph.x + info.offset_x * scale_x;
            let bitmap_width = info.glyph_width * scale_x;
            let right = left + bitmap_width;
            let first = (left / cell_size.0).floor().max(0.0) as usize;
            let end = ((right / cell_size.0).ceil().max(0.0) as usize).min(grid.cols);
            // Split at color boundaries so selecting part of a ligature
            // recolors that cell without breaking contextual shaping.
            let mut col = first;
            let colored_cell = |visual: usize| {
                let logical = if (glyph.cluster_start..glyph.cluster_end).contains(&visual) {
                    logical_cols[visual]
                } else {
                    glyph.source_col
                };
                &grid.cells[row * grid.cols + logical]
            };
            while col < end {
                let cell = colored_cell(col);
                let fg = effective_fg(cell);
                let hidden = cell.flags.contains(crate::grid::CellFlags::HIDDEN);
                let mut next = col + 1;
                while next < end {
                    let other = colored_cell(next);
                    if effective_fg(other) != fg
                        || other.flags.contains(crate::grid::CellFlags::HIDDEN) != hidden
                    {
                        break;
                    }
                    next += 1;
                }
                if !hidden
                    && let Some((x, width, uv)) = clip_glyph(
                        left,
                        bitmap_width,
                        info.atlas_uv,
                        col as f32 * cell_size.0,
                        next as f32 * cell_size.0,
                    )
                {
                    instances.push(GpuInstance {
                        cell_pos: [col as u32, row as u32],
                        atlas_uv: uv,
                        fg_color: rgb_to_f32(fg),
                        bg_color: [0.0; 4],
                        glyph_offset: [x - col as f32 * cell_size.0, glyph.y + info.offset_y],
                        glyph_size: [width, info.glyph_height],
                        use_atlas: GLYPH_USE_ATLAS,
                        _pad: [0; 3],
                    });
                }
                col = next;
            }
        }
        for col in 0..grid.cols {
            let cell = &grid.cells[row * grid.cols + col];
            if cell.flags.intersects(
                crate::grid::CellFlags::HIDDEN | crate::grid::CellFlags::LEADING_WIDE_CHAR_SPACER,
            ) {
                continue;
            }
            append_decorations(
                &mut instances,
                cell,
                [shaped.visual_cols[col] as u32, row as u32],
                cell_size,
            );
        }
    }

    {
        use crate::grid::CursorStyle;
        let (cx, cy) = grid.cursor_position;
        if include_cursor && grid.cursor_visible && grid.get(cx, cy).is_some() {
            let cursor_color = grid.cursor_color;
            let cell_w = cell_size.0 * grid.cursor_width as f32;
            let cell_h = cell_size.1;

            let quad = match (grid.cursor_style, focused) {
                (CursorStyle::HollowBlock, _) | (CursorStyle::Block, false) => {
                    Some(([0.0f32, 0.0f32], [cell_w, cell_h], HOLLOW_BLOCK_USE_ATLAS))
                }
                (CursorStyle::Block, true) => None,
                (CursorStyle::Bar, _) => {
                    let x = if shaped_rows[cy].rtl[cx] { (cell_w - 2.0).max(0.0) } else { 0.0 };
                    Some(([x, 0.0f32], [2.0f32, cell_h], CURSOR_USE_ATLAS))
                }
                (CursorStyle::Underline, _) => {
                    Some(([0.0f32, cell_h - 2.0f32], [cell_w, 2.0f32], CURSOR_USE_ATLAS))
                }
            };

            if let Some((glyph_offset, glyph_size, use_atlas)) = quad {
                instances.push(GpuInstance {
                    cell_pos: [shaped_rows[cy].visual_cols[cx] as u32, cy as u32],
                    atlas_uv: [0.0; 4],
                    fg_color: [0.0; 4],
                    bg_color: rgb_to_f32(cursor_color),
                    glyph_offset,
                    glyph_size,
                    use_atlas,
                    _pad: [0; 3],
                });
            }
        }
    }

    (instances, background_count)
}

fn append_decorations(
    instances: &mut Vec<GpuInstance>,
    cell: &crate::grid::RenderCell,
    position: [u32; 2],
    size: (f32, f32),
) {
    use crate::grid::CellFlags;
    let thickness = (size.1 / 18.0).max(1.0);
    let mut push = |y: f32, height: f32, kind, color| {
        instances.push(GpuInstance {
            cell_pos: position,
            atlas_uv: [0.0; 4],
            fg_color: rgb_to_f32(color),
            bg_color: [0.0; 4],
            glyph_offset: [0.0, y.max(0.0)],
            glyph_size: [size.0, height],
            use_atlas: kind,
            _pad: [0; 3],
        })
    };
    if cell.flags.contains(CellFlags::UNDERLINE) {
        let color = cell.underline_color.unwrap_or_else(|| effective_fg(cell));
        let y = size.1 - thickness * 2.0;
        if cell.flags.contains(CellFlags::DOUBLE_UNDERLINE) {
            push(y - thickness * 2.0, thickness, 4, color);
            push(y, thickness, 4, color);
        } else if cell.flags.contains(CellFlags::UNDERCURL) {
            push(size.1 - thickness * 4.0, thickness * 3.0, 7, color);
        } else {
            let kind = if cell.flags.contains(CellFlags::DOTTED_UNDERLINE) {
                5
            } else if cell.flags.contains(CellFlags::DASHED_UNDERLINE) {
                6
            } else {
                4
            };
            push(y, thickness, kind, color);
        }
    }
    if cell.flags.contains(CellFlags::STRIKEOUT) {
        push(size.1 * 0.5, thickness, 4, effective_fg(cell));
    }
}

fn create_cell_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("cell_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    })
}
