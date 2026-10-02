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
use crate::scheduling::{FramePacer, FrameSchedule, ParseQueue};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};

/// Target interval between animation frames (~30 FPS).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Events the main event loop receives from other threads.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Wake this window to drain queued PTY output.
    PtyOutput(WindowId),
    Search(WindowId, crate::search_platform::SearchAction),
}

/// State for one terminal window.
struct AppState {
    search_panel: Option<crate::search_platform::SearchPanel>,
    search: crate::search::Search,
    /// The OS window, shared with the wgpu surface via `Arc`.
    window: Arc<Window>,
    window_title: String,
    terminal: Terminal,
    renderer: Renderer,
    /// Real cell metrics from the renderer (used for resize calculations).
    cell_metrics: CellMetrics,
    /// Current physical mouse cursor position in pixels.
    mouse_position: (f64, f64),
    pointer_inside: bool,
    hovered_link: Option<crate::hyperlinks::LinkTarget>,
    link_press: crate::link_input::LinkPress,
    link_menu_release: bool,
    pointer_cursor: Option<CursorIcon>,
    mouse_pressed: bool,
    held_buttons: mouse_enc::HeldButtons,
    scroll_accumulator: mouse_enc::ScrollAccumulator,
    /// Press position in physical pixels, used to distinguish clicks from drags.
    mouse_press_origin: Option<(f64, f64)>,
    /// Last drag selection, pasted by middle-click independently of the clipboard.
    primary_selection: Option<String>,
    /// Current keyboard modifier state (updated via `ModifiersChanged`).
    modifiers: ModifiersState,
    preedit: Option<crate::preedit::Preedit>,
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
    last_mouse_report: Option<(u32, u32, mouse_enc::MouseButton)>,
    /// Rebuild cell instances when true; otherwise reuse the cached frame.
    content_dirty: bool,
    layout_dirty: bool,
    /// Forced frames after focus changes, to accommodate AppKit redraw coalescing.
    focus_redraw_frames: u8,
    /// Pending focus-gain time; cleared on focus loss or bloom commitment.
    focus_gain_at: Option<Instant>,
    /// Start of the committed bloom; cleared after its configured duration.
    bloom_start: Option<Instant>,
    frame_pacer: FramePacer,
}

impl AppState {
    fn invalidate_search(&mut self) {
        if self.search.invalidate() {
            self.update_search_status();
        }
    }

    fn update_search_status(&self) {
        if self.search.active
            && let Some(panel) = &self.search_panel
        {
            panel.set_status(&self.search.status());
        }
    }

    fn show_search(&mut self, proxy: &EventLoopProxy<UserEvent>, id: WindowId) {
        if self.search_panel.is_none() {
            let proxy = proxy.clone();
            match crate::search_platform::SearchPanel::new(&self.window, move |action| {
                let _ = proxy.send_event(UserEvent::Search(id, action));
            }) {
                Ok(panel) => self.search_panel = Some(panel),
                Err(error) => {
                    log::warn!("could not open search: {error}");
                    return;
                }
            }
        }
        self.preedit = None;
        self.link_press.cancel();
        self.search.active = true;
        self.update_search_status();
        self.search_panel.as_ref().unwrap().show();
        self.mark_content_dirty();
        self.request_redraw();
    }

    fn reveal_search_match(&mut self) {
        if let Some(hit) = self.search.current() {
            let offset = self.terminal.grid().display_offset();
            let top = -(offset as i32);
            let bottom = top + self.terminal.screen_lines() as i32 - 1;
            if hit.start.line.0 < top || hit.end.line.0 > bottom {
                let desired = (-hit.start.line.0).max(0) as usize;
                if desired > offset {
                    self.terminal.scroll_up(desired - offset);
                } else {
                    self.terminal.scroll_down(offset - desired);
                }
            }
        }
        self.update_search_status();
        self.mark_content_dirty();
        self.request_redraw();
    }

    fn mark_content_dirty(&mut self) {
        self.content_dirty = true;
        self.layout_dirty = true;
    }

    fn link_under_pointer(&self) -> Option<alacritty_terminal::term::cell::Hyperlink> {
        if !self.pointer_inside || !self.focused || self.preedit.is_some() {
            return None;
        }
        let (col, row) = link_cell(
            self.mouse_position,
            &self.cell_metrics,
            self.terminal.columns(),
            self.terminal.screen_lines(),
        )?;
        let (logical_col, _) = self.renderer.logical_column(col, row);
        crate::hyperlinks::at(&self.terminal, logical_col, row)
    }

    fn refresh_link_hover(&mut self) {
        let target = self.link_under_pointer();
        let unchanged = match (&self.hovered_link, &target) {
            (Some(current), Some(target)) => current.matches(target),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            let next = target.map(crate::hyperlinks::LinkTarget::new);
            let preview = next.as_ref().map(|link| link.preview());
            if self.hovered_link.as_ref().map(|link| link.preview()) != preview
                && let Err(error) = crate::link_platform::set_hover(&self.window, preview)
            {
                log::warn!("link preview failed: {error}");
            }
            self.hovered_link = next;
        }
        let clickable = self.modifiers.super_key()
            && self.hovered_link.as_ref().is_some_and(|link| link.can_open());
        let icon = if clickable { CursorIcon::Pointer } else { CursorIcon::Text };
        if self.pointer_cursor != Some(icon) {
            self.window.set_cursor(icon);
            self.pointer_cursor = Some(icon);
        }
    }

