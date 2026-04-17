//! Main application state and winit event-loop integration.
//!
//! [`App`] implements [`winit::application::ApplicationHandler`] and owns one
//! or more terminal windows keyed by [`WindowId`].  Each window has its own
//! PTY, renderer, and input state.  New windows are spawned via Cmd+N; when
//! the last window is closed the event loop exits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mechanic_config::Config;
use mechanic_core::{
    GridColumn, GridLine, GridPoint, GridSide, MouseProtocol, PtyWaker, Terminal, TerminalSize,
};
use mechanic_renderer::{CellMetrics, Renderer};

use crate::mouse as mouse_enc;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

// ── Frame timing ──────────────────────────────────────────────────────────────

/// Target interval between animation frames (~30 FPS).
///
/// 30 FPS is visually indistinguishable from 60 FPS for the animations
/// we run (opacity fade over 30 s, corner gradient oscillation with a
/// 3-s period, electron pulses with 2–3 s periods — all far too slow
/// for the extra frames to matter) while halving the CPU/GPU cost.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

// ── User events ───────────────────────────────────────────────────────────────

/// Events the main event loop receives from other threads.
///
/// `winit::EventLoopProxy::send_event` is the only cross-thread wake
/// mechanism winit provides.  We use it so the main loop can sleep on
/// `ControlFlow::Wait` at idle and still get woken promptly by PTY
/// output from the reader threads.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// The reader thread for `WindowId`'s PTY sent bytes to the
    /// channel — please redraw so `process_input` drains them and the
    /// glyphs appear this frame rather than the next polling tick.
    PtyOutput(WindowId),
}

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
    /// Where the left mouse button was pressed (physical pixels).  Used to
    /// distinguish a click (no drag) from a click-and-drag when the button
    /// is released, so a pure click can move the shell cursor instead of
    /// leaving a single-character selection.
    mouse_press_origin: Option<(f64, f64)>,
    /// X11-style "primary" selection — populated automatically whenever
    /// the user finishes a drag-select, separate from the macOS clipboard.
    /// Pasted by middle-click without needing an explicit Cmd+C step.
    primary_selection: Option<String>,
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
    /// Current font size in points, tracked so Cmd++/Cmd+-/Cmd+0 can step
    /// relative to the live value (not the config default).
    current_font_size: f32,
    /// Populated once the child shell has exited and the window is in
    /// "frozen" state — no further PTY I/O, awaiting user dismissal
    /// (any printable key, Enter, Esc, Space, or Cmd+W) or respawn
    /// (Cmd+R).  `None` while the shell is alive.  The inner `Option`
    /// mirrors [`ProcessOutcome::child_exit`] — `Some(None)` means the
    /// library-internal `Event::Exit` fired (no status available).
    exit_status: Option<Option<std::process::ExitStatus>>,
    /// Last `(col, row)` we forwarded via a mouse-motion escape to
    /// the PTY.  Used to deduplicate — winit delivers CursorMoved at
    /// roughly display-refresh rate while dragging, but the program
    /// running inside only cares about cell-level granularity.  Reset
    /// to `None` when no forwarded drag is in progress.
    last_mouse_report: Option<(u32, u32)>,
    /// `true` when the renderable grid state (cells, cursor, selection,
    /// display offset, terminal size) has changed since the last full
    /// render.  Gates the [`Renderer::render`] vs.
    /// [`Renderer::render_animation`] choice in the redraw handler:
    /// dirty frames rebuild the instance buffer; clean frames re-issue
    /// the cached draw with only the globals uniform refreshed.
    ///
    /// Conservatively set to `true` by every event that alters grid
    /// content (PTY output, resize, scroll, selection change, font
    /// size, respawn, exit banner).  Cleared back to `false` after a
    /// full render completes.  Initialised to `true` so the first
    /// frame of a new window is always a full render.
    content_dirty: bool,
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
    /// Event-loop proxy cloned into PTY reader threads so they can
    /// wake the main loop via `UserEvent::PtyOutput` when new bytes
    /// arrive.  Cheap to clone (internally an `Arc`).
    proxy: EventLoopProxy<UserEvent>,
    /// Master switch for the shader-side animations: the corner
    /// gradient's brightness breath and color pulse, and the electron
    /// traces that ride the logo's circuit lines.  `true` only when
    /// the user passed `--hot-cpu`.
    ///
    /// When `false` (the default) the shader's `focused` uniform is
    /// forced to `0.0` every frame — the gradient still renders but
    /// holds a constant midpoint color and brightness, and the
    /// electron pulses are suppressed.  Crucially, this also lets
    /// `classify_animation` return `Idle` for focused windows, so the
    /// event loop actually sleeps at idle instead of rendering 30 FPS
    /// of a barely-moving gradient.
    ///
    /// The opacity fade-in / fade-out is decoupled from this flag —
    /// it's driven by user-interaction timing rather than the shader
    /// clock, and runs regardless.  The fade's animation cost is
    /// bounded: it only fires during the `fade_begin_secs` →
    /// `fade_end_secs` window after a blur, then settles into `Idle`.
    hot_cpu: bool,
    /// Master switch for honouring programs' mouse-tracking requests.
    /// `false` when the user passed `--no-mouse-tracking`.  When off,
    /// DECSET 1000/1002/1003/1006 are silently ignored at the routing
    /// layer — drag-select and middle-click-paste work the same way
    /// whether or not the shell program asked for mouse events.
    mouse_tracking: bool,
}

impl App {
    /// Create a new `App` with the given configuration.
    ///
    /// `proxy` is the event-loop proxy used by PTY reader threads to
    /// wake the main loop.  `hot_cpu` enables the shader's
    /// corner-gradient breath / color pulse and electron traces
    /// (`true` when `--hot-cpu` was passed; `false` by default).
    /// `mouse_tracking` controls whether programs' DECSET mouse
    /// requests are honoured (`false` when `--no-mouse-tracking` was
    /// passed).
    ///
    /// The first window is created in [`Self::resumed`].
    pub fn new(
        config: Config,
        proxy: EventLoopProxy<UserEvent>,
        hot_cpu: bool,
        mouse_tracking: bool,
    ) -> Self {
        Self { config, windows: HashMap::new(), proxy, hot_cpu, mouse_tracking }
    }

    /// Build a [`PtyWaker`] for the given window using `self.proxy`.
    ///
    /// Convenience wrapper over [`make_waker_for`] for call sites
    /// where a `&self` borrow is not disputed by the borrow checker
    /// (i.e. `spawn_window`, where `&mut self` is held exclusively).
    fn make_waker(&self, window_id: WindowId) -> PtyWaker {
        make_waker_for(&self.proxy, window_id)
    }

