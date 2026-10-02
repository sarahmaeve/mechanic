//! Main application state and winit event-loop integration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mechanic_config::Config;
use mechanic_core::{
    GridColumn, GridLine, GridPoint, GridSide, MouseProtocol, PtyWaker, Terminal, TerminalSize,
};
use mechanic_renderer::{CellMetrics, FrameUniforms, Renderer};

use crate::mouse as mouse_enc;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

/// Target interval between animation frames (~30 FPS).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Events the main event loop receives from other threads.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Wake this window to drain queued PTY output.
    PtyOutput(WindowId),
}

/// State for one terminal window.
struct AppState {
    /// The OS window, shared with the wgpu surface via `Arc`.
    window: Arc<Window>,
    terminal: Terminal,
    renderer: Renderer,
    /// Real cell metrics from the renderer (used for resize calculations).
    cell_metrics: CellMetrics,
    /// Current physical mouse cursor position in pixels.
    mouse_position: (f64, f64),
    mouse_pressed: bool,
    /// Press position in physical pixels, used to distinguish clicks from drags.
    mouse_press_origin: Option<(f64, f64)>,
    /// Last drag selection, pasted by middle-click independently of the clipboard.
    primary_selection: Option<String>,
    /// Current keyboard modifier state (updated via `ModifiersChanged`).
    modifiers: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    /// Instant when this window was created (used to compute the `time` uniform).
    start_time: std::time::Instant,
    /// Keyboard focus, which selects active or idle opacity.
    focused: bool,
    /// Live font size in points for incremental zoom shortcuts.
    current_font_size: f32,
    /// `None` while running; `Some(None)` means exited without a known status.
    exit_status: Option<Option<std::process::ExitStatus>>,
    /// Last reported mouse cell, used to deduplicate motion events.
    last_mouse_report: Option<(u32, u32)>,
    /// Rebuild cell instances when true; otherwise reuse the cached frame.
    content_dirty: bool,
    /// Forced frames after focus changes, to accommodate AppKit redraw coalescing.
    focus_redraw_frames: u8,
    /// Pending focus-gain time; cleared on focus loss or bloom commitment.
    focus_gain_at: Option<Instant>,
    /// Start of the committed bloom; cleared after its configured duration.
    bloom_start: Option<Instant>,
}

/// Brief redraw burst after focus changes so opacity updates reach the compositor.
const FOCUS_REDRAW_BURST_FRAMES: u8 = 5;

pub struct App {
    /// User configuration (theme, font, shell) shared by all windows.
    config: Config,
    /// All currently open windows, keyed by winit's [`WindowId`].
    windows: HashMap<WindowId, AppState>,
    /// Wakes the main loop from PTY reader threads.
    proxy: EventLoopProxy<UserEvent>,
    /// Enables continuous shader animation via `--hot-cpu`.
    hot_cpu: bool,
    /// Allows forwarding mouse events when the terminal program requests them.
    mouse_tracking: bool,
}

impl App {
    pub fn new(
        config: Config,
        proxy: EventLoopProxy<UserEvent>,
        hot_cpu: bool,
        mouse_tracking: bool,
    ) -> Self {
        Self { config, windows: HashMap::new(), proxy, hot_cpu, mouse_tracking }
    }

    fn make_waker(&self, window_id: WindowId) -> PtyWaker {
        make_waker_for(&self.proxy, window_id)
    }

    /// Remove a window and exit the event loop if no windows remain.
    fn close_window(&mut self, id: WindowId, event_loop: &ActiveEventLoop) {
        self.windows.remove(&id);
        if self.windows.is_empty() {
            log::info!("all windows closed — exiting");
            event_loop.exit();
        }
    }

    /// Update font metrics and resize the terminal to match.
    fn apply_font_size(state: &mut AppState, new_size: f32) {
        let new_metrics = state.renderer.set_font_size(new_size);
        state.cell_metrics = new_metrics;
        state.current_font_size = new_size;

        let inner = state.window.inner_size();
        let term_size = Self::terminal_size_from_metrics(inner.width, inner.height, &new_metrics);
        state.terminal.resize(term_size);

        state.content_dirty = true;
        state.window.request_redraw();
    }

    /// Compute [`TerminalSize`] from a physical pixel surface size and real cell metrics.
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

    /// Spawn a new Mechanic window with its own PTY, terminal, and renderer.
    fn spawn_window(&mut self, event_loop: &ActiveEventLoop) -> Option<WindowId> {
        let offset = (self.windows.len() as i32).saturating_mul(24);
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

        let window_id = window.id();
        let waker = self.make_waker(window_id);
        let terminal = match Terminal::new(&self.config, terminal_size, waker) {
            Ok(t) => t,
            Err(e) => {
                log::error!("failed to create terminal: {e}");
                return None;
            }
        };

        let clipboard =
            arboard::Clipboard::new().map_err(|e| log::warn!("clipboard unavailable: {e}")).ok();

        window.set_ime_allowed(true);

        let now = std::time::Instant::now();
        let state = AppState {
            window: window.clone(),
            terminal,
            renderer,
            cell_metrics,
            mouse_position: (0.0, 0.0),
            mouse_pressed: false,
            mouse_press_origin: None,
            primary_selection: None,
            modifiers: ModifiersState::empty(),
            clipboard,
            start_time: now,
            focused: true,
            current_font_size: self.config.font.size,
            exit_status: None,
            last_mouse_report: None,
            content_dirty: true,
            focus_redraw_frames: FOCUS_REDRAW_BURST_FRAMES,
            focus_gain_at: Some(now),
            bloom_start: None,
        };

        self.windows.insert(window_id, state);
        window.request_redraw();

        log::info!("spawned window {window_id:?} (total: {})", self.windows.len());
        Some(window_id)
    }
}