    fn request_redraw(&mut self) {
        if self.frame_pacer.request_redraw() {
            self.window.request_redraw();
        }
    }
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
    animations: mechanic_config::theme::AnimationConfig,
    /// Allows forwarding mouse events when the terminal program requests them.
    mouse_tracking: bool,
    pending_parsers: ParseQueue<WindowId>,
}

impl App {
    pub fn new(
        config: Config,
        proxy: EventLoopProxy<UserEvent>,
        mut animations: mechanic_config::theme::AnimationConfig,
        mouse_tracking: bool,
    ) -> Self {
        animations.logo &= config.theme.logo_size > 0;
        Self {
            config,
            windows: HashMap::new(),
            proxy,
            animations,
            mouse_tracking,
            pending_parsers: ParseQueue::new(),
        }
    }

    fn make_waker(&self, window_id: WindowId) -> PtyWaker {
        make_waker_for(&self.proxy, window_id)
    }

    fn toggle_animations(&mut self) {
        self.animations = toggled_animations(self.animations, self.config.theme.logo_size > 0);
        for state in self.windows.values_mut() {
            state.focus_gain_at = None;
            state.bloom_start = None;
            state.focus_redraw_frames = 0;
            state.request_redraw();
        }
    }

    /// Remove a window and exit the event loop if no windows remain.
    fn close_window(&mut self, id: WindowId, event_loop: &ActiveEventLoop) {
        self.windows.remove(&id);
        self.pending_parsers.remove(&id);
        if self.windows.is_empty() {
            log::info!("all windows closed — exiting");
            event_loop.exit();
        }
    }

