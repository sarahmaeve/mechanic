//! Bounded native-window check of cached presentation before/after real surface replacement.
//! cargo run -p mechanic-renderer --release --example surface_recovery -- OUTPUT.csv
//! This explicitly replaces a healthy surface; it does not simulate device loss.

use std::{
    fs::File,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use mechanic_config::{
    FontConfig,
    theme::{LogoStyle, Rgb},
};
use mechanic_renderer::{
    FrameUniforms, RenderGrid,
    pipeline::{RenderState, init_surface},
    text::TextRenderer,
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

const PAIRS: usize = 12;
const INTERVAL: Duration = Duration::from_millis(33);

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

struct Check {
    output: File,
    window: Option<Arc<Window>>,
    state: Option<RenderState>,
    text: Option<TextRenderer>,
    config: FontConfig,
    grid: RenderGrid,
    deadline: Instant,
    next: Instant,
    warmups: usize,
    pair: usize,
    passed: usize,
    redraw_pending: bool,
}

impl ApplicationHandler for Check {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Mechanic surface recovery check")
                        .with_inner_size(PhysicalSize::new(320, 160)),
                )
                .unwrap(),
        );
        let init = pollster::block_on(init_surface(Arc::clone(&window), (320, 160))).unwrap();
        let text = TextRenderer::new(&init.device, &init.queue, &self.config, 1.0);
        let state = RenderState::new_with_atlas(
            init,
            &text.atlas_view,
            text.atlas_generation(),
            text.cell_metrics(),
            Rgb::new(0, 0, 0),
            LogoStyle::Triangle,
        )
        .unwrap();
        self.window = Some(window);
        self.text = Some(text);
        self.state = Some(state);
        self.deadline = Instant::now() + Duration::from_secs(8);
        self.next = Instant::now() + Duration::from_millis(250);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }
        if !matches!(event, WindowEvent::RedrawRequested) || !self.redraw_pending {
            return;
        }
        self.redraw_pending = false;
        if let (Some(window), Some(state), Some(text)) =
            (&self.window, &mut self.state, &mut self.text)
        {
            window.pre_present_notify();
            if self.warmups < 3 {
                if state.render(&self.grid, text, &self.config, uniforms()) {
                    self.warmups += 1;
                }
            } else {
                let generation = text.atlas_generation();
                let baseline_started = Instant::now();
                let baseline = state.render_animation(uniforms());
                let baseline_ns = baseline_started.elapsed().as_nanos();
                let recovery_started = Instant::now();
                let recreated = state.recreate_surface();
                let recreate_ns = recovery_started.elapsed().as_nanos();
                window.pre_present_notify();
                let render_started = Instant::now();
                let restored = state.render_animation(uniforms());
                let restored_ns = render_started.elapsed().as_nanos();
                let same_atlas = generation == text.atlas_generation();
                writeln!(self.output, "{},{baseline_ns},{recreate_ns},{restored_ns},{baseline},{recreated},{restored},{same_atlas}", self.pair).unwrap();
                if baseline && recreated && restored && same_atlas {
                    self.passed += 1;
                }
                self.pair += 1;
                if self.pair == PAIRS {
                    event_loop.exit();
                }
            }
            self.next = Instant::now() + INTERVAL;
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.deadline {
            event_loop.exit();
            return;
        }
        if !self.redraw_pending
            && now >= self.next
            && let Some(window) = &self.window
        {
            self.redraw_pending = true;
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(if self.redraw_pending {
            self.deadline
        } else {
            self.next.min(self.deadline)
        }));
    }
}

fn main() {
    // Also bound startup failures that prevent winit from dispatching events.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
        eprintln!("native surface recovery check timed out; result is inconclusive");
        std::process::exit(2);
    });
    let path = std::env::args().nth(1).expect("usage: surface_recovery OUTPUT.csv");
    let mut output = File::options().write(true).create_new(true).open(path).unwrap();
    writeln!(output, "pair,baseline_render_ns,recreate_ns,recovered_render_ns,baseline_presented,recreated,recovered_presented,same_atlas_generation").unwrap();
    let now = Instant::now();
    let mut grid = RenderGrid::new(16, 4);
    for (col, character) in "cached recovery".chars().enumerate() {
        grid.cells[col].character = character;
    }
    let mut check = Check {
        output,
        window: None,
        state: None,
        text: None,
        config: FontConfig { family: "Menlo".into(), ..Default::default() },
        grid,
        deadline: now + Duration::from_secs(8),
        next: now,
        warmups: 0,
        pair: 0,
        passed: 0,
        redraw_pending: false,
    };
    EventLoop::new().unwrap().run_app(&mut check).unwrap();
    check.output.flush().unwrap();
    println!(
        "paired native surface replacements: {}/{} successful (attempted {})",
        check.passed, PAIRS, check.pair
    );
    if check.passed != PAIRS {
        std::process::exit(1);
    }
}