    // ── Window management helpers ─────────────────────────────────────────────

    /// Remove a window and exit the event loop if no windows remain.
    fn close_window(&mut self, id: WindowId, event_loop: &ActiveEventLoop) {
        self.windows.remove(&id);
        if self.windows.is_empty() {
            log::info!("all windows closed — exiting");
            event_loop.exit();
        }
    }

    // ── Cell-size helpers ─────────────────────────────────────────────────────

    /// Apply a new font size to a window's renderer and resize the terminal
    /// grid so it matches the new cell dimensions.
    ///
    /// Clamping of the size itself happens in `Renderer::set_font_size`; the
    /// caller is expected to pass a sensible value.
    fn apply_font_size(state: &mut AppState, new_size: f32) {
        let new_metrics = state.renderer.set_font_size(new_size);
        state.cell_metrics = new_metrics;
        state.current_font_size = new_size;

        let inner = state.window.inner_size();
        let term_size = Self::terminal_size_from_metrics(inner.width, inner.height, &new_metrics);
        state.terminal.resize(term_size);

        // Cell metrics and grid dimensions changed — cached instance
        // data is stale.
        state.content_dirty = true;
        state.window.request_redraw();
    }

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

        // `Renderer::new` is async (wgpu adapter/device requests).  We use
        // `pollster::block_on` to drive the future to completion on the main
        // thread without spawning a runtime.
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

        // Real waker: PTY reader thread calls this after every chunk
        // send, posting a UserEvent::PtyOutput that wakes our main
        // loop from ControlFlow::Wait and triggers a redraw.
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

        // Enable IME events so the OS delivers Preedit/Commit notifications
        // for composed input (accented Latin characters, CJK input, etc.).
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
            last_input_time: now,
            start_time: now,
            focused: true,
            current_font_size: self.config.font.size,
            exit_status: None,
            last_mouse_report: None,
            content_dirty: true,
        };

        self.windows.insert(window_id, state);
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
    display_offset: usize,
) -> (GridPoint, GridSide) {
    let col = (x / cell_width as f64) as usize;
    let row = (y / cell_height as f64) as usize;
    let col = col.min(cols.saturating_sub(1));
    let row = row.min(rows.saturating_sub(1));

    let frac = (x / cell_width as f64).fract();
    let side = if frac < 0.5 { GridSide::Left } else { GridSide::Right };

    // Convert viewport row to alacritty grid line.  When the user has
    // scrolled back, the top of the viewport is `Line(-display_offset)`
    // rather than `Line(0)` — without this correction, selections in
    // scrollback would target lines in the active area instead.
    let grid_line = row as i32 - display_offset as i32;
    (GridPoint::new(GridLine(grid_line), GridColumn(col)), side)
}

// ── ApplicationHandler ────────────────────────────────────────────────────────