/// Map physical pixels to a grid point and cell half, accounting for scrollback.
fn pixel_to_grid_point(
    x: f64,
    y: f64,
    cell_width: f32,
    cell_height: f32,
    cols: usize,
    rows: usize,
    display_offset: usize,
) -> (GridPoint, GridSide) {
    let col = (x / cell_width as f64) as usize;
    let row = (y / cell_height as f64) as usize;
    let col = col.min(cols.saturating_sub(1));
    let row = row.min(rows.saturating_sub(1));

    let frac = (x / cell_width as f64).fract();
    let side = if frac < 0.5 { GridSide::Left } else { GridSide::Right };

    let grid_line = row as i32 - display_offset as i32;
    (GridPoint::new(GridLine(grid_line), GridColumn(col)), side)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }

        if self.spawn_window(event_loop).is_none() {
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::KeyboardInput { event: ref key_event, .. } = event
            && key_event.state == ElementState::Pressed
        {
            let modifiers_snapshot = self.windows.get(&id).map(|s| s.modifiers);
            if let (Some(modifiers), Key::Character(c)) =
                (modifiers_snapshot, &key_event.logical_key)
                && modifiers.super_key()
                && let Some(shortcut) = cmd_shortcut(c.as_str())
                && shortcut.is_app_level()
            {
                match shortcut {
                    CmdShortcut::SpawnWindow => {
                        let _ = self.spawn_window(event_loop);
                    }
                    CmdShortcut::CloseWindow => {
                        self.close_window(id, event_loop);
                    }
                    other => {
                        debug_assert!(!other.is_app_level());
                    }
                }
                return;
            }
        }

        let Some(state) = self.windows.get_mut(&id) else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => {
                log::info!("window {id:?} close requested");
                self.close_window(id, event_loop);
            }

            WindowEvent::Resized(size) => {
                state.renderer.resize((size.width, size.height));

                let new_term_size =
                    Self::terminal_size_from_metrics(size.width, size.height, &state.cell_metrics);
                state.terminal.resize(new_term_size);

                state.content_dirty = true;
                state.window.request_redraw();
            }

            WindowEvent::ModifiersChanged(mods) => {
                state.modifiers = mods.state();
            }

            WindowEvent::Focused(focused) => {
                log::debug!("window {id:?} focused: {focused}");
                state.focused = focused;
                state.content_dirty = true;
                state.focus_redraw_frames = FOCUS_REDRAW_BURST_FRAMES;

                if focused {
                    state.focus_gain_at = Some(Instant::now());
                    state.bloom_start = None;
                } else {
                    // Cancel pending bloom; let an already committed animation finish.
                    state.focus_gain_at = None;
                }

                state.window.request_redraw();
            }

            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if state.exit_status.is_some() && key_event.state == ElementState::Pressed {
                    let key = &key_event.logical_key;
                    let mods = state.modifiers;

                    if mods.super_key() && matches!(key, Key::Character(c) if c.as_str() == "r") {
                        let waker = make_waker_for(&self.proxy, id);
                        respawn_shell(state, &self.config, id, waker);
                        return;
                    }

                    let allow_fall_through = mods.super_key()
                        && matches!(key, Key::Character(c) if matches!(c.as_str(), "c" | "a"));

                    if !allow_fall_through {
                        if !mods.super_key() && is_dismissal_key(key) {
                            self.close_window(id, event_loop);
                        }
                        return;
                    }
                }

                if key_event.state == ElementState::Pressed
                    && state.modifiers.super_key()
                    && let Key::Character(c) = &key_event.logical_key
                    && let Some(shortcut) = cmd_shortcut(c.as_str())
                {
                    match shortcut {
                        CmdShortcut::SpawnWindow | CmdShortcut::CloseWindow => {
                            debug_assert!(shortcut.is_app_level());
                        }
                        CmdShortcut::Copy => {
                            if let Some(text) = state.terminal.selection_text()
                                && let Some(cb) = state.clipboard.as_mut()
                                && let Err(e) = cb.set_text(text)
                            {
                                log::warn!("clipboard set failed: {e}");
                            }
                            return;
                        }
                        CmdShortcut::Paste => {
                            if let Some(cb) = state.clipboard.as_mut()
                                && let Ok(text) = cb.get_text()
                                && let Err(e) = state.terminal.paste(&text)
                            {
                                log::warn!("PTY paste failed: {e}");
                            }
                            state.content_dirty = true;
                            state.window.request_redraw();
                            return;
                        }
                        CmdShortcut::ClearScrollback => {
                            state.terminal.clear_history();
                            state.content_dirty = true;
                            state.window.request_redraw();
                            return;
                        }
                        CmdShortcut::SelectAll => {
                            state.terminal.select_all();
                            state.content_dirty = true;
                            state.window.request_redraw();
                            return;
                        }
                        CmdShortcut::FontSizeIncrease => {
                            let new_size = (state.current_font_size + 1.0).min(72.0);
                            Self::apply_font_size(state, new_size);
                            return;
                        }
                        CmdShortcut::FontSizeDecrease => {
                            let new_size = (state.current_font_size - 1.0).max(6.0);
                            Self::apply_font_size(state, new_size);
                            return;
                        }
                        CmdShortcut::FontSizeReset => {
                            Self::apply_font_size(state, self.config.font.size);
                            return;
                        }
                        CmdShortcut::ReadlineUndo => {
                            if let Err(e) = state.terminal.write_to_pty(b"\x1f") {
                                log::warn!("PTY undo write failed: {e}");
                            }
                            state.content_dirty = true;
                            state.window.request_redraw();
                            return;
                        }
                    }
                }

                if let Some(bytes) = crate::input::translate_key(
                    &key_event,
                    state.modifiers,
                    state.terminal.cursor_app_mode(),
                ) {
                    // Escape must still reach programs such as vim when a selection exists.
                    if state.terminal.selection_range().is_some() {
                        state.terminal.clear_selection();
                    }
                    if let Err(e) = state.terminal.write_to_pty(&bytes) {
                        log::warn!("PTY write failed: {e}");
                    }
                }
                state.content_dirty = true;
                state.window.request_redraw();
            }

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
                state.content_dirty = true;
                state.window.request_redraw();
            }

            WindowEvent::MouseInput { state: btn_state, button: win_button, .. } => {
                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                if let Some(sgr) = route {
                    if let Some(btn) = winit_to_mouse_button(win_button) {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        let kind = match btn_state {
                            ElementState::Pressed => mouse_enc::MouseEventKind::Press,
                            ElementState::Released => mouse_enc::MouseEventKind::Release,
                        };
                        let bytes = mouse_enc::encode(sgr, btn, state.modifiers, kind, col, row);
                        if let Err(e) = state.terminal.write_to_pty(&bytes) {
                            log::warn!("PTY mouse write failed: {e}");
                        }
                        if matches!(win_button, MouseButton::Left) {
                            state.mouse_pressed = matches!(btn_state, ElementState::Pressed);
                        }
                    }
                    if matches!(btn_state, ElementState::Released) {
                        state.last_mouse_report = None;
                    }
                    state.content_dirty = true;
                    state.window.request_redraw();
                    return;
                }

                match (btn_state, win_button) {
                    (btn_state, MouseButton::Left) => {
                        let (x, y) = state.mouse_position;
                        let cw = state.cell_metrics.cell_width;
                        let ch = state.cell_metrics.cell_height;
                        let cols = state.terminal.columns();
                        let rows = state.terminal.screen_lines();
                        let display_offset = state.terminal.grid().display_offset();
                        let (point, side) =
                            pixel_to_grid_point(x, y, cw, ch, cols, rows, display_offset);

                        match btn_state {
                            ElementState::Pressed => {
                                state.mouse_pressed = true;
                                state.mouse_press_origin = Some((x, y));
                                state.terminal.start_selection(point, side);
                            }
                            ElementState::Released => {
                                state.mouse_pressed = false;
                                const CLICK_DRAG_THRESHOLD_PX: f64 = 5.0;
                                let was_drag = state
                                    .mouse_press_origin
                                    .map(|(ox, oy)| {
                                        let dx = x - ox;
                                        let dy = y - oy;
                                        (dx * dx + dy * dy).sqrt() > CLICK_DRAG_THRESHOLD_PX
                                    })
                                    .unwrap_or(false);
                                state.mouse_press_origin = None;

                                if !was_drag {
                                    state.terminal.clear_selection();

                                    let (cursor_row, cursor_col, scrolled) = {
                                        let grid = state.terminal.grid();
                                        let cp = grid.cursor.point;
                                        (cp.line.0, cp.column.0 as i32, grid.display_offset() != 0)
                                    };
                                    let click_row = point.line.0;
                                    let click_col = point.column.0 as i32;

                                    if !scrolled && click_row == cursor_row {
                                        let delta = click_col - cursor_col;
                                        if delta != 0 {
                                            let seq: &[u8] =
                                                if delta > 0 { b"\x1b[C" } else { b"\x1b[D" };
                                            let mut payload = Vec::with_capacity(
                                                seq.len() * delta.unsigned_abs() as usize,
                                            );
                                            for _ in 0..delta.unsigned_abs() {
                                                payload.extend_from_slice(seq);
                                            }
                                            if let Err(e) = state.terminal.write_to_pty(&payload) {
                                                log::warn!("PTY cursor-move write failed: {e}");
                                            }
                                        }
                                    }
                                } else {
                                    state.primary_selection = state.terminal.selection_text();
                                }
                            }
                        }
                        state.content_dirty = true;
                        state.window.request_redraw();
                    }

                    (ElementState::Pressed, MouseButton::Middle) => {
                        if let Some(text) = state.primary_selection.as_ref() {
                            if let Err(e) = state.terminal.paste(text) {
                                log::warn!("PTY middle-click paste failed: {e}");
                            }
                            state.content_dirty = true;
                            state.window.request_redraw();
                        }
                    }

                    _ => {}
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_position = (position.x, position.y);

                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                if let Some(sgr) = route {
                    let proto = state.terminal.mouse_protocol();
                    let emit = proto.report_motion || (proto.report_drag && state.mouse_pressed);
                    if emit {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        if state.last_mouse_report != Some((col, row)) {
                            state.last_mouse_report = Some((col, row));
                            let btn = mouse_enc::MouseButton::Left;
                            let bytes = mouse_enc::encode(
                                sgr,
                                btn,
                                state.modifiers,
                                mouse_enc::MouseEventKind::Motion,
                                col,
                                row,
                            );
                            if let Err(e) = state.terminal.write_to_pty(&bytes) {
                                log::warn!("PTY mouse motion write failed: {e}");
                            }
                        }
                    }
                    return;
                }

                if state.mouse_pressed {
                    let cw = state.cell_metrics.cell_width;
                    let ch = state.cell_metrics.cell_height;
                    let cols = state.terminal.columns();
                    let rows = state.terminal.screen_lines();
                    let display_offset = state.terminal.grid().display_offset();
                    let (point, side) = pixel_to_grid_point(
                        position.x,
                        position.y,
                        cw,
                        ch,
                        cols,
                        rows,
                        display_offset,
                    );
                    state.terminal.update_selection(point, side);
                    state.content_dirty = true;
                    state.window.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let cell_height = state.cell_metrics.cell_height;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / cell_height as f64) as i32,
                };

                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                if let Some(sgr) = route {
                    if lines != 0 {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        let btn = if lines > 0 {
                            mouse_enc::MouseButton::WheelUp
                        } else {
                            mouse_enc::MouseButton::WheelDown
                        };
                        for _ in 0..lines.unsigned_abs() {
                            let bytes = mouse_enc::encode(
                                sgr,
                                btn,
                                state.modifiers,
                                mouse_enc::MouseEventKind::Press,
                                col,
                                row,
                            );
                            if let Err(e) = state.terminal.write_to_pty(&bytes) {
                                log::warn!("PTY wheel write failed: {e}");
                                break;
                            }
                        }
                    }
                    state.content_dirty = true;
                    state.window.request_redraw();
                    return;
                }

                if lines > 0 {
                    state.terminal.scroll_up(lines as usize);
                } else if lines < 0 {
                    state.terminal.scroll_down((-lines) as usize);
                }
                state.content_dirty = true;
                state.window.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                let outcome = state.terminal.process_input();

                if outcome.grid_maybe_changed {
                    state.content_dirty = true;
                }

                if let Some(error) = outcome.io_error {
                    log::error!("window {id:?} PTY transport failed: {error}");
                    state.terminal.inject_local(b"\r\n\x1b[31m[terminal I/O failed; Cmd+R to restart, any key to close]\x1b[0m\r\n");
                    state.exit_status = Some(None);
                    state.content_dirty = true;
                }

                if let Some(status) = outcome.child_exit
                    && state.exit_status.is_none()
                {
                    let should_close = match self.config.terminal.close_on_exit {
                        mechanic_config::CloseOnExitPolicy::Always => true,
                        mechanic_config::CloseOnExitPolicy::Success => {
                            status.is_none_or(|s| s.success())
                        }
                        mechanic_config::CloseOnExitPolicy::Never => false,
                    };

                    log::info!(
                        "window {id:?} shell exited with {} — {}",
                        format_exit_status(status),
                        if should_close { "closing" } else { "freezing" },
                    );

                    if should_close {
                        self.close_window(id, event_loop);
                        return;
                    }

                    inject_exit_banner(&mut state.terminal, status);
                    state.exit_status = Some(status);
                    state.content_dirty = true;
                    state.window.request_redraw();
                }

                render_frame(state, &self.config, self.hot_cpu);

                state.focus_redraw_frames = state.focus_redraw_frames.saturating_sub(1);
                if outcome.more_output {
                    // Yield to window events before parsing the next batch.
                    state.window.request_redraw();
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut earliest_deadline: Option<Instant> = None;

        let bloom_duration =
            Duration::from_millis(self.config.theme.opacity.bloom_duration_ms as u64);

        for state in self.windows.values() {
            let bloom_active = state
                .bloom_start
                .is_some_and(|t| now.saturating_duration_since(t) < bloom_duration);
            let input = AnimationInputs {
                is_alive: state.exit_status.is_none(),
                focused: state.focused,
                focus_redraw_frames: state.focus_redraw_frames,
                bloom_active,
            };
            let anim = classify_animation(input, self.hot_cpu, now);
            match anim {
                AnimationState::Active { next_frame } => {
                    state.window.request_redraw();
                    merge_deadline(&mut earliest_deadline, next_frame);
                }
                AnimationState::Idle => {}
            }
        }

        event_loop.set_control_flow(match earliest_deadline {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput(id) => {
                if let Some(state) = self.windows.get(&id)
                    && state.exit_status.is_none()
                {
                    state.window.request_redraw();
                }
            }
        }
    }
}

/// Return the SGR flag for PTY forwarding, or `None` for local handling.
fn route_mouse(
    protocol: MouseProtocol,
    mouse_tracking_enabled: bool,
    shift_held: bool,
    window_frozen: bool,
) -> Option<bool> {
    if window_frozen {
        return None;
    }
    if !mouse_tracking_enabled {
        return None;
    }
    if shift_held {
        return None;
    }
    if !protocol.is_tracking() {
        return None;
    }
    Some(protocol.sgr)
}

/// Translate a winit button identifier to the subset we can encode.
fn winit_to_mouse_button(b: MouseButton) -> Option<mouse_enc::MouseButton> {
    match b {
        MouseButton::Left => Some(mouse_enc::MouseButton::Left),
        MouseButton::Middle => Some(mouse_enc::MouseButton::Middle),
        MouseButton::Right => Some(mouse_enc::MouseButton::Right),
        _ => None,
    }
}

/// Map physical pixels to 1-based mouse coordinates, clamped to the visible grid.
fn grid_coords_1based(
    pos: (f64, f64),
    metrics: &CellMetrics,
    cols: usize,
    rows: usize,
) -> (u32, u32) {
    let cw = (metrics.cell_width as f64).max(1.0);
    let ch = (metrics.cell_height as f64).max(1.0);
    let col0 = (pos.0 / cw).max(0.0) as u32;
    let row0 = (pos.1 / ch).max(0.0) as u32;
    let col0 = col0.min(cols.saturating_sub(1) as u32);
    let row0 = row0.min(rows.saturating_sub(1) as u32);
    (col0 + 1, row0 + 1)
}

/// Wake the event loop for output from the specified window.
fn make_waker_for(proxy: &EventLoopProxy<UserEvent>, window_id: WindowId) -> PtyWaker {
    let proxy = proxy.clone();
    Arc::new(move || {
        let _ = proxy.send_event(UserEvent::PtyOutput(window_id));
    })
}

/// Select active or idle content opacity from keyboard focus.
fn opacity_for_focus(focused: bool, config: &mechanic_config::OpacityConfig) -> f32 {
    if focused { config.content_active_opacity } else { config.content_idle_opacity }
}

/// Per-frame multiplier for glyph coverage, as a function of focus.
fn text_opacity_for_focus(focused: bool, config: &mechanic_config::OpacityConfig) -> f32 {
    if focused { 1.0 } else { config.text_idle_opacity }
}

/// Cmd shortcuts intercepted before keyboard input reaches the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmdShortcut {
    /// Cmd+N — spawn a new Mechanic window.
    SpawnWindow,
    /// Cmd+W — close the current window.
    CloseWindow,
    /// Cmd+C — copy the current selection to the system clipboard.
    Copy,
    /// Cmd+V — paste from the system clipboard into the PTY.
    Paste,
    /// Cmd+K — clear the scrollback buffer (iTerm2 convention).
    ClearScrollback,
    /// Cmd+A — select the entire terminal buffer including scrollback.
    SelectAll,
    /// Cmd++ / Cmd+= — step the font size up by 1 point.
    FontSizeIncrease,
    /// Cmd+- — step the font size down by 1 point.
    FontSizeDecrease,
    /// Cmd+0 — reset font size to the configured default.
    FontSizeReset,
    /// Cmd+Z — send readline's undo sequence (`Ctrl+_`, 0x1F).
    ReadlineUndo,
}

