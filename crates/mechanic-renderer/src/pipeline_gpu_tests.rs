use super::*;

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn cursor_geometry_and_overlapping_glyph_coverage() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
    });
    let pipeline =
        create_cell_pipeline(&device, &shader, None, wgpu::TextureFormat::Rgba8Unorm, true);
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 64, height: 32, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let atlas = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        atlas.as_image_copy(),
        &[128],
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(1), rows_per_image: None },
        atlas.size(),
    );
    let globals = Globals {
        viewport_size: [64.0, 32.0],
        cell_size: [16.0, 16.0],
        time: 0.0,
        content_opacity: 0.5,
        shader_focused: 0.0,
        text_opacity: 1.0,
        bloom_progress: 0.0,
        bloom_peak_multiplier: 1.0,
        _pad: [0.0; 2],
    };
    let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::bytes_of(&globals),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let atlas_view = atlas.create_view(&Default::default());
    let sampler = device.create_sampler(&Default::default());
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&atlas_view),
            },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&atlas_view),
            },
        ],
    });
    let base = GpuInstance {
        cell_pos: [0, 0],
        atlas_uv: [0.0, 0.0, 1.0, 1.0],
        fg_color: [1.0, 1.0, 1.0, 1.0],
        bg_color: [1.0, 0.0, 0.0, 1.0],
        glyph_offset: [0.0; 2],
        glyph_size: [2.0, 16.0],
        use_atlas: CURSOR_USE_ATLAS,
        _pad: [0; 3],
    };
    let instances = [
        base,
        GpuInstance {
            cell_pos: [1, 0],
            glyph_offset: [0.0, 14.0],
            glyph_size: [16.0, 2.0],
            ..base
        },
        GpuInstance {
            cell_pos: [2, 0],
            glyph_size: [32.0, 16.0],
            use_atlas: HOLLOW_BLOCK_USE_ATLAS,
            ..base
        },
        GpuInstance {
            cell_pos: [0, 1],
            glyph_size: [16.0, 16.0],
            use_atlas: GLYPH_USE_ATLAS,
            ..base
        },
        GpuInstance {
            cell_pos: [0, 1],
            glyph_offset: [8.0, 0.0],
            glyph_size: [16.0, 16.0],
            use_atlas: GLYPH_USE_ATLAS,
            ..base
        },
    ];
    let instance_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&instances),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 256 * 32,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    let target_view = target.create_view(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 0.5 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_vertex_buffer(0, instance_buf.slice(..));
        pass.draw(0..6, 0..instances.len() as u32);
    }
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: None,
            },
        },
        target.size(),
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.map_async(wgpu::MapMode::Read, .., move |result| tx.send(result).unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let bytes = readback.get_mapped_range(..).unwrap();
    let pixel = |x: usize, y: usize| &bytes[(y * 64 + x) * 4..(y * 64 + x + 1) * 4];
    assert_eq!(pixel(0, 8)[0], 255, "bar missing");
    assert_eq!(pixel(3, 8)[0], 0, "bar became block");
    assert_eq!(pixel(20, 14)[0], 255, "underline missing");
    assert_eq!(pixel(20, 12)[0], 0, "underline became block");
    assert_eq!(pixel(32, 8)[0], 255, "wide hollow left edge");
    assert_eq!(pixel(48, 8)[0], 0, "spurious border in middle of wide cursor");
    assert_eq!(pixel(63, 8)[0], 255, "wide hollow right edge");
    assert!((i16::from(pixel(4, 24)[0]) - 128).abs() <= 1);
    assert!((i16::from(pixel(12, 24)[0]) - 192).abs() <= 1, "second glyph erased first");
    assert!((i16::from(pixel(12, 24)[3]) - 128).abs() <= 1, "glyph changed window opacity");
}

