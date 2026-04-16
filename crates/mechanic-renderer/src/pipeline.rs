// wgpu render pipeline: surface setup, instance buffer, and per-frame rendering.

use std::mem;

use bytemuck::{Pod, Zeroable};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use wgpu::util::DeviceExt as _;

use crate::{
    background,
    grid::RenderGrid,
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

// ── Globals uniform ───────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Globals {
    viewport_size: [f32; 2],
    cell_size: [f32; 2],
    time: f32,
    content_opacity: f32,
    /// 1.0 when the window has keyboard focus, 0.0 when blurred.  Gates the
    /// corner-gradient color pulse so unfocused/faded windows stay static.
    focused: f32,
    _pad: f32, // keep 16-byte aligned
}

// ── RenderState ───────────────────────────────────────────────────────────────

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
    /// Cell dimensions in pixels (from real font metrics).
    pub cell_size: (f32, f32),
    /// Current surface size in pixels.
    pub size: (u32, u32),
    /// Background clear color.
    pub clear_color: wgpu::Color,
}

impl RenderState {
    /// Create a new `RenderState` from a window.
    ///
    /// `cell_metrics` must come from `TextRenderer::cell_metrics()` — they
    /// provide the real cell dimensions measured from the shaped font.
    ///
    /// # Safety
    ///
    /// `window` must remain valid for the entire lifetime of the returned
    /// `RenderState` (the `'static` surface lifetime is achieved by taking
    /// ownership of the window handle via `SurfaceTarget::Window`).
    pub async fn new<W>(
        window: W,
        size: (u32, u32),
        cell_metrics: CellMetrics,
        bg: Rgb,
    ) -> Result<Self, Box<dyn std::error::Error>>
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

        // Pick an alpha mode that supports transparency.  PreMultiplied is
        // ideal (the shader already outputs premultiplied colors), but not
        // every macOS GPU configuration advertises it.  Fall back gracefully.
        log::info!("surface alpha modes available: {:?}", caps.alpha_modes);
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            wgpu::CompositeAlphaMode::PostMultiplied
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

        // ── Shader ───────────────────────────────────────────────────────────

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
        });

        // ── Bind group layout ─────────────────────────────────────────────────
        //
        // group(0) binding(0) = Globals uniform
        // group(0) binding(1) = atlas texture
        // group(0) binding(2) = atlas sampler

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
            focused: 1.0,
            _pad: 0.0,
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

        // ── Placeholder bind group (atlas filled in later) ────────────────────
        //
        // We create a 1×1 dummy texture so the bind group is valid from the
        // start; it gets replaced by `update_atlas_bind_group`.
        let dummy_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy_atlas"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let dummy_view = dummy_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group =
            Self::make_bind_group(&device, &bind_group_layout, &globals_buf, &dummy_view, &sampler);

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
            cell_size,
            size,
            clear_color: background::clear_color(bg),
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        globals_buf: &wgpu::Buffer,
        atlas_view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
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
        );
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
            focused: 1.0,
            _pad: 0.0,
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
    }

    /// Update the cell size used by the pipeline's globals uniform.
    ///
    /// The next `render()` call will write the new cell_size to the GPU.
    /// Used by `Renderer::set_font_size` after the text renderer is rebuilt
    /// at a new point size.
    pub fn set_cell_size(&mut self, cell_size: (f32, f32)) {
        self.cell_size = cell_size;
    }

    /// Render a single frame.
    pub fn render(
        &mut self,
        grid: &RenderGrid,
        text_renderer: &mut TextRenderer,
        font_config: &mechanic_config::font::FontConfig,
        content_opacity: f32,
        time: f32,
        focused: bool,
    ) {
        // ── Update globals uniform ────────────────────────────────────────────

        let globals = Globals {
            viewport_size: [self.size.0 as f32, self.size.1 as f32],
            cell_size: [self.cell_size.0, self.cell_size.1],
            time,
            content_opacity,
            focused: if focused { 1.0 } else { 0.0 },
            _pad: 0.0,
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // ── Build instance list ───────────────────────────────────────────────

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

                // Glyph instance (only when a glyph exists).
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

        {
            use crate::grid::CursorStyle;
            use mechanic_config::theme::palette;

            // Block cursors are rendered by recoloring the cell itself in
            // `convert.rs` (so the character stays visible), so nothing to
            // draw here.  Bar and Underline don't overlap the glyph and are
            // still drawn as separate quads on top.
            let needs_quad = !matches!(grid.cursor_style, CursorStyle::Block);

            if needs_quad {
                let (cx, cy) = grid.cursor_position;
                if grid.get(cx, cy).is_some() {
                    let cursor_color = palette::CELESTE;
                    let cell_w = self.cell_size.0;
                    let cell_h = self.cell_size.1;

                    // Bar: 2px wide on the left edge.
                    // Underline: full width, 2px tall at the bottom edge.
                    let (glyph_offset, glyph_size) = match grid.cursor_style {
                        CursorStyle::Bar => ([0.0f32, 0.0f32], [2.0f32, cell_h]),
                        CursorStyle::Underline => ([0.0f32, cell_h - 2.0f32], [cell_w, 2.0f32]),
                        CursorStyle::Block => unreachable!(),
                    };

                    instances.push(GpuInstance {
                        cell_pos: [cx as u32, cy as u32],
                        atlas_uv: [0.0; 4],
                        fg_color: [0.0; 4],
                        bg_color: rgb_to_f32(cursor_color),
                        glyph_offset,
                        glyph_size,
                        use_atlas: 0,
                        _pad: [0; 3],
                    });
                }
            }
        }

        // ── Update bind group once (atlas may have grown during rasterization) ─

        self.update_atlas_bind_group(&text_renderer.atlas_view);

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
            a: content_opacity as f64,
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
    }
}