impl CmdShortcut {
    /// Dispatch window lifecycle shortcuts before borrowing individual window state.
    fn is_app_level(self) -> bool {
        matches!(self, Self::SpawnWindow | Self::CloseWindow)
    }
}

/// Map a Cmd-modified character to a [`CmdShortcut`].
fn cmd_shortcut(c: &str) -> Option<CmdShortcut> {
    match c {
        "n" => Some(CmdShortcut::SpawnWindow),
        "w" => Some(CmdShortcut::CloseWindow),
        "c" => Some(CmdShortcut::Copy),
        "v" => Some(CmdShortcut::Paste),
        "k" => Some(CmdShortcut::ClearScrollback),
        "a" => Some(CmdShortcut::SelectAll),
        "+" | "=" => Some(CmdShortcut::FontSizeIncrease),
        "-" => Some(CmdShortcut::FontSizeDecrease),
        "0" => Some(CmdShortcut::FontSizeReset),
        "z" => Some(CmdShortcut::ReadlineUndo),
        _ => None,
    }
}

/// Render one frame for `state` using the current focus / grid state.
fn render_frame(state: &mut AppState, config: &Config, hot_cpu: bool) {
    let now = Instant::now();

    let dwell = Duration::from_millis(config.theme.opacity.bloom_dwell_ms as u64);
    let duration = Duration::from_millis(config.theme.opacity.bloom_duration_ms as u64);
    if let Some(start) = maybe_commit_bloom(state.focus_gain_at, state.bloom_start, dwell, now) {
        state.bloom_start = Some(start);
        state.focus_gain_at = None;
    }
    let bloom_progress = compute_bloom_progress(state.bloom_start, duration, now);

    let opacity = opacity_for_focus(state.focused, &config.theme.opacity);
    let text_opacity = text_opacity_for_focus(state.focused, &config.theme.opacity);

    let time = state.start_time.elapsed().as_secs_f32();

    let shader_focused = state.focused && hot_cpu;

    let uniforms = FrameUniforms {
        content_opacity: opacity,
        text_opacity,
        time,
        shader_focused,
        window_focused: state.focused,
        bloom_progress,
        bloom_peak_multiplier: config.theme.opacity.bloom_peak_multiplier,
    };

    let did_animation_render = !state.content_dirty && state.renderer.render_animation(uniforms);

    // A missing cached frame also requires a full render, even when content is clean.
    if !did_animation_render {
        let grid = crate::convert::convert_grid(&state.terminal, &config.theme, state.focused);
        state.renderer.render(&grid, uniforms);
        state.content_dirty = false;
    }

    if let Some(t) = state.bloom_start
        && now.saturating_duration_since(t) >= duration
    {
        state.bloom_start = None;
    }

    let base_title = state.terminal.title();
    let base = if base_title.is_empty() { "Mechanic" } else { base_title };
    let title_string = match state.exit_status {
        Some(status) => format!("{base} — {}", format_title_suffix(status)),
        None => base.to_string(),
    };
    state.window.set_title(&title_string);
}