#[test]
#[ignore = "renders a multilingual PNG using Metal; run explicitly on macOS"]
fn multilingual_pixel_fixture() {
    use crate::grid::{CellFlags, RenderCell};
    use unicode_width::UnicodeWidthChar;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = mechanic_config::font::FontConfig {
        family: "Menlo".into(),
        size: 14.0,
        ..Default::default()
    };
    let mut text = TextRenderer::new(&device, &queue, &config, 2.0);
    let metrics = text.cell_metrics();
    let mut grid = RenderGrid::new(84, 24);
    grid.cursor_visible = false;
    let samples = [
        "Russian: Съешь ещё этих мягких французских булок, да выпей чаю.",
        "Ukrainian: Україна, Київ, ґанок, їжак, єдність, п’ять.",
        "Japanese: 日本語の表示。ひらがな、カタカナ、漢字。東京、大阪、京都。",
        "French: À bientôt ! Noël, cœur, français, où, déjà, façade.",
        "German: Grüße aus Köln. Äpfel, Öl, über, Straße, groß.",
        "Spanish: ¡Buenos días! ¿Qué tal? España, niño, pingüino.",
        "Portuguese: Olá! São Paulo, coração, ação, avó, português.",
        "Italian: Città, perché, più, già, caffè, un po’ d’acqua.",
        "Decomposed: Cafe\u{301} A\u{308} n\u{303} o\u{302} a\u{308}\u{301} | ガ キ\u{3099}",
        "السَّلَامُ عَلَيْكُمْ، لا إله إلا الله. العربية لغة جميلة.",
        "قالت الصحيفة: «ارتفعت المبيعات بنسبة ١٢٪ في عام 2026». هل يستمر النمو؟",
        "تقرير (OpenData): بلغت القيمة 123.45 دولاراً، في ١ أكتوبر ٢٠٢٦.",
        "",
        "أعلنت وزارة الثقافة، اليوم الخميس، افتتاح معرض جديد للكتاب بمشاركة دور نشر من بلدان متعددة. وقال المنظمون إن البرنامج يضم أكثر من ١٢٠ فعالية، بينها ندوات عن الترجمة والتعليم، وإن عدد الزوار ارتفع بنسبة 15% مقارنة بالعام الماضي. وأضاف التقرير: «القراءة تفتح أبواب المعرفة، وتقرّب الشعوب». وتستمر الفعاليات حتى نهاية الأسبوع، مع إتاحة بعض اللقاءات عبر الإنترنت (OpenData). وأكد المشاركون أهمية دعم المكتبات العامة وتشجيع الأطفال على القراءة.",
    ];
    let mut row = 0;
    for sample in samples {
        let mut col = 0;
        for ch in sample.chars() {
            let width = ch.width().unwrap_or(0);
            if width == 0 {
                if col > 0 {
                    let base = if grid.cells[row * grid.cols + col - 1]
                        .flags
                        .contains(CellFlags::WIDE_CHAR_SPACER)
                    {
                        col - 2
                    } else {
                        col - 1
                    };
                    grid.cells[row * grid.cols + base].zerowidth.push(ch);
                }
                continue;
            }
            if col + width > grid.cols {
                grid.wrapped[row] = true;
                row += 1;
                col = 0;
            }
            assert!(row < grid.rows);
            grid.cells[row * grid.cols + col] = RenderCell {
                character: ch,
                fg: Rgb::new(225, 235, 240),
                bg: Rgb::new(12, 16, 20),
                flags: if width == 2 { CellFlags::WIDE_CHAR } else { CellFlags::empty() },
                ..Default::default()
            };
            if width == 2 {
                grid.cells[row * grid.cols + col + 1] = RenderCell {
                    flags: CellFlags::WIDE_CHAR_SPACER,
                    fg: Rgb::new(225, 235, 240),
                    bg: Rgb::new(12, 16, 20),
                    ..Default::default()
                };
            }
            col += width;
        }
        row += 1;
    }
    let shaped = text.shape_grid(&grid, &config);
    text.prepare_glyphs(
        shaped.iter().flat_map(|row| row.glyphs.iter().map(|glyph| glyph.cache_key)),
        &device,
        &queue,
    )
    .unwrap();
    let cell_size = (metrics.cell_width, metrics.cell_height);
    let (instances, backgrounds) = build_instances(&grid, &shaped, &text, cell_size, true);
    let width = (grid.cols as f32 * cell_size.0).ceil() as u32;
    let height = (grid.rows as f32 * cell_size.1).ceil() as u32;
    let pixels = render_fixture_pixels(
        &device,
        &queue,
        &text.atlas_view,
        None,
        fixture_globals((width, height), cell_size),
        &instances,
        backgrounds,
    );
    let path = std::env::var_os("MECHANIC_TEXT_FIXTURE_PNG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mechanic-text-fixture.png"));
    let size = resvg::tiny_skia::IntSize::from_wh(width, height).unwrap();
    resvg::tiny_skia::Pixmap::from_vec(pixels, size).unwrap().save_png(&path).unwrap();
    eprintln!("multilingual GPU fixture: {}", path.display());
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn triangle_logo_is_static_by_default_and_pulses_when_enabled() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let logo = crate::logo::Logo::new(&device, &queue);
    let instances = [GpuInstance {
        cell_pos: [0, 0],
        atlas_uv: [0.0; 4],
        fg_color: [0.0; 4],
        bg_color: [0.02, 0.03, 0.04, 1.0],
        glyph_offset: [0.0; 2],
        glyph_size: [0.0; 2],
        use_atlas: 0,
        _pad: [0; 3],
    }];
    let render = |time, enabled| {
        let mut globals = fixture_globals((320, 320), (320.0, 320.0));
        globals.time = time;
        globals.shader_focused = if enabled { 1.0 } else { 0.0 };
        render_fixture_pixels(&device, &queue, &logo.view, Some(&logo.view), globals, &instances, 1)
    };
    let quiet = render(0.0, false);
    assert_eq!(quiet, render(1.0, false), "quiet logo changed with time");
    let first = render(0.0, true);
    let next = render(1.0, true);
    let center = (135 * 320 + 169) * 4 + 1;
    let glow = i16::from(first[center]) - i16::from(quiet[center]);
    assert!((5..=15).contains(&glow), "central glow missing or too bright: {glow}");
    // Pulse positions on each perimeter at t=0 and t=1, in SVG coordinates.
    for (pixels, other, positions) in [
        (&first, &next, [(24.0, 36.0), (154.0, 111.0)]),
        (&next, &first, [(180.0, 36.0), (128.0, 66.0)]),
    ] {
        for (x, y) in positions {
            let x = (34.0 + x * 270.0 / 256.0) as usize;
            let y = (34.0 + y * 270.0 / 256.0) as usize;
            let index = (y * 320 + x) * 4;
            assert!(
                i16::from(pixels[index]) - i16::from(other[index]) > 100,
                "pulse missing or stationary at {x},{y}"
            );
        }
    }
    if let Some(path) = std::env::var_os("MECHANIC_LOGO_FIXTURE_PNG") {
        let size = resvg::tiny_skia::IntSize::from_wh(320, 320).unwrap();
        resvg::tiny_skia::Pixmap::from_vec(first, size).unwrap().save_png(path).unwrap();
    }
}

