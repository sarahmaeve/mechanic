//! Offscreen tests exercise the same pane preparation and render-pass code as a window.
use std::{collections::HashMap, sync::atomic::AtomicBool, time::Duration};

use wgpu::util::DeviceExt as _;

use super::*;
use crate::{
    logo::Logo,
    pipeline::{SurfaceRecovery, create_cell_bind_group_layout, create_cell_pipeline},
};

fn fixture() -> (RenderState, TextRenderer, FontConfig) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let config = FontConfig { family: "Menlo".into(), size: 14.0, ..Default::default() };
    let text = TextRenderer::new(&device, &queue, &config, 1.0);
    let metrics = text.cell_metrics();
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cell.wgsl").into()),
    });
    let bind_group_layout = create_cell_bind_group_layout(&device);
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let pipeline = create_cell_pipeline(&device, &shader, Some(&layout), format, false);
    let foreground_pipeline = create_cell_pipeline(&device, &shader, Some(&layout), format, true);
    let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::bytes_of(&Globals::zeroed()),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 256 * mem::size_of::<GpuInstance>() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let sampler = device.create_sampler(&Default::default());
    let logo = Logo::new(&device, &queue, mechanic_config::theme::LogoStyle::Triangle);
    let bind_group = RenderState::make_bind_group(
        &device,
        &bind_group_layout,
        &globals_buf,
        &text.atlas_view,
        &sampler,
        &logo.view,
    );
    let size = (160, 96);
    let state = RenderState {
        device,
        queue,
        surface: None,
        surface_factory: Box::new(|| panic!("offscreen fixture cannot acquire a window surface")),
        adapter,
        device_lost: Arc::new(AtomicBool::new(false)),
        surface_recovery: SurfaceRecovery::default(),
        surface_config: wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0,
            height: size.1,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        },
        pipeline,
        foreground_pipeline,
        bind_group_layout,
        bind_group,
        globals_buf,
        instance_buf,
        instance_capacity: 256,
        sampler,
        logo,
        cell_size: (metrics.cell_width, metrics.cell_height),
        size,
        clear_color: wgpu::Color::BLACK,
        last_atlas_generation: text.atlas_generation(),
        last_instance_count: 0,
        last_background_count: 0,
        shaped_rows: vec![],
        instance_cache: InstanceCache::default(),
        panes: HashMap::new(),
        pane_order: vec![],
        pane_frame_cached: false,
        pane_colors: (Rgb::new(100, 200, 255), Rgb::new(50, 60, 70)),
    };
    (state, text, config)
}

fn uniforms() -> FrameUniforms {
    FrameUniforms {
        logo_size: 0,
        content_opacity: 1.0,
        text_opacity: 1.0,
        time: 0.0,
        animate_background: false,
        animate_logo: false,
        window_focused: true,
        bloom_progress: 0.0,
        bloom_peak_multiplier: 1.0,
    }
}

fn pixels(state: &RenderState) -> Vec<u8> {
    state.write_pane_uniforms(uniforms());
    let target = state.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pane_test_target"),
        size: wgpu::Extent3d {
            width: state.size.0,
            height: state.size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let stride = (state.size.0 * 4).div_ceil(256) * 256;
    let readback = state.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(stride) * u64::from(state.size.1),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let view = target.create_view(&Default::default());
    let mut encoder = state.device.create_command_encoder(&Default::default());
    state.draw_panes(&mut encoder, &view, uniforms());
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
    let submission = state.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.map_async(wgpu::MapMode::Read, .., move |result| tx.send(result).unwrap());
    state
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(10)),
        })
        .unwrap();
    rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    let mapped = readback.get_mapped_range(..).unwrap();
    mapped
        .chunks(stride as usize)
        .flat_map(|row| row[..state.size.0 as usize * 4].iter().copied())
        .collect()
}

