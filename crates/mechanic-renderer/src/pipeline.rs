// wgpu render pipeline: surface setup, instance buffer, and per-frame rendering.

use std::mem;

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

// ── GPU instance data ─────────────────────────────────────────────────────────

/// Per-cell data uploaded to the GPU as vertex attributes (step mode: Instance).
///
/// Layout must match the `Instance` struct in `cell.wgsl`.
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
    /// Padding to keep 16-byte alignment.
    pub _pad: [u32; 3],
}

fn rgb_to_f32(c: Rgb) -> [f32; 4] {
    [f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0, 1.0]
}

// ── Instance discriminants ────────────────────────────────────────────────────
//
// The `use_atlas` field on [`GpuInstance`] selects which fragment-shader
// branch the instance flows through.  Three values are meaningful; the
// shader panics (via the default arm / discard) on anything else.
//
// Kept in sync with the `if in.use_atlas == N` chain in `cell.wgsl`.

/// Solid-fill background or cursor quad — the fragment outputs
/// `bg_color` directly (optionally mixed with the gradient in the
/// background path).  Covers cell backgrounds and bar/underline
/// cursors.
const SOLID_USE_ATLAS: u32 = 0;

/// Atlas-sampled glyph — the fragment samples the atlas texture at
/// the instance's UV rect, multiplies coverage by `text_opacity`,
/// and mixes `fg_color` over `bg_color` accordingly.
#[expect(dead_code, reason = "reserved for future callers; paired with the _USE_ATLAS constants")]
const GLYPH_USE_ATLAS: u32 = 1;

/// Hollow-block cursor outline — the fragment keeps only pixels
/// within [`HOLLOW_CURSOR_BORDER_PX`] of a cell edge (in local-cell
/// coordinates) and `discard`s the interior so the cell's glyph
/// beneath stays readable.  Emitted by the renderer only for
/// unfocused windows with a Block cursor style.
const HOLLOW_BLOCK_USE_ATLAS: u32 = 2;

/// Thickness of the hollow-block cursor outline, in physical
/// pixels.  `1.5` reads as a single clean pixel on non-Retina
/// displays and a crisp 3 px on 2× Retina; fine-tune if it looks
/// too thin or too thick once running.  Kept in sync with the
/// WGSL fragment shader's hollow-block branch.
#[expect(dead_code, reason = "used in the shader; recorded here so both sides drift together")]
const HOLLOW_CURSOR_BORDER_PX: f32 = 1.5;

// ── Per-frame shader inputs ───────────────────────────────────────────────────