impl ApplicationHandler<UserEvent> for App {
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
        }

        // Control flow is managed by `about_to_wait`, which picks
        // `WaitUntil(next_frame)` while any window is animating and
        // falls through to `Wait` when every window is idle.  No need
        // to seed a value here — `about_to_wait` fires before the
        // first sleep.
    }

    /// Handles all windowing events for a single window.
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Intercept window-management shortcuts (Cmd+N, Cmd+W) before the
        // per-window state lookup so we can mutate `self.windows`.  Both
        // need `&mut self`, which would conflict with a borrowed AppState.
        if let WindowEvent::KeyboardInput { event: ref key_event, .. } = event {
            if key_event.state == ElementState::Pressed {
                let modifiers_snapshot = self.windows.get(&id).map(|s| s.modifiers);
                if let (Some(modifiers), Key::Character(c)) =
                    (modifiers_snapshot, &key_event.logical_key)
                {
                    if modifiers.super_key() {
                        match c.as_str() {
                            "n" => {
                                let _ = self.spawn_window(event_loop);
                                return;
                            }
                            "w" => {
                                // Close the window that received the event.
                                self.close_window(id, event_loop);
                                return;
                            }
                            _ => {}
                        }
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
                self.close_window(id, event_loop);
            }

            // ── Resize ────────────────────────────────────────────────────────
            WindowEvent::Resized(size) => {
                state.renderer.resize((size.width, size.height));

                let new_term_size =
                    Self::terminal_size_from_metrics(size.width, size.height, &state.cell_metrics);
                state.terminal.resize(new_term_size);

                // Grid dimensions changed — the retained instance
                // buffer references cells by (col, row) against the
                // previous size.  Force a full render next frame.
                state.content_dirty = true;
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
                // On blur, kickstart the fade to idle by pretending the last
                // input happened `fade_begin_secs` ago.  On focus, reset the
                // timer so any subsequent period of inactivity restarts the
                // fade from the active opacity.
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
                // ── Frozen-window dispatch ────────────────────────────────
                //
                // The shell has exited and we're holding the window open for
                // inspection (per close_on_exit = "success" with non-zero
                // exit, or "never" policy).  Restrict the keyboard surface
                // so accidental typing doesn't hit the dead PTY:
                //
                // - Cmd+R        → respawn the shell in place.
                // - Cmd+C / Cmd+A → fall through to their normal handlers
                //                   so users can copy the final output
                //                   before dismissing.
                // - Any printable key, Enter, Esc, or Space → close.
                // - Cmd+W is handled globally above and already closes.
                // - Everything else is swallowed.
                if state.exit_status.is_some() && key_event.state == ElementState::Pressed {
                    let key = &key_event.logical_key;
                    let mods = state.modifiers;

                    // Cmd+R — respawn.
                    if mods.super_key()
                        && matches!(key, Key::Character(c) if c.as_str() == "r")
                    {
                        // Construct the waker inline: going through
                        // `self.make_waker(id)` would conflict with the
                        // `&mut self.windows` borrow that holds `state`.
                        // Here `self.proxy` and `self.config` are
                        // disjoint fields from `self.windows`, so the
                        // borrow checker allows these direct-field
                        // accesses alongside the `state` borrow.
                        let waker = make_waker_for(&self.proxy, id);
                        respawn_shell(state, &self.config, id, waker);
                        return;
                    }

                    // Cmd+C / Cmd+A — allow fall-through to normal handling.
                    let allow_fall_through = mods.super_key()
                        && matches!(key, Key::Character(c) if matches!(c.as_str(), "c" | "a"));

                    if !allow_fall_through {
                        // Non-Cmd dismissal key → close the window.
                        if !mods.super_key() && is_dismissal_key(key) {
                            self.close_window(id, event_loop);
                        }
                        // Non-dismissal, non-allowed-Cmd: swallow.
                        return;
                    }
                    // else: fall through to the Cmd+C / Cmd+A handler below.
                }

                // Handle Cmd+C (copy) and Cmd+V (paste) before the normal
                // key translation, so these shortcuts are not forwarded to the PTY.
                // (Cmd+N was handled above, before the state lookup.)
                if key_event.state == ElementState::Pressed && state.modifiers.super_key() {
                    if let Key::Character(c) = &key_event.logical_key {
                        match c.as_str() {
                            "c" => {
                                // Copy to clipboard only — no grid
                                // state changes, no redraw needed.  The
                                // selection is already highlighted from
                                // whatever drag produced it.
                                if let Some(text) = state.terminal.selection_text() {
                                    if let Some(cb) = state.clipboard.as_mut() {
                                        if let Err(e) = cb.set_text(text) {
                                            log::warn!("clipboard set failed: {e}");
                                        }
                                    }
                                }
                                return;
                            }
                            "v" => {
                                // Delegate clipboard → PTY entirely to
                                // `Terminal::paste`, which applies the
                                // safety filter (strip bracketed-paste
                                // markers, normalize CR/CRLF, strip
                                // trailing newline when DECSET 2004 is
                                // off) and then wraps in `\x1b[200~…~`
                                // when bracketed paste is active so
                                // readline treats the whole paste as one
                                // edit (one Cmd+Z, no history expansion).
                                //
                                // The filter is crucial: a clipboard
                                // payload containing `\x1b[201~` would
                                // otherwise escape the wrap and smuggle
                                // keystrokes into the shell.
                                if let Some(cb) = state.clipboard.as_mut() {
                                    if let Ok(text) = cb.get_text() {
                                        if let Err(e) = state.terminal.paste(&text) {
                                            log::warn!("PTY paste failed: {e}");
                                        }
                                    }
                                }
                                // `paste` snaps display to the bottom —
                                // display_offset changed.
                                state.content_dirty = true;
                                state.window.request_redraw();
                                return;
                            }
                            "k" => {
                                // Cmd+K — clear scrollback (matches iTerm2).
                                state.terminal.clear_history();
                                state.content_dirty = true;
                                state.window.request_redraw();
                                return;
                            }
                            "a" => {
                                // Cmd+A — select the full terminal buffer
                                // including scrollback.
                                state.terminal.select_all();
                                state.content_dirty = true;
                                state.window.request_redraw();
                                return;
                            }
                            // Cmd++ requires Shift on US keyboards (Shift+=),
                            // which the OS delivers as "+".  Cmd+= without
                            // Shift is accepted too for convenience.
                            "+" | "=" => {
                                let new_size = (state.current_font_size + 1.0).min(72.0);
                                Self::apply_font_size(state, new_size);
                                return;
                            }
                            "-" => {
                                let new_size = (state.current_font_size - 1.0).max(6.0);
                                Self::apply_font_size(state, new_size);
                                return;
                            }
                            "0" => {
                                // Reset to the configured default size.
                                Self::apply_font_size(state, self.config.font.size);
                                return;
                            }
                            "z" => {
                                // Cmd+Z — undo last edit on the current shell
                                // input line.  Maps to readline's undo
                                // (Ctrl+_ = 0x1F), which unwinds recent
                                // insertions, deletions, pastes, etc.  Only
                                // affects the line being edited; doesn't
                                // touch executed commands or scrollback.
                                if let Err(e) = state.terminal.write_to_pty(b"\x1f") {
                                    log::warn!("PTY undo write failed: {e}");
                                }
                                // write_to_pty snaps display to bottom.
                                state.content_dirty = true;
                                state.window.request_redraw();
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                state.last_input_time = std::time::Instant::now();

                if let Some(bytes) = crate::input::translate_key(&key_event, state.modifiers, state.terminal.cursor_app_mode()) {
                    // Clear any visible selection as a side effect of typing.
                    //
                    // Earlier we tried to gate Escape on selection presence
                    // (swallow Esc, clear selection, don't forward to PTY)
                    // which felt clean but broke vim in subtle ways: every
                    // left-click creates a degenerate selection via
                    // `start_selection`, and middle-click paste deliberately
                    // preserves the selection.  A user could end up with a
                    // stray invisible selection that silently swallowed
                    // every Esc.  Now Esc always reaches the PTY (so vim
                    // exits insert mode) AND the selection still clears
                    // visually as a free side effect — same end result for
                    // selection clearing, no broken vim.
                    if state.terminal.selection_range().is_some() {
                        state.terminal.clear_selection();
                    }
                    if let Err(e) = state.terminal.write_to_pty(&bytes) {
                        log::warn!("PTY write failed: {e}");
                    }
                }
                // Selection may have cleared and/or display snapped to
                // bottom via `write_to_pty`; either way the render
                // output differs from the cached one.
                state.content_dirty = true;
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
                        // Position the IME candidate window near the terminal
                        // cursor so popups appear where the user is typing
                        // instead of the window's top-left corner.
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
                        // Inline preedit display (showing the in-progress
                        // composition at the cursor) is deferred — a full
                        // implementation would overlay the preedit string.
                        let _ = (text, cursor);
                    }
                    Ime::Enabled | Ime::Disabled => {}
                }
                // Ime::Commit wrote to PTY; Ime::Preedit updated the IME
                // candidate area.  In either case the upcoming frame may
                // differ from the cached one — conservatively dirty.
                state.content_dirty = true;
                state.window.request_redraw();
            }

            // ── Mouse button press/release ────────────────────────────────────
            //
            // Unified arm for Left/Middle/Right presses and releases.  The
            // routing decision (forward to PTY vs. handle locally) is made
            // once at the top of the arm by `route_mouse`, then each button
            // / state combination takes the appropriate branch.
            WindowEvent::MouseInput { state: btn_state, button: win_button, .. } => {
                state.last_input_time = std::time::Instant::now();

                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                // ── Forwarded path ────────────────────────────────────────
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
                        // Track button-down state locally so CursorMoved
                        // knows whether to emit drag motion.  We only
                        // track the left button — right/middle drag are
                        // rare enough that their motion can be dropped
                        // without visible regression.
                        if matches!(win_button, MouseButton::Left) {
                            state.mouse_pressed = matches!(btn_state, ElementState::Pressed);
                        }
                    }
                    // Reset motion-dedup on release so the next drag starts fresh.
                    if matches!(btn_state, ElementState::Released) {
                        state.last_mouse_report = None;
                    }
                    // Forwarded mouse event wrote to PTY — display
                    // snapped to bottom and the program may echo.
                    state.content_dirty = true;
                    state.window.request_redraw();
                    return;
                }

                // ── Local path ────────────────────────────────────────────
                match (btn_state, win_button) {
                    // ── Left button: selection + click-to-move-cursor ─────
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
                                // Click-vs-drag threshold: 5px matches the OS
                                // default.  Tiny motions shouldn't be treated
                                // as selections — they misfire click-to-move.
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
                                    // Click-to-move: emit N arrow-keys equal
                                    // to the column delta, only if the shell
                                    // is at the live view and the click is on
                                    // the same row as the cursor.  A naive
                                    // heuristic (breaks on wide chars and
                                    // TUIs) but useful for ASCII readline.
                                    // TUIs that enable mouse tracking take
                                    // the forwarded path above — so this
                                    // only runs when the shell explicitly
                                    // opted out of mouse input.
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
                                            if let Err(e) =
                                                state.terminal.write_to_pty(&payload)
                                            {
                                                log::warn!("PTY cursor-move write failed: {e}");
                                            }
                                        }
                                    }
                                } else {
                                    // Drag completed — capture into the
                                    // X11-style primary selection for
                                    // middle-click paste.
                                    state.primary_selection = state.terminal.selection_text();
                                }
                            }
                        }
                        // Selection state changed (start/clear) and/or
                        // a click-to-move wrote to the PTY.
                        state.content_dirty = true;
                        state.window.request_redraw();
                    }

                    // ── Middle-click paste (primary selection) ────────────
                    (ElementState::Pressed, MouseButton::Middle) => {
                        if let Some(text) = state.primary_selection.as_ref() {
                            if let Err(e) = state.terminal.paste(text) {
                                log::warn!("PTY middle-click paste failed: {e}");
                            }
                            state.content_dirty = true;
                            state.window.request_redraw();
                        }
                    }

                    // Middle release / Right any / Back / Forward /
                    // Other in local mode: ignored.  Terminal.app and
                    // iTerm2 do the same — right-click opens a context
                    // menu that we haven't built yet.
                    _ => {}
                }
            }

            // ── Cursor movement ───────────────────────────────────────────────
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_position = (position.x, position.y);

                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                // ── Forwarded path ────────────────────────────────────────
                if let Some(sgr) = route {
                    let proto = state.terminal.mouse_protocol();
                    // DECSET 1002 = drag-only; DECSET 1003 = all motion.
                    // Emit motion when either is active AND the program's
                    // constraints are met:
                    //   1003: always (firehose)
                    //   1002: only when a button is held
                    //   1000 alone: never (clicks only, no motion)
                    let emit = proto.report_motion
                        || (proto.report_drag && state.mouse_pressed);
                    if emit {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        // Dedup at cell granularity — winit fires
                        // CursorMoved every pixel, but the shell only
                        // cares about cell-level grid coordinates.
                        if state.last_mouse_report != Some((col, row)) {
                            state.last_mouse_report = Some((col, row));
                            // We only track left-button down locally;
                            // encode motion as if it were left-button
                            // drag (the common case for 1002).
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

                // ── Local path: update selection if dragging ──────────────
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
                    // Selection range changed — highlighted cells differ.
                    state.content_dirty = true;
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

                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                // ── Forwarded path ────────────────────────────────────────
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
                        // Emit one wheel "click" per unit of scroll —
                        // that's how vim/tmux expect it.
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
                    // Forwarded wheel wrote to PTY (display snapped to
                    // bottom).
                    state.content_dirty = true;
                    state.window.request_redraw();
                    return;
                }

                // ── Local path: scroll the viewport ───────────────────────
                if lines > 0 {
                    state.terminal.scroll_up(lines as usize);
                } else if lines < 0 {
                    state.terminal.scroll_down((-lines) as usize);
                }
                // display_offset changed — the visible slice of
                // scrollback is different from the cached frame.
                state.content_dirty = true;
                state.window.request_redraw();
            }

            // ── Redraw ────────────────────────────────────────────────────────
            WindowEvent::RedrawRequested => {
                // Drain PTY bytes, update grid, collect outcome events.
                let outcome = state.terminal.process_input();

                // Any bytes fed to the VTE parser may have changed the
                // grid — force the full-render branch below so the new
                // content shows up this frame rather than waiting for
                // another event to dirty the cache.
                if outcome.grid_maybe_changed {
                    state.content_dirty = true;
                }

                // If the shell exited this frame, decide close-vs-freeze
                // and (on freeze) write a banner line the user can read.
                if let Some(status) = outcome.child_exit {
                    if state.exit_status.is_none() {
                        let should_close = match self.config.terminal.close_on_exit {
                            mechanic_config::CloseOnExitPolicy::Always => true,
                            // Treat "no status available" (library-internal
                            // Event::Exit, very rare) as success — nothing
                            // actually failed, we were just told to leave.
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

                        // Frozen path: inject an amber banner line the
                        // user can read in-grid, and remember the status
                        // so the keyboard handler can switch to the
                        // dismissal-key policy.
                        inject_exit_banner(&mut state.terminal, status);
                        state.exit_status = Some(status);
                        // Banner wrote to the grid via inject_local —
                        // cached instances are stale.
                        state.content_dirty = true;
                        state.window.request_redraw();
                    }
                }

                // Opacity: fade interpolation uses wall-clock elapsed
                // since last input.  Always runs — the fade is cheap
                // (only ticks during the few seconds between
                // `fade_begin_secs` and `fade_end_secs` after a blur,
                // then settles) and is a genuinely useful signal
                // about which window was most recently active.
                let opacity = {
                    let elapsed_secs = state.last_input_time.elapsed().as_secs_f32();
                    compute_opacity(elapsed_secs, &self.config.theme.opacity)
                };

                let time = state.start_time.elapsed().as_secs_f32();

                // Shader animations (corner gradient brightness breath,
                // color pulse, electron traces on the logo) are all
                // gated on the `focused` uniform.  Unless `--hot-cpu`
                // was passed we force it to `false` regardless of real
                // focus state — freezes the gradient at its midpoint
                // color and constant brightness, and suppresses
                // electron pulses.  The gradient itself still renders
                // as a static corner accent.
                let shader_focused = state.focused && self.hot_cpu;

                // Two render paths:
                //
                // 1. Animation fast path — only the shader
                //    time/opacity/focused uniforms changed (focused
                //    pulse, idle fade).  Reissue the previous frame's
                //    cached instance draw against a new globals
                //    uniform.  Skips grid conversion and the ~200 KB
                //    instance rebuild+upload per frame.
                //
                // 2. Full render — grid state changed (PTY output,
                //    resize, scroll, selection, etc.) or no prior
                //    render has populated the instance cache yet.
                //    Convert the grid, build instances, upload, draw.
                //
                // Short-circuit: attempt path 1 only when
                // `!content_dirty`; fall through to path 2 when
                // `render_animation` reports no cached instances (first
                // frame, or post-resize before a full render).
                let did_animation_render = !state.content_dirty
                    && state.renderer.render_animation(opacity, time, shader_focused);

                if !did_animation_render {
                    let grid = crate::convert::convert_grid(&state.terminal, &self.config.theme);
                    state.renderer.render(&grid, opacity, time, shader_focused);
                    state.content_dirty = false;
                }

                // Title: base from the shell's OSC-set title (or "Mechanic"
                // when unset), suffixed with exit info when the window is
                // frozen so the user can see exit status at a glance even
                // when the grid has scrolled past the banner line.
                let base_title = state.terminal.title();
                let base = if base_title.is_empty() { "Mechanic" } else { base_title };
                let title_string = match state.exit_status {
                    Some(status) => format!("{base} — {}", format_title_suffix(status)),
                    None => base.to_string(),
                };
                state.window.set_title(&title_string);
            }

            _ => {}
        }
    }

    /// Called just before the event loop sleeps.
    ///
    /// Decides control flow for the next iteration based on what each
    /// window needs:
    ///
    /// - Any window currently animating (focused gradient, or mid-fade)
    ///   gets a redraw request; we set `ControlFlow::WaitUntil(now + 33ms)`
    ///   to wake for the next ~30 FPS frame.
    /// - Any window whose animation will *start* later (unfocused, fade
    ///   not yet begun) schedules a wake at that future moment.
    /// - Windows that are fully static (frozen shell, or unfocused past
    ///   fade-end, or `--no-animation`) contribute no deadline and no
    ///   redraw — the loop sleeps on `ControlFlow::Wait` until user
    ///   input or a PTY-output user event arrives.
    ///
    /// We take the earliest deadline across all windows so a single
    /// global timer drives everyone.  Simpler than per-window vsync
    /// alignment; `PresentMode::Fifo` still aligns actual presents to
    /// each monitor's refresh rate.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut earliest_deadline: Option<Instant> = None;

        for state in self.windows.values() {
            let input = AnimationInputs {
                is_alive: state.exit_status.is_none(),
                focused: state.focused,
                last_input_time: state.last_input_time,
            };
            let anim = classify_animation(
                input,
                &self.config.theme.opacity,
                self.hot_cpu,
                now,
            );
            match anim {
                AnimationState::Active { next_frame } => {
                    state.window.request_redraw();
                    merge_deadline(&mut earliest_deadline, next_frame);
                }
                AnimationState::WakeAt(deadline) => {
                    merge_deadline(&mut earliest_deadline, deadline);
                }
                AnimationState::Idle => {
                    // No redraw, no deadline contribution.
                }
            }
        }

        event_loop.set_control_flow(match earliest_deadline {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    /// Handle a user event pushed from a background thread.
    ///
    /// Currently the only producer is each window's PTY reader thread,
    /// which posts [`UserEvent::PtyOutput`] after draining bytes into
    /// the channel.  Requesting a redraw on the target window causes
    /// the next frame's `RedrawRequested` to fire `process_input`,
    /// which drains the channel and renders the new output.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput(id) => {
                if let Some(state) = self.windows.get(&id) {
                    // Ignored for frozen windows — `process_input` would
                    // observe nothing new (reader thread has exited) and
                    // we don't want to hot-loop on stray wakes.
                    if state.exit_status.is_none() {
                        state.window.request_redraw();
                    }
                }
            }
        }
    }
}