/// Decide whether the focus-gain bloom should commit this frame.
fn maybe_commit_bloom(
    focus_gain_at: Option<Instant>,
    bloom_start: Option<Instant>,
    dwell: Duration,
    now: Instant,
) -> Option<Instant> {
    if bloom_start.is_some() {
        return None;
    }
    focus_gain_at.and_then(
        |t| {
            if now.saturating_duration_since(t) >= dwell { Some(now) } else { None }
        },
    )
}

/// Linear bloom progress, clamped to `[0, 1]`; zero when no bloom is active.
fn compute_bloom_progress(bloom_start: Option<Instant>, duration: Duration, now: Instant) -> f32 {
    match bloom_start {
        None => 0.0,
        Some(t) => {
            let elapsed = now.saturating_duration_since(t).as_secs_f32();
            let total = duration.as_secs_f32().max(f32::EPSILON);
            (elapsed / total).clamp(0.0, 1.0)
        }
    }
}

/// What a window needs from the event-loop scheduler for the next tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimationState {
    /// Window has active animation.  Redraw now; next frame at `next_frame`.
    Active { next_frame: Instant },
    /// Sleep until input or PTY output arrives.
    Idle,
}

/// Window state used by the animation scheduler.
#[derive(Debug, Clone, Copy)]
struct AnimationInputs {
    is_alive: bool,
    focused: bool,
    /// Forced redraws remaining after a focus change, independent of `hot_cpu`.
    focus_redraw_frames: u8,
    /// Whether the caller considers the bloom within its configured duration.
    bloom_active: bool,
}