/// Per-frame values the caller feeds to `render` / `render_animation`
/// — bundled into a struct so adding another shader input doesn't push
/// the render signature past clippy's 7-argument threshold or make
/// the call site a forest of positional floats.
///
/// Everything else the shader needs — viewport size, cell size, atlas
/// contents — is derived from the renderer's own retained state and
/// is not part of this struct.
///
/// Two distinct focus-derived flags live here:
///
/// - [`shader_focused`](Self::shader_focused) gates the shader's
///   continuous time-based animations (gradient breath, colour
///   rotation, electron pulses).  It's `true` only when the window
///   has real keyboard focus *and* `--hot-cpu` is on, because those
///   animations are unbounded and must stay opt-in.
/// - [`window_focused`](Self::window_focused) tracks the real OS
///   focus state, independent of `--hot-cpu`.  Used for the focus-
///   aware cursor style (solid block when focused, hollow outline
///   when blurred) and anything else that should follow real focus
///   even when the shader clock is frozen.
#[derive(Debug, Clone, Copy)]
pub struct FrameUniforms {
    /// Window-level alpha: how much of the desktop bleeds through.
    /// `1.0` = fully opaque window; `0.0` = fully transparent.
    /// Applied to every pixel the surface emits.
    pub content_opacity: f32,
    /// Multiplier on glyph coverage in the text path.  `1.0` = text
    /// renders at full contrast against its cell background; lower
    /// values ghost text toward the background so an unfocused
    /// window reads as idle without touching `content_opacity`.
    /// Not applied to background cells — only to text.
    pub text_opacity: f32,
    /// Seconds since the window was created.  Drives the shader's
    /// time-based animations (corner gradient breath, colour pulse,
    /// electron traces) when `shader_focused` is true.
    pub time: f32,
    /// Whether the shader's continuous time-based animations should
    /// advance.  `false` pins the gradient/electrons at `t=0`.
    /// `true` only when the window has OS focus **and** `--hot-cpu`
    /// is on — the continuous animations are unbounded, so they
    /// stay opt-in per `design/CPU-SPEC.md` rule 3.
    pub shader_focused: bool,
    /// Real OS keyboard focus state, independent of `--hot-cpu`.
    /// Drives the solid-vs-hollow block cursor selection and other
    /// signals that should reflect focus even in the quiet default
    /// mode.  `true` when the window is the key window; `false`
    /// when another window or app has focus.
    pub window_focused: bool,
    /// Progress through the focus-gain bloom animation, in
    /// `[0.0, 1.0]`.  `0.0` before the bloom has committed or after
    /// it has completed; fractional values during the ≈ 250 ms
    /// animation window.  The shader applies a `sin(progress × π)`
    /// envelope so the curve eases naturally into and out of peak.
    /// See [`OpacityConfig::bloom_duration_ms`] for the timing.
    ///
    /// [`OpacityConfig::bloom_duration_ms`]: mechanic_config::theme::OpacityConfig::bloom_duration_ms
    pub bloom_progress: f32,
    /// Peak scale factor applied to the corner logo's display
    /// opacity at the midpoint of the bloom curve.  Passed through
    /// to the shader so the effect can be tuned from
    /// `mechanic.toml` without a rebuild.  `1.0` disables the
    /// visible bloom (scheduler still runs the animation but no
    /// pixel difference appears); `1.4` is the default lift.
    /// Mirrors [`OpacityConfig::bloom_peak_multiplier`].
    ///
    /// [`OpacityConfig::bloom_peak_multiplier`]: mechanic_config::theme::OpacityConfig::bloom_peak_multiplier
    pub bloom_peak_multiplier: f32,
}

// ── Globals uniform ───────────────────────────────────────────────────────────

/// GPU-side mirror of [`FrameUniforms`], laid out to match the
/// `Globals` struct in `cell.wgsl`.
///
/// Layout: 48 bytes, 16-byte aligned.  The first 32 bytes are the
/// original four-field layout (viewport, cell, time, content_opacity,
/// shader_focused, text_opacity); the added 16 bytes hold
/// `bloom_progress` plus three `f32` padding slots to keep the struct
/// size a multiple of 16.  The padding is intentional headroom — the
/// next shader input (say, a cursor blink phase, or a carousel tilt
/// angle for Phase 6) can slot into one of those `_pad` positions
/// without re-aligning the struct, avoiding a churn commit that
/// renames every uniform binding.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Globals {
    viewport_size: [f32; 2],
    cell_size: [f32; 2],
    time: f32,
    content_opacity: f32,
    /// 1.0 when the shader's continuous animations should advance,
    /// 0.0 otherwise.  Derived from [`FrameUniforms::shader_focused`]
    /// — OS focus AND `--hot-cpu`.  Not a raw focus bit.
    shader_focused: f32,
    /// Glyph-coverage multiplier for the text path.  1.0 for focused
    /// windows; configurable idle value for blurred windows.  See
    /// [`FrameUniforms::text_opacity`].
    text_opacity: f32,
    /// Progress through the focus-gain bloom in `[0, 1]`.  See
    /// [`FrameUniforms::bloom_progress`].
    bloom_progress: f32,
    /// Peak multiplier applied to logo opacity at bloom midpoint.
    /// See [`FrameUniforms::bloom_peak_multiplier`].
    bloom_peak_multiplier: f32,
    /// Reserved for future per-frame uniforms.  Kept at end of
    /// struct so adding a value is a rename, not a reshuffle.
    _pad: [f32; 2],
}