// ── Mouse routing ─────────────────────────────────────────────────────────────

/// Decide whether a mouse event should be forwarded to the PTY (and
/// if so, which encoding to use) or handled locally.
///
/// Returns `Some(sgr)` when forwarding, where `sgr` selects the wire
/// format (`true` for DECSET 1006 SGR, `false` for legacy X10).
/// Returns `None` when the event should fall through to the local
/// selection / scrollback / click-to-move behaviour.
///
/// Precedence:
/// 1. Frozen window → never forward.  Shell is dead; there's no one
///    listening on the other end.
/// 2. `--no-mouse-tracking` at the CLI → never forward, regardless of
///    what the program asked for.  The user's meta-preference wins.
/// 3. Shift held → never forward.  iTerm2/kitty convention: Shift is
///    the "let the terminal handle this" override for users who want
///    to select text in a program that's captured the mouse.
/// 4. Program hasn't enabled any tracking mode → never forward.
/// 5. Otherwise → forward, with `sgr` taken from the protocol.
///
/// Pure function; trivially unit-testable.
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
///
/// Back, Forward, and Other buttons are not yet forwarded — the wire
/// protocol has numbers available for them (8-11 in the standard
/// extension) but no widely-deployed TUI actually listens for those,
/// so the added complexity hasn't been worth it.
fn winit_to_mouse_button(b: MouseButton) -> Option<mouse_enc::MouseButton> {
    match b {
        MouseButton::Left => Some(mouse_enc::MouseButton::Left),
        MouseButton::Middle => Some(mouse_enc::MouseButton::Middle),
        MouseButton::Right => Some(mouse_enc::MouseButton::Right),
        _ => None,
    }
}

