//! Main application state and winit event-loop integration.
//!
//! [`App`] implements [`winit::application::ApplicationHandler`] and drives
//! the entire lifecycle of a Mechanic terminal window:
//!
//! - Window creation (on `resumed`)
//! - Input forwarding to the PTY (on `KeyboardInput`)
//! - Terminal grid conversion and GPU rendering (on `RedrawRequested`)
//! - Clean shutdown (on `CloseRequested`)

use std::sync::Arc;

use mechanic_config::Config;
use mechanic_core::{Terminal, TerminalSize};
use mechanic_renderer::Renderer;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes, WindowId};

// ── AppState ──────────────────────────────────────────────────────────────────

/// Per-window state created once the OS has given us a surface.
///
/// This is `None` until [`App::resumed`] fires for the first time.
struct AppState {
    /// The OS window, shared with the wgpu surface via `Arc`.
    window: Arc<Window>,
    /// Running terminal session (PTY + alacritty_terminal grid).
    terminal: Terminal,
    /// GPU renderer (wgpu pipeline + cosmic-text).
    renderer: Renderer,
}

// ── App ───────────────────────────────────────────────────────────────────────

/// Top-level application struct.
///
/// Constructed before the event loop starts and passed to
/// [`winit::event_loop::EventLoop::run_app`].
pub struct App {
    /// User configuration (theme, font, shell).
    config: Config,
    /// Present only after the first `resumed` event.
    state: Option<AppState>,
}

impl App {
    /// Create a new `App` with the given configuration.
    ///
    /// Window and terminal initialisation happen lazily in [`Self::resumed`].
    pub fn new(config: Config) -> Self {
        Self { config, state: None }
    }

    // ── Cell-size helpers ─────────────────────────────────────────────────────

    /// Estimated pixel width of one character cell.
    ///
    /// Matches the approximation used by the renderer:
    /// `cell_width = font_size * 0.6`.
    fn cell_width(&self) -> f32 {
        self.config.font.size * 0.6
    }

    /// Estimated pixel height of one character cell.
    ///
    /// Matches the approximation used by the renderer:
    /// `cell_height = font_size * 1.3`.
    fn cell_height(&self) -> f32 {
        self.config.font.size * 1.3
    }

    /// Compute [`TerminalSize`] from a physical pixel surface size.
    fn terminal_size_from_pixels(&self, width: u32, height: u32) -> TerminalSize {
        let cw = self.cell_width().max(1.0);
        let ch = self.cell_height().max(1.0);

        let columns = ((width as f32) / cw).floor() as usize;
        let rows = ((height as f32) / ch).floor() as usize;

        // Guard against zero sizes which the terminal rejects.
        TerminalSize {
            columns: columns.max(1),
            rows: rows.max(1),
            cell_width: cw as usize,
            cell_height: ch as usize,
        }
    }
}

// ── ApplicationHandler ────────────────────────────────────────────────────────

impl ApplicationHandler for App {
    /// Called when the application is first started (and on iOS/Android resume).
    ///
    /// Creates the OS window, spawns the terminal PTY, and initialises the GPU
    /// renderer.  Skips initialisation if already done (safe re-entry).
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            // Already initialised — nothing to do on a spurious resume.
            return;
        }

        // ── Create window ─────────────────────────────────────────────────────
        let attrs = WindowAttributes::default()
            .with_title("Mechanic")
            .with_inner_size(LogicalSize::new(1024u32, 768u32));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        // ── Terminal ──────────────────────────────────────────────────────────
        let size = window.inner_size();
        let terminal_size = self.terminal_size_from_pixels(size.width, size.height);

        let terminal = match Terminal::new(&self.config, terminal_size) {
            Ok(t) => t,
            Err(e) => {
                log::error!("failed to create terminal: {e}");
                event_loop.exit();
                return;
            }
        };

        // ── Renderer ─────────────────────────────────────────────────────────
        // `Renderer::new` is async (wgpu adapter/device requests).
        // Use `pollster::block_on` to drive the future to completion on the
        // current thread without spawning a runtime.
        let renderer = match pollster::block_on(Renderer::new(
            window.clone(),
            (size.width, size.height),
            &self.config.theme,
            self.config.font.clone(),
        )) {
            Ok(r) => r,
            Err(e) => {
                log::error!("failed to create renderer: {e}");
                event_loop.exit();
                return;
            }
        };

        // ── Store state and request first frame ───────────────────────────────
        self.state = Some(AppState { window: window.clone(), terminal, renderer });
        window.request_redraw();
    }

    /// Handles all windowing events for a single window.
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            // ── Close ─────────────────────────────────────────────────────────
            WindowEvent::CloseRequested => {
                log::info!("window close requested — exiting");
                event_loop.exit();
            }

            // ── Resize ────────────────────────────────────────────────────────
            WindowEvent::Resized(size) => {
                state.renderer.resize((size.width, size.height));

                // Compute cell dimensions from config without borrowing `self`
                // mutably a second time while `state` is already borrowed.
                let font_size = self.config.font.size;
                let cw = (font_size * 0.6).max(1.0);
                let ch = (font_size * 1.3).max(1.0);
                let new_term_size = TerminalSize {
                    columns: ((size.width as f32 / cw).floor() as usize).max(1),
                    rows: ((size.height as f32 / ch).floor() as usize).max(1),
                    cell_width: cw as usize,
                    cell_height: ch as usize,
                };
                state.terminal.resize(new_term_size);

                state.window.request_redraw();
            }

            // ── Keyboard input ────────────────────────────────────────────────
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if let Some(bytes) = crate::input::translate_key(&key_event) {
                    if let Err(e) = state.terminal.write_to_pty(&bytes) {
                        log::warn!("PTY write failed: {e}");
                    }
                }
                state.window.request_redraw();
            }

            // ── Redraw ────────────────────────────────────────────────────────
            WindowEvent::RedrawRequested => {
                // 1. Drain PTY output and update the terminal grid.
                state.terminal.process_input();

                // 2. Convert the grid to a renderer-friendly snapshot.
                let grid = crate::convert::convert_grid(&state.terminal, &self.config.theme);

                // 3. Submit the frame to the GPU.
                state.renderer.render(&grid);
            }

            _ => {}
        }
    }

    /// Called just before the event loop sleeps.
    ///
    /// For Phase 1 we request continuous redraws so the terminal output is
    /// always reflected without additional wakeup logic.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }
}