// ── RenderState ───────────────────────────────────────────────────────────────

/// Intermediate result of `init_surface`: device/queue/surface ready, but no
/// pipeline or atlas yet.
pub struct SurfaceInit {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
}

/// Holds all wgpu objects needed to render terminal frames.
pub struct RenderState {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    sampler: wgpu::Sampler,
    /// Rasterized corner logo.  Kept here so its texture view stays alive
    /// for the lifetime of the bind group.
    logo: Logo,
    /// Cell dimensions in pixels (from real font metrics).
    pub cell_size: (f32, f32),
    /// Current surface size in pixels.
    pub size: (u32, u32),
    /// Background clear color.
    pub clear_color: wgpu::Color,
    /// Atlas generation at the time the bind group was last built.
    /// When this diverges from `TextRenderer::atlas_generation()` the bind
    /// group is rebuilt to point at the new atlas texture.
    last_atlas_generation: u64,
    /// Count of instances uploaded by the most recent full [`Self::render`]
    /// call.  Zero before the first full render.
    ///
    /// [`Self::render_animation`] uses this to know how many instances
    /// to draw from the retained `instance_buf` on frames where only
    /// the time/opacity/focused uniforms changed — the grid itself is
    /// unchanged, so we skip the ~200 KB instance rebuild+upload and
    /// just re-issue the same draw against a new globals uniform.
    last_instance_count: u32,
}

/// Initialise the wgpu instance, adapter, device, queue, and configured
/// surface — without building any pipelines or textures.
///
/// The returned `SurfaceInit` can be used to construct a `TextRenderer` first
/// (so its atlas view is available), then passed to
/// `RenderState::new_with_atlas` along with that atlas view.
///
/// # Safety
///
/// `window` must remain valid for the entire lifetime of the returned
/// `SurfaceInit::surface` (the `'static` surface lifetime is achieved by
/// taking ownership of the window handle via `SurfaceTarget::Window`).
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

    let surface = instance.create_surface(window)?;

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
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

    let caps = surface.get_capabilities(&adapter);
    let surface_format =
        caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);

    // Invariant: the fragment shader emits non-premultiplied colors.
    // PostMultiplied matches that; PreMultiplied would cause double
    // darkening on drivers that advertise it.  If cell.wgsl is ever
    // changed to premultiply, flip this preference.
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
    };
    surface.configure(&device, &surface_config);

    Ok(SurfaceInit { device, queue, surface, surface_config })
}