fn assert_idle(state: &mut RenderState, id: u64, grid: &RenderGrid, text: &TextRenderer) {
    let pane = state.panes.get_mut(&id).unwrap();
    assert!(
        pane.cache
            .update(
                grid,
                &pane.layout.rows,
                (text.atlas_generation(), state.cell_size),
                pane.active,
                |_, _| panic!("idle pane rebuilt geometry"),
            )
            .is_empty()
    );
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn multi_pane_pixels_clip_move_resize_and_preserve_idle_cache() {
    let (mut state, mut text, config) = fixture();
    let mut left = RenderGrid::new(12, 4);
    let mut right = RenderGrid::new(12, 4);
    left.cursor_visible = false;
    right.cursor_visible = false;
    for cell in &mut left.cells {
        cell.bg = Rgb::new(255, 0, 0);
    }
    for cell in &mut right.cells {
        cell.bg = Rgb::new(0, 255, 0);
    }
    left.cells[0].character = 'L';
    right.cells[0].character = 'R';
    let mut left_rect = PaneRect { x: 5, y: 7, width: 47, height: 25 };
    let right_rect = PaneRect { x: 69, y: 13, width: 50, height: 36 };
    fn scene<'a>(
        left_rect: PaneRect,
        right_rect: PaneRect,
        left: &'a RenderGrid,
        right: &'a RenderGrid,
    ) -> [RenderPane<'a>; 2] {
        [
            RenderPane { id: 17, rect: left_rect, grid: left, active: true },
            RenderPane { id: 99, rect: right_rect, grid: right, active: false },
        ]
    }
    assert!(
        state
            .prepare_panes_frame(
                &scene(left_rect, right_rect, &left, &right),
                &mut text,
                &config,
                uniforms()
            )
            .unwrap()
            > 0
    );
    let first = pixels(&state);
    let pixel = |x: usize, y: usize| &first[(y * 160 + x) * 4..(y * 160 + x + 1) * 4];
    assert!(pixel(30, 15)[0] > 245 && pixel(30, 15)[1] < 15, "first pane lost red content");
    assert!(pixel(95, 25)[1] > 245 && pixel(95, 25)[0] < 15, "second pane lost green content");
    for (x, y) in [(4, 15), (53, 15), (30, 33), (68, 25), (120, 25), (95, 50)] {
        assert_eq!(pixel(x, y), [0, 0, 0, 255], "pane leaked outside scissor at {x},{y}");
    }
    let idle_rows = state.panes[&99].layout.rows.clone();
    let idle_geometry =
        bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&99].cache.instances).to_vec();
    assert_eq!(
        state.prepare_panes_frame(
            &scene(left_rect, right_rect, &left, &right),
            &mut text,
            &config,
            uniforms()
        ),
        Some(0)
    );
    assert_idle(&mut state, 99, &right, &text);
    left.cells[1].character = 'X';
    left_rect = PaneRect { x: 13, y: 55, width: 31, height: 27 };
    assert!(
        state
            .prepare_panes_frame(
                &scene(left_rect, right_rect, &left, &right),
                &mut text,
                &config,
                uniforms()
            )
            .unwrap()
            > 0
    );
    assert!(
        idle_rows.iter().zip(&state.panes[&99].layout.rows).all(|(old, new)| Arc::ptr_eq(old, new))
    );
    assert_eq!(
        bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&99].cache.instances),
        idle_geometry
    );
    assert_idle(&mut state, 99, &right, &text);
    let moved = pixels(&state);
    let pixel = |x: usize, y: usize| &moved[(y * 160 + x) * 4..(y * 160 + x + 1) * 4];
    assert_eq!(pixel(30, 15), [0, 0, 0, 255], "old pane location was not cleared");
    assert!(pixel(30, 65)[0] > 245, "moved pane is missing");
    assert_eq!(pixel(45, 65), [0, 0, 0, 255], "resized pane scissor not applied");
    // A zero-area pane remains a valid scene item and never creates a GPU scissor.
    left_rect.width = 0;
    state
        .prepare_panes_frame(
            &scene(left_rect, right_rect, &left, &right),
            &mut text,
            &config,
            uniforms(),
        )
        .unwrap();
    let hidden = pixels(&state);
    assert_eq!(&hidden[(65 * 160 + 30) * 4..(65 * 160 + 31) * 4], [0, 0, 0, 255]);
    // Removing a pane drops its buffers and layout rather than accumulating closed panes.
    state
        .prepare_panes_frame(
            &[RenderPane { id: 99, rect: right_rect, grid: &right, active: true }],
            &mut text,
            &config,
            uniforms(),
        )
        .unwrap();
    assert_eq!(state.panes.len(), 1);
    assert!(!state.panes.contains_key(&17));
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn multi_pane_bidi_hit_mapping_and_global_font_invalidation() {
    let (mut state, mut text, mut config) = fixture();
    let mut rtl = RenderGrid::new(12, 2);
    let mut ltr = RenderGrid::new(12, 2);
    for (cell, ch) in rtl.cells.iter_mut().zip("שלום عربي".chars()) {
        cell.character = ch;
    }
    for (cell, ch) in ltr.cells.iter_mut().zip("hello world".chars()) {
        cell.character = ch;
    }
    let scene = [
        RenderPane {
            id: 1,
            rect: PaneRect { width: 70, height: 90, ..Default::default() },
            grid: &rtl,
            active: true,
        },
        RenderPane {
            id: 2,
            rect: PaneRect { x: 80, width: 70, height: 90, ..Default::default() },
            grid: &ltr,
            active: false,
        },
    ];
    state.prepare_panes_frame(&scene, &mut text, &config, uniforms()).unwrap();
    let mut reversed = false;
    for logical in 0..rtl.cols {
        let visual = state.pane_visual_column(1, logical, 0);
        let (mapped, direction) = state.pane_logical_column(1, visual, 0);
        assert_eq!(mapped, logical, "pane 1 bidi map failed roundtrip");
        reversed |= direction && visual != logical;
        assert_eq!(state.pane_logical_column(2, logical, 0), (logical, false));
    }
    assert!(reversed, "RTL content was not reordered");
    assert_eq!(state.pane_logical_column(123, 4, 0), (4, false));
    let rtl_rect = scene[0].rect;
    let ltr_rect = scene[1].rect;
    let old = state.panes[&1].layout.rows.clone();
    // Preparing mouse/IME layout for one pane leaves the other pane's mapping intact.
    rtl.cells[0].character = 'A';
    state.prepare_pane_layout(1, &rtl, &mut text, &config);
    assert!(!Arc::ptr_eq(&old[0], &state.panes[&1].layout.rows[0]));
    assert_eq!(state.pane_visual_column(2, 4, 0), 4);
    config.size += 4.0;
    text = TextRenderer::new(&state.device, &state.queue, &config, 1.0);
    state.update_atlas_bind_group(&text.atlas_view);
    state.sync_atlas_generation(text.atlas_generation());
    let metrics = text.cell_metrics();
    state.set_cell_size((metrics.cell_width, metrics.cell_height));
    assert!(
        state.panes.values().all(|pane| pane.layout.rows.is_empty() && pane.layout.key.is_none())
    );
    let scene = [
        RenderPane { id: 1, rect: rtl_rect, grid: &rtl, active: true },
        RenderPane { id: 2, rect: ltr_rect, grid: &ltr, active: false },
    ];
    state.prepare_panes_frame(&scene, &mut text, &config, uniforms()).unwrap();
    for id in [1, 2] {
        let pane = &state.panes[&id];
        assert_eq!(pane.layout.rows.len(), 2);
        assert!(!pane.cache.instances.is_empty());
    }
}