/// Convert a pixel position to 1-based grid coordinates for mouse
/// encoding.  Clamps to the visible grid — clicks slightly outside
/// the window report the edge cell rather than out-of-bounds.
fn grid_coords_1based(
    pos: (f64, f64),
    metrics: &CellMetrics,
    cols: usize,
    rows: usize,
) -> (u32, u32) {
    let cw = (metrics.cell_width as f64).max(1.0);
    let ch = (metrics.cell_height as f64).max(1.0);
    // 0-based cell coordinates first so we can clamp, then +1 for the
    // wire format's 1-based convention.
    let col0 = (pos.0 / cw).max(0.0) as u32;
    let row0 = (pos.1 / ch).max(0.0) as u32;
    let col0 = col0.min(cols.saturating_sub(1) as u32);
    let row0 = row0.min(rows.saturating_sub(1) as u32);
    (col0 + 1, row0 + 1)
}

// ── Event-loop proxy wiring ───────────────────────────────────────────────────

/// Build a [`PtyWaker`] that fires `UserEvent::PtyOutput(window_id)`
/// through `proxy` whenever called.
///
/// The closure captures a clone of `proxy` (cheap — it's `Arc`-backed)
/// and the window id.  Send errors (event loop closed) are silently
/// ignored: if the loop has shut down, the window has too, and the
/// reader thread is about to exit anyway.
///
/// Lives at module scope as a free function so call sites inside
/// `App::window_event` can use it while holding a `&mut self.windows`
/// borrow — a method on `&self` would be barred by the borrow checker
/// even though only the disjoint `self.proxy` field is touched.
fn make_waker_for(proxy: &EventLoopProxy<UserEvent>, window_id: WindowId) -> PtyWaker {
    let proxy = proxy.clone();
    Arc::new(move || {
        let _ = proxy.send_event(UserEvent::PtyOutput(window_id));
    })
}

