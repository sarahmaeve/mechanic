//! Main application state and winit event-loop integration.
//!
//! [`App`] implements [`winit::application::ApplicationHandler`] and owns one
//! or more terminal windows keyed by [`WindowId`].  Each window has its own
//! PTY, renderer, and input state.  New windows are spawned via Cmd+N; when
//! the last window is closed the event loop exits.

use std::collections::HashMap;
use std::sync::Arc;

use mechanic_config::Config;
use mechanic_core::{GridColumn, GridLine, GridPoint, GridSide, Terminal, TerminalSize};
use mechanic_renderer::{CellMetrics, Renderer};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState};
use winit::window::{Window, WindowAttributes, WindowId};

// ── AppState ──────────────────────────────────────────────────────────────────

/// Per-window state — one instance per open Mechanic window.
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
    /// Instant of the last user interaction (key press, mouse click, etc.).
    last_input_time: std::time::Instant,
    /// Instant when this window was created (used to compute the `time` uniform).
    start_time: std::time::Instant,
    /// Whether the window currently has keyboard focus.  When unfocused the
    /// window fades toward `content_idle_opacity`.
    focused: bool,
}

// ── App ───────────────────────────────────────────────────────────────────────

/// Top-level application struct.
///
/// Owns the shared configuration and a map of currently open windows.
pub struct App {
    /// User configuration (theme, font, shell) shared by all windows.
    config: Config,
    /// All currently open windows, keyed by winit's [`WindowId`].
    windows: HashMap<WindowId, AppState>,
    /// Counter used to offset subsequent windows so they don't stack exactly
    /// on top of the first one.
    window_count: u32,
}

impl App {
    /// Create a new `App` with the given configuration.
    ///
    /// The first window is created in [`Self::resumed`].
    pub fn new(config: Config) -> Self {
        Self { config, windows: HashMap::new(), window_count: 0 }
    }

    // ── Cell-size helpers ─────────────────────────────────────────────────────

    /// Compute [`TerminalSize`] from a physical pixel surface size and real
    /// cell metrics.
    fn terminal_size_from_metrics(width: u32, height: u32, metrics: &CellMetrics) -> TerminalSize {
        let cw = metrics.cell_width.max(1.0);
        let ch = metrics.cell_height.max(1.0);

        let columns = ((width as f32) / cw).floor() as usize;
        let rows = ((height as f32) / ch).floor() as usize;

        TerminalSize {
            columns: columns.max(1),
            rows: rows.max(1),
            cell_width: cw as usize,
            cell_height: ch as usize,
        }
    }

    // ── Window spawning ───────────────────────────────────────────────────────

    /// Spawn a new Mechanic window with its own PTY, terminal, and renderer.
    ///
    /// Returns the new window's [`WindowId`] on success.  Logs and returns
    /// `None` on failure so the caller can decide whether to exit or continue.
    fn spawn_window(&mut self, event_loop: &ActiveEventLoop) -> Option<WindowId> {
        // Offset subsequent windows diagonally so new ones are visible behind
        // the spawning one.  First window (count == 0) uses the default position.
        let offset = self.window_count.saturating_mul(24) as i32;
        let mut attrs = WindowAttributes::default()
            .with_title("Mechanic")
            .with_inner_size(LogicalSize::new(1024u32, 768u32))
            .with_transparent(true);
        if offset > 0 {
            attrs = attrs.with_position(PhysicalPosition::new(offset, offset));
        }

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("failed to create window: {e}");
                return None;
            }
        };

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
                return None;
            }
        };

        let cell_metrics = renderer.cell_metrics();
        let terminal_size =
            Self::terminal_size_from_metrics(size.width, size.height, &cell_metrics);

        let terminal = match Terminal::new(&self.config, terminal_size) {
            Ok(t) => t,
            Err(e) => {
                log::error!("failed to create terminal: {e}");
                return None;
            }
        };

        let clipboard =
            arboard::Clipboard::new().map_err(|e| log::warn!("clipboard unavailable: {e}")).ok();

        window.set_ime_allowed(true);

        let window_id = window.id();
        let now = std::time::Instant::now();
        let state = AppState {
            window: window.clone(),
            terminal,
            renderer,
            cell_metrics,
            mouse_position: (0.0, 0.0),
            mouse_pressed: false,
            modifiers: ModifiersState::empty(),
            clipboard,
            last_input_time: now,
            start_time: now,
            focused: true,
        };

        self.windows.insert(window_id, state);
        self.window_count += 1;
        window.request_redraw();

        log::info!("spawned window {window_id:?} (total: {})", self.windows.len());
        Some(window_id)
    }
}

