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

/// Solid background/cursor shader branch.
const SOLID_USE_ATLAS: u32 = 0;

/// Glyph shader branch.
#[expect(dead_code, reason = "reserved for future callers; paired with the _USE_ATLAS constants")]
const GLYPH_USE_ATLAS: u32 = 1;

/// Hollow block cursor for unfocused windows.
const HOLLOW_BLOCK_USE_ATLAS: u32 = 2;

/// Cursor border in physical pixels; must match cell.wgsl.
#[expect(dead_code, reason = "used in the shader; recorded here so both sides drift together")]
const HOLLOW_CURSOR_BORDER_PX: f32 = 1.5;

/// Per-frame shader inputs.
#[derive(Debug, Clone, Copy)]
pub struct FrameUniforms {
    /// Surface opacity, from 0 (transparent) to 1 (opaque).
    pub content_opacity: f32,
    /// Glyph coverage multiplier; does not affect backgrounds.
    pub text_opacity: f32,
    /// Seconds since window creation.
    pub time: f32,
    /// Enables continuous animation when focused and --hot-cpu is set.
    pub shader_focused: bool,
    /// OS keyboard focus, independent of the animation flag.
    pub window_focused: bool,
    /// Focus bloom progress in [0, 1]; zero when inactive.
    pub bloom_progress: f32,
    /// Logo opacity multiplier at the bloom's midpoint.
    pub bloom_peak_multiplier: f32,
}

/// GPU-side mirror of [`FrameUniforms`], laid out to match the `Globals` struct in `cell.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Globals {
    viewport_size: [f32; 2],
    cell_size: [f32; 2],
    time: f32,
    content_opacity: f32,
    /// Float representation of [`FrameUniforms::shader_focused`].
    shader_focused: f32,
    /// Glyph-coverage multiplier for the text path.  1.0 for focused windows; configurable idle value for blurred windows.  See [`FrameUniforms::text_opacity`].
    text_opacity: f32,
    /// Progress through the focus-gain bloom in `[0, 1]`.  See [`FrameUniforms::bloom_progress`].
    bloom_progress: f32,
    /// Peak multiplier applied to logo opacity at bloom midpoint. See [`FrameUniforms::bloom_peak_multiplier`].
    bloom_peak_multiplier: f32,
    /// Padding to match WGSL uniform alignment.
    _pad: [f32; 2],
}

/// Intermediate result of `init_surface`: device/queue/surface ready, but no pipeline or atlas yet.
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

    let surface = instance.create_surface(window)?;

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

    Ok(SurfaceInit { device, queue, surface, surface_config })
}