// ── Animation scheduling ──────────────────────────────────────────────────────

/// What a window needs from the event-loop scheduler for the next tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimationState {
    /// Window has active animation.  Redraw now; next frame at `next_frame`.
    Active { next_frame: Instant },
    /// Window is static right now but animation will start at `deadline`
    /// (e.g. fade-begin for an unfocused window that's still in its
    /// pre-fade grace period).  Don't redraw yet — just schedule a wake.
    WakeAt(Instant),
    /// Window is fully static with no scheduled future animation.
    /// Only user input or PTY output should wake us on its behalf.
    Idle,
}

/// Inputs to [`classify_animation`] — the minimal slice of `AppState`
/// the scheduler actually needs.  A small struct (rather than three
/// positional args) keeps call sites readable and makes unit tests
/// self-documenting.
#[derive(Debug, Clone, Copy)]
struct AnimationInputs {
    /// Is the shell still alive (vs. frozen awaiting dismissal)?
    is_alive: bool,
    /// Does the window currently hold keyboard focus?
    focused: bool,
    /// When was the last user interaction in this window?
    last_input_time: Instant,
}

/// Decide what scheduling a window needs right now.
///
/// Pure function — unit-testable without GPU, Terminal, or real
/// windowing.  Rules:
///
/// 1. Frozen window (shell exited, awaiting dismissal) → `Idle`.
/// 2. Focused + `hot_cpu` → `Active` every `FRAME_INTERVAL`.  The
///    corner-gradient breath, color pulse, and electron traces are
///    continuous animations driven by the shader clock, so the event
///    loop has to keep rendering to move them forward.
/// 3. Focused + `!hot_cpu` → `Idle`.  The shader `focused` uniform is
///    forced to 0 in the render path so nothing time-dependent is
///    running — there's no reason to wake up.  This is the default
///    and the main win: a focused idle window costs nothing.
/// 4. Unfocused, `elapsed < fade_begin` → `WakeAt(last_input + fade_begin)`.
///    Nothing is animating yet, but the fade will start at that instant.
/// 5. Unfocused, `fade_begin <= elapsed <= fade_end` → `Active` (mid-fade).
///    The opacity fade is independent of `hot_cpu` and always runs.
/// 6. Unfocused, `elapsed > fade_end` → `Idle`.  Opacity has settled at
///    `content_idle_opacity`; the shader is static; nothing to redraw.
fn classify_animation(
    input: AnimationInputs,
    opacity_cfg: &mechanic_config::OpacityConfig,
    hot_cpu: bool,
    now: Instant,
) -> AnimationState {
    // Rule 1.
    if !input.is_alive {
        return AnimationState::Idle;
    }
    // Rule 2 / 3: focused → depends on hot_cpu.
    if input.focused {
        if hot_cpu {
            return AnimationState::Active { next_frame: now + FRAME_INTERVAL };
        }
        // Focused and quiet — nothing time-dependent is rendered.
        return AnimationState::Idle;
    }

    // Unfocused — check fade state.  `saturating_duration_since` handles
    // the (impossible-in-practice) case where `last_input_time > now`.
    let elapsed = now.saturating_duration_since(input.last_input_time);
    let fade_begin = Duration::from_secs(u64::from(opacity_cfg.fade_begin_secs));
    let fade_end = Duration::from_secs(u64::from(opacity_cfg.fade_end_secs));

    if elapsed < fade_begin {
        // Rule 4 — wake when the fade is due to start.
        let wake_in = fade_begin - elapsed;
        AnimationState::WakeAt(now + wake_in)
    } else if elapsed <= fade_end {
        // Rule 5 — mid-fade, animate continuously.
        AnimationState::Active { next_frame: now + FRAME_INTERVAL }
    } else {
        // Rule 6 — past fade end, nothing moves.
        AnimationState::Idle
    }
}

/// Keep the earlier of two deadlines.
fn merge_deadline(current: &mut Option<Instant>, candidate: Instant) {
    *current = Some(match *current {
        Some(existing) => existing.min(candidate),
        None => candidate,
    });
}

// ── Frozen-window dismissal / respawn helpers ─────────────────────────────────

/// Which keys close a frozen window.
///
/// Matches the spec: any printable key, Enter, Esc, or Space.  Modifier-
/// only presses (Shift/Ctrl/Alt/Super alone) and non-printable navigation
/// keys (arrows, F-keys, Home/End, dead keys) are deliberately NOT
/// dismissal keys — accidental hover-bumps on a modifier key shouldn't
/// throw away the window's final output.
///
/// Space is both a `Key::Character(" ")` on most platforms and
/// `Key::Named(NamedKey::Space)` on some — we match both to be safe.
fn is_dismissal_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Character(_) | Key::Named(NamedKey::Enter | NamedKey::Escape | NamedKey::Space)
    )
}

