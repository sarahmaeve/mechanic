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
        animation_flags: 0,
        text_opacity: 1.0,
        bloom_progress: 0.0,
        bloom_peak_multiplier: 1.0,
        logo_size: 270.0,
        logo_style: 0,
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
fn numbers_before_rtl_text_have_visible_pixels() {
    use mechanic_config::font::FontConfig;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = FontConfig { family: "Menlo".into(), size: 18.0, ..Default::default() };
    let mut text = TextRenderer::new(&device, &queue, &config, 2.0);
    let metrics = text.cell_metrics();
    let cell_size = (metrics.cell_width.ceil(), metrics.cell_height.ceil());
    for content in ["123 العربية", " 12 345 (67) עברית 89 "] {
        let mut grid = RenderGrid::new(content.chars().count(), 1);
        grid.cursor_visible = false;
        for (cell, character) in grid.cells.iter_mut().zip(content.chars()) {
            *cell = crate::RenderCell {
                character,
                fg: Rgb::new(255, 255, 255),
                bg: Rgb::new(0, 0, 0),
                ..Default::default()
            };
        }
        let shaped = text.shape_grid(&grid, &config);
        text.prepare_frame(&shaped, &device, &queue).unwrap();
        let (instances, backgrounds) = build_instances(&grid, &shaped, &text, cell_size, true);
        let width = grid.cols as u32 * cell_size.0 as u32;
        let height = cell_size.1 as u32;
        let pixels = render_fixture_pixels(
            &device,
            &queue,
            &text.atlas_view,
            None,
            fixture_globals((width, height), cell_size),
            &instances,
            backgrounds,
        );
        for (col, cell) in grid.cells.iter().enumerate() {
            if cell.character.is_ascii_digit() {
                let left = shaped[0].visual_cols[col] as u32 * cell_size.0 as u32;
                let inset = cell_size.0 as u32 / 4;
                let has_ink = (0..height).any(|y| {
                    (left + inset..left + cell_size.0 as u32 - inset)
                        .any(|x| pixels[((y * width + x) * 4) as usize] > 80)
                });
                assert!(has_ink, "missing digit pixels at {col} in {content:?}");
            }
        }
    }
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn wrapped_arabic_pixels_survive_viewport_clipping() {
    use mechanic_config::font::FontConfig;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = FontConfig { family: "Menlo".into(), size: 18.0, ..Default::default() };
    let mut text = TextRenderer::new(&device, &queue, &config, 2.0);
    let metrics = text.cell_metrics();
    let cell_size = (metrics.cell_width.ceil(), metrics.cell_height.ceil());
    let mut render = |grid: &RenderGrid, row: usize| {
        let shaped = text.shape_grid(grid, &config);
        text.prepare_frame(&shaped, &device, &queue).unwrap();
        let (instances, backgrounds) = build_instances(grid, &shaped, &text, cell_size, true);
        // Isolate the row so neighboring glyph overhangs cannot affect comparison.
        let mut selected_backgrounds = 0;
        let instances: Vec<_> = instances
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut instance)| {
                if instance.cell_pos[1] != row as u32 {
                    return None;
                }
                selected_backgrounds += u32::from(index < backgrounds as usize);
                instance.cell_pos[1] = 0;
                Some(instance)
            })
            .collect();
        render_fixture_pixels(
            &device,
            &queue,
            &text.atlas_view,
            None,
            fixture_globals((cell_size.0 as u32, cell_size.1 as u32), cell_size),
            &instances,
            selected_backgrounds,
        )
    };
    // Every character crosses a physical row; lam-alef must remain visible.
    for word in ["بلا", "العربية"] {
        let chars: Vec<_> = word.chars().collect();
        let mut grid = RenderGrid::new(1, chars.len());
        grid.cursor_visible = false;
        for (row, ch) in chars.iter().copied().enumerate() {
            grid.cells[row] = crate::RenderCell {
                character: ch,
                fg: Rgb::new(255, 255, 255),
                bg: Rgb::new(0, 0, 0),
                ..Default::default()
            };
            grid.wrapped[row] = row + 1 < chars.len();
        }
        for row in 0..chars.len() {
            let expected = render(&grid, row);
            assert!(
                expected.as_chunks::<4>().0.iter().any(|pixel| pixel[0] > 80),
                "missing ink in {word}, row {row}"
            );
            let mut viewport = RenderGrid::new(1, 1);
            viewport.cursor_visible = false;
            viewport.cells[0] = grid.cells[row].clone();
            viewport.bidi_prefix = chars[..row].iter().collect();
            viewport.bidi_suffix = chars[row + 1..].iter().collect();
            let actual = render(&viewport, 0);
            let max_delta =
                actual.iter().zip(&expected).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(max_delta <= 1, "viewport changed {word}, row {row}: max delta {max_delta}");
        }
    }
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
    let logo = crate::logo::Logo::new(&device, &queue, mechanic_config::theme::LogoStyle::Triangle);
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
    for logo_size in [270.0, f32::from(mechanic_config::theme::DEFAULT_LOGO_SIZE)] {
        let coordinate =
            |value: f32| (320.0 - 16.0 - logo_size + value * logo_size / 256.0) as usize;
        let render = |time, enabled| {
            let mut globals = fixture_globals((320, 320), (320.0, 320.0));
            globals.logo_size = logo_size;
            globals.time = time;
            globals.animation_flags = if enabled { 2 } else { 0 };
            render_fixture_pixels(
                &device,
                &queue,
                &logo.view,
                Some(&logo.view),
                globals,
                &instances,
                1,
            )
        };
        let quiet = render(0.0, false);
        assert_eq!(quiet, render(1.0, false), "quiet logo changed with time");
        let first = render(0.0, true);
        let next = render(1.0, true);
        let center = (coordinate(96.0) * 320 + coordinate(128.0)) * 4 + 1;
        let glow = i16::from(first[center]) - i16::from(quiet[center]);
        assert!((5..=15).contains(&glow), "central glow missing or too bright: {glow}");
        // Pulse positions on each perimeter at t=0 and t=1, in SVG coordinates.
        for (pixels, other, positions) in [
            (&first, &next, [(24.0, 36.0), (154.0, 111.0)]),
            (&next, &first, [(180.0, 36.0), (128.0, 66.0)]),
        ] {
            for (x, y) in positions {
                let x = coordinate(x);
                let y = coordinate(y);
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
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn atom_logo_keeps_three_electrons_on_their_orbits() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let logo = crate::logo::Logo::new(&device, &queue, mechanic_config::theme::LogoStyle::Atom);
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
    for logo_size in [270.0, f32::from(mechanic_config::theme::DEFAULT_LOGO_SIZE)] {
        let coordinate = |value: f32| (304.0 - logo_size + value * logo_size / 256.0) as usize;
        let render_flags = |time, animation_flags, size| {
            let mut globals = fixture_globals((320, 320), (320.0, 320.0));
            globals.logo_style = logo.style as u32;
            globals.logo_size = size;
            globals.time = time;
            globals.animation_flags = animation_flags;
            render_fixture_pixels(
                &device,
                &queue,
                &logo.view,
                Some(&logo.view),
                globals,
                &instances,
                1,
            )
        };
        let render = |time, enabled, size| render_flags(time, if enabled { 2 } else { 0 }, size);
        let quiet = render(0.0, false, logo_size);
        assert_eq!(quiet, render(2.0, false, logo_size), "quiet atom changed with time");
        let first = render(0.0, true, logo_size);
        let next = render(1.0, true, logo_size);
        let hidden = render(0.0, false, 0.0);
        let background_only = render_flags(1.0, 1, logo_size);
        let corner = (310 * 320 + 310) * 4;
        assert_eq!(
            &first[corner..corner + 3],
            &next[corner..corner + 3],
            "logo animation changed background lighting"
        );
        assert_ne!(
            &quiet[corner..corner + 3],
            &background_only[corner..corner + 3],
            "background animation did not change lighting"
        );
        let center = (coordinate(128.0) * 320 + coordinate(128.0)) * 4 + 1;
        assert!(quiet[center] > hidden[center] + 50, "nucleus missing");
        for (time, pixels, other) in [(0.0, &first, &next), (1.0, &next, &first)] {
            for (phase, angle) in [
                (time * 1.2_f32, 0.0_f32),
                (2.1 - time, std::f32::consts::FRAC_PI_3),
                (4.2 + time * 0.85, -std::f32::consts::FRAC_PI_3),
            ] {
                let x = 104.0 * phase.cos();
                let y = 38.0 * phase.sin();
                let px = coordinate(128.0 + x * angle.cos() - y * angle.sin());
                let py = coordinate(128.0 + x * angle.sin() + y * angle.cos());
                let index = (py * 320 + px) * 4;
                if time == 1.0 {
                    assert!(
                        (i16::from(background_only[index]) - i16::from(quiet[index])).abs() <= 5,
                        "background animation moved an electron"
                    );
                }
                assert!(
                    i16::from(pixels[index]) - i16::from(other[index]) > 65,
                    "electron missing or stationary at {px},{py}"
                );
            }
        }
        if let Some(path) = std::env::var_os("MECHANIC_ATOM_FIXTURE_PNG") {
            let size = resvg::tiny_skia::IntSize::from_wh(320, 320).unwrap();
            resvg::tiny_skia::Pixmap::from_vec(first, size).unwrap().save_png(path).unwrap();
        }
    }
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn decorations_render_and_conceal_with_text() {
    use crate::grid::CellFlags;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = mechanic_config::FontConfig::default();
    let mut text = TextRenderer::new(&device, &queue, &config, 1.0);
    let mut grid = RenderGrid::new(6, 2);
    grid.cursor_visible = false;
    for (col, style) in [
        CellFlags::UNDERLINE,
        CellFlags::DOUBLE_UNDERLINE,
        CellFlags::UNDERCURL,
        CellFlags::DOTTED_UNDERLINE,
        CellFlags::DASHED_UNDERLINE,
        CellFlags::STRIKEOUT,
    ]
    .into_iter()
    .enumerate()
    {
        for row in 0..2 {
            let cell = grid.get_mut(col, row).unwrap();
            cell.flags = style | if col == 5 { CellFlags::empty() } else { CellFlags::UNDERLINE };
            if row == 1 {
                cell.flags |= CellFlags::HIDDEN;
            }
            cell.fg = Rgb::new(255, 0, 0);
            if col == 0 {
                cell.underline_color = Some(Rgb::new(0, 255, 0));
            }
        }
    }
    let shaped = text.shape_grid(&grid, &config);
    text.prepare_frame(&shaped, &device, &queue).unwrap();
    let (instances, backgrounds) = build_instances(&grid, &shaped, &text, (32.0, 36.0), true);
    let pixels = render_fixture_pixels(
        &device,
        &queue,
        &text.atlas_view,
        None,
        fixture_globals((192, 72), (32.0, 36.0)),
        &instances,
        backgrounds,
    );
    for col in 0usize..6 {
        let channel = if col == 0 { 1 } else { 0 };
        let count = |row: usize| {
            (row * 36..(row + 1) * 36)
                .flat_map(|y| (col * 32..(col + 1) * 32).map(move |x| (y * 192 + x) * 4 + channel))
                .filter(|index| pixels[*index] > 100)
                .count()
        };
        assert!(count(0) > 8, "decoration {col} missing");
        assert_eq!(count(1), 0, "concealed decoration {col} visible");
    }
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn row_cache_matches_full_rebuild_after_layout_and_cursor_changes() {
    use crate::grid::{CellFlags, CursorStyle};
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = mechanic_config::FontConfig { family: "Menlo".into(), ..Default::default() };
    let mut text = TextRenderer::new(&device, &queue, &config, 1.0);
    let metrics = text.cell_metrics();
    let mut cell_size = (metrics.cell_width, metrics.cell_height);
    let mut cache = InstanceCache::default();
    let mut uploaded = Vec::new();
    let mut grid = RenderGrid::new(16, 5);
    for (row, sample) in
        ["السلام عليكم", "café Straße", "Україна Россия", "日本語", "abc"].iter().enumerate()
    {
        let mut col = 0;
        for ch in sample.chars() {
            let wide = unicode_width::UnicodeWidthChar::width(ch) == Some(2);
            let cell = grid.get_mut(col, row).unwrap();
            cell.character = ch;
            if wide {
                cell.flags = CellFlags::WIDE_CHAR;
                grid.get_mut(col + 1, row).unwrap().flags = CellFlags::WIDE_CHAR_SPACER;
            }
            col += if wide { 2 } else { 1 };
        }
    }
    let mut saved_cells = grid.cells.clone();
    for frame in 0..16 {
        match frame {
            1 => grid.cells[18].fg = Rgb::new(255, 0, 0),
            2 => grid.cells[18].zerowidth.push('\u{301}'),
            3 => grid.cells[18].character = ' ',
            4 => grid.cells[18].character = 'Ж',
            5 => {
                grid.cursor_style = CursorStyle::Bar;
                grid.cursor_position = (2, 2);
            }
            6 => grid.cursor_position = (2, 1),
            7 => grid.cursor_visible = false,
            8 => {
                grid.wrapped[0] = true;
                grid.bidi_prefix = "عربي ".into();
            }
            9 => {
                grid.cells[18].flags |=
                    CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDERLINE | CellFlags::STRIKEOUT
            }
            10 => {
                grid.cells[18].underline_color = Some(Rgb::new(0, 255, 0));
            }
            11 => {
                saved_cells.clone_from(&grid.cells);
                cache.invalidate();
            }
            12 => {
                cell_size.0 += 1.0;
            }
            13 => grid = RenderGrid::new(8, 3),
            14 => {
                grid.cursor_style = CursorStyle::Bar;
                grid.cursor_position = (0, 0);
                for (col, ch) in "العربية".chars().enumerate() {
                    grid.cells[col].character = ch;
                }
            }
            15 => grid.cells[0].character = 'X',
            _ => {}
        }
        let shaped = text.shape_grid(&grid, &config);
        text.prepare_frame(&shaped, &device, &queue).unwrap();
        let epoch = (text.atlas_generation(), cell_size);
        let ranges = cache.update(&grid, &shaped, epoch, true, |row, reusable| {
            build_instances_for_rows(&grid, &shaped, &text, cell_size, true, row..row + 1, reusable)
                .0
        });
        uploaded.resize(cache.instances.len(), GpuInstance::zeroed());
        for range in &ranges {
            uploaded[range.clone()].copy_from_slice(&cache.instances[range.clone()]);
        }
        let (expected, backgrounds) = build_instances(&grid, &shaped, &text, cell_size, true);
        assert_eq!(cache.background_count, backgrounds);
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&uploaded),
            bytemuck::cast_slice::<_, u8>(&expected),
            "frame {frame}"
        );
        assert!(
            cache
                .update(&grid, &shaped, epoch, true, |_, _| panic!("unchanged row rebuilt"))
                .is_empty()
        );
        if frame == 11 {
            assert_eq!(grid.cells, saved_cells);
        }
    }
}

#[test]
#[ignore = "offscreen geometry benchmark; run explicitly with --release on macOS"]
fn row_cache_geometry_benchmark() {
    use std::io::Write;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = mechanic_config::FontConfig { family: "Menlo".into(), ..Default::default() };
    let mut text = TextRenderer::new(&device, &queue, &config, 2.0);
    let metrics = text.cell_metrics();
    let cell_size = (metrics.cell_width, metrics.cell_height);
    let path = std::env::var_os("MECHANIC_ROW_CACHE_BENCH_CSV")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mechanic-row-cache.csv"));
    let mut csv = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(csv, "workload,iteration,full_ns,cached_ns,full_upload_bytes,cached_upload_bytes")
        .unwrap();
    for workload in ["cell", "row", "full", "scroll"] {
        let mut grid = RenderGrid::new(121, 42);
        grid.cursor_visible = false;
        for (index, cell) in grid.cells.iter_mut().enumerate() {
            cell.character = (b'A' + (index % 26) as u8) as char;
        }
        let mut cache = InstanceCache::default();
        for iteration in 0..220 {
            let ch = (b'A' + (iteration % 26) as u8) as char;
            match workload {
                "cell" => grid.cells[21 * 121 + 60].character = ch,
                "row" => {
                    for cell in &mut grid.cells[21 * 121..22 * 121] {
                        cell.character = ch;
                    }
                }
                "full" => {
                    for cell in &mut grid.cells {
                        cell.character = ch;
                    }
                }
                "scroll" => {
                    grid.cells.rotate_left(121);
                    for cell in &mut grid.cells[41 * 121..] {
                        cell.character = ch;
                    }
                }
                _ => unreachable!(),
            }
            let shaped = text.shape_grid(&grid, &config);
            text.prepare_frame(&shaped, &device, &queue).unwrap();
            let epoch = (text.atlas_generation(), cell_size);
            let full = || {
                let start = Instant::now();
                let instances = build_instances(&grid, &shaped, &text, cell_size, true).0;
                let ns = start.elapsed().as_nanos();
                (ns, std::hint::black_box(instances))
            };
            let mut cached = || {
                let start = Instant::now();
                let ranges = cache.update(&grid, &shaped, epoch, true, |row, reusable| {
                    build_instances_for_rows(
                        &grid,
                        &shaped,
                        &text,
                        cell_size,
                        true,
                        row..row + 1,
                        reusable,
                    )
                    .0
                });
                (start.elapsed().as_nanos(), ranges)
            };
            let ((full_ns, expected), (cached_ns, ranges)) = if iteration % 2 == 0 {
                (full(), cached())
            } else {
                let result = cached();
                (full(), result)
            };
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&cache.instances),
                bytemuck::cast_slice::<_, u8>(&expected)
            );
            if iteration >= 20 {
                writeln!(
                    csv,
                    "{workload},{iteration},{full_ns},{cached_ns},{},{}",
                    expected.len() * mem::size_of::<GpuInstance>(),
                    ranges.iter().map(|r| r.len()).sum::<usize>() * mem::size_of::<GpuInstance>()
                )
                .unwrap();
            }
        }
    }
    csv.flush().unwrap();
    eprintln!("row-cache geometry timings: {}", path.display());
}

fn fixture_globals(size: (u32, u32), cell_size: (f32, f32)) -> Globals {
    Globals {
        viewport_size: [size.0 as f32, size.1 as f32],
        cell_size: [cell_size.0, cell_size.1],
        time: 0.0,
        content_opacity: 1.0,
        animation_flags: 0,
        text_opacity: 1.0,
        bloom_progress: 0.0,
        bloom_peak_multiplier: 1.0,
        logo_size: 270.0,
        logo_style: 0,
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