    fn pump_parser(&mut self, event_loop: &ActiveEventLoop) {
        let Some(id) = self.pending_parsers.pop() else {
            return;
        };
        let Some(state) = self.windows.get_mut(&id) else {
            return;
        };
        if state.exit_status.is_some() {
            return;
        }
        let mouse_protocol = state.terminal.mouse_protocol();
        let outcome = state.terminal.process_input();
        if state.terminal.mouse_protocol() != mouse_protocol
            || outcome.child_exit.is_some()
            || outcome.io_error.is_some()
        {
            state.scroll_accumulator.reset();
            state.last_mouse_report = None;
        }
        if outcome.child_exit.is_some() || outcome.io_error.is_some() {
            state.preedit = None;
        }
        if outcome.grid_maybe_changed {
            state.invalidate_search();
            state.mark_content_dirty();
        }

        // Fatal transport failures freeze the window even if a child exit was
        // delivered with the same final output batch.
        if let Some(error) = outcome.io_error {
            state.invalidate_search();
            log::error!("window {id:?} PTY transport failed: {error}");
            state.terminal.inject_local(
                b"\r\n\x1b[31m[terminal I/O failed; Cmd+R to restart, any key to close]\x1b[0m\r\n",
            );
            state.exit_status = Some(None);
            state.mark_content_dirty();
            state.request_redraw();
        }

        if let Some(status) = outcome.child_exit
            && state.exit_status.is_none()
        {
            let should_close = match self.config.terminal.close_on_exit {
                mechanic_config::CloseOnExitPolicy::Always => true,
                mechanic_config::CloseOnExitPolicy::Success => status.is_none_or(|s| s.success()),
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
            state.invalidate_search();
            state.exit_status = Some(status);
            state.mark_content_dirty();
            state.request_redraw();
        }

        if state.exit_status.is_some() {
            self.pending_parsers.remove(&id);
        } else if outcome.more_output {
            self.pending_parsers.enqueue(id);
        }
    }

    /// Update font metrics and resize the terminal to match.
    fn apply_font_size(state: &mut AppState, new_size: f32) {
        state.link_press.cancel();
        let new_metrics = state.renderer.set_font_size(new_size);
        state.cell_metrics = new_metrics;
        state.scroll_accumulator.reset();
        state.last_mouse_report = None;
        state.current_font_size = new_size;

        let inner = state.window.inner_size();
        let term_size = Self::terminal_size_from_metrics(inner.width, inner.height, &new_metrics);
        state.terminal.resize(term_size);
        state.invalidate_search();

        state.mark_content_dirty();
        state.request_redraw();
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
        let mut state = AppState {
            search_panel: None,
            search: crate::search::Search::default(),
            window: window.clone(),
            window_title: "Mechanic".into(),
            terminal,
            renderer,
            cell_metrics,
            mouse_position: (0.0, 0.0),
            pointer_inside: false,
            hovered_link: None,
            link_press: crate::link_input::LinkPress::default(),
            link_menu_release: false,
            pointer_cursor: None,
            mouse_pressed: false,
            held_buttons: mouse_enc::HeldButtons::default(),
            scroll_accumulator: mouse_enc::ScrollAccumulator::default(),
            mouse_press_origin: None,
            primary_selection: None,
            modifiers: ModifiersState::empty(),
            preedit: None,
            clipboard,
            start_time: now,
            focused: true,
            current_font_size: self.config.font.size,
            exit_status: None,
            last_mouse_report: None,
            content_dirty: true,
            layout_dirty: true,
            focus_redraw_frames: FOCUS_REDRAW_BURST_FRAMES,
            focus_gain_at: self.animations.logo.then_some(now),
            bloom_start: None,
            frame_pacer: FramePacer::new(now),
        };

        state.request_redraw();
        self.windows.insert(window_id, state);
        self.pending_parsers.enqueue(window_id);

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

fn logical_selection_point(
    renderer: &Renderer,
    mut point: GridPoint,
    mut side: GridSide,
    display_offset: usize,
) -> (GridPoint, GridSide) {
    let row = (point.line.0 + display_offset as i32).max(0) as usize;
    let (col, rtl) = renderer.logical_column(point.column.0, row);
    point.column = GridColumn(col);
    if rtl {
        side = match side {
            GridSide::Left => GridSide::Right,
            GridSide::Right => GridSide::Left,
        };
    }
    (point, side)
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
            if modifiers_snapshot.is_some_and(|modifiers| {
                animation_toggle_shortcut(key_event.physical_key, modifiers)
            }) {
                if !key_event.repeat {
                    self.toggle_animations();
                }
                return;
            }
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

        if state.layout_dirty
            && matches!(
                &event,
                WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::CursorMoved { .. }
                    | WindowEvent::ModifiersChanged(_)
                    | WindowEvent::CursorEntered { .. }
            )
        {
            let grid =
                crate::convert::convert_grid(&state.terminal, &self.config.theme, state.focused);
            state.renderer.prepare_layout(&grid);
            state.layout_dirty = false;
        }

        match event {
            WindowEvent::CloseRequested => {
                log::info!("window {id:?} close requested");
                self.close_window(id, event_loop);
            }

            WindowEvent::Resized(size) => {
                state.link_press.cancel();
                state.scroll_accumulator.reset();
                state.last_mouse_report = None;
                state.renderer.resize((size.width, size.height));

                let new_term_size =
                    Self::terminal_size_from_metrics(size.width, size.height, &state.cell_metrics);
                state.terminal.resize(new_term_size);
                state.invalidate_search();

                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::ModifiersChanged(mods) => {
                if state.modifiers != mods.state() {
                    state.scroll_accumulator.reset();
                    state.last_mouse_report = None;
                }
                state.modifiers = mods.state();
                state.refresh_link_hover();
            }

            WindowEvent::Focused(focused) => {
                log::debug!("window {id:?} focused: {focused}");
                state.focused = focused;
                state.mark_content_dirty();
                state.focus_redraw_frames = FOCUS_REDRAW_BURST_FRAMES;

                if focused {
                    state.focus_gain_at = self.animations.logo.then(Instant::now);
                    state.bloom_start = None;
                } else {
                    state.link_press.cancel();
                    state.preedit = None;
                    state.held_buttons.clear();
                    state.mouse_pressed = false;
                    state.mouse_press_origin = None;
                    state.last_mouse_report = None;
                    state.scroll_accumulator.reset();
                    // Cancel pending bloom; let an already committed animation finish.
                    state.focus_gain_at = None;
                }

                state.refresh_link_hover();

                state.request_redraw();
            }

            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if key_event.state == ElementState::Pressed {
                    state.link_press.cancel();
                }
                if key_event.state == ElementState::Pressed {
                    match workspace_shortcut(&key_event.logical_key, state.modifiers) {
                        Some(WorkspaceShortcut::Find) => {
                            state.show_search(&self.proxy, id);
                            return;
                        }
                        Some(WorkspaceShortcut::FindNext | WorkspaceShortcut::FindPrevious) => {
                            let backwards = state.modifiers.shift_key();
                            if !state.search.active {
                                state.show_search(&self.proxy, id);
                            }
                            state.search.navigate(&state.terminal, backwards);
                            state.reveal_search_match();
                            return;
                        }
                        Some(WorkspaceShortcut::PreviousPrompt | WorkspaceShortcut::NextPrompt) => {
                            if key_event.logical_key == Key::Named(NamedKey::ArrowUp) {
                                state.terminal.jump_to_previous_prompt();
                            } else {
                                state.terminal.jump_to_next_prompt();
                            }
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        Some(WorkspaceShortcut::CopyCommandOutput) => {
                            if let Some(output) = state.terminal.last_command_output()
                                && let Some(clipboard) = &mut state.clipboard
                                && let Err(error) = clipboard.set_text(output)
                            {
                                log::warn!("could not copy command output: {error}");
                            }
                            return;
                        }
                        None => {}
                    }
                }
                if state.exit_status.is_some() && key_event.state == ElementState::Pressed {
                    let key = &key_event.logical_key;
                    let mods = state.modifiers;

                    if mods.super_key() && matches!(key, Key::Character(c) if c.as_str() == "r") {
                        let waker = make_waker_for(&self.proxy, id);
                        respawn_shell(state, &self.config, id, waker);
                        if state.exit_status.is_none() {
                            self.pending_parsers.enqueue(id);
                        }
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
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        CmdShortcut::ClearScrollback => {
                            state.terminal.clear_history();
                            state.invalidate_search();
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        CmdShortcut::SelectAll => {
                            state.terminal.select_all();
                            state.mark_content_dirty();
                            state.request_redraw();
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
                            state.mark_content_dirty();
                            state.request_redraw();
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
                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::Ime(ime_event) => {
                match ime_event {
                    Ime::Commit(text) => {
                        state.preedit = None;
                        if let Err(e) = state.terminal.write_to_pty(text.as_bytes()) {
                            log::warn!("PTY IME commit failed: {e}");
                        }
                    }
                    Ime::Preedit(text, cursor) => {
                        state.preedit = crate::preedit::Preedit::new(text, cursor);
                        let mut grid = crate::convert::convert_grid(
                            &state.terminal,
                            &self.config.theme,
                            state.focused && state.preedit.is_none(),
                        );
                        if let Some(preedit) = &state.preedit {
                            preedit.overlay(&mut grid, &self.config.theme);
                        }
                        state.renderer.prepare_layout(&grid);
                        let (cx, cy) = grid.cursor_position;
                        let cw = state.cell_metrics.cell_width;
                        let ch = state.cell_metrics.cell_height;
                        if cx < grid.cols && cy < grid.rows {
                            let px = state.renderer.visual_column(cx, cy) as f64 * cw as f64;
                            let py = cy as f64 * ch as f64;
                            state.window.set_ime_cursor_area(
                                winit::dpi::PhysicalPosition::new(px, py),
                                winit::dpi::PhysicalSize::new(cw as f64, ch as f64),
                            );
                        }
                    }
                    Ime::Disabled => state.preedit = None,
                    Ime::Enabled => {}
                }
                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::MouseInput { state: btn_state, button: win_button, .. } => {
                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                state.refresh_link_hover();
                if win_button == MouseButton::Left && btn_state == ElementState::Pressed {
                    state.link_press = crate::link_input::LinkPress::default();
                } else if btn_state == ElementState::Pressed {
                    state.link_press.cancel();
                }
                if win_button == MouseButton::Left
                    && btn_state == ElementState::Released
                    && state.link_press.active()
                {
                    state.link_press.moved(state.mouse_position);
                    let current = state.link_under_pointer();
                    if let Some(target) = state.link_press.release(current.as_ref()) {
                        open_link(&crate::hyperlinks::LinkTarget::new(target));
                    }
                    return;
                }
                if win_button == MouseButton::Right {
                    if btn_state == ElementState::Released && state.link_menu_release {
                        state.link_menu_release = false;
                        return;
                    }
                    if btn_state == ElementState::Pressed {
                        state.link_menu_release = false;
                    }
                }
                if btn_state == ElementState::Pressed
                    && let Some(link) = state.link_under_pointer()
                {
                    let target = crate::hyperlinks::LinkTarget::new(link.clone());
                    if win_button == MouseButton::Left
                        && crate::link_input::opens_link(
                            state.modifiers.super_key(),
                            target.can_open(),
                        )
                    {
                        state.link_press.begin(link, state.mouse_position);
                        state.mouse_pressed = false;
                        state.mouse_press_origin = None;
                        return;
                    }
                    if win_button == MouseButton::Right
                        && crate::link_input::shows_menu(
                            state.modifiers.super_key(),
                            route.is_some(),
                        )
                    {
                        state.link_menu_release = true;
                        // Keep the clicked target while AppKit runs its menu loop.
                        match crate::link_platform::context_menu(
                            &state.window,
                            state.mouse_position,
                            target.can_open(),
                        ) {
                            Some(crate::link_platform::LinkAction::Open) => open_link(&target),
                            Some(crate::link_platform::LinkAction::Copy) => {
                                if let Some(clipboard) = &mut state.clipboard
                                    && let Err(error) = clipboard.set_text(target.uri().to_owned())
                                {
                                    log::warn!("copy link failed: {error}");
                                }
                            }
                            None => {}
                        }
                        return;
                    }
                }
                if let Some(button) = winit_to_mouse_button(win_button) {
                    state.held_buttons.update(button, btn_state == ElementState::Pressed);
                    state.last_mouse_report = None;
                }

                if let Some(sgr) = route {
                    if let Some(btn) = winit_to_mouse_button(win_button) {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        let col =
                            state.renderer.logical_column((col - 1) as usize, (row - 1) as usize).0
                                as u32
                                + 1;
                        let kind = match btn_state {
                            ElementState::Pressed => mouse_enc::MouseEventKind::Press,
                            ElementState::Released => mouse_enc::MouseEventKind::Release,
                        };
                        let bytes = mouse_enc::encode(sgr, btn, state.modifiers, kind, col, row);
                        if let Err(e) = state.terminal.write_to_pty(&bytes) {
                            log::warn!("PTY mouse write failed: {e}");
                        }
                        state.mouse_pressed = false;
                        state.mouse_press_origin = None;
                    }
                    if matches!(btn_state, ElementState::Released) {
                        state.last_mouse_report = None;
                    }
                    state.mark_content_dirty();
                    state.request_redraw();
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
                        let (point, side) =
                            logical_selection_point(&state.renderer, point, side, display_offset);

                        match btn_state {
                            ElementState::Pressed => {
                                state.mouse_pressed = true;
                                state.mouse_press_origin = Some((x, y));
                                state.terminal.start_selection(point, side);
                            }
                            ElementState::Released => {
                                state.mouse_pressed = false;
                                if state.mouse_press_origin.is_none() {
                                    return;
                                }
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
                        state.mark_content_dirty();
                        state.request_redraw();
                    }

                    (ElementState::Pressed, MouseButton::Middle) => {
                        if let Some(text) = state.primary_selection.as_ref() {
                            if let Err(e) = state.terminal.paste(text) {
                                log::warn!("PTY middle-click paste failed: {e}");
                            }
                            state.mark_content_dirty();
                            state.request_redraw();
                        }
                    }

                    _ => {}
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_position = (position.x, position.y);
                state.pointer_inside = true;
                state.link_press.moved(state.mouse_position);
                state.refresh_link_hover();
                if state.link_press.active() {
                    return;
                }

                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );

                if let Some(sgr) = route {
                    let proto = state.terminal.mouse_protocol();
                    if let Some(btn) =
                        state.held_buttons.report_button(proto.report_motion, proto.report_drag)
                    {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        let col =
                            state.renderer.logical_column((col - 1) as usize, (row - 1) as usize).0
                                as u32
                                + 1;
                        if state.last_mouse_report != Some((col, row, btn)) {
                            state.last_mouse_report = Some((col, row, btn));
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

                state.last_mouse_report = None;

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
                    let (point, side) =
                        logical_selection_point(&state.renderer, point, side, display_offset);
                    state.terminal.update_selection(point, side);
                    state.mark_content_dirty();
                    state.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, phase, .. } => {
                state.link_press.cancel();
                let route = route_mouse(
                    state.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.exit_status.is_some(),
                );
                let lines = state.scroll_accumulator.lines(
                    delta,
                    phase,
                    route,
                    state.modifiers,
                    state.cell_metrics.cell_height,
                );
                if lines == 0 {
                    return;
                }

                if let Some(sgr) = route {
                    if lines != 0 {
                        let (col, row) = grid_coords_1based(
                            state.mouse_position,
                            &state.cell_metrics,
                            state.terminal.columns(),
                            state.terminal.screen_lines(),
                        );
                        let col =
                            state.renderer.logical_column((col - 1) as usize, (row - 1) as usize).0
                                as u32
                                + 1;
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
                    state.mark_content_dirty();
                    state.request_redraw();
                    return;
                }

                if lines > 0 {
                    state.terminal.scroll_up(lines as usize);
                } else if lines < 0 {
                    state.terminal.scroll_down(lines.unsigned_abs() as usize);
                }
                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                if state.frame_pacer.is_occluded() {
                    return;
                }
                render_frame(state, &self.config, self.animations);
                // Renderer reports no presentation result. Pace from the end
                // of each render attempt, including a skipped surface frame.
                state.frame_pacer.rendered(Instant::now());
                state.focus_redraw_frames = state.focus_redraw_frames.saturating_sub(1);
            }

            WindowEvent::Occluded(occluded) => {
                state.frame_pacer.set_occluded(occluded);
                if !occluded {
                    // An outstanding request may have been suppressed while hidden.
                    state.request_redraw();
                }
            }

            WindowEvent::CursorEntered { .. } => {
                state.pointer_inside = true;
                state.refresh_link_hover();
            }

            WindowEvent::CursorLeft { .. } => {
                state.pointer_inside = false;
                state.link_press.cancel();
                state.refresh_link_hover();
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // One bounded parse batch globally per turn, independent of rendering.
        // Requeue continuations rather than self-posting user events: macOS
        // drains those events before it can dispatch native input events.
        self.pump_parser(event_loop);
        let now = Instant::now();
        let mut earliest_deadline: Option<Instant> = None;

        for state in self.windows.values_mut() {
            let input = AnimationInputs {
                is_alive: state.exit_status.is_none(),
                focused: state.focused,
                focus_redraw_frames: state.focus_redraw_frames,
                bloom_start: state.bloom_start,
            };
            let anim = classify_animation(
                input,
                self.animations.enabled(),
                state.frame_pacer.last_render(),
            );
            let mut animation_deadline = match anim {
                AnimationState::Active { next_frame } => Some(next_frame),
                AnimationState::Idle => None,
            };
            if self.animations.logo
                && state.exit_status.is_none()
                && let Some(gain) = state.focus_gain_at
            {
                let dwell = Duration::from_millis(self.config.theme.opacity.bloom_dwell_ms as u64);
                merge_deadline(&mut animation_deadline, gain + dwell);
            }
            match state.frame_pacer.schedule(now, state.content_dirty, animation_deadline) {
                FrameSchedule::Redraw => state.window.request_redraw(),
                FrameSchedule::WaitUntil(deadline) => {
                    merge_deadline(&mut earliest_deadline, deadline);
                }
                FrameSchedule::Idle => {}
            }
        }

        event_loop.set_control_flow(if !self.pending_parsers.is_empty() {
            ControlFlow::Poll
        } else {
            match earliest_deadline {
                Some(t) => ControlFlow::WaitUntil(t),
                None => ControlFlow::Wait,
            }
        });
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Search(id, action) => {
                let Some(state) = self.windows.get_mut(&id) else {
                    return;
                };
                if !state.search.active {
                    return;
                }
                match action {
                    crate::search_platform::SearchAction::Query(query) => {
                        state.search.set_query(query, &state.terminal)
                    }
                    crate::search_platform::SearchAction::CaseSensitive(enabled) => {
                        state.search.set_case_sensitive(enabled, &state.terminal)
                    }
                    crate::search_platform::SearchAction::Next => {
                        state.search.navigate(&state.terminal, false)
                    }
                    crate::search_platform::SearchAction::Previous => {
                        state.search.navigate(&state.terminal, true)
                    }
                    crate::search_platform::SearchAction::Close => {
                        state.search.active = false;
                        if let Some(panel) = &state.search_panel {
                            panel.close();
                        }
                        state.window.focus_window();
                    }
                }
                if state.search.active {
                    state.reveal_search_match();
                } else {
                    state.mark_content_dirty();
                    state.request_redraw();
                }
            }
            UserEvent::PtyOutput(id) => {
                if let Some(state) = self.windows.get(&id)
                    && state.exit_status.is_none()
                {
                    self.pending_parsers.enqueue(id);
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

fn animation_toggle_shortcut(key: PhysicalKey, modifiers: ModifiersState) -> bool {
    key == PhysicalKey::Code(KeyCode::KeyA)
        && modifiers == (ModifiersState::SUPER | ModifiersState::SHIFT)
}

#[derive(Debug, PartialEq, Eq)]
enum WorkspaceShortcut {
    Find,
    FindNext,
    FindPrevious,
    PreviousPrompt,
    NextPrompt,
    CopyCommandOutput,
}

fn workspace_shortcut(key: &Key, modifiers: ModifiersState) -> Option<WorkspaceShortcut> {
    if modifiers == ModifiersState::SUPER {
        return match key {
            Key::Character(c) if c.eq_ignore_ascii_case("f") => Some(WorkspaceShortcut::Find),
            Key::Character(c) if c.eq_ignore_ascii_case("g") => Some(WorkspaceShortcut::FindNext),
            _ => None,
        };
    }
    if modifiers == (ModifiersState::SUPER | ModifiersState::SHIFT) {
        return match key {
            Key::Character(c) if c.eq_ignore_ascii_case("g") => {
                Some(WorkspaceShortcut::FindPrevious)
            }
            Key::Character(c) if c.eq_ignore_ascii_case("c") => {
                Some(WorkspaceShortcut::CopyCommandOutput)
            }
            Key::Named(NamedKey::ArrowUp) => Some(WorkspaceShortcut::PreviousPrompt),
            Key::Named(NamedKey::ArrowDown) => Some(WorkspaceShortcut::NextPrompt),
            _ => None,
        };
    }
    None
}

fn shell_window_title(
    title: &str,
    cwd: Option<&str>,
    running: bool,
    status: Option<i32>,
) -> String {
    let mut title = if title.is_empty() { "Mechanic".to_owned() } else { title.to_owned() };
    if let Some(cwd) = cwd {
        title.push_str(" — ");
        title.extend(cwd.chars().filter(|c| !c.is_control()).take(256));
    }
    if running {
        title.push_str(" [running]");
    } else if let Some(status) = status {
        if status == 0 {
            title.push_str(" [ok]");
        } else {
            title.push_str(&format!(" [exit {status}]"));
        }
    }
    title
}

fn toggled_animations(
    current: mechanic_config::theme::AnimationConfig,
    logo_visible: bool,
) -> mechanic_config::theme::AnimationConfig {
    let enabled = !current.enabled();
    mechanic_config::theme::AnimationConfig { background: enabled, logo: enabled && logo_visible }
}

/// Render one frame for `state` using the current focus / grid state.
fn render_frame(
    state: &mut AppState,
    config: &Config,
    animations: mechanic_config::theme::AnimationConfig,
) {
    let now = Instant::now();

    let dwell = Duration::from_millis(config.theme.opacity.bloom_dwell_ms as u64);
    let duration = Duration::from_millis(config.theme.opacity.bloom_duration_ms as u64);
    if animations.logo
        && let Some(start) = maybe_commit_bloom(state.focus_gain_at, state.bloom_start, dwell, now)
    {
        state.bloom_start = Some(start);
        state.focus_gain_at = None;
    }
    let bloom_progress = if animations.logo {
        compute_bloom_progress(state.bloom_start, duration, now)
    } else {
        0.0
    };

    let opacity = opacity_for_focus(state.focused, &config.theme.opacity);
    let text_opacity = text_opacity_for_focus(state.focused, &config.theme.opacity);

    let time = state.start_time.elapsed().as_secs_f32();

    let uniforms = FrameUniforms {
        logo_size: config.theme.logo_size,
        content_opacity: opacity,
        text_opacity,
        time,
        animate_background: state.focused && animations.background,
        animate_logo: state.focused && animations.logo,
        window_focused: state.focused,
        bloom_progress,
        bloom_peak_multiplier: config.theme.opacity.bloom_peak_multiplier,
    };

    let did_animation_render = !state.content_dirty && state.renderer.render_animation(uniforms);

    // A missing cached frame also requires a full render, even when content is clean.
    if !did_animation_render {
        let conversion_started =
            log::log_enabled!(target: "mechanic_render_profile", log::Level::Trace)
                .then(Instant::now);
        let mut grid = crate::convert::convert_grid(
            &state.terminal,
            &config.theme,
            state.focused && state.preedit.is_none(),
        );
        state.search.highlight(&mut grid, &state.terminal, &config.theme);
        if let Some(preedit) = &state.preedit {
            preedit.overlay(&mut grid, &config.theme);
        }
        if let Some(started) = conversion_started {
            let conversion_ns = started.elapsed().as_nanos();
            log::trace!(target: "mechanic_render_profile",
                "render-profile conversion_ns={conversion_ns} cols={} rows={}",
                grid.cols, grid.rows,
            );
        }
        state.content_dirty = !state.renderer.render(&grid, uniforms);
        state.layout_dirty = false;
        state.refresh_link_hover();
    }

    if let Some(t) = state.bloom_start
        && now.saturating_duration_since(t) >= duration
    {
        state.bloom_start = None;
    }

    let shell = state.terminal.shell_integration();
    let shell_running = state.exit_status.is_none() && shell.is_running();
    let integrated_title = shell_window_title(
        state.terminal.title(),
        shell.cwd(),
        shell_running,
        state.exit_status.is_none().then(|| shell.last_exit_status()).flatten(),
    );
    let base_title = integrated_title.as_str();
    let base = if base_title.is_empty() { "Mechanic" } else { base_title };
    let title_string = match state.exit_status {
        Some(status) => format!("{base} — {}", format_title_suffix(status)),
        None => base.to_string(),
    };
    if title_string != state.window_title {
        state.window.set_title(&title_string);
        state.window_title = title_string;
    }
}

fn open_link(target: &crate::hyperlinks::LinkTarget) {
    if let Some(url) = target.web_url()
        && let Err(error) = crate::link_platform::open_web_url(url)
    {
        log::warn!("open link failed: {error}");
    }
}

/// Link hits exclude window padding instead of clamping to the nearest cell.
fn link_cell(
    (x, y): (f64, f64),
    metrics: &CellMetrics,
    cols: usize,
    rows: usize,
) -> Option<(usize, usize)> {
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || !metrics.cell_width.is_finite()
        || !metrics.cell_height.is_finite()
        || metrics.cell_width <= 0.0
        || metrics.cell_height <= 0.0
    {
        return None;
    }
    let col = (x / f64::from(metrics.cell_width)).floor() as usize;
    let row = (y / f64::from(metrics.cell_height)).floor() as usize;
    (col < cols && row < rows).then_some((col, row))
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
    /// Window has active animation; its next frame is due at `next_frame`.
    Active { next_frame: Instant },
    /// Sleep until input or PTY output arrives.
    Idle,
}

/// Window state used by the animation scheduler.
#[derive(Debug, Clone, Copy)]
struct AnimationInputs {
    is_alive: bool,
    focused: bool,
    /// Forced redraws remaining after a focus change, independent of animation settings.
    focus_redraw_frames: u8,
    /// Keep scheduling until a final frame clears even an expired bloom.
    bloom_start: Option<Instant>,
}

/// Frozen windows are idle; focus bursts, bloom, and enabled effects need frames.
fn classify_animation(
    input: AnimationInputs,
    animations_enabled: bool,
    now: Instant,
) -> AnimationState {
    if !input.is_alive {
        return AnimationState::Idle;
    }
    if input.focus_redraw_frames > 0 {
        return AnimationState::Active { next_frame: now + FRAME_INTERVAL };
    }
    if animations_enabled && input.bloom_start.is_some() {
        return AnimationState::Active { next_frame: now + FRAME_INTERVAL };
    }
    if input.focused && animations_enabled {
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
            state.invalidate_search();
            state.terminal = new_term;
            state.link_press.cancel();
            state.preedit = None;
            state.held_buttons.clear();
            state.scroll_accumulator.reset();
            state.last_mouse_report = None;
            state.mouse_pressed = false;
            state.mouse_press_origin = None;
            state.exit_status = None;
            state.mark_content_dirty();
            let occluded = state.frame_pacer.is_occluded();
            state.frame_pacer = FramePacer::new(Instant::now());
            state.frame_pacer.set_occluded(occluded);
            state.request_redraw();
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

    #[test]
    fn workspace_shortcuts_do_not_capture_shell_keys() {
        for key in
            [Key::Character("f".into()), Key::Character("g".into()), Key::Named(NamedKey::ArrowUp)]
        {
            for modifiers in [ModifiersState::empty(), ModifiersState::CONTROL, ModifiersState::ALT]
            {
                assert_eq!(workspace_shortcut(&key, modifiers), None);
            }
        }
        let shifted = ModifiersState::SUPER | ModifiersState::SHIFT;
        assert_eq!(
            workspace_shortcut(&Key::Character("f".into()), ModifiersState::SUPER),
            Some(WorkspaceShortcut::Find)
        );
        assert_eq!(
            workspace_shortcut(&Key::Character("G".into()), shifted),
            Some(WorkspaceShortcut::FindPrevious)
        );
        assert_eq!(
            workspace_shortcut(&Key::Named(NamedKey::ArrowUp), shifted),
            Some(WorkspaceShortcut::PreviousPrompt)
        );
        assert_eq!(
            workspace_shortcut(&Key::Character("C".into()), shifted),
            Some(WorkspaceShortcut::CopyCommandOutput)
        );
        assert_eq!(workspace_shortcut(&Key::Character("c".into()), ModifiersState::SUPER), None);
    }

    #[test]
    fn shell_titles_distinguish_running_completion_and_absent_metadata() {
        assert_eq!(shell_window_title("", None, false, None), "Mechanic");
        assert_eq!(
            shell_window_title("vim", Some("/tmp/café"), true, Some(2)),
            "vim — /tmp/café [running]"
        );
        assert_eq!(shell_window_title("", Some("/tmp"), false, Some(0)), "Mechanic — /tmp [ok]");
        assert_eq!(shell_window_title("", None, false, Some(7)), "Mechanic [exit 7]");
        assert!(!shell_window_title("", Some("/tmp\nfoo"), false, None).contains('\n'));
    }

    #[test]
    fn animation_toggle_does_not_take_select_all_or_extra_modifiers() {
        let key = PhysicalKey::Code(KeyCode::KeyA);
        let chord = ModifiersState::SUPER | ModifiersState::SHIFT;
        assert!(animation_toggle_shortcut(key, chord));
        assert!(!animation_toggle_shortcut(key, ModifiersState::SUPER));
        assert!(!animation_toggle_shortcut(key, chord | ModifiersState::ALT));
        assert!(!animation_toggle_shortcut(key, chord | ModifiersState::CONTROL));
        assert!(!animation_toggle_shortcut(PhysicalKey::Code(KeyCode::KeyB), chord));
        assert_eq!(cmd_shortcut("a"), Some(CmdShortcut::SelectAll));
    }

    #[test]
    fn animation_switch_unifies_partial_states_and_returns_to_idle() {
        use mechanic_config::theme::AnimationConfig;
        let on = toggled_animations(AnimationConfig::default(), true);
        assert!(on.logo && on.background);
        for current in [
            on,
            AnimationConfig { logo: true, background: false },
            AnimationConfig { logo: false, background: true },
        ] {
            let off = toggled_animations(current, true);
            assert!(!off.logo && !off.background);
            let now = Instant::now();
            assert_eq!(
                classify_animation(inputs(true, true), off.enabled(), now),
                AnimationState::Idle
            );
            let mut pacer = FramePacer::new(now);
            pacer.request_redraw();
            pacer.rendered(now);
            assert_eq!(pacer.schedule(now, false, None), FrameSchedule::Idle);
        }
        let hidden_logo = toggled_animations(AnimationConfig::default(), false);
        assert!(!hidden_logo.logo && hidden_logo.background);
        assert!(!toggled_animations(hidden_logo, false).enabled());
    }

    #[test]
    fn hyperlink_hits_exclude_padding_and_invalid_coordinates() {
        let metrics = CellMetrics { cell_width: 10.0, cell_height: 20.0, ascent: 15.0 };
        assert_eq!(link_cell((19.9, 39.9), &metrics, 2, 2), Some((1, 1)));
        for point in [(20.0, 1.0), (1.0, 40.0), (-0.1, 0.0), (f64::NAN, 0.0)] {
            assert!(link_cell(point, &metrics, 2, 2).is_none());
        }
        assert!(link_cell((0.0, 0.0), &metrics, 0, 0).is_none());
    }

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
        AnimationInputs { is_alive, focused, focus_redraw_frames: 0, bloom_start: None }
    }

    fn inputs_with_burst(
        is_alive: bool,
        focused: bool,
        focus_redraw_frames: u8,
    ) -> AnimationInputs {
        AnimationInputs { is_alive, focused, focus_redraw_frames, bloom_start: None }
    }

    fn inputs_with_bloom(is_alive: bool, focused: bool) -> AnimationInputs {
        AnimationInputs {
            is_alive,
            focused,
            focus_redraw_frames: 0,
            bloom_start: Some(Instant::now()),
        }
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
    fn anim_focused_with_animations_disabled_is_idle() {
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
        match classify_animation(inputs_with_bloom(true, true), true, now) {
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
            classify_animation(inputs_with_bloom(true, false), true, Instant::now()),
            AnimationState::Active { .. }
        ));
    }

    #[test]
    fn expired_bloom_keeps_a_final_frame_scheduled_until_cleared() {
        let now = Instant::now();
        let last_render = now - Duration::from_millis(20);
        let mut input = inputs(true, false);
        input.bloom_start = Some(now - Duration::from_secs(1));
        assert_eq!(compute_bloom_progress(input.bloom_start, Duration::from_millis(100), now), 1.0,);
        let AnimationState::Active { next_frame } = classify_animation(input, true, last_render)
        else {
            panic!("expired bloom still needs a frame to restore baseline brightness");
        };
        let mut pacer = FramePacer::new(last_render);
        assert_eq!(
            pacer.schedule(now, false, Some(next_frame)),
            FrameSchedule::WaitUntil(next_frame),
        );
        assert_eq!(pacer.schedule(next_frame, false, Some(next_frame)), FrameSchedule::Redraw);
        // render_frame clears the completed animation after drawing progress1.
        input.bloom_start = None;
        pacer.rendered(next_frame);
        assert_eq!(classify_animation(input, false, next_frame), AnimationState::Idle);
        assert_eq!(pacer.schedule(next_frame, false, None), FrameSchedule::Idle);
    }

    #[test]
    fn disabled_animations_ignore_bloom_state() {
        assert_eq!(
            classify_animation(inputs_with_bloom(true, true), false, Instant::now()),
            AnimationState::Idle
        );
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