/// Respawn the shell inside an already-frozen window.
///
/// Constructs a fresh [`Terminal`] with the window's current grid size
/// and replaces `state.terminal` — the old `Terminal`'s `Drop` handles
/// the cleanup (its PTY handle closes, its reader thread sees EOF and
/// exits).  On success, clears the `exit_status` marker so the window
/// leaves frozen state.
///
/// If `Terminal::new` fails we log and leave the window frozen — the
/// user can Cmd+W to dismiss.
fn respawn_shell(state: &mut AppState, config: &Config, id: WindowId, waker: PtyWaker) {
    let size = state.terminal.size();
    match Terminal::new(config, size, waker) {
        Ok(new_term) => {
            state.terminal = new_term;
            state.exit_status = None;
            state.last_input_time = std::time::Instant::now();
            // Fresh terminal → fresh grid → cached instances are stale.
            state.content_dirty = true;
            state.window.request_redraw();
            log::info!("window {id:?} shell respawned");
        }
        Err(e) => {
            log::error!("window {id:?} respawn failed: {e}");
        }
    }
}

// ── Exit-banner helpers ───────────────────────────────────────────────────────

/// Write an amber "shell exited" banner into the terminal grid.
///
/// Uses [`Terminal::inject_local`] to feed the bytes through the VTE
/// parser so SGR colour codes render normally and the line ends up in
/// scrollback like any other shell output.
///
/// Sequence breakdown:
/// - `\r\n` — move to column 0 on a fresh line so we don't overwrite
///   whatever the shell's last partial line was.
/// - `\x1b[33m` — amber foreground (SGR 33 = yellow; rendered as
///   `theme.ansi.yellow` which maps to our `AMBER` constant).
/// - `\x1b[0m` — reset so subsequent content (if any, e.g. on respawn)
///   isn't tinted.
/// - Trailing `\r\n` — cursor lands on the line below for a tidy visual.
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
                // Unix: the child was killed by a signal rather than exiting normally.
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

