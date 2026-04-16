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
use mechanic_core::{GridColumn, GridLine, GridPoint, GridSide, Terminal, TerminalSize};
use mechanic_renderer::{CellMetrics, Renderer};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState};
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
    /// Real cell metrics from the renderer (used for resize calculations).
    cell_metrics: CellMetrics,
    /// Current physical mouse cursor position in pixels.
    mouse_position: (f64, f64),
    /// Whether the left mouse button is currently held down.
    mouse_pressed: bool,
    /// Current keyboard modifier state (updated via `ModifiersChanged`).
    modifiers: ModifiersState,
    /// Clipboard handle for copy/paste operations.
    clipboard: Option<arboard::Clipboard>,
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

    /// Compute [`TerminalSize`] from a physical pixel surface size and real
    /// cell metrics.
    fn terminal_size_from_metrics(width: u32, height: u32, metrics: &CellMetrics) -> TerminalSize {
        let cw = metrics.cell_width.max(1.0);
        let ch = metrics.cell_height.max(1.0);

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

// ── Grid coordinate helpers ───────────────────────────────────────────────────

/// Convert a physical pixel position `(x, y)` to a terminal grid [`GridPoint`]
/// and the [`GridSide`] within that cell (left or right half).
///
/// The side is used by alacritty's selection logic to determine whether the
/// click is closer to the start or end of the cell.
fn pixel_to_grid_point(
    x: f64,
    y: f64,
    cell_width: f32,
    cell_height: f32,
    cols: usize,
    rows: usize,
) -> (GridPoint, GridSide) {
    let col = (x / cell_width as f64) as usize;
    let row = (y / cell_height as f64) as usize;
    let col = col.min(cols.saturating_sub(1));
    let row = row.min(rows.saturating_sub(1));

    // Side is Left if cursor is in the left half of the cell, Right otherwise.
    let frac = (x / cell_width as f64).fract();
    let side = if frac < 0.5 { GridSide::Left } else { GridSide::Right };

    (GridPoint::new(GridLine(row as i32), GridColumn(col)), side)
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

        // ── Renderer ─────────────────────────────────────────────────────────
        // `Renderer::new` is async (wgpu adapter/device requests).
        // Use `pollster::block_on` to drive the future to completion on the
        // current thread without spawning a runtime.
        let size = window.inner_size();
        let scale_factor = window.scale_factor() as f32;

        let renderer = match pollster::block_on(Renderer::new(
            window.clone(),
            (size.width, size.height),
            scale_factor,
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

        // Query real cell metrics from the renderer so terminal sizing and
        // resize calculations use the actual font dimensions.
        let cell_metrics = renderer.cell_metrics();

        // ── Terminal ──────────────────────────────────────────────────────────
        let terminal_size =
            Self::terminal_size_from_metrics(size.width, size.height, &cell_metrics);

        let terminal = match Terminal::new(&self.config, terminal_size) {
            Ok(t) => t,
            Err(e) => {
                log::error!("failed to create terminal: {e}");
                event_loop.exit();
                return;
            }
        };

        // ── Clipboard ─────────────────────────────────────────────────────────
        let clipboard =
            arboard::Clipboard::new().map_err(|e| log::warn!("clipboard unavailable: {e}")).ok();

        // ── Store state and request first frame ───────────────────────────────
        self.state = Some(AppState {
            window: window.clone(),
            terminal,
            renderer,
            cell_metrics,
            mouse_position: (0.0, 0.0),
            mouse_pressed: false,
            modifiers: ModifiersState::empty(),
            clipboard,
        });
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

                let new_term_size =
                    Self::terminal_size_from_metrics(size.width, size.height, &state.cell_metrics);
                state.terminal.resize(new_term_size);

                state.window.request_redraw();
            }

            // ── Modifier keys ─────────────────────────────────────────────────
            WindowEvent::ModifiersChanged(mods) => {
                state.modifiers = mods.state();
            }

            // ── Keyboard input ────────────────────────────────────────────────
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                // Handle Cmd+C (copy) and Cmd+V (paste) before the normal
                // key translation, so these shortcuts are not forwarded to the PTY.
                if key_event.state == ElementState::Pressed && state.modifiers.super_key() {
                    if let Key::Character(c) = &key_event.logical_key {
                        match c.as_str() {
                            "c" => {
                                // Copy selected text to the macOS clipboard.
                                if let Some(text) = state.terminal.selection_text() {
                                    if let Some(cb) = state.clipboard.as_mut() {
                                        if let Err(e) = cb.set_text(text) {
                                            log::warn!("clipboard set failed: {e}");
                                        }
                                    }
                                }
                                state.window.request_redraw();
                                return;
                            }
                            "v" => {
                                // Paste from the macOS clipboard into the PTY.
                                let text =
                                    state.clipboard.as_mut().and_then(|cb| cb.get_text().ok());
                                if let Some(text) = text {
                                    if let Err(e) = state.terminal.write_to_pty(text.as_bytes()) {
                                        log::warn!("PTY paste failed: {e}");
                                    }
                                }
                                state.window.request_redraw();
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                if let Some(bytes) = crate::input::translate_key(&key_event) {
                    if let Err(e) = state.terminal.write_to_pty(&bytes) {
                        log::warn!("PTY write failed: {e}");
                    }
                }
                state.window.request_redraw();
            }

            // ── Mouse button press/release ────────────────────────────────────
            WindowEvent::MouseInput { state: btn_state, button: MouseButton::Left, .. } => {
                let (x, y) = state.mouse_position;
                let cw = state.cell_metrics.cell_width;
                let ch = state.cell_metrics.cell_height;
                let cols = state.terminal.columns();
                let rows = state.terminal.screen_lines();
                let (point, side) = pixel_to_grid_point(x, y, cw, ch, cols, rows);

                match btn_state {
                    ElementState::Pressed => {
                        state.mouse_pressed = true;
                        state.terminal.start_selection(point, side);
                    }
                    ElementState::Released => {
                        state.mouse_pressed = false;
                        // Selection stays until cleared by a new click or keyboard input.
                    }
                }
                state.window.request_redraw();
            }

            // ── Cursor movement ───────────────────────────────────────────────
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_position = (position.x, position.y);

                if state.mouse_pressed {
                    let cw = state.cell_metrics.cell_width;
                    let ch = state.cell_metrics.cell_height;
                    let cols = state.terminal.columns();
                    let rows = state.terminal.screen_lines();
                    let (point, side) =
                        pixel_to_grid_point(position.x, position.y, cw, ch, cols, rows);
                    state.terminal.update_selection(point, side);
                    state.window.request_redraw();
                }
            }

            // ── Mouse wheel / scroll ──────────────────────────────────────────
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_height = state.cell_metrics.cell_height;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / cell_height as f64) as i32,
                };
                if lines > 0 {
                    state.terminal.scroll_up(lines as usize);
                } else if lines < 0 {
                    state.terminal.scroll_down((-lines) as usize);
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

                // 4. Update the window title if the terminal has set one.
                let title = state.terminal.title();
                if !title.is_empty() {
                    state.window.set_title(title);
                } else {
                    state.window.set_title("Mechanic");
                }
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
