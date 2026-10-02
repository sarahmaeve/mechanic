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
        dividers: DividerState::default(),
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

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn multi_pane_dividers_show_hover_both_orientations_and_clear_after_close() {
    let (mut state, mut text, config) = fixture();
    let theme = mechanic_config::theme::Theme::default();
    state.set_divider_colors(theme.foreground, theme.background, theme.cursor);
    let (idle, highlighted) = state.dividers.colors.unwrap();
    let mut left = RenderGrid::new(20, 8);
    let mut right = RenderGrid::new(20, 8);
    left.cursor_visible = false;
    right.cursor_visible = false;
    left.cells[0].character = 'L';
    right.cells[0].character = 'R';
    for (left_rect, right_rect, rect) in [
        (
            PaneRect { x: 0, y: 0, width: 70, height: 96 },
            PaneRect { x: 82, y: 0, width: 78, height: 96 },
            PaneRect { x: 70, y: 0, width: 12, height: 96 },
        ),
        (
            PaneRect { x: 0, y: 0, width: 160, height: 42 },
            PaneRect { x: 0, y: 54, width: 160, height: 42 },
            PaneRect { x: 0, y: 42, width: 160, height: 12 },
        ),
    ] {
        let scene = [
            RenderPane { id: 1, rect: left_rect, grid: &left, active: true },
            RenderPane { id: 2, rect: right_rect, grid: &right, active: false },
        ];
        state.prepare_panes_frame(&scene, &mut text, &config, uniforms()).unwrap();
        let rows = state.panes[&2].layout.rows.clone();
        let geometry =
            bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&2].cache.instances).to_vec();
        let divider = RenderDivider { rect, highlighted: false };
        state.set_pane_dividers(&[divider]);
        let mut profile = RenderProfile::default();
        assert_eq!(
            state.dividers.update(&state.device, &state.queue, Some(&mut profile)),
            mem::size_of::<GpuInstance>()
        );
        assert_eq!(profile.upload_bytes, mem::size_of::<GpuInstance>());
        assert_eq!(profile.instance_count, 1);
        let samples = if rect.width < rect.height {
            [(rect.x, 48), (rect.x + rect.width / 2, 48), (rect.x + rect.width - 1, 48)]
        } else {
            [(75, rect.y), (75, rect.y + rect.height / 2), (75, rect.y + rect.height - 1)]
        };
        let expected = |color: Rgb| [color.r, color.g, color.b, 255];
        let assert_strip = |pixels: &[u8], color: [u8; 4]| {
            for (x, y) in samples {
                let offset = (y as usize * 160 + x as usize) * 4;
                assert_eq!(&pixels[offset..offset + 4], color, "divider pixel missing at {x},{y}");
            }
        };
        assert_strip(&pixels(&state), expected(idle));
        // Identical event state neither rebuilds nor uploads divider geometry.
        state.set_pane_dividers(&[divider]);
        assert!(!state.dividers.dirty);
        assert_eq!(state.dividers.update(&state.device, &state.queue, None), 0);
        assert_eq!(state.prepare_panes_frame(&scene, &mut text, &config, uniforms()), Some(0));
        state.pane_frame_cached = true;
        state.set_pane_dividers(&[RenderDivider { highlighted: true, ..divider }]);
        assert!(state.pane_frame_cached, "hover invalidated cached terminal scene");
        assert_eq!(
            state.dividers.update(&state.device, &state.queue, None),
            mem::size_of::<GpuInstance>()
        );
        assert_strip(&pixels(&state), expected(highlighted));
        assert!(rows.iter().zip(&state.panes[&2].layout.rows).all(|(a, b)| Arc::ptr_eq(a, b)));
        assert_eq!(
            bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&2].cache.instances),
            geometry
        );
        assert_idle(&mut state, 2, &right, &text);
        state.set_pane_dividers(&[]);
        assert_eq!(state.dividers.update(&state.device, &state.queue, None), 0);
        assert!(state.dividers.instances.is_empty());
        assert_strip(&pixels(&state), [0, 0, 0, 255]);
        // Closing the adjacent pane expands the retained pane over the old gap.
        state
            .prepare_panes_frame(
                &[RenderPane {
                    id: 1,
                    rect: PaneRect { width: 160, height: 96, ..Default::default() },
                    grid: &left,
                    active: true,
                }],
                &mut text,
                &config,
                uniforms(),
            )
            .unwrap();
        assert_eq!(state.panes.len(), 1);
        let closed = pixels(&state);
        for (x, y) in samples {
            let offset = (y as usize * 160 + x as usize) * 4;
            assert!(closed[offset + 2] < 30, "closed divider persisted at {x},{y}");
        }
    }
}