/// Long-form exit-status string for the `log::info!` telemetry line —
/// e.g. `"code 0"`, `"code 137"`, `"signal 9 (SIGKILL)"`, `"no status"`.
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

    // ── Exit-status formatting ────────────────────────────────────────────────

    #[cfg(unix)]
    fn status_from_code(code: i32) -> std::process::ExitStatus {
        // On Unix, a normal exit with code N encodes as the high byte
        // of the raw status: (code & 0xff) << 8.  wait(2)'s W_EXITCODE
        // macro.  This matches what ExitStatus::code() will return.
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw((code & 0xff) << 8)
    }

    #[cfg(unix)]
    fn status_from_signal(sig: i32) -> std::process::ExitStatus {
        // Signal-kill encoding: low 7 bits hold the signal number.
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
        // SIGKILL = 9.
        let msg = format_banner_message(Some(status_from_signal(9)));
        assert!(msg.contains("signal 9"));
    }

    #[test]
    fn banner_message_for_no_status() {
        // None payload = library-internal Event::Exit, no info available.
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

    // ── Frozen-window dismissal ───────────────────────────────────────────────

    #[test]
    fn dismissal_printable_char() {
        // Any character-bearing key dismisses — covers letters, digits,
        // symbols, and Space-as-character on platforms that deliver it that way.
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
        // A stray bump on Shift / Ctrl / Alt / Super should never throw
        // the window away.  The user may just be preparing to hit Cmd+C.
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Shift)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Control)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Alt)));
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Super)));
    }

    #[test]
    fn navigation_keys_do_not_dismiss() {
        // Arrow / Home / End / Page keys are non-destructive navigation;
        // they shouldn't close a frozen window since the user might
        // want to scroll through the final output.
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

    // ── Animation scheduling ──────────────────────────────────────────────────

    fn default_opacity_cfg() -> mechanic_config::OpacityConfig {
        mechanic_config::OpacityConfig {
            title_bar_opacity: 0.95,
            content_active_opacity: 0.95,
            content_idle_opacity: 0.80,
            fade_begin_secs: 30,
            fade_end_secs: 60,
        }
    }

    /// Build inputs where `last_input_time` is exactly `last_input_ago`
    /// before `now`.  Both the inputs and the caller-visible `now`
    /// share the same reference instant so boundary tests aren't
    /// subject to a race between two independent `Instant::now()`
    /// calls.
    fn inputs_at(
        now: Instant,
        is_alive: bool,
        focused: bool,
        last_input_ago: Duration,
    ) -> AnimationInputs {
        AnimationInputs {
            is_alive,
            focused,
            last_input_time: now.checked_sub(last_input_ago).unwrap_or(now),
        }
    }

    /// Convenience wrapper that uses `Instant::now()` for tests that
    /// don't care about sub-microsecond timing.
    fn inputs(is_alive: bool, focused: bool, last_input_ago: Duration) -> AnimationInputs {
        inputs_at(Instant::now(), is_alive, focused, last_input_ago)
    }

    #[test]
    fn anim_frozen_window_is_idle() {
        // Rule 1: shell exited → nothing to render.
        let cfg = default_opacity_cfg();
        let input = inputs(false, true, Duration::ZERO);
        assert_eq!(
            classify_animation(input, &cfg, true, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_focused_quiet_default_is_idle() {
        // Rule 3 (the new default): focused but hot_cpu=false → Idle.
        // This is the main CPU-savings win — a focused idle window
        // should not wake the event loop 30 times a second just to
        // redraw pixels that don't change.
        let cfg = default_opacity_cfg();
        let input = inputs(true, true, Duration::ZERO);
        assert_eq!(
            classify_animation(input, &cfg, false, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_focused_hot_cpu_is_active() {
        // Rule 2: focused + --hot-cpu → continuous animation at
        // FRAME_INTERVAL so the shader time-based effects actually
        // advance frame-to-frame.
        let cfg = default_opacity_cfg();
        let input = inputs(true, true, Duration::from_secs(1000));
        let now = Instant::now();
        match classify_animation(input, &cfg, true, now) {
            AnimationState::Active { next_frame } => {
                // next_frame should be roughly one FRAME_INTERVAL out.
                let delta = next_frame.saturating_duration_since(now);
                assert!(delta >= FRAME_INTERVAL);
                assert!(delta <= FRAME_INTERVAL + Duration::from_millis(5));
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn anim_unfocused_pre_fade_wakes_later() {
        // Rule 4: unfocused + before fade_begin → WakeAt(fade_begin).
        // `hot_cpu` should not affect this — pass `false` to prove
        // the fade's redraw scheduling is independent.
        let cfg = default_opacity_cfg();
        let elapsed = Duration::from_secs(10); // well before fade_begin = 30
        let input = inputs(true, false, elapsed);
        let now = Instant::now();
        match classify_animation(input, &cfg, false, now) {
            AnimationState::WakeAt(deadline) => {
                // Deadline should be ~20s from now (fade_begin 30 - elapsed 10).
                let delta = deadline.saturating_duration_since(now);
                assert!(delta >= Duration::from_secs(19));
                assert!(delta <= Duration::from_secs(21));
            }
            other => panic!("expected WakeAt, got {other:?}"),
        }
    }

    #[test]
    fn anim_unfocused_mid_fade_is_active_without_hot_cpu() {
        // Rule 5: unfocused + mid-fade → Active regardless of
        // `hot_cpu`.  The fade is our one unconditional animation —
        // it communicates "this window is going idle" and is cheap
        // (runs only for a few seconds per blur).
        let cfg = default_opacity_cfg();
        let elapsed = Duration::from_secs(45); // between 30 and 60
        let input = inputs(true, false, elapsed);
        let now = Instant::now();
        assert!(matches!(
            classify_animation(input, &cfg, false, now),
            AnimationState::Active { .. }
        ));
    }

    #[test]
    fn anim_unfocused_past_fade_end_is_idle() {
        // Rule 6: unfocused + elapsed > fade_end → fully static.
        let cfg = default_opacity_cfg();
        let elapsed = Duration::from_secs(120); // well past fade_end = 60
        let input = inputs(true, false, elapsed);
        assert_eq!(
            classify_animation(input, &cfg, false, Instant::now()),
            AnimationState::Idle
        );
    }

    #[test]
    fn anim_unfocused_exactly_at_fade_end_still_active() {
        // Boundary: elapsed == fade_end is treated as "still fading"
        // (rule 5's `<= fade_end`) so the last frame of the fade
        // actually renders before we go idle.  Use `inputs_at` so the
        // inputs and classifier's `now` share a reference instant —
        // otherwise nanoseconds of drift push us past fade_end.
        let cfg = default_opacity_cfg();
        let now = Instant::now();
        let input = inputs_at(now, true, false, Duration::from_secs(60));
        assert!(matches!(
            classify_animation(input, &cfg, false, now),
            AnimationState::Active { .. }
        ));
    }

    // ── Mouse routing ─────────────────────────────────────────────────────────

    fn tracking_proto(sgr: bool) -> MouseProtocol {
        MouseProtocol {
            report_click: true,
            report_drag: true,
            report_motion: false,
            sgr,
        }
    }

    #[test]
    fn route_mouse_forwards_when_program_tracks() {
        // Canonical case: program enabled 1000/1002/1006, user is
        // not holding Shift, window is alive, CLI flag is on.
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, true, false, false), Some(true));
    }

    #[test]
    fn route_mouse_sgr_flag_passes_through() {
        // Program enabled 1000/1002 but NOT 1006 — we should still
        // forward, but with sgr=false so the caller uses X10 framing.
        let proto = tracking_proto(false);
        assert_eq!(route_mouse(proto, true, false, false), Some(false));
    }

    #[test]
    fn route_mouse_no_tracking_returns_none() {
        // Program didn't enable any DECSET tracking → local handling.
        let proto = MouseProtocol::default();
        assert_eq!(route_mouse(proto, true, false, false), None);
    }

    #[test]
    fn route_mouse_frozen_window_returns_none() {
        // Rule 1: frozen window never forwards.  Shell is dead.
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, true, false, true), None);
    }

    #[test]
    fn route_mouse_cli_flag_off_returns_none() {
        // Rule 2: --no-mouse-tracking overrides program request.
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, false, false, false), None);
    }

    #[test]
    fn route_mouse_shift_override_returns_none() {
        // Rule 3: Shift is "let terminal handle this" per iTerm2
        // convention.  Forward path declines so local selection can run.
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, true, true, false), None);
    }

    #[test]
    fn route_mouse_precedence_frozen_beats_cli_flag() {
        // If both a frozen window and --no-mouse-tracking would both
        // produce None anyway, but confirm neither produces a false-
        // forward: frozen takes precedence (doesn't matter which wins,
        // both return None).
        let proto = tracking_proto(true);
        assert_eq!(route_mouse(proto, false, false, true), None);
    }

    // ── grid_coords_1based ────────────────────────────────────────────────────

    fn metrics(cw: f32, ch: f32) -> CellMetrics {
        CellMetrics { cell_width: cw, cell_height: ch, ascent: ch * 0.8 }
    }

    #[test]
    fn grid_coords_origin_maps_to_one_one() {
        // Wire format is 1-based; (0,0) pixel should be (1,1) on wire.
        assert_eq!(grid_coords_1based((0.0, 0.0), &metrics(8.0, 16.0), 80, 24), (1, 1));
    }

    #[test]
    fn grid_coords_typical_click() {
        // 8px wide cells, click at x=24 → col 3 (0-based) → wire col 4.
        // 16px tall cells, y=48 → row 3 → wire row 4.
        assert_eq!(grid_coords_1based((24.0, 48.0), &metrics(8.0, 16.0), 80, 24), (4, 4));
    }

    #[test]
    fn grid_coords_clamps_right_edge() {
        // Click past the right edge clamps to the last visible column.
        // 80 cols × 8px = 640; click at x=9999.
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
        // Winit can deliver slightly negative coords during drags that
        // leave the window.  Clamp to the top-left cell rather than
        // letting a cast produce garbage.
        assert_eq!(
            grid_coords_1based((-10.0, -10.0), &metrics(8.0, 16.0), 80, 24),
            (1, 1)
        );
    }

    #[test]
    fn grid_coords_tolerates_tiny_cells() {
        // A zero or sub-pixel cell_width shouldn't panic.  `.max(1.0)`
        // inside the helper protects against div-by-zero.
        let _ = grid_coords_1based((0.0, 0.0), &metrics(0.0, 0.0), 10, 10);
    }

    // ── merge_deadline ────────────────────────────────────────────────────────

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
        // Tab is borderline — kitty dismisses on any key, but the spec
        // here is "printable + Enter/Esc/Space".  Tab isn't printable
        // in the glyph sense, so it's not a dismissal key.
        assert!(!is_dismissal_key(&Key::Named(NamedKey::Tab)));
    }

    #[cfg(unix)]
    #[test]
    fn telemetry_exit_status_shapes() {
        assert_eq!(format_exit_status(Some(status_from_code(0))), "code 0");
        assert_eq!(format_exit_status(Some(status_from_signal(9))), "signal 9");
        assert_eq!(format_exit_status(None), "no status");
    }

    // ── Opacity ───────────────────────────────────────────────────────────────

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