/// Frozen windows are idle; focus bursts, bloom, and focused `hot_cpu` need frames.
fn classify_animation(input: AnimationInputs, hot_cpu: bool, now: Instant) -> AnimationState {
    if !input.is_alive {
        return AnimationState::Idle;
    }
    if input.focus_redraw_frames > 0 {
        return AnimationState::Active { next_frame: now + FRAME_INTERVAL };
    }
    if input.bloom_active {
        return AnimationState::Active { next_frame: now + FRAME_INTERVAL };
    }
    if input.focused && hot_cpu {
        return AnimationState::Active { next_frame: now + FRAME_INTERVAL };
    }
    AnimationState::Idle
}

/// Keep the earlier of two deadlines.
fn merge_deadline(current: &mut Option<Instant>, candidate: Instant) {
    *current = Some(match *current {
        Some(existing) => existing.min(candidate),
        None => candidate,
    });
}

/// Which keys close a frozen window.
fn is_dismissal_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Character(_) | Key::Named(NamedKey::Enter | NamedKey::Escape | NamedKey::Space)
    )
}

/// Respawn the shell inside an already-frozen window.
fn respawn_shell(state: &mut AppState, config: &Config, id: WindowId, waker: PtyWaker) {
    let size = state.terminal.size();
    match Terminal::new(config, size, waker) {
        Ok(new_term) => {
            state.terminal = new_term;
            state.exit_status = None;
            state.content_dirty = true;
            state.window.request_redraw();
            log::info!("window {id:?} shell respawned");
        }
        Err(e) => {
            log::error!("window {id:?} respawn failed: {e}");
        }
    }
}

/// Write an amber "shell exited" banner into the terminal grid.
fn inject_exit_banner(terminal: &mut Terminal, status: Option<std::process::ExitStatus>) {
    let msg = format_banner_message(status);
    let bytes = format!("\r\n\x1b[33m{msg}\x1b[0m\r\n");
    terminal.inject_local(bytes.as_bytes());
}

/// Human-readable banner line shown in the grid on freeze.
fn format_banner_message(status: Option<std::process::ExitStatus>) -> String {
    match status {
        None => "[shell exited — press any key to close, Cmd+R to respawn]".to_string(),
        Some(s) => {
            if let Some(code) = s.code() {
                format!(
                    "[shell exited with code {code} — press any key to close, Cmd+R to respawn]"
                )
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    if let Some(sig) = s.signal() {
                        return format!(
                            "[shell killed by signal {sig} — press any key to close, Cmd+R to respawn]"
                        );
                    }
                }
                "[shell exited abnormally — press any key to close, Cmd+R to respawn]".to_string()
            }
        }
    }
}

/// Compact title-bar suffix, e.g. `[exit 137]` or `[signal 9]`.
fn format_title_suffix(status: Option<std::process::ExitStatus>) -> String {
    match status {
        None => "[exited]".to_string(),
        Some(s) => {
            if let Some(code) = s.code() {
                format!("[exit {code}]")
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    if let Some(sig) = s.signal() {
                        return format!("[signal {sig}]");
                    }
                }
                "[exited]".to_string()
            }
        }
    }
}