#[test]
#[ignore = "requires a Metal device; run explicitly on macOS"]
fn multi_pane_grips_and_drop_preview_preserve_cache_and_clip_to_window() {
    let (mut state, mut text, config) = fixture();
    let theme = mechanic_config::theme::Theme::default();
    state.set_divider_colors(theme.foreground, theme.background, theme.cursor);
    let (idle, active) = state.dividers.colors.unwrap();
    let mut left = RenderGrid::new(20, 8);
    let mut right = RenderGrid::new(20, 8);
    left.cursor_visible = false;
    right.cursor_visible = false;
    left.cells[0].character = 'L';
    right.cells[0].character = 'R';
    let scene = [
        RenderPane {
            id: 1,
            rect: PaneRect { x: 0, y: 36, width: 70, height: 60 },
            grid: &left,
            active: true,
        },
        RenderPane {
            id: 2,
            rect: PaneRect { x: 80, y: 36, width: 80, height: 60 },
            grid: &right,
            active: false,
        },
    ];
    state.prepare_panes_frame(&scene, &mut text, &config, uniforms()).unwrap();
    let rows = state.panes[&2].layout.rows.clone();
    let geometry =
        bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&2].cache.instances).to_vec();
    let handles = [
        RenderPaneHandle {
            rect: PaneRect { width: 70, height: 36, ..Default::default() },
            highlighted: false,
        },
        RenderPaneHandle {
            rect: PaneRect { x: 80, width: 80, height: 36, ..Default::default() },
            highlighted: false,
        },
    ];
    let divider = RenderDivider {
        rect: PaneRect { x: 70, width: 10, height: 96, ..Default::default() },
        highlighted: false,
    };
    state.set_pane_dividers(&[divider]);
    state.set_pane_handles(&handles);
    let mut profile = RenderProfile::default();
    assert_eq!(
        state.dividers.update(&state.device, &state.queue, Some(&mut profile)),
        7 * mem::size_of::<GpuInstance>()
    );
    assert_eq!(profile.instance_count, 7);
    assert_eq!(profile.upload_bytes, 7 * mem::size_of::<GpuInstance>());
    let pixel = |pixels: &[u8], x: usize, y: usize| {
        pixels[(y * 160 + x) * 4..(y * 160 + x + 1) * 4].to_vec()
    };
    let color = |color: Rgb| vec![color.r, color.g, color.b, 255];
    let initial = pixels(&state);
    for y in [12, 17, 22] {
        assert_eq!(pixel(&initial, 30, y), color(idle), "first drag grip bar missing");
        assert_eq!(pixel(&initial, 115, y), color(idle), "second drag grip bar missing");
    }
    assert_eq!(pixel(&initial, 75, 50), color(idle), "grips replaced the existing divider");
    for (x, y) in [(25, 12), (45, 12), (30, 15), (30, 25), (5, 5)] {
        assert_eq!(pixel(&initial, x, y), vec![0, 0, 0, 255], "grip leaked outside its bars");
    }
    state.pane_frame_cached = true;
    let highlighted_handles = [RenderPaneHandle { highlighted: true, ..handles[0] }, handles[1]];
    let preview = PaneRect { x: 8, y: 43, width: 56, height: 39 };
    state.set_pane_handles(&highlighted_handles);
    state.set_pane_drop_preview(Some(preview));
    assert!(state.pane_frame_cached, "drag decorations invalidated cached terminal scene");
    let mut profile = RenderProfile::default();
    assert_eq!(
        state.dividers.update(&state.device, &state.queue, Some(&mut profile)),
        11 * mem::size_of::<GpuInstance>()
    );
    assert_eq!(profile.upload_bytes, 11 * mem::size_of::<GpuInstance>());
    assert_eq!(profile.instance_count, 11);
    let hovered = pixels(&state);
    for y in [12, 17, 22] {
        assert_eq!(pixel(&hovered, 30, y), color(active));
    }
    for (x, y) in [(32, 43), (32, 46), (8, 60), (11, 60), (63, 60), (32, 81)] {
        assert_eq!(pixel(&hovered, x, y), color(active), "drop outline missing at {x},{y}");
    }
    for (x, y) in [(32, 47), (12, 60), (7, 60), (64, 60), (32, 42), (32, 82)] {
        assert!(pixel(&hovered, x, y)[2] < 30, "drop outline filled or exceeded bounds at {x},{y}");
    }
    state.set_pane_handles(&highlighted_handles);
    state.set_pane_drop_preview(Some(preview));
    state.set_pane_dividers(&[divider]);
    assert!(!state.dividers.dirty);
    let mut idle_profile = RenderProfile::default();
    assert_eq!(state.dividers.update(&state.device, &state.queue, Some(&mut idle_profile)), 0);
    assert_eq!(idle_profile.upload_bytes, 0);
    assert_eq!(idle_profile.instance_count, 11);
    assert_eq!(state.prepare_panes_frame(&scene, &mut text, &config, uniforms()), Some(0));
    assert!(rows.iter().zip(&state.panes[&2].layout.rows).all(|(a, b)| Arc::ptr_eq(a, b)));
    assert_eq!(bytemuck::cast_slice::<GpuInstance, u8>(&state.panes[&2].cache.instances), geometry);
    assert_idle(&mut state, 2, &right, &text);
    // Partly off-window decorations use the window scissor rather than invalid
    // rectangles from their own unclamped geometry.
    state.set_pane_handles(&[RenderPaneHandle {
        rect: PaneRect { x: 145, width: 18, height: 36, ..Default::default() },
        highlighted: true,
    }]);
    state.set_pane_drop_preview(Some(PaneRect { x: 150, y: 85, width: 30, height: 30 }));
    state.dividers.update(&state.device, &state.queue, None);
    let clipped = pixels(&state);
    assert_eq!(pixel(&clipped, 159, 12), color(active));
    assert_eq!(pixel(&clipped, 159, 86), color(active));
    assert_eq!(pixel(&clipped, 150, 95), color(active));
    state.set_pane_handles(&[]);
    state.set_pane_drop_preview(None);
    state.set_pane_dividers(&[]);
    assert_eq!(state.dividers.update(&state.device, &state.queue, None), 0);
    assert!(state.dividers.instances.is_empty());
    let cleared = pixels(&state);
    assert_eq!(pixel(&cleared, 30, 12), vec![0, 0, 0, 255]);
    assert_eq!(pixel(&cleared, 159, 12), vec![0, 0, 0, 255]);
    assert_eq!(
        pixel(&cleared, 159, 86),
        pixel(&initial, 159, 86),
        "drop preview did not restore original pane border"
    );
}