impl RenderState {
    /// Build the wgpu pipeline and initial bind group using the *real* atlas
    /// view from `TextRenderer`.
    ///
    /// Call `init_surface` first to obtain a `SurfaceInit`, construct a
    /// `TextRenderer` with the device/queue it provides, then call this.
    pub fn new_with_atlas(
        SurfaceInit { device, queue, surface, surface_config }: SurfaceInit,
        atlas_view: &wgpu::TextureView,
        atlas_generation: u64,
        cell_metrics: CellMetrics,
        bg: Rgb,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let size = (surface_config.width, surface_config.height);
        let surface_format = surface_config.format;

        // ── Shader ───────────────────────────────────────────────────────────

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
        });

        // ── Bind group layout ─────────────────────────────────────────────────
        //
        // group(0) binding(0) = Globals uniform
        // group(0) binding(1) = glyph atlas texture
        // group(0) binding(2) = shared filtering sampler (used by both textures)
        // group(0) binding(3) = corner logo texture

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cell_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        // ── Vertex buffer layouts ─────────────────────────────────────────────
        //
        // Slot 0: per-instance GpuInstance (step_mode: Instance)

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<GpuInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Uint32x2,   // cell_pos
                1 => Float32x4,  // atlas_uv
                2 => Float32x4,  // fg_color
                3 => Float32x4,  // bg_color
                4 => Float32x2,  // glyph_offset
                5 => Float32x2,  // glyph_size
                6 => Uint32,     // use_atlas
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cell_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[instance_layout],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    // No blending: every pixel is fully written by exactly one
                    // cell or glyph draw.  Alpha blending would corrupt the
                    // alpha channel, breaking the PostMultiplied compositor on
                    // macOS (pixels would end up near-opaque even when the
                    // shader outputs partial alpha).
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // ── Globals uniform buffer ────────────────────────────────────────────

        let cell_size = (cell_metrics.cell_width, cell_metrics.cell_height);
        let globals = Globals {
            viewport_size: [size.0 as f32, size.1 as f32],
            cell_size: [cell_size.0, cell_size.1],
            time: 0.0,
            content_opacity: 1.0,
            shader_focused: 1.0,
            text_opacity: 1.0,
            bloom_progress: 0.0,
            // Sentinel `1.0` so the initial frame and any post-resize
            // frame that arrives before `render()` repopulates the
            // uniform render identically with or without the bloom
            // field present — `mix(1.0, 1.0, anything) = 1.0`, so
            // there's no visible effect until the real value lands.
            bloom_peak_multiplier: 1.0,
            _pad: [0.0; 2],
        };

        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals_buf"),
            contents: bytemuck::bytes_of(&globals),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        // ── Sampler ───────────────────────────────────────────────────────────

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

        // ── Initial instance buffer ───────────────────────────────────────────

        const INITIAL_CAPACITY: usize = 256;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instance_buf"),
            size: (INITIAL_CAPACITY * mem::size_of::<GpuInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Rasterize the corner logo SVG once at startup.
        let logo = Logo::new(&device, &queue);

        // Bind group uses the real atlas view — no dummy texture needed.
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
            surface,
            surface_config,
            pipeline,
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
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

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
    ///
    /// Called after the glyph atlas grows — the atlas texture is replaced
    /// so the bind group's binding 1 needs to be re-pointed.  The logo
    /// (binding 3) is stable and re-bound from `self.logo`.
    pub fn update_atlas_bind_group(&mut self, atlas_view: &wgpu::TextureView) {
        self.bind_group = Self::make_bind_group(
            &self.device,
            &self.bind_group_layout,
            &self.globals_buf,
            atlas_view,
            &self.sampler,
            &self.logo.view,
        );
    }

    /// Sync the stored atlas generation to `gen`.
    ///
    /// Call this after `update_atlas_bind_group` in contexts where the
    /// TextRenderer is rebuilt entirely (e.g. `set_font_size`), so the
    /// per-frame generation check in `render` doesn't trigger a redundant
    /// bind-group rebuild on the very next frame.
    pub fn sync_atlas_generation(&mut self, generation: u64) {
        self.last_atlas_generation = generation;
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Reconfigure the surface after a window resize.
    pub fn resize(&mut self, new_size: (u32, u32)) {
        if new_size.0 == 0 || new_size.1 == 0 {
            return;
        }
        self.size = new_size;
        self.surface_config.width = new_size.0;
        self.surface_config.height = new_size.1;
        self.surface.configure(&self.device, &self.surface_config);

        // Update the globals uniform (time/opacity are overwritten each frame).
        let globals = Globals {
            viewport_size: [new_size.0 as f32, new_size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time: 0.0,
            content_opacity: 1.0,
            shader_focused: 1.0,
            text_opacity: 1.0,
            bloom_progress: 0.0,
            // Sentinel `1.0` so the initial frame and any post-resize
            // frame that arrives before `render()` repopulates the
            // uniform render identically with or without the bloom
            // field present — `mix(1.0, 1.0, anything) = 1.0`, so
            // there's no visible effect until the real value lands.
            bloom_peak_multiplier: 1.0,
            _pad: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // The retained instance buffer was built against the previous
        // grid dimensions — redrawing it under a new viewport would
        // leave cells mispositioned.  Zero the count so
        // `render_animation` returns false until the next full render
        // refills the cache at the new size.
        self.last_instance_count = 0;
    }

    /// Update the cell size used by the pipeline's globals uniform.
    ///
    /// The next `render()` call will write the new cell_size to the GPU.
    /// Used by `Renderer::set_font_size` after the text renderer is rebuilt
    /// at a new point size.
    pub fn set_cell_size(&mut self, cell_size: (f32, f32)) {
        self.cell_size = cell_size;
        // Instance cache is keyed to the old cell size via glyph offsets
        // / sizes in pixels — invalidate so the next frame rebuilds
        // against the new metrics.
        self.last_instance_count = 0;
    }

    /// Render a single frame.
    pub fn render(
        &mut self,
        grid: &RenderGrid,
        text_renderer: &mut TextRenderer,
        font_config: &mechanic_config::font::FontConfig,
        uniforms: FrameUniforms,
    ) {
        // ── Update globals uniform ────────────────────────────────────────────

        let globals = Globals {
            viewport_size: [self.size.0 as f32, self.size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time: uniforms.time,
            content_opacity: uniforms.content_opacity,
            shader_focused: if uniforms.shader_focused { 1.0 } else { 0.0 },
            text_opacity: uniforms.text_opacity,
            bloom_progress: uniforms.bloom_progress,
            bloom_peak_multiplier: uniforms.bloom_peak_multiplier,
            _pad: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // ── Pass 1: populate the atlas with every unique glyph this frame ─────
        //
        // Atlas grows (capacity-doubling in TextRenderer::alloc_slot) clear
        // the atlas_map and invalidate UVs that were computed against the
        // pre-grow texture layout.  If a grow happens *during* instance
        // emission, some instances reference old UVs and later instances
        // reference new UVs — the draw samples both against the (now-new)
        // texture and the user sees a one-frame flash of garbled glyphs.
        //
        // Rasterizing all unique glyphs up front moves every grow to
        // before instance emission, so the atlas stays stable through the
        // rest of the frame.  The second-pass rasterize_char calls below
        // all hit the char_cache fast path — no growth can occur.
        //
        // Typical English-grid frames have <200 unique (char, bold, italic)
        // triples; the HashSet is tiny.
        let unique_glyphs = collect_unique_glyphs(grid);
        text_renderer.populate_atlas(
            unique_glyphs,
            &self.device,
            &self.queue,
            font_config,
        );

        // ── Pass 2: build instance list against the now-stable atlas ──────────

        let total_cells = grid.cols * grid.rows;
        let mut instances: Vec<GpuInstance> = Vec::with_capacity(total_cells * 2);

        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let Some(cell) = grid.get(col, row) else {
                    continue;
                };

                let mut fg = cell.fg;
                let mut bg = cell.bg;

                // Apply inverse flag.
                if cell.flags.contains(crate::grid::CellFlags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // Background instance (always drawn).
                instances.push(GpuInstance {
                    cell_pos: [col as u32, row as u32],
                    atlas_uv: [0.0; 4],
                    fg_color: [0.0; 4],
                    bg_color: rgb_to_f32(bg),
                    glyph_offset: [0.0; 2],
                    glyph_size: [0.0; 2],
                    use_atlas: 0,
                    _pad: [0; 3],
                });

                // Glyph instance (only when a glyph exists).  After the
                // pass-1 populate, this call always hits the cache and
                // cannot trigger an atlas grow.
                if cell.character != ' ' {
                    let bold = cell.flags.contains(crate::grid::CellFlags::BOLD);
                    let italic = cell.flags.contains(crate::grid::CellFlags::ITALIC);

                    if let Some(info) = text_renderer.rasterize_char(
                        cell.character,
                        bold,
                        italic,
                        &self.device,
                        &self.queue,
                        font_config,
                    ) {
                        instances.push(GpuInstance {
                            cell_pos: [col as u32, row as u32],
                            atlas_uv: info.atlas_uv,
                            fg_color: rgb_to_f32(fg),
                            bg_color: rgb_to_f32(bg),
                            glyph_offset: [info.offset_x, info.offset_y],
                            glyph_size: [info.glyph_width, info.glyph_height],
                            use_atlas: 1,
                            _pad: [0; 3],
                        });
                    }
                }
            }
        }

        // ── Draw cursor ───────────────────────────────────────────────────────
        //
        // Three paths, selected by `(cursor_style, window_focused)`:
        //
        // 1. Block + focused   → no quad here.  `convert.rs` has already
        //                        recolored the cell's background to the
        //                        cursor colour; the glyph remains visible
        //                        through the solid block for free.
        // 2. Block + unfocused → emit a full-cell quad with
        //                        `use_atlas = 2u` ("hollow-block").  The
        //                        fragment shader keeps only pixels within
        //                        `HOLLOW_CURSOR_BORDER_PX` of a cell edge
        //                        and discards the interior, so the cell's
        //                        original glyph shows through the outline.
        //                        Standard iTerm2 / Terminal.app convention.
        // 3. Bar / Underline    → emit a sub-cell quad (2 px strip) in
        //                        cursor colour.  Focus-state-independent —
        //                        these cursor styles don't cover the glyph
        //                        in either state, so there's no readable/
        //                        unreadable distinction to make.

        {
            use crate::grid::CursorStyle;
            use mechanic_config::theme::palette;

            let (cx, cy) = grid.cursor_position;
            if grid.get(cx, cy).is_some() {
                let cursor_color = palette::CELESTE;
                let cell_w = self.cell_size.0;
                let cell_h = self.cell_size.1;

                // Pick the quad geometry and shader-path discriminant for
                // each (style, focus) combination, or `None` for the "no
                // quad needed" case (focused block, handled in convert.rs).
                let quad = match (grid.cursor_style, uniforms.window_focused) {
                    // Hollow block — full-cell quad, shader outlines it.
                    (CursorStyle::Block, false) => {
                        Some(([0.0f32, 0.0f32], [cell_w, cell_h], HOLLOW_BLOCK_USE_ATLAS))
                    }
                    // Solid block, focused — cell already recoloured, skip.
                    (CursorStyle::Block, true) => None,
                    // Bar — 2 px strip at the left edge.
                    (CursorStyle::Bar, _) => {
                        Some(([0.0f32, 0.0f32], [2.0f32, cell_h], SOLID_USE_ATLAS))
                    }
                    // Underline — 2 px strip at the bottom edge.
                    (CursorStyle::Underline, _) => {
                        Some(([0.0f32, cell_h - 2.0f32], [cell_w, 2.0f32], SOLID_USE_ATLAS))
                    }
                };

                if let Some((glyph_offset, glyph_size, use_atlas)) = quad {
                    instances.push(GpuInstance {
                        cell_pos: [cx as u32, cy as u32],
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

        // ── Rebuild bind group only when the atlas texture was recreated ─────
        //
        // atlas_generation increments each time alloc_slot grows the texture.
        // Checking here avoids the per-frame bind-group rebuild cost.
        let current_gen = text_renderer.atlas_generation();
        if current_gen != self.last_atlas_generation {
            self.update_atlas_bind_group(&text_renderer.atlas_view);
            self.last_atlas_generation = current_gen;
        }

        // ── Upload instances ──────────────────────────────────────────────────

        let instance_bytes = bytemuck::cast_slice::<GpuInstance, u8>(&instances);

        if instances.len() > self.instance_capacity {
            // Grow the buffer.
            let new_cap = instances.len().next_power_of_two();
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instance_buf"),
                size: (new_cap * mem::size_of::<GpuInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
        }

        self.queue.write_buffer(&self.instance_buf, 0, instance_bytes);

        // ── Render pass ───────────────────────────────────────────────────────

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.surface_config);
                return;
            }
            _ => return,
        };

        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame_encoder"),
        });

        // Non-premultiplied clear color for the macOS PostMultiplied compositor.
        // Every cell fully writes its pixels (blend: None), so this clear only
        // shows through if the grid doesn't cover the entire surface (edge
        // pixels from fractional cell sizing).
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
            // 6 vertices per quad (2 triangles), `instances.len()` instances.
            pass.draw(0..6, 0..instances.len() as u32);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();

        // Record the instance count so a subsequent `render_animation`
        // frame knows how many instances to re-draw from the retained
        // `instance_buf`.  Updated after submit so a failed surface
        // acquisition above doesn't leave us with a bogus count.
        self.last_instance_count = instances.len() as u32;
    }

    /// Fast-path frame: re-issue the previous full render's draw with
    /// a fresh globals uniform.
    ///
    /// Used for frames driven purely by animation cadence — the corner
    /// gradient pulse and electron traces in the shader are functions
    /// of the `time` and `focused` uniforms only.  The per-cell
    /// instance data (foreground/background colors, glyph UVs, cursor
    /// position) doesn't change frame-to-frame when the user is idle
    /// and the shell is quiet, so rebuilding ~4000 `GpuInstance`s and
    /// re-uploading ~200 KB per frame is pure waste.
    ///
    /// Returns `false` if no prior full render has populated the
    /// instance buffer — in that case the caller must fall back to
    /// [`Self::render`] with a freshly-converted grid.  Returns `true`
    /// on success (a frame was submitted and presented).
    ///
    /// Atlas and bind group are left untouched.  This path never
    /// triggers an atlas grow because it rasterises no glyphs.
    pub fn render_animation(&mut self, uniforms: FrameUniforms) -> bool {
        if self.last_instance_count == 0 {
            // Nothing cached yet — first frame of the window's life, or
            // after a resize that hasn't been followed by a full render.
            return false;
        }

        // Update only the globals uniform.
        let globals = Globals {
            viewport_size: [self.size.0 as f32, self.size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time: uniforms.time,
            content_opacity: uniforms.content_opacity,
            shader_focused: if uniforms.shader_focused { 1.0 } else { 0.0 },
            text_opacity: uniforms.text_opacity,
            bloom_progress: uniforms.bloom_progress,
            bloom_peak_multiplier: uniforms.bloom_peak_multiplier,
            _pad: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.surface_config);
                return true;
            }
            _ => return true,
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
            pass.draw(0..6, 0..self.last_instance_count);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
        true
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Collect the set of unique `(char, bold, italic)` glyph keys that
/// need to be rendered for `grid` this frame.
///
/// Pure function over the grid — no GPU interaction — so the atlas-
/// pre-population logic can be exercised without a wgpu device.
/// Space cells are skipped because the renderer draws no glyph for
/// them (the cell's background quad is sufficient).
fn collect_unique_glyphs(
    grid: &RenderGrid,
) -> std::collections::HashSet<(char, bool, bool)> {
    let mut unique = std::collections::HashSet::with_capacity(128);
    for cell in &grid.cells {
        if cell.character != ' ' {
            let bold = cell.flags.contains(crate::grid::CellFlags::BOLD);
            let italic = cell.flags.contains(crate::grid::CellFlags::ITALIC);
            unique.insert((cell.character, bold, italic));
        }
    }
    unique
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellFlags, RenderCell, RenderGrid};

    fn make_grid_with_cells(cols: usize, rows: usize, cells: Vec<RenderCell>) -> RenderGrid {
        let mut grid = RenderGrid::new(cols, rows);
        for (i, cell) in cells.into_iter().enumerate() {
            if i < grid.cells.len() {
                grid.cells[i] = cell;
            }
        }
        grid
    }

    fn cell(ch: char, flags: CellFlags) -> RenderCell {
        RenderCell { character: ch, flags, ..Default::default() }
    }

    #[test]
    fn unique_glyphs_empty_grid() {
        // A blank grid (all default-constructed cells are spaces) has
        // no glyphs to rasterize.  The fix must not submit empty work.
        let grid = RenderGrid::new(10, 5);
        assert!(collect_unique_glyphs(&grid).is_empty());
    }

    #[test]
    fn unique_glyphs_all_spaces_produces_empty_set() {
        let grid = make_grid_with_cells(
            3,
            1,
            vec![cell(' ', CellFlags::empty()), cell(' ', CellFlags::empty()), cell(' ', CellFlags::empty())],
        );
        assert!(collect_unique_glyphs(&grid).is_empty());
    }

    #[test]
    fn unique_glyphs_dedups_repeated_chars() {
        // Many cells showing "h" at the same style should yield one
        // entry — this is why we use a HashSet.  A filled-screen of a
        // single character stays cheap to pre-rasterize.
        let cells = vec![cell('h', CellFlags::empty()); 20];
        let grid = make_grid_with_cells(5, 4, cells);
        let u = collect_unique_glyphs(&grid);
        assert_eq!(u.len(), 1);
        assert!(u.contains(&('h', false, false)));
    }

    #[test]
    fn unique_glyphs_distinguishes_style_variants() {
        // Same character, different (bold, italic) combinations are
        // distinct atlas entries because they rasterize to different
        // bitmaps.  Atlas population must cover each combo.
        let grid = make_grid_with_cells(
            4,
            1,
            vec![
                cell('a', CellFlags::empty()),
                cell('a', CellFlags::BOLD),
                cell('a', CellFlags::ITALIC),
                cell('a', CellFlags::BOLD | CellFlags::ITALIC),
            ],
        );
        let u = collect_unique_glyphs(&grid);
        assert_eq!(u.len(), 4);
        assert!(u.contains(&('a', false, false)));
        assert!(u.contains(&('a', true, false)));
        assert!(u.contains(&('a', false, true)));
        assert!(u.contains(&('a', true, true)));
    }

    #[test]
    fn unique_glyphs_mixed_chars_and_spaces() {
        // Realistic scattered mix: the returned set contains exactly
        // the non-space chars, each counted once.
        let grid = make_grid_with_cells(
            6,
            1,
            vec![
                cell('H', CellFlags::empty()),
                cell('i', CellFlags::empty()),
                cell(' ', CellFlags::empty()),
                cell('!', CellFlags::empty()),
                cell(' ', CellFlags::empty()),
                cell('H', CellFlags::empty()),
            ],
        );
        let u = collect_unique_glyphs(&grid);
        assert_eq!(u.len(), 3);
        assert!(u.contains(&('H', false, false)));
        assert!(u.contains(&('i', false, false)));
        assert!(u.contains(&('!', false, false)));
    }

    #[test]
    fn unique_glyphs_underlined_does_not_split_from_plain() {
        // Underline is a rendering flag, not a glyph-rasterization
        // flag — the same glyph bitmap is used, the underline is
        // drawn as a separate quad in the shader.  So an underlined
        // 'a' and a plain 'a' are the same atlas key.  (Only BOLD
        // and ITALIC affect the bitmap the atlas stores.)
        let grid = make_grid_with_cells(
            2,
            1,
            vec![
                cell('a', CellFlags::empty()),
                cell('a', CellFlags::UNDERLINE),
            ],
        );
        let u = collect_unique_glyphs(&grid);
        assert_eq!(u.len(), 1);
        assert!(u.contains(&('a', false, false)));
    }
}