// ── Grid coordinate helpers ───────────────────────────────────────────────────

/// Convert a physical pixel position `(x, y)` to a terminal grid [`GridPoint`]
/// and the [`GridSide`] within that cell (left or right half).
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

    let frac = (x / cell_width as f64).fract();
    let side = if frac < 0.5 { GridSide::Left } else { GridSide::Right };

    (GridPoint::new(GridLine(row as i32), GridColumn(col)), side)
}

// ── ApplicationHandler ────────────────────────────────────────────────────────

impl ApplicationHandler for App {
    /// Called when the application is first started (and on iOS/Android resume).
    ///
    /// Spawns the initial window the first time it fires.  Later resumes are
    /// no-ops (we don't want to spawn an extra window every time).
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }

        if self.spawn_window(event_loop).is_none() {
            event_loop.exit();
            return;
        }

        // Continuous polling so the fade animation renders smoothly.  With
        // ControlFlow::Wait the event loop can sleep indefinitely when no
        // events arrive, which makes the opacity fade jerky or frozen.
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    }

    /// Handles all windowing events for a single window.
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Intercept Cmd+N before the per-window state lookup so we can mutate
        // `self.windows`.  The key event state read doesn't need a window
        // reference for this shortcut.
        if let WindowEvent::KeyboardInput { event: ref key_event, .. } = event {
            if key_event.state == ElementState::Pressed {
                let modifiers_snapshot = self.windows.get(&id).map(|s| s.modifiers);
                if let (Some(modifiers), Key::Character(c)) =
                    (modifiers_snapshot, &key_event.logical_key)
                {
                    if modifiers.super_key() && c.as_str() == "n" {
                        let _ = self.spawn_window(event_loop);
                        return;
                    }
                }
            }
        }

        let Some(state) = self.windows.get_mut(&id) else {
            return;
        };

        match event {
            // ── Close ─────────────────────────────────────────────────────────
            //
            // Remove this window from the map.  When the last window closes
            // we exit the event loop — Terminal.app and iTerm2 both behave
            // this way on macOS (closing the last window quits the app).
            WindowEvent::CloseRequested => {
                log::info!("window {id:?} close requested");
                self.windows.remove(&id);
                if self.windows.is_empty() {
                    log::info!("all windows closed — exiting");
                    event_loop.exit();
                }
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

            // ── Window focus changes ──────────────────────────────────────────
            //
            // On blur, kickstart the fade to idle by pretending the last input
            // happened `fade_begin_secs` ago.  On focus, reset the timer.
            WindowEvent::Focused(focused) => {
                state.focused = focused;
                if focused {
                    state.last_input_time = std::time::Instant::now();
                } else {
                    let fade_begin = self.config.theme.opacity.fade_begin_secs;
                    state.last_input_time = std::time::Instant::now()
                        .checked_sub(std::time::Duration::from_secs(fade_begin as u64))
                        .unwrap_or_else(std::time::Instant::now);
                }
                state.window.request_redraw();
            }

            // ── Keyboard input ────────────────────────────────────────────────
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                // Handle Cmd+C (copy) and Cmd+V (paste) before the normal
                // key translation, so these shortcuts are not forwarded to the PTY.
                // (Cmd+N was handled above, before the state lookup.)
                if key_event.state == ElementState::Pressed && state.modifiers.super_key() {
                    if let Key::Character(c) = &key_event.logical_key {
                        match c.as_str() {
                            "c" => {
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

                state.last_input_time = std::time::Instant::now();
                if let Some(bytes) = crate::input::translate_key(&key_event, state.modifiers) {
                    if let Err(e) = state.terminal.write_to_pty(&bytes) {
                        log::warn!("PTY write failed: {e}");
                    }
                }
                state.window.request_redraw();
            }

            // ── IME composition ───────────────────────────────────────────────
            WindowEvent::Ime(ime_event) => {
                match ime_event {
                    Ime::Commit(text) => {
                        if let Err(e) = state.terminal.write_to_pty(text.as_bytes()) {
                            log::warn!("PTY IME commit failed: {e}");
                        }
                    }
                    Ime::Preedit(text, cursor) => {
                        let (cx, cy) = {
                            let grid = state.terminal.grid();
                            let cp = grid.cursor.point;
                            (cp.column.0, cp.line.0)
                        };
                        let cw = state.cell_metrics.cell_width;
                        let ch = state.cell_metrics.cell_height;
                        let px = cx as f64 * cw as f64;
                        let py = cy as f64 * ch as f64;
                        state.window.set_ime_cursor_area(
                            LogicalPosition::new(px, py),
                            LogicalSize::new(cw as f64, ch as f64),
                        );
                        let _ = (text, cursor);
                    }
                    Ime::Enabled | Ime::Disabled => {}
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

                state.last_input_time = std::time::Instant::now();
                match btn_state {
                    ElementState::Pressed => {
                        state.mouse_pressed = true;
                        state.terminal.start_selection(point, side);
                    }
                    ElementState::Released => {
                        state.mouse_pressed = false;
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
                state.last_input_time = std::time::Instant::now();
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
                state.terminal.process_input();

                let grid = crate::convert::convert_grid(&state.terminal, &self.config.theme);

                let elapsed_secs = state.last_input_time.elapsed().as_secs_f32();
                let opacity = compute_opacity(elapsed_secs, &self.config.theme.opacity);

                let time = state.start_time.elapsed().as_secs_f32();

                state.renderer.render(&grid, opacity, time);

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
    /// Request a redraw on every open window so the opacity fade continues
    /// to animate even when no input events are arriving.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        for state in self.windows.values() {
            state.window.request_redraw();
        }
    }
}

// ── Opacity computation ───────────────────────────────────────────────────────

/// Compute content opacity based on seconds since last user interaction.
///
/// Returns `content_active_opacity` during active use, smoothly fading
/// to `content_idle_opacity` between `fade_begin_secs` and `fade_end_secs`.
fn compute_opacity(elapsed_secs: f32, config: &mechanic_config::OpacityConfig) -> f32 {
    let begin = config.fade_begin_secs as f32;
    let end = config.fade_end_secs as f32;

    if elapsed_secs <= begin {
        config.content_active_opacity
    } else if elapsed_secs >= end {
        config.content_idle_opacity
    } else {
        let t = (elapsed_secs - begin) / (end - begin);
        let smooth_t = t * t * (3.0 - 2.0 * t);
        config.content_active_opacity
            + (config.content_idle_opacity - config.content_active_opacity) * smooth_t
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mechanic_config::OpacityConfig;

    fn default_opacity() -> OpacityConfig {
        OpacityConfig {
            title_bar_opacity: 0.95,
            content_active_opacity: 0.95,
            content_idle_opacity: 0.80,
            fade_begin_secs: 30,
            fade_end_secs: 60,
        }
    }

    #[test]
    fn opacity_active_during_interaction() {
        let config = default_opacity();
        let opacity = compute_opacity(0.0, &config);
        assert!((opacity - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_still_active_at_fade_begin() {
        let config = default_opacity();
        let opacity = compute_opacity(30.0, &config);
        assert!((opacity - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_idle_after_fade_end() {
        let config = default_opacity();
        let opacity = compute_opacity(60.0, &config);
        assert!((opacity - 0.80).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_idle_well_past_fade_end() {
        let config = default_opacity();
        let opacity = compute_opacity(300.0, &config);
        assert!((opacity - 0.80).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_midpoint_is_between_active_and_idle() {
        let config = default_opacity();
        let opacity = compute_opacity(45.0, &config);
        assert!(opacity < 0.95);
        assert!(opacity > 0.80);
    }

    #[test]
    fn opacity_monotonically_decreases_during_fade() {
        let config = default_opacity();
        let mut prev = compute_opacity(30.0, &config);
        for secs in 31..=60 {
            let current = compute_opacity(secs as f32, &config);
            assert!(current <= prev, "opacity should decrease: {prev} -> {current} at {secs}s");
            prev = current;
        }
    }
}