/// Exit status for logging, including the signal name when available.
fn format_exit_status(status: Option<std::process::ExitStatus>) -> String {
    match status {
        None => "no status".to_string(),
        Some(s) => {
            if let Some(code) = s.code() {
                format!("code {code}")
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    if let Some(sig) = s.signal() {
                        return format!("signal {sig}");
                    }
                }
                "no code or signal".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn status_from_code(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw((code & 0xff) << 8)
    }

    #[cfg(unix)]
    fn status_from_signal(sig: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(sig & 0x7f)
    }

    #[cfg(unix)]
    #[test]
    fn banner_message_for_clean_exit() {
        let msg = format_banner_message(Some(status_from_code(0)));
        assert!(msg.contains("code 0"));
        assert!(msg.contains("press any key"));
        assert!(msg.contains("Cmd+R"));
    }

    #[cfg(unix)]
    #[test]
    fn banner_message_for_nonzero_exit() {
        let msg = format_banner_message(Some(status_from_code(137)));
        assert!(msg.contains("code 137"));
    }

    #[cfg(unix)]
    #[test]
    fn banner_message_for_signal() {
        let msg = format_banner_message(Some(status_from_signal(9)));
        assert!(msg.contains("signal 9"));
    }

    #[test]
    fn banner_message_for_no_status() {
        let msg = format_banner_message(None);
        assert!(msg.contains("shell exited"));
        assert!(!msg.contains("code"));
        assert!(!msg.contains("signal"));
    }

    #[cfg(unix)]
    #[test]
    fn title_suffix_exit_code() {
        assert_eq!(format_title_suffix(Some(status_from_code(0))), "[exit 0]");
        assert_eq!(format_title_suffix(Some(status_from_code(1))), "[exit 1]");
        assert_eq!(format_title_suffix(Some(status_from_code(137))), "[exit 137]");
    }

    #[cfg(unix)]
    #[test]
    fn title_suffix_signal() {
        assert_eq!(format_title_suffix(Some(status_from_signal(15))), "[signal 15]");
    }

    #[test]
    fn title_suffix_no_status() {
        assert_eq!(format_title_suffix(None), "[exited]");
    }

    #[test]
    fn dismissal_printable_char() {
        assert!(is_dismissal_key(&Key::Character(winit::keyboard::SmolStr::new("a"))));
        assert!(is_dismissal_key(&Key::Character(winit::keyboard::SmolStr::new("!"))));
        assert!(is_dismissal_key(&Key::Character(winit::keyboard::SmolStr::new(" "))));
    }

    #[test]
    fn dismissal_named_enter_escape_space() {
        assert!(is_dismissal_key(&Key::Named(NamedKey::Enter)));
        assert!(is_dismissal_key(&Key::Named(NamedKey::Escape)));
        assert!(is_dismissal_key(&Key::Named(NamedKey::Space)));
    }

    #[test]
    fn modifier_alone_does_not_dismiss() {
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Shift)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Control)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Alt)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Super)));
    }

    #[test]
    fn navigation_keys_do_not_dismiss() {
        assert!(!is_dismissal_key(&Key::Named(NamedKey::ArrowUp)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::ArrowDown)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Home)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::End)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::PageUp)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::PageDown)));
    }

    #[test]
    fn function_keys_do_not_dismiss() {
        assert!(!is_dismissal_key(&Key::Named(NamedKey::F1)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::F12)));
    }

    fn inputs(is_alive: bool, focused: bool) -> AnimationInputs {
        AnimationInputs { is_alive, focused, focus_redraw_frames: 0, bloom_active: false }
    }

    fn inputs_with_burst(
        is_alive: bool,
        focused: bool,
        focus_redraw_frames: u8,
    ) -> AnimationInputs {
        AnimationInputs { is_alive, focused, focus_redraw_frames, bloom_active: false }
    }

    fn inputs_with_bloom(is_alive: bool, focused: bool) -> AnimationInputs {
        AnimationInputs { is_alive, focused, focus_redraw_frames: 0, bloom_active: true }
    }

    #[test]
    fn anim_frozen_window_is_idle() {
        assert_eq!(
            classify_animation(inputs(false, true), true, Instant::now()),
            AnimationState::Idle
        );
        assert_eq!(
            classify_animation(inputs(false, false), true, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_focused_quiet_default_is_idle() {
        assert_eq!(
            classify_animation(inputs(true, true), false, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_focused_hot_cpu_is_active() {
        let now = Instant::now();
        match classify_animation(inputs(true, true), true, now) {
            AnimationState::Active { next_frame } => {
                let delta = next_frame.saturating_duration_since(now);
                assert!(delta >= FRAME_INTERVAL);
                assert!(delta <= FRAME_INTERVAL + Duration::from_millis(5));
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn anim_unfocused_is_always_idle() {
        assert_eq!(
            classify_animation(inputs(true, false), false, Instant::now()),
            AnimationState::Idle
        );
        assert_eq!(
            classify_animation(inputs(true, false), true, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_focus_redraw_burst_forces_active_regardless_of_focus() {
        let now = Instant::now();
        match classify_animation(inputs_with_burst(true, false, 5), false, now) {
            AnimationState::Active { next_frame } => {
                let delta = next_frame.saturating_duration_since(now);
                assert!(delta >= FRAME_INTERVAL);
                assert!(delta <= FRAME_INTERVAL + Duration::from_millis(5));
            }
            other => panic!("expected Active during focus burst, got {other:?}"),
        }

        assert!(matches!(
            classify_animation(inputs_with_burst(true, true, 3), false, now),
            AnimationState::Active { .. }
        ));
    }

    #[test]
    fn anim_focus_redraw_burst_drains_to_idle() {
        assert_eq!(
            classify_animation(inputs_with_burst(true, false, 0), false, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_frozen_window_ignores_focus_redraw_burst() {
        assert_eq!(
            classify_animation(inputs_with_burst(false, true, 5), true, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_bloom_active_focused_is_active() {
        let now = Instant::now();
        match classify_animation(inputs_with_bloom(true, true), false, now) {
            AnimationState::Active { next_frame } => {
                let delta = next_frame.saturating_duration_since(now);
                assert!(delta >= FRAME_INTERVAL);
                assert!(delta <= FRAME_INTERVAL + Duration::from_millis(5));
            }
            other => panic!("expected Active during bloom, got {other:?}"),
        }
    }

    #[test]
    fn anim_bloom_runs_to_completion_even_after_focus_loss() {
        assert!(matches!(
            classify_animation(inputs_with_bloom(true, false), false, Instant::now()),
            AnimationState::Active { .. }
        ));
    }

    #[test]
    fn anim_bloom_overrides_hot_cpu_off_default() {
        assert!(matches!(
            classify_animation(inputs_with_bloom(true, true), false, Instant::now()),
            AnimationState::Active { .. }
        ));
    }

    #[test]
    fn anim_frozen_window_ignores_bloom() {
        assert_eq!(
            classify_animation(inputs_with_bloom(false, true), false, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_bloom_and_hot_cpu_compose_as_active() {
        assert!(matches!(
            classify_animation(inputs_with_bloom(true, true), true, Instant::now()),
            AnimationState::Active { .. }
        ));
    }

    #[test]
    fn commit_fires_when_dwell_elapsed() {
        let now = Instant::now();
        let gained = now - Duration::from_millis(200);
        let dwell = Duration::from_millis(120);
        let result = maybe_commit_bloom(Some(gained), None, dwell, now);
        assert_eq!(result, Some(now));
    }

    #[test]
    fn commit_waits_when_dwell_not_elapsed() {
        let now = Instant::now();
        let gained = now - Duration::from_millis(50);
        let dwell = Duration::from_millis(120);
        assert_eq!(maybe_commit_bloom(Some(gained), None, dwell, now), None);
    }

    #[test]
    fn commit_declines_when_bloom_already_in_flight() {
        let now = Instant::now();
        let gained = now - Duration::from_millis(200);
        let already = now - Duration::from_millis(50);
        let dwell = Duration::from_millis(120);
        assert_eq!(maybe_commit_bloom(Some(gained), Some(already), dwell, now), None);
    }

    #[test]
    fn commit_declines_without_focus_gain() {
        let now = Instant::now();
        let dwell = Duration::from_millis(120);
        assert_eq!(maybe_commit_bloom(None, None, dwell, now), None);
    }

    #[test]
    fn commit_fires_exactly_at_dwell_boundary() {
        let now = Instant::now();
        let gained = now - Duration::from_millis(120);
        let dwell = Duration::from_millis(120);
        assert_eq!(maybe_commit_bloom(Some(gained), None, dwell, now), Some(now));
    }

    #[test]
    fn progress_is_zero_when_no_bloom() {
        let now = Instant::now();
        let duration = Duration::from_millis(250);
        assert_eq!(compute_bloom_progress(None, duration, now), 0.0);
    }

    #[test]
    fn progress_is_zero_at_bloom_start() {
        let now = Instant::now();
        let duration = Duration::from_millis(250);
        assert_eq!(compute_bloom_progress(Some(now), duration, now), 0.0);
    }

    #[test]
    fn progress_is_half_at_midpoint() {
        let now = Instant::now();
        let start = now - Duration::from_millis(125);
        let duration = Duration::from_millis(250);
        let p = compute_bloom_progress(Some(start), duration, now);
        assert!((p - 0.5).abs() < 0.01, "midpoint progress should be ≈0.5, got {p}");
    }

    #[test]
    fn progress_clamps_to_one_at_and_past_end() {
        let now = Instant::now();
        let duration = Duration::from_millis(250);
        assert_eq!(compute_bloom_progress(Some(now - duration), duration, now), 1.0);
        assert_eq!(compute_bloom_progress(Some(now - duration * 2), duration, now), 1.0);
    }

    #[test]
    fn progress_is_monotonic_across_duration() {
        let now = Instant::now();
        let duration = Duration::from_millis(250);
        let start = now - Duration::from_millis(200);
        let p_now = compute_bloom_progress(Some(start), duration, now);
        let p_later =
            compute_bloom_progress(Some(start), duration, now + Duration::from_millis(20));
        assert!(p_later >= p_now, "progress must be monotonic: {p_later} < {p_now}");
    }

    fn opacity_cfg(active: f32, idle: f32) -> mechanic_config::OpacityConfig {
        opacity_cfg_full(active, idle, 0.55)
    }

    fn opacity_cfg_full(active: f32, idle: f32, text_idle: f32) -> mechanic_config::OpacityConfig {
        let defaults = mechanic_config::OpacityConfig::default();
        mechanic_config::OpacityConfig {
            title_bar_opacity: 0.95,
            content_active_opacity: active,
            content_idle_opacity: idle,
            text_idle_opacity: text_idle,
            bloom_duration_ms: defaults.bloom_duration_ms,
            bloom_dwell_ms: defaults.bloom_dwell_ms,
            bloom_peak_multiplier: defaults.bloom_peak_multiplier,
        }
    }

    #[test]
    fn opacity_focused_picks_active_value() {
        let cfg = opacity_cfg(0.85, 0.65);
        assert!((opacity_for_focus(true, &cfg) - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_unfocused_picks_idle_value() {
        let cfg = opacity_cfg(0.85, 0.65);
        assert!((opacity_for_focus(false, &cfg) - 0.65).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_follows_config_values() {
        let cfg = opacity_cfg(0.42, 0.13);
        assert!((opacity_for_focus(true, &cfg) - 0.42).abs() < f32::EPSILON);
        assert!((opacity_for_focus(false, &cfg) - 0.13).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_snap_is_discontinuous_at_focus_edge() {
        let cfg = opacity_cfg(0.9, 0.5);
        let focused = opacity_for_focus(true, &cfg);
        let blurred = opacity_for_focus(false, &cfg);
        assert_eq!(focused, 0.9);
        assert_eq!(blurred, 0.5);
        assert!((focused - blurred - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn text_opacity_focused_is_full_strength() {
        let cfg = opacity_cfg_full(0.85, 0.65, 0.55);
        assert_eq!(text_opacity_for_focus(true, &cfg), 1.0);
    }

    #[test]
    fn text_opacity_unfocused_uses_config_value() {
        let cfg = opacity_cfg_full(0.85, 0.65, 0.55);
        assert!((text_opacity_for_focus(false, &cfg) - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn text_opacity_ignores_window_alpha_values() {
        let cfg = opacity_cfg_full(0.5, 0.9, 0.3);
        assert_eq!(text_opacity_for_focus(true, &cfg), 1.0);
        assert!((text_opacity_for_focus(false, &cfg) - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn text_opacity_edge_values_pass_through() {
        let cfg_off = opacity_cfg_full(0.85, 0.65, 1.0);
        assert_eq!(text_opacity_for_focus(false, &cfg_off), 1.0);

        let cfg_invisible = opacity_cfg_full(0.85, 0.65, 0.0);
        assert_eq!(text_opacity_for_focus(false, &cfg_invisible), 0.0);
    }

    #[test]
    fn cmd_shortcut_known_keys_map_to_actions() {
        assert_eq!(cmd_shortcut("n"), Some(CmdShortcut::SpawnWindow));
        assert_eq!(cmd_shortcut("w"), Some(CmdShortcut::CloseWindow));
        assert_eq!(cmd_shortcut("c"), Some(CmdShortcut::Copy));
        assert_eq!(cmd_shortcut("v"), Some(CmdShortcut::Paste));
        assert_eq!(cmd_shortcut("k"), Some(CmdShortcut::ClearScrollback));
        assert_eq!(cmd_shortcut("a"), Some(CmdShortcut::SelectAll));
        assert_eq!(cmd_shortcut("+"), Some(CmdShortcut::FontSizeIncrease));
        assert_eq!(cmd_shortcut("="), Some(CmdShortcut::FontSizeIncrease));
        assert_eq!(cmd_shortcut("-"), Some(CmdShortcut::FontSizeDecrease));
        assert_eq!(cmd_shortcut("0"), Some(CmdShortcut::FontSizeReset));
        assert_eq!(cmd_shortcut("z"), Some(CmdShortcut::ReadlineUndo));
    }

    #[test]
    fn cmd_shortcut_backtick_is_unclaimed() {
        assert_eq!(cmd_shortcut("`"), None);
    }

    #[test]
    fn cmd_shortcut_unknown_characters_are_none() {
        for unclaimed in [
            "b", "d", "e", "f", "g", "h", "i", "j", "l", "m", "o", "p", "q", "r", "s", "t", "u",
            "x", "y", "1", "2", "9", "!", "@", "#", "~", ".", "/", "",
        ] {
            assert_eq!(cmd_shortcut(unclaimed), None, "{unclaimed:?} should be unclaimed");
        }
    }

    #[test]
    fn cmd_shortcut_is_case_sensitive_lowercase_only() {
        assert_eq!(cmd_shortcut("C"), None);
        assert_eq!(cmd_shortcut("V"), None);
        assert_eq!(cmd_shortcut("N"), None);
    }

    #[test]
    fn cmd_shortcut_multi_character_strings_are_none() {
        assert_eq!(cmd_shortcut("nn"), None);
        assert_eq!(cmd_shortcut(" c"), None);
        assert_eq!(cmd_shortcut("c "), None);
    }

    #[test]
    fn cmd_shortcut_is_app_level_only_for_window_lifecycle() {
        assert!(CmdShortcut::SpawnWindow.is_app_level());
        assert!(CmdShortcut::CloseWindow.is_app_level());

        for window_level in [
            CmdShortcut::Copy,
            CmdShortcut::Paste,
            CmdShortcut::ClearScrollback,
            CmdShortcut::SelectAll,
            CmdShortcut::FontSizeIncrease,
            CmdShortcut::FontSizeDecrease,
            CmdShortcut::FontSizeReset,
            CmdShortcut::ReadlineUndo,
        ] {
            assert!(!window_level.is_app_level(), "{window_level:?} must not be app-level");
        }
    }

    fn tracking_proto(sgr: bool) -> MouseProtocol {
        MouseProtocol { report_click: true, report_drag: true, report_motion: false, sgr }
    }

    #[test]
    fn route_mouse_forwards_when_program_tracks() {
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, true, false, false), Some(true));
    }

    #[test]
    fn route_mouse_sgr_flag_passes_through() {
        let proto = tracking_proto(false);
        assert_eq!(route_mouse(proto, true, false, false), Some(false));
    }

    #[test]
    fn route_mouse_no_tracking_returns_none() {
        let proto = MouseProtocol::default();
        assert_eq!(route_mouse(proto, true, false, false), None);
    }

    #[test]
    fn route_mouse_frozen_window_returns_none() {
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, true, false, true), None);
    }

    #[test]
    fn route_mouse_cli_flag_off_returns_none() {
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, false, false, false), None);
    }

    #[test]
    fn route_mouse_shift_override_returns_none() {
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, true, true, false), None);
    }

    #[test]
    fn route_mouse_precedence_frozen_beats_cli_flag() {
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, false, false, true), None);
    }

    fn metrics(cw: f32, ch: f32) -> CellMetrics {
        CellMetrics { cell_width: cw, cell_height: ch, ascent: ch * 0.8 }
    }

    #[test]
    fn grid_coords_origin_maps_to_one_one() {
        assert_eq!(grid_coords_1based((0.0, 0.0), &metrics(8.0, 16.0), 80, 24), (1, 1));
    }

    #[test]
    fn grid_coords_typical_click() {
        assert_eq!(grid_coords_1based((24.0, 48.0), &metrics(8.0, 16.0), 80, 24), (4, 4));
    }

    #[test]
    fn grid_coords_clamps_right_edge() {
        let (col, _row) = grid_coords_1based((9999.0, 0.0), &metrics(8.0, 16.0), 80, 24);
        assert_eq!(col, 80); // 79 (0-based last col) + 1
    }

    #[test]
    fn grid_coords_clamps_bottom_edge() {
        let (_col, row) = grid_coords_1based((0.0, 9999.0), &metrics(8.0, 16.0), 80, 24);
        assert_eq!(row, 24); // 23 + 1
    }

    #[test]
    fn grid_coords_negative_clamps_to_one() {
        assert_eq!(grid_coords_1based((-10.0, -10.0), &metrics(8.0, 16.0), 80, 24), (1, 1));
    }

    #[test]
    fn grid_coords_tolerates_tiny_cells() {
        let _ = grid_coords_1based((0.0, 0.0), &metrics(0.0, 0.0), 10, 10);
    }

    #[test]
    fn merge_deadline_picks_earliest() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t0 + Duration::from_secs(2);

        let mut acc: Option<Instant> = None;
        merge_deadline(&mut acc, t2);
        assert_eq!(acc, Some(t2));

        merge_deadline(&mut acc, t1);
        assert_eq!(acc, Some(t1), "should prefer earlier");

        merge_deadline(&mut acc, t2);
        assert_eq!(acc, Some(t1), "later candidate shouldn't overwrite");
    }

    #[test]
    fn tab_does_not_dismiss() {
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Tab)));
    }

    #[cfg(unix)]
    #[test]
    fn telemetry_exit_status_shapes() {
        assert_eq!(format_exit_status(Some(status_from_code(0))), "code 0");
        assert_eq!(format_exit_status(Some(status_from_signal(9))), "signal 9");
        assert_eq!(format_exit_status(None), "no status");
    }
}