fn fixture_globals(size: (u32, u32), cell_size: (f32, f32)) -> Globals {
    Globals {
        viewport_size: [size.0 as f32, size.1 as f32],
        cell_size: [cell_size.0, cell_size.1],
        time: 0.0,
        content_opacity: 1.0,
        shader_focused: 0.0,
        text_opacity: 1.0,
        bloom_progress: 0.0,
        bloom_peak_multiplier: 1.0,
        _pad: [0.0; 2],
    }
}

fn render_fixture_pixels(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atlas: &wgpu::TextureView,
    logo: Option<&wgpu::TextureView>,
    globals: Globals,
    instances: &[GpuInstance],
    backgrounds: u32,
) -> Vec<u8> {
    let size = (globals.viewport_size[0] as u32, globals.viewport_size[1] as u32);
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
    });
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let binding_layout = create_cell_bind_group_layout(device);
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&binding_layout)],
        immediate_size: 0,
    });
    let background = create_cell_pipeline(device, &shader, Some(&layout), format, false);
    let foreground = create_cell_pipeline(device, &shader, Some(&layout), format, true);
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let blank_logo = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let blank_logo_view = blank_logo.create_view(&Default::default());
    let logo_view = logo.unwrap_or(&blank_logo_view);
    let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::bytes_of(&globals),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let group = RenderState::make_bind_group(
        device,
        &binding_layout,
        &globals_buf,
        atlas,
        &sampler,
        logo_view,
    );
    let instance_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(instances),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let stride = (size.0 * 4).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(stride) * u64::from(size.1),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    let view = target.create_view(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_bind_group(0, &group, &[]);
        pass.set_vertex_buffer(0, instance_buf.slice(..));
        pass.set_pipeline(&background);
        pass.draw(0..6, 0..backgrounds);
        pass.set_pipeline(&foreground);
        pass.draw(0..6, backgrounds..instances.len() as u32);
    }
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
        },
        target.size(),
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.map_async(wgpu::MapMode::Read, .., move |result| tx.send(result).unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let bytes = readback.get_mapped_range(..).unwrap();
    bytes
        .chunks(stride as usize)
        .flat_map(|row| row[..size.0 as usize * 4].iter().copied())
        .collect()
}