#[test]
#[ignore = "offscreen CPU preparation benchmark; coordinate timing and run with --release"]
fn multi_pane_update_benchmark() {
    use std::{io::Write, time::Instant};
    let (mut state, mut text, config) = fixture();
    let path = std::env::var_os("MECHANIC_PANE_BENCH_CSV")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mechanic-pane-update.csv"));
    let mut csv = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(csv, "panes,workload,iteration,prepare_ns,upload_bytes,total_cells").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    // Same total cell count for 1, 2 and 4 panes; change one cell in one/all panes.
    for count in [1, 2, 4] {
        for workload in ["busy_one", "all_active"] {
            let mut grids: Vec<_> = (0..count)
                .map(|index| {
                    let mut grid = RenderGrid::new(120 / count, 40);
                    grid.cursor_visible = false;
                    for (cell_index, cell) in grid.cells.iter_mut().enumerate() {
                        cell.character = char::from(b'A' + ((cell_index + index) % 26) as u8);
                    }
                    grid
                })
                .collect();
            for iteration in 0..120 {
                assert!(Instant::now() < deadline, "pane benchmark exceeded its 60-second budget");
                for (index, grid) in grids.iter_mut().enumerate() {
                    if workload == "all_active" || index == 0 {
                        grid.cells[17].character = char::from(b'A' + (iteration % 26) as u8);
                    }
                }
                let scene: Vec<_> = grids
                    .iter()
                    .enumerate()
                    .map(|(index, grid)| RenderPane {
                        id: index as u64,
                        rect: PaneRect {
                            x: (index * 150 / count) as u32,
                            y: 0,
                            width: (150 / count) as u32,
                            height: 90,
                        },
                        grid,
                        active: index == 0,
                    })
                    .collect();
                let idle_rows: Vec<_> =
                    state.panes.get(&1).map(|pane| pane.layout.rows.clone()).unwrap_or_default();
                let start = Instant::now();
                let upload_bytes =
                    state.prepare_panes_frame(&scene, &mut text, &config, uniforms()).unwrap();
                let prepare_ns = start.elapsed().as_nanos();
                if workload == "busy_one" && iteration > 0 && count > 1 {
                    assert!(
                        idle_rows
                            .iter()
                            .zip(&state.panes[&1].layout.rows)
                            .all(|(a, b)| Arc::ptr_eq(a, b))
                    );
                    for (id, grid) in grids.iter().enumerate().skip(1) {
                        assert_idle(&mut state, id as u64, grid, &text);
                    }
                }
                // Submit the bounded upload batch; exclude synchronization from CPU timings.
                state.queue.submit(std::iter::empty());
                state
                    .device
                    .poll(wgpu::PollType::Wait {
                        submission_index: None,
                        timeout: Some(Duration::from_secs(10)),
                    })
                    .unwrap();
                writeln!(csv, "{count},{workload},{iteration},{prepare_ns},{upload_bytes},4800")
                    .unwrap();
            }
        }
    }
    csv.flush().unwrap();
    eprintln!("pane preparation benchmark: {}", path.display());
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn multi_pane_atlas_growth_and_shape_cache_eviction_keep_idle_pane_valid() {
    let (mut state, mut text, config) = fixture();
    let mut idle = RenderGrid::new(12, 2);
    idle.cursor_visible = false;
    for (cell, ch) in idle.cells.iter_mut().zip("שלום عربي".chars()) {
        cell.character = ch;
    }
    let mut busy = RenderGrid::new(32, 16);
    busy.cursor_visible = false;
    let idle_rect = PaneRect { width: 70, height: 90, ..Default::default() };
    let busy_rect = PaneRect { x: 80, width: 70, height: 90, ..Default::default() };
    fn scene<'a>(
        idle: &'a RenderGrid,
        busy: &'a RenderGrid,
        idle_rect: PaneRect,
        busy_rect: PaneRect,
    ) -> [RenderPane<'a>; 2] {
        [
            RenderPane { id: 1, rect: idle_rect, grid: idle, active: true },
            RenderPane { id: 2, rect: busy_rect, grid: busy, active: false },
        ]
    }
    state
        .prepare_panes_frame(
            &scene(&idle, &busy, idle_rect, busy_rect),
            &mut text,
            &config,
            uniforms(),
        )
        .unwrap();
    let idle_rows = state.panes[&1].layout.rows.clone();
    let before = pixels(&state);
    let generation = text.atlas_generation();
    let input = (0x21..0x180).chain(0x410..0x460).filter_map(char::from_u32).filter(|ch| {
        !ch.is_control()
            && unicode_script::UnicodeScript::script(ch) != unicode_script::Script::Inherited
    });
    for (cell, ch) in busy.cells.iter_mut().zip(input) {
        cell.character = ch;
    }
    state
        .prepare_panes_frame(
            &scene(&idle, &busy, idle_rect, busy_rect),
            &mut text,
            &config,
            uniforms(),
        )
        .unwrap();
    assert!(text.atlas_generation() > generation, "second pane did not exercise atlas growth");
    assert!(idle_rows.iter().zip(&state.panes[&1].layout.rows).all(|(a, b)| Arc::ptr_eq(a, b)));
    for id in [1, 2] {
        let pane = &state.panes[&id];
        // Spaces and default-ignorable glyphs legitimately have no bitmap slot.
        let resident: Vec<_> = pane
            .layout
            .rows
            .iter()
            .flat_map(|row| &row.glyphs)
            .filter_map(|glyph| text.glyph_info(glyph.cache_key))
            .collect();
        assert!(!resident.is_empty());
        assert!(
            resident.iter().all(|info| info.atlas_uv.iter().all(|uv| (0.0..=1.0).contains(uv)))
        );
        if id == 2 {
            assert!(resident.len() > 128);
        }
    }
    let (expected, _) = crate::pipeline::build_instances(
        &idle,
        &state.panes[&1].layout.rows,
        &text,
        state.cell_size,
        true,
    );
    assert_eq!(
        bytemuck::cast_slice::<GpuInstance, u8>(&expected),
        bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&1].cache.instances)
    );
    let after = pixels(&state);
    for y in 0..90usize {
        assert_eq!(
            &before[y * 160 * 4..(y * 160 + 70) * 4],
            &after[y * 160 * 4..(y * 160 + 70) * 4],
            "idle pane pixels changed after neighbor grew atlas"
        );
    }
    // Evict the shared cache entry with unrelated layouts; the pane must retain
    // its own shaping identity and both directions of its hit-test permutation.
    for index in 0..300 {
        let mut noise = RenderGrid::new(12, 1);
        for (cell, ch) in noise.cells.iter_mut().zip(format!("noise {index:04}").chars()) {
            cell.character = ch;
        }
        text.shape_grid(&noise, &config);
    }
    let uncached = text.shape_grid(&idle, &config);
    assert!(
        !Arc::ptr_eq(&uncached[0], &idle_rows[0]),
        "fixture did not evict shared shape cache entry"
    );
    state.prepare_pane_layout(1, &idle, &mut text, &config);
    assert!(
        Arc::ptr_eq(&state.panes[&1].layout.rows[0], &idle_rows[0]),
        "idle pane reshaped after shared cache eviction"
    );
    for logical in 0..idle.cols {
        let visual = state.pane_visual_column(1, logical, 0);
        assert_eq!(state.pane_logical_column(1, visual, 0).0, logical);
    }
    assert_idle(&mut state, 1, &idle, &text);
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn multi_pane_profile_counts_scene_instances_and_uploads() {
    let (mut state, mut text, config) = fixture();
    let mut left = RenderGrid::new(8, 2);
    let mut right = RenderGrid::new(8, 2);
    left.cells[0].character = 'L';
    right.cells[0].character = 'R';
    let scene = [
        RenderPane {
            id: 1,
            rect: PaneRect { width: 70, height: 90, ..Default::default() },
            grid: &left,
            active: true,
        },
        RenderPane {
            id: 2,
            rect: PaneRect { x: 80, width: 70, height: 90, ..Default::default() },
            grid: &right,
            active: false,
        },
    ];
    let generation = text.atlas_generation();
    let mut profile = RenderProfile::default();
    let uploaded = state
        .prepare_panes_frame_profiled(&scene, &mut text, &config, uniforms(), Some(&mut profile))
        .unwrap();
    assert_eq!(profile.upload_bytes, uploaded);
    assert_eq!(
        profile.instance_count,
        state
            .panes
            .values()
            .map(|pane| (pane.instance_count + pane.border_count) as usize)
            .sum::<usize>()
    );
    assert!(profile.instance_count > left.cols * left.rows + right.cols * right.rows);
    assert_eq!(profile.atlas_changed, text.atlas_generation() != generation);
    // A repeated scene still reports its complete instance count while recording
    // no instance uploads; per-frame uniform uploads are added at presentation.
    let mut repeated = RenderProfile::default();
    assert_eq!(
        state.prepare_panes_frame_profiled(
            &scene,
            &mut text,
            &config,
            uniforms(),
            Some(&mut repeated)
        ),
        Some(0)
    );
    assert_eq!(repeated.upload_bytes, 0);
    assert_eq!(repeated.instance_count, profile.instance_count);
    assert!(!repeated.atlas_changed);
    assert_eq!(repeated.surface_ns, 0);
    assert_eq!(repeated.submit_present_ns, 0);
}