impl RenderState {
    /// Build the pipeline and bind the text renderer's atlas.
    pub fn new_with_atlas(
        SurfaceInit { device, queue, surface, surface_config }: SurfaceInit,
        atlas_view: &wgpu::TextureView,
        atlas_generation: u64,
        cell_metrics: CellMetrics,
        bg: Rgb,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let size = (surface_config.width, surface_config.height);
        let surface_format = surface_config.format;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
        });

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
                buffers: &[Some(instance_layout)],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
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

        let cell_size = (cell_metrics.cell_width, cell_metrics.cell_height);
        let globals = Globals {
            viewport_size: [size.0 as f32, size.1 as f32],
            cell_size: [cell_size.0, cell_size.1],
            time: 0.0,
            content_opacity: 1.0,
            shader_focused: 1.0,
            text_opacity: 1.0,
            bloom_progress: 0.0,
            bloom_peak_multiplier: 1.0,
            _pad: [0.0; 2],
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

        let logo = Logo::new(&device, &queue);

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
        self.surface.configure(&self.device, &self.surface_config);

        let globals = Globals {
            viewport_size: [new_size.0 as f32, new_size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time: 0.0,
            content_opacity: 1.0,
            shader_focused: 1.0,
            text_opacity: 1.0,
            bloom_progress: 0.0,
            bloom_peak_multiplier: 1.0,
            _pad: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        self.last_instance_count = 0;
    }

    /// Update the cell size used by the pipeline's globals uniform.
    pub fn set_cell_size(&mut self, cell_size: (f32, f32)) {
        self.cell_size = cell_size;
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

        let unique_glyphs = collect_unique_glyphs(grid);
        text_renderer.populate_atlas(unique_glyphs, &self.device, &self.queue, font_config);

        let total_cells = grid.cols * grid.rows;
        let mut instances: Vec<GpuInstance> = Vec::with_capacity(total_cells * 2);

        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let Some(cell) = grid.get(col, row) else {
                    continue;
                };

                let mut fg = cell.fg;
                let mut bg = cell.bg;

                if cell.flags.contains(crate::grid::CellFlags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }

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

        {
            use crate::grid::CursorStyle;
            use mechanic_config::theme::palette;

            let (cx, cy) = grid.cursor_position;
            if grid.get(cx, cy).is_some() {
                let cursor_color = palette::CELESTE;
                let cell_w = self.cell_size.0;
                let cell_h = self.cell_size.1;

                let quad = match (grid.cursor_style, uniforms.window_focused) {
                    (CursorStyle::Block, false) => {
                        Some(([0.0f32, 0.0f32], [cell_w, cell_h], HOLLOW_BLOCK_USE_ATLAS))
                    }
                    (CursorStyle::Block, true) => None,
                    (CursorStyle::Bar, _) => {
                        Some(([0.0f32, 0.0f32], [2.0f32, cell_h], SOLID_USE_ATLAS))
                    }
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

        let current_gen = text_renderer.atlas_generation();
        if current_gen != self.last_atlas_generation {
            self.update_atlas_bind_group(&text_renderer.atlas_view);
            self.last_atlas_generation = current_gen;
        }

        let instance_bytes = bytemuck::cast_slice::<GpuInstance, u8>(&instances);

        if instances.len() > self.instance_capacity {
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
            pass.draw(0..6, 0..instances.len() as u32);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(surface_texture);

        self.last_instance_count = instances.len() as u32;
    }

    /// Draw cached instances with new uniforms; false if no full frame is cached.
    pub fn render_animation(&mut self, uniforms: FrameUniforms) -> bool {
        if self.last_instance_count == 0 {
            return false;
        }

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
        self.queue.present(surface_texture);
        true
    }
}

/// Collect the set of unique `(char, bold, italic)` glyph keys that need to be rendered for `grid` this frame.
fn collect_unique_glyphs(grid: &RenderGrid) -> std::collections::HashSet<(char, bool, bool)> {
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
        let grid = RenderGrid::new(10, 5);
        assert!(collect_unique_glyphs(&grid).is_empty());
    }

    #[test]
    fn unique_glyphs_all_spaces_produces_empty_set() {
        let grid = make_grid_with_cells(
            3,
            1,
            vec![
                cell(' ', CellFlags::empty()),
                cell(' ', CellFlags::empty()),
                cell(' ', CellFlags::empty()),
            ],
        );
        assert!(collect_unique_glyphs(&grid).is_empty());
    }

    #[test]
    fn unique_glyphs_dedups_repeated_chars() {
        let cells = vec![cell('h', CellFlags::empty()); 20];
        let grid = make_grid_with_cells(5, 4, cells);
        let u = collect_unique_glyphs(&grid);
        assert_eq!(u.len(), 1);
        assert!(u.contains(&('h', false, false)));
    }

    #[test]
    fn unique_glyphs_distinguishes_style_variants() {
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
        let grid = make_grid_with_cells(
            2,
            1,
            vec![cell('a', CellFlags::empty()), cell('a', CellFlags::UNDERLINE)],
        );
        let u = collect_unique_glyphs(&grid);
        assert_eq!(u.len(), 1);
        assert!(u.contains(&('a', false, false)));
    }
}
