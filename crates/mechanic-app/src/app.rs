//! Main application state and winit event-loop integration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mechanic_config::Config;
use mechanic_core::{
    GridColumn, GridLine, GridPoint, GridSide, MouseProtocol, PtyWaker, Terminal, TerminalSize,
};
use mechanic_renderer::{
    CellMetrics, FrameUniforms, RenderDivider, RenderGrid, RenderPane, RenderPaneHandle, Renderer,
};

use crate::notifications::{CompletionNotification, CompletionScope, NotificationPolicy};
use crate::notifications_platform::NativeNotifications;
use crate::panes::{Axis, Direction, Hit, Layout, PaneId, PaneTree, Rect, Size};

use crate::mouse as mouse_enc;
use crate::scheduling::{FramePacer, FrameSchedule, ParseQueue};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};

#[path = "app_control.rs"]
mod app_control;
#[path = "app_pane_drag.rs"]
mod app_pane_drag;
#[path = "app_pane_move.rs"]
mod app_pane_move;
#[path = "app_session.rs"]
mod app_session;

/// Target interval between animation frames (~30 FPS).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Events the main event loop receives from other threads.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Wake this window to drain queued PTY output.
    PtyOutput(WindowId, PaneId, u64),
    Search(WindowId, PaneId, u64, crate::search_platform::SearchAction),
    NotificationReady(CompletionNotification),
    Control(crate::control::ControlEvent),
}

/// Independent terminal state retained when focus or pointer routing changes.
struct PaneState {
    search_panel: Option<crate::search_platform::SearchPanel>,
    search: crate::search::Search,
    terminal: Terminal,
    session: u64,
    directory: Option<std::path::PathBuf>,
    directory_metadata: (Option<String>, Option<String>),
    completions: std::collections::VecDeque<mechanic_core::CommandCompletion>,
    cached_grid: Option<RenderGrid>,
    /// Current physical mouse cursor position in pixels.
    mouse_position: (f64, f64),
    pointer_inside: bool,
    hovered_link: Option<crate::hyperlinks::LinkTarget>,
    link_press: crate::link_input::LinkPress,
    link_menu_release: bool,
    mouse_pressed: bool,
    held_buttons: mouse_enc::HeldButtons,
    scroll_accumulator: mouse_enc::ScrollAccumulator,
    /// Press position in physical pixels, used to distinguish clicks from drags.
    mouse_press_origin: Option<(f64, f64)>,
    /// Last drag selection, pasted by middle-click independently of the clipboard.
    primary_selection: Option<String>,
    preedit: Option<crate::preedit::Preedit>,
    exit_status: Option<Option<std::process::ExitStatus>>,
    /// Last reported mouse cell, used to deduplicate motion events.
    last_mouse_report: Option<(u32, u32, mouse_enc::MouseButton)>,
    content_dirty: bool,
    layout_dirty: bool,
}

impl PaneState {
    fn new(terminal: Terminal, session: u64) -> Self {
        Self {
            terminal,
            session,
            directory: None,
            directory_metadata: (None, None),
            completions: std::collections::VecDeque::new(),
            cached_grid: None,
            search_panel: None,
            search: crate::search::Search::default(),
            mouse_position: (0.0, 0.0),
            pointer_inside: false,
            hovered_link: None,
            link_press: crate::link_input::LinkPress::default(),
            link_menu_release: false,
            mouse_pressed: false,
            held_buttons: mouse_enc::HeldButtons::default(),
            scroll_accumulator: mouse_enc::ScrollAccumulator::default(),
            mouse_press_origin: None,
            primary_selection: None,
            preedit: None,
            exit_status: None,
            last_mouse_report: None,
            content_dirty: true,
            layout_dirty: true,
        }
    }
}

/// One native window, renderer, and pane tree. The loaded pane is swapped as a
/// whole for event dispatch; the tree's active ID remains keyboard focus.
struct AppState {
    window: Arc<Window>,
    window_title: String,
    renderer: Renderer,
    cell_metrics: CellMetrics,
    pane: PaneState,
    loaded_pane: PaneId,
    other_panes: HashMap<PaneId, PaneState>,
    tree: PaneTree,
    layout: Layout,
    divider_drag: Option<u64>,
    divider_hover: Option<u64>,
    pane_drag_visual: app_pane_drag::PaneDragVisual,
    captured_pane: Option<PaneId>,
    pointer_position: (f64, f64),
    pointer_inside: bool,
    pointer_cursor: Option<CursorIcon>,
    hovered_preview: Option<String>,
    modifiers: ModifiersState,
    cancelled_ime_commit: bool,
    clipboard: Option<arboard::Clipboard>,
    /// Instant when this window was created (used to compute the `time` uniform).
    start_time: std::time::Instant,
    /// Keyboard focus, which selects active or idle opacity.
    focused: bool,
    /// Live font size in points for incremental zoom shortcuts.
    current_font_size: f32,
    /// Rebuild cell instances when true; otherwise reuse the cached frame.
    content_dirty: bool,
    /// Forced frames after focus changes, to accommodate AppKit redraw coalescing.
    focus_redraw_frames: u8,
    /// Pending focus-gain time; cleared on focus loss or bloom commitment.
    focus_gain_at: Option<Instant>,
    /// Start of the committed bloom; cleared after its configured duration.
    bloom_start: Option<Instant>,
    frame_pacer: FramePacer,
}

impl AppState {
    fn pane_state(&self, id: PaneId) -> Option<&PaneState> {
        if self.loaded_pane == id { Some(&self.pane) } else { self.other_panes.get(&id) }
    }

    fn load_pane(&mut self, id: PaneId) -> bool {
        if self.loaded_pane == id {
            return true;
        }
        let Some(mut pane) = self.other_panes.remove(&id) else {
            return false;
        };
        std::mem::swap(&mut pane, &mut self.pane);
        self.other_panes.insert(self.loaded_pane, pane);
        self.loaded_pane = id;
        true
    }

    fn focus_pane(&mut self, id: PaneId) {
        if self.tree.active() == id {
            return;
        }
        self.load_pane(self.tree.active());
        self.cancel_preedit();
        self.pane.link_press.cancel();
        if let Some(panel) = &self.pane.search_panel {
            panel.close();
        }
        self.mark_content_dirty();
        if self.tree.focus(id) {
            self.load_pane(id);
            self.mark_content_dirty();
            self.request_redraw();
        }
    }

    fn cancel_preedit(&mut self) {
        if self.pane.preedit.take().is_some() {
            // End native marked text before changing the input destination. A
            // queued commit from that cancelled composition is discarded until
            // the native Disabled/Enabled boundary or a new preedit arrives.
            self.cancelled_ime_commit = true;
            self.window.set_ime_allowed(false);
            self.window.set_ime_allowed(true);
        }
    }

    fn has_live_pane(&self) -> bool {
        self.pane.exit_status.is_none()
            || self.other_panes.values().any(|p| p.exit_status.is_none())
    }

    fn minimum_pane_size(&self) -> Size {
        Size {
            width: (self.cell_metrics.cell_width * 8.0).ceil().max(1.0) as u32,
            height: (self.cell_metrics.cell_height * 3.0).ceil().max(1.0) as u32
                + self.pane_header_height(),
        }
    }

    fn resize_panes(&mut self) {
        let size = self.window.inner_size();
        self.layout = self.tree.layout(
            Rect { x: 0, y: 0, width: size.width, height: size.height },
            self.minimum_pane_size(),
            (6.0 * self.window.scale_factor()).round().max(1.0) as u32,
        );
        let active = self.tree.active();
        for item in self.layout.panes.clone() {
            self.load_pane(item.id);
            let content = self.pane_content_rect(item.id).unwrap();
            self.pane.link_press.cancel();
            self.pane.scroll_accumulator.reset();
            self.pane.last_mouse_report = None;
            self.pane.terminal.resize(App::terminal_size_from_metrics(
                content.width,
                content.height,
                &self.cell_metrics,
            ));
            self.invalidate_search();
            self.mark_content_dirty();
        }
        self.load_pane(active);
        self.refresh_pointer_hover();
    }

    fn invalidate_search(&mut self) {
        if self.pane.search.invalidate() {
            self.update_search_status();
        }
    }

    fn update_search_status(&self) {
        if self.pane.search.active
            && let Some(panel) = &self.pane.search_panel
        {
            panel.set_status(&self.pane.search.status());
        }
    }

    fn show_search(&mut self, proxy: &EventLoopProxy<UserEvent>, id: WindowId) {
        if self.pane.search_panel.is_none() {
            let proxy = proxy.clone();
            let pane_id = self.loaded_pane;
            let session = self.pane.session;
            match crate::search_platform::SearchPanel::new(&self.window, move |action| {
                let _ = proxy.send_event(UserEvent::Search(id, pane_id, session, action));
            }) {
                Ok(panel) => self.pane.search_panel = Some(panel),
                Err(error) => {
                    log::warn!("could not open search: {error}");
                    return;
                }
            }
        }
        self.cancel_preedit();
        self.pane.link_press.cancel();
        self.pane.search.active = true;
        self.update_search_status();
        self.pane.search_panel.as_ref().unwrap().show();
        self.mark_content_dirty();
        self.request_redraw();
    }

    fn reveal_search_match(&mut self) {
        if let Some(hit) = self.pane.search.current() {
            let offset = self.pane.terminal.grid().display_offset();
            let top = -(offset as i32);
            let bottom = top + self.pane.terminal.screen_lines() as i32 - 1;
            if hit.start.line.0 < top || hit.end.line.0 > bottom {
                let desired = (-hit.start.line.0).max(0) as usize;
                if desired > offset {
                    self.pane.terminal.scroll_up(desired - offset);
                } else {
                    self.pane.terminal.scroll_down(offset - desired);
                }
            }
        }
        self.update_search_status();
        self.mark_content_dirty();
        self.request_redraw();
    }

    fn mark_content_dirty(&mut self) {
        self.content_dirty = true;
        self.pane.content_dirty = true;
        self.pane.layout_dirty = true;
    }

    fn link_under_pointer(&self) -> Option<alacritty_terminal::term::cell::Hyperlink> {
        if !self.pane.pointer_inside || !self.focused || self.pane.preedit.is_some() {
            return None;
        }
        let (col, row) = link_cell(
            self.pane.mouse_position,
            &self.cell_metrics,
            self.pane.terminal.columns(),
            self.pane.terminal.screen_lines(),
        )?;
        let (logical_col, _) = self.renderer.pane_logical_column(self.loaded_pane, col, row);
        crate::hyperlinks::at(&self.pane.terminal, logical_col, row)
    }

    fn set_divider_hover(&mut self, divider: Option<u64>) {
        if self.divider_hover != divider {
            self.divider_hover = divider;
            self.content_dirty = true;
            self.request_redraw();
        }
    }

    fn refresh_pointer_hover(&mut self) {
        if self.refresh_pane_handle_hover() {
            return;
        }
        let hit = self
            .pointer_inside
            .then(|| {
                self.captured_pane.map(Hit::Pane).or_else(|| {
                    self.layout.hit_test(self.pointer_position.0, self.pointer_position.1)
                })
            })
            .flatten();
        self.set_divider_hover(match hit {
            Some(Hit::Divider(id)) => Some(id),
            _ => None,
        });
        if let Some(Hit::Pane(id)) = hit {
            let loaded = self.loaded_pane;
            self.load_pane(id);
            if let Some(rect) = self.pane_content_rect(id) {
                self.pane.mouse_position = pane_local_position(rect, self.pointer_position);
                self.pane.pointer_inside = true;
            }
            self.refresh_link_hover();
            self.load_pane(loaded);
        } else if self.divider_hover.is_some() || self.divider_drag.is_some() {
            self.refresh_link_hover();
        } else {
            if self.hovered_preview.take().is_some() {
                let _ = crate::link_platform::set_hover(&self.window, None);
            }
            if self.pointer_cursor != Some(CursorIcon::Text) {
                self.window.set_cursor(CursorIcon::Text);
                self.pointer_cursor = Some(CursorIcon::Text);
            }
        }
    }

    fn refresh_link_hover(&mut self) {
        if self.refresh_pane_handle_cursor() {
            return;
        }
        if let Some(divider) = self
            .divider_drag
            .or(self.divider_hover)
            .and_then(|id| self.layout.dividers.iter().find(|divider| divider.id == id))
        {
            let icon = match divider.axis {
                Axis::Vertical => CursorIcon::ColResize,
                Axis::Horizontal => CursorIcon::RowResize,
            };
            if self.pointer_cursor != Some(icon) {
                self.window.set_cursor(icon);
                self.pointer_cursor = Some(icon);
            }
            if self.hovered_preview.take().is_some() {
                let _ = crate::link_platform::set_hover(&self.window, None);
            }
            return;
        }
        let target = self.link_under_pointer();
        let unchanged = match (&self.pane.hovered_link, &target) {
            (Some(current), Some(target)) => current.matches(target),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            let next = target.map(crate::hyperlinks::LinkTarget::new);
            self.pane.hovered_link = next;
        }
        let preview = self.pane.hovered_link.as_ref().map(|link| link.preview().to_owned());
        if self.hovered_preview != preview {
            if let Err(error) = crate::link_platform::set_hover(&self.window, preview.as_deref()) {
                log::warn!("link preview failed: {error}");
            }
            self.hovered_preview = preview;
        }
        let clickable = self.modifiers.super_key()
            && self.pane.hovered_link.as_ref().is_some_and(|link| link.can_open());
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
    pending_parsers: ParseQueue<(WindowId, PaneId, u64)>,
    session_routes: HashMap<u64, app_pane_move::SessionRoute>,
    notification_policy: NotificationPolicy,
    native_notifications: NativeNotifications,
    /// Never reuse a PTY identity even when the platform reuses a WindowId.
    next_session: u64,
    session_service: app_session::SessionService,
    control_service: app_control::ControlService,
    pane_drag: Option<app_pane_drag::PaneDrag>,
    pane_header_buttons: Vec<MouseButton>,
    #[cfg(test)]
    pub(crate) hidden_windows: bool,
}

impl App {
    pub fn new(
        config: Config,
        proxy: EventLoopProxy<UserEvent>,
        mut animations: mechanic_config::theme::AnimationConfig,
        mouse_tracking: bool,
    ) -> Self {
        animations.logo &= config.theme.logo_size > 0;
        let notification_policy = NotificationPolicy::new(&config.notifications);
        let notification_proxy = proxy.clone();
        let native_notifications =
            NativeNotifications::new(config.notifications.enabled, move |ready| {
                let _ = notification_proxy.send_event(UserEvent::NotificationReady(ready));
            });
        Self {
            config,
            windows: HashMap::new(),
            proxy,
            animations,
            mouse_tracking,
            pending_parsers: ParseQueue::new(),
            session_routes: HashMap::new(),
            notification_policy,
            native_notifications,
            next_session: 1,
            session_service: app_session::SessionService::default(),
            control_service: app_control::ControlService::default(),
            pane_drag: None,
            pane_header_buttons: Vec::new(),
            #[cfg(test)]
            hidden_windows: false,
        }
    }

    fn make_waker(&self, window_id: WindowId, pane_id: PaneId, session: u64) -> PtyWaker {
        make_waker_for(&self.proxy, window_id, pane_id, session)
    }

    fn allocate_session(&mut self) -> Option<u64> {
        allocate_session_token(&mut self.next_session)
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
        // The last close retains the final nonempty layout for the next launch.
        if self.windows.len() == 1 && self.windows.contains_key(&id) {
            self.note_session_change();
        }
        if let Some(state) = self.windows.remove(&id) {
            for pane in state.tree.pane_ids() {
                let session = if pane == state.loaded_pane {
                    state.pane.session
                } else {
                    state.other_panes[&pane].session
                };
                self.pending_parsers.remove(&(id, pane, session));
                self.session_routes.remove(&session);
                self.control_service.retire(session, crate::control::ErrorCode::Closed);
            }
        }
        self.notification_policy.forget_window(id);
        self.note_session_change();
        if self.windows.is_empty() {
            self.flush_session();
            log::info!("all windows closed — exiting");
            event_loop.exit();
        }
    }

    fn pump_parser(&mut self, event_loop: &ActiveEventLoop) {
        let Some((id, pane_id, session)) = self.pending_parsers.pop() else {
            return;
        };
        let Some(state) = self.windows.get_mut(&id) else {
            return;
        };
        if !state.load_pane(pane_id)
            || state.pane.session != session
            || state.pane.exit_status.is_some()
        {
            state.load_pane(state.tree.active());
            return;
        }
        let mouse_protocol = state.pane.terminal.mouse_protocol();
        let outcome = state.pane.terminal.process_input();
        let directory_changed = app_session::refresh_directory(&mut state.pane);
        for completion in state.pane.terminal.drain_command_completions().collect::<Vec<_>>() {
            self.control_service.complete(session, &completion);
            if self.control_service.server.is_some() {
                state.pane.completions.push_back(completion.clone());
                if state.pane.completions.len() > app_control::MAX_COMPLETIONS {
                    state.pane.completions.pop_front();
                }
            }
            let scope = CompletionScope { window: id, pane: pane_id, session };
            if let Some(notification) =
                self.notification_policy.consider(scope, state.focused, &completion)
            {
                self.native_notifications.deliver(&state.window, notification);
            }
        }
        if state.pane.terminal.mouse_protocol() != mouse_protocol
            || outcome.child_exit.is_some()
            || outcome.io_error.is_some()
        {
            state.pane.scroll_accumulator.reset();
            state.pane.last_mouse_report = None;
        }
        if outcome.child_exit.is_some() || outcome.io_error.is_some() {
            if pane_id == state.tree.active() {
                state.cancel_preedit();
            } else {
                state.pane.preedit = None;
            }
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
            state.pane.terminal.inject_local(
                b"\r\n\x1b[31m[terminal I/O failed; Cmd+R to restart, any key to close]\x1b[0m\r\n",
            );
            state.pane.exit_status = Some(None);
            state.mark_content_dirty();
            state.request_redraw();
        }

        if let Some(status) = outcome.child_exit
            && state.pane.exit_status.is_none()
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
                self.close_pane(id, pane_id, event_loop);
                return;
            }
            inject_exit_banner(&mut state.pane.terminal, status);
            state.invalidate_search();
            state.pane.exit_status = Some(status);
            state.mark_content_dirty();
            state.request_redraw();
        }

        if state.pane.exit_status.is_some() {
            self.control_service.retire(session, crate::control::ErrorCode::Closed);
            self.pending_parsers.remove(&(id, pane_id, session));
        } else if outcome.more_output {
            self.pending_parsers.enqueue((id, pane_id, session));
        }
        state.load_pane(state.tree.active());
        if directory_changed {
            self.note_session_change();
        }
    }

    /// Update font metrics and resize the terminal to match.
    fn apply_font_size(state: &mut AppState, new_size: f32) {
        state.pane.link_press.cancel();
        let new_metrics = state.renderer.set_font_size(new_size);
        state.cell_metrics = new_metrics;
        state.pane.scroll_accumulator.reset();
        state.pane.last_mouse_report = None;
        state.current_font_size = new_size;

        state.resize_panes();

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
    fn spawn_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        directory: Option<&std::path::Path>,
    ) -> Option<WindowId> {
        self.spawn_window_with_snapshot(event_loop, directory, None)
    }

    fn spawn_restored_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        snapshot: &crate::session::WindowSnapshot,
    ) -> Option<WindowId> {
        self.spawn_window_with_snapshot(event_loop, None, Some(snapshot))
    }

    fn spawn_window_with_snapshot(
        &mut self,
        event_loop: &ActiveEventLoop,
        directory: Option<&std::path::Path>,
        snapshot: Option<&crate::session::WindowSnapshot>,
    ) -> Option<WindowId> {
        let session = self.allocate_session()?;
        if self.windows.len() >= crate::session::MAX_WINDOWS {
            return None;
        }
        let mut tree = match snapshot {
            Some(snapshot) => PaneTree::from_snapshot(snapshot.panes.clone()).ok()?,
            None => PaneTree::new(1),
        };
        let pane_id = tree.pane_ids()[0];
        let directory = snapshot
            .and_then(|snapshot| snapshot.directories.get(&pane_id))
            .map(std::path::PathBuf::as_path)
            .or(directory);
        let font_size = snapshot.map_or(self.config.font.size, |snapshot| snapshot.font_size);
        let offset = (self.windows.len() as i32).saturating_mul(24);
        let logical_size = snapshot.map_or([1024.0, 768.0], |snapshot| snapshot.logical_size);
        let mut attrs = WindowAttributes::default()
            .with_title("Mechanic")
            .with_inner_size(LogicalSize::new(logical_size[0], logical_size[1]))
            .with_transparent(true);
        #[cfg(test)]
        {
            attrs = attrs.with_visible(!self.hidden_windows);
        }
        if let Some(position) =
            snapshot.and_then(|snapshot| snapshot.logical_position).filter(|position| {
                app_session::position_is_visible(event_loop, *position, logical_size)
            })
        {
            attrs = attrs.with_position(winit::dpi::LogicalPosition::new(position[0], position[1]));
        } else if offset > 0 {
            attrs = attrs.with_position(PhysicalPosition::new(offset, offset));
        }

        let resources = self.create_window_resources(event_loop, attrs, font_size)?;
        let window = &resources.window;
        let size = window.inner_size();
        let cell_metrics = resources.renderer.cell_metrics();
        let terminal_size =
            Self::terminal_size_from_metrics(size.width, size.height, &cell_metrics);

        let window_id = window.id();
        let waker = self.make_waker(window_id, pane_id, session);
        let terminal =
            match Terminal::new_in_directory(&self.config, terminal_size, waker.clone(), directory)
            {
                Ok(t) => t,
                Err(e) => {
                    log::error!("failed to create terminal: {e}");
                    // A saved directory may disappear between checking and
                    // spawn. Retry the configured shell at its normal cwd.
                    match Terminal::new_in_directory(&self.config, terminal_size, waker, None) {
                        Ok(terminal) => terminal,
                        Err(error) => {
                            log::error!("terminal fallback failed: {error}");
                            return None;
                        }
                    }
                }
            };
        let mut pane = PaneState::new(terminal, session);
        pane.directory = directory
            .filter(|path| path.is_dir())
            .map(std::path::Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        let mut other_panes = HashMap::new();
        if let Some(snapshot) = snapshot {
            for other_id in tree.pane_ids().into_iter().filter(|id| *id != pane_id) {
                let Some(other_session) = self.allocate_session() else {
                    tree.close(other_id);
                    continue;
                };
                let directory =
                    snapshot.directories.get(&other_id).map(std::path::PathBuf::as_path);
                let waker = self.make_waker(window_id, other_id, other_session);
                let terminal = Terminal::new_in_directory(
                    &self.config,
                    terminal_size,
                    waker.clone(),
                    directory,
                )
                .or_else(|_| Terminal::new_in_directory(&self.config, terminal_size, waker, None));
                match terminal {
                    Ok(terminal) => {
                        let mut pane = PaneState::new(terminal, other_session);
                        pane.directory = directory
                            .filter(|path| path.is_dir())
                            .map(std::path::Path::to_path_buf)
                            .or_else(|| std::env::current_dir().ok());
                        other_panes.insert(other_id, pane);
                    }
                    Err(error) => {
                        log::warn!("could not restore pane {other_id}: {error}");
                        tree.close(other_id);
                    }
                }
            }
        }

        let mut state = self.state_for_window(resources, pane_id, pane, tree, other_panes);
        state.resize_panes();
        state.request_redraw();
        for pane_id in state.tree.pane_ids() {
            let session = state.pane_state(pane_id)?.session;
            self.session_routes
                .insert(session, app_pane_move::SessionRoute::new(window_id, pane_id));
            self.pending_parsers.enqueue((window_id, pane_id, session));
        }
        self.windows.insert(window_id, state);
        self.note_session_change();

        log::info!("spawned window {window_id:?} (total: {})", self.windows.len());
        Some(window_id)
    }

    fn state_for_window(
        &self,
        resources: app_pane_move::WindowResources,
        pane_id: PaneId,
        pane: PaneState,
        tree: PaneTree,
        other_panes: HashMap<PaneId, PaneState>,
    ) -> AppState {
        let app_pane_move::WindowResources { window, renderer, font_size } = resources;
        let size = window.inner_size();
        let cell_metrics = renderer.cell_metrics();
        let clipboard =
            arboard::Clipboard::new().map_err(|e| log::warn!("clipboard unavailable: {e}")).ok();

        window.set_ime_allowed(true);

        let now = std::time::Instant::now();
        AppState {
            window: window.clone(),
            window_title: "Mechanic".into(),
            renderer,
            cell_metrics,
            loaded_pane: pane_id,
            other_panes,
            tree,
            layout: PaneTree::new(pane_id).layout(
                Rect { x: 0, y: 0, width: size.width, height: size.height },
                Size { width: 1, height: 1 },
                4,
            ),
            divider_drag: None,
            divider_hover: None,
            pane_drag_visual: Default::default(),
            captured_pane: None,
            pointer_position: (0.0, 0.0),
            pointer_inside: false,
            pane,
            pointer_cursor: None,
            hovered_preview: None,
            modifiers: ModifiersState::empty(),
            cancelled_ime_commit: false,
            clipboard,
            start_time: now,
            focused: true,
            current_font_size: font_size,
            content_dirty: true,
            focus_redraw_frames: FOCUS_REDRAW_BURST_FRAMES,
            focus_gain_at: self.animations.logo.then_some(now),
            bloom_start: None,
            frame_pacer: FramePacer::new(now),
        }
    }

    fn split_pane(&mut self, id: WindowId, axis: Axis) -> Option<PaneId> {
        let session = self.allocate_session()?;
        let state = self.windows.get_mut(&id)?;
        state.load_pane(state.tree.active());
        if !state.tree.can_split_active(axis, &state.layout) {
            return None;
        }
        let directory = state.pane.directory.clone();
        let original = state.loaded_pane;
        let pane_id = state.tree.split_active(axis)?;
        let size = state.pane.terminal.size();
        let waker = make_waker_for(&self.proxy, id, pane_id, session);
        let terminal =
            match Terminal::new_in_directory(&self.config, size, waker, directory.as_deref()) {
                Ok(terminal) => terminal,
                Err(error) => {
                    state.tree.close(pane_id);
                    state.tree.focus(original);
                    log::error!("could not split pane: {error}");
                    return None;
                }
            };
        state.cancel_preedit();
        state.pane.link_press.cancel();
        if let Some(panel) = &state.pane.search_panel {
            panel.close();
        }
        let mut pane = PaneState::new(terminal, session);
        pane.directory = directory.or_else(|| std::env::current_dir().ok());
        state.other_panes.insert(pane_id, pane);
        state.resize_panes();
        state.request_redraw();
        self.pending_parsers.enqueue((id, pane_id, session));
        self.session_routes.insert(session, app_pane_move::SessionRoute::new(id, pane_id));
        self.note_session_change();
        Some(pane_id)
    }

    fn close_pane(&mut self, id: WindowId, pane_id: PaneId, event_loop: &ActiveEventLoop) {
        let Some(state) = self.windows.get_mut(&id) else {
            return;
        };
        if !state.tree.pane_ids().contains(&pane_id) {
            return;
        }
        if state.tree.len() == 1 {
            self.close_window(id, event_loop);
            return;
        }
        state.load_pane(pane_id);
        let session = state.pane.session;
        state.cancel_preedit();
        self.notification_policy.forget_scope(CompletionScope {
            window: id,
            pane: pane_id,
            session,
        });
        if state.captured_pane == Some(pane_id) {
            state.captured_pane = None;
        }
        state.tree.close(pane_id);
        state.load_pane(state.tree.active());
        state.other_panes.remove(&pane_id);
        self.pending_parsers.remove(&(id, pane_id, session));
        self.session_routes.remove(&session);
        self.control_service.retire(session, crate::control::ErrorCode::Closed);
        state.resize_panes();
        state.request_redraw();
        self.note_session_change();
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
    pane_id: PaneId,
    mut point: GridPoint,
    mut side: GridSide,
    display_offset: usize,
) -> (GridPoint, GridSide) {
    let row = (point.line.0 + display_offset as i32).max(0) as usize;
    let (col, rtl) = renderer.pane_logical_column(pane_id, point.column.0, row);
    point.column = GridColumn(col);
    if rtl {
        side = match side {
            GridSide::Left => GridSide::Right,
            GridSide::Right => GridSide::Left,
        };
    }
    (point, side)
}

impl App {
    fn dispatch_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: WindowId,
        event: WindowEvent,
    ) {
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
                && modifiers == ModifiersState::SUPER
                && let Some(shortcut) = cmd_shortcut(c.as_str())
                && shortcut.is_app_level()
            {
                match shortcut {
                    CmdShortcut::SpawnWindow => {
                        let directory =
                            self.windows.get(&id).and_then(|state| state.pane.directory.clone());
                        let _ = self.spawn_window(event_loop, directory.as_deref());
                    }
                    CmdShortcut::CloseWindow => {
                        let pane = self.windows[&id].tree.active();
                        self.close_pane(id, pane, event_loop);
                    }
                    CmdShortcut::Quit => self.quit(event_loop),
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

        if state.pane.layout_dirty
            && matches!(
                &event,
                WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::CursorMoved { .. }
                    | WindowEvent::ModifiersChanged(_)
                    | WindowEvent::CursorEntered { .. }
            )
        {
            let grid = crate::convert::convert_grid(
                &state.pane.terminal,
                &self.config.theme,
                state.focused,
            );
            state.renderer.prepare_pane_layout(state.loaded_pane, &grid);
            state.pane.layout_dirty = false;
        }

        match event {
            WindowEvent::CloseRequested => {
                log::info!("window {id:?} close requested");
                self.close_window(id, event_loop);
            }

            WindowEvent::Resized(size) => {
                state.pane.link_press.cancel();
                state.pane.scroll_accumulator.reset();
                state.pane.last_mouse_report = None;
                state.renderer.resize((size.width, size.height));

                state.resize_panes();

                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.cell_metrics = state.renderer.set_scale_factor(scale_factor as f32);
                state.resize_panes();
                state.request_redraw();
            }

            WindowEvent::ModifiersChanged(mods) => {
                if state.modifiers != mods.state() {
                    state.pane.scroll_accumulator.reset();
                    state.pane.last_mouse_report = None;
                }
                state.modifiers = mods.state();
                state.refresh_link_hover();
            }

            WindowEvent::Focused(focused) => {
                log::debug!("window {id:?} focused: {focused}");
                state.focused = focused;
                let active = state.tree.active();
                if !focused {
                    state.load_pane(active);
                    state.cancel_preedit();
                }
                for pane in state.tree.pane_ids() {
                    state.load_pane(pane);
                    if !focused {
                        state.pane.link_press.cancel();
                        state.pane.preedit = None;
                        state.pane.held_buttons.clear();
                        state.pane.mouse_pressed = false;
                        state.pane.mouse_press_origin = None;
                        state.pane.last_mouse_report = None;
                        state.pane.scroll_accumulator.reset();
                    }
                    state.mark_content_dirty();
                }
                state.load_pane(active);
                state.focus_redraw_frames = FOCUS_REDRAW_BURST_FRAMES;

                if focused {
                    state.focus_gain_at = self.animations.logo.then(Instant::now);
                    state.bloom_start = None;
                } else {
                    state.captured_pane = None;
                    state.divider_drag = None;
                    state.set_divider_hover(None);
                    // Cancel pending bloom; let an already committed animation finish.
                    state.focus_gain_at = None;
                }

                state.refresh_link_hover();

                state.request_redraw();
            }

            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if key_event.state == ElementState::Pressed {
                    state.pane.link_press.cancel();
                }
                if key_event.state == ElementState::Pressed {
                    match workspace_shortcut(&key_event.logical_key, state.modifiers) {
                        Some(WorkspaceShortcut::Find) => {
                            state.show_search(&self.proxy, id);
                            return;
                        }
                        Some(WorkspaceShortcut::FindNext | WorkspaceShortcut::FindPrevious) => {
                            let backwards = state.modifiers.shift_key();
                            if !state.pane.search.active {
                                state.show_search(&self.proxy, id);
                            }
                            state.pane.search.navigate(&state.pane.terminal, backwards);
                            state.reveal_search_match();
                            return;
                        }
                        Some(WorkspaceShortcut::PreviousPrompt | WorkspaceShortcut::NextPrompt) => {
                            if key_event.logical_key == Key::Named(NamedKey::ArrowUp) {
                                state.pane.terminal.jump_to_previous_prompt();
                            } else {
                                state.pane.terminal.jump_to_next_prompt();
                            }
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        Some(WorkspaceShortcut::CopyCommandOutput) => {
                            if let Some(output) = state.pane.terminal.last_command_output()
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
                if state.pane.exit_status.is_some() && key_event.state == ElementState::Pressed {
                    let key = &key_event.logical_key;
                    let mods = state.modifiers;

                    if mods.super_key() && matches!(key, Key::Character(c) if c.as_str() == "r") {
                        let pane_id = state.loaded_pane;
                        let old_session = state.pane.session;
                        let Some(session) = allocate_session_token(&mut self.next_session) else {
                            log::error!("terminal session identity space exhausted");
                            return;
                        };
                        let waker = make_waker_for(&self.proxy, id, pane_id, session);
                        respawn_shell(state, &self.config, id, waker, session);
                        if state.pane.exit_status.is_none() {
                            self.session_routes.remove(&old_session);
                            self.session_routes
                                .insert(session, app_pane_move::SessionRoute::new(id, pane_id));
                            self.control_service
                                .retire(old_session, crate::control::ErrorCode::Restarted);
                            self.pending_parsers.remove(&(id, pane_id, old_session));
                            self.notification_policy.forget_scope(CompletionScope {
                                window: id,
                                pane: pane_id,
                                session: old_session,
                            });
                            self.pending_parsers.enqueue((id, pane_id, state.pane.session));
                        }
                        return;
                    }

                    let allow_fall_through = mods.super_key()
                        && matches!(key, Key::Character(c) if matches!(c.as_str(), "c" | "a"));

                    if !allow_fall_through {
                        if !mods.super_key() && is_dismissal_key(key) {
                            let pane_id = state.loaded_pane;
                            self.close_pane(id, pane_id, event_loop);
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
                        CmdShortcut::SpawnWindow | CmdShortcut::CloseWindow | CmdShortcut::Quit => {
                            debug_assert!(shortcut.is_app_level());
                        }
                        CmdShortcut::Copy => {
                            if let Some(text) = state.pane.terminal.selection_text()
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
                                && let Err(e) = state.pane.terminal.paste(&text)
                            {
                                log::warn!("PTY paste failed: {e}");
                            }
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        CmdShortcut::ClearScrollback => {
                            state.pane.terminal.clear_history();
                            state.invalidate_search();
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        CmdShortcut::SelectAll => {
                            state.pane.terminal.select_all();
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
                            if let Err(e) = state.pane.terminal.write_to_pty(b"\x1f") {
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
                    state.pane.terminal.cursor_app_mode(),
                ) {
                    // Escape must still reach programs such as vim when a selection exists.
                    if state.pane.terminal.selection_range().is_some() {
                        state.pane.terminal.clear_selection();
                    }
                    if let Err(e) = state.pane.terminal.write_to_pty(&bytes) {
                        log::warn!("PTY write failed: {e}");
                    }
                }
                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::Ime(ime_event) => {
                match ime_event {
                    Ime::Commit(text) => {
                        state.pane.preedit = None;
                        if state.cancelled_ime_commit {
                            state.cancelled_ime_commit = false;
                            state.mark_content_dirty();
                            state.request_redraw();
                            return;
                        }
                        if let Err(e) = state.pane.terminal.write_to_pty(text.as_bytes()) {
                            log::warn!("PTY IME commit failed: {e}");
                        }
                    }
                    Ime::Preedit(text, cursor) => {
                        if !text.is_empty() {
                            state.cancelled_ime_commit = false;
                        }
                        state.pane.preedit = crate::preedit::Preedit::new(text, cursor);
                        let mut grid = crate::convert::convert_grid(
                            &state.pane.terminal,
                            &self.config.theme,
                            state.focused && state.pane.preedit.is_none(),
                        );
                        if let Some(preedit) = &state.pane.preedit {
                            preedit.overlay(&mut grid, &self.config.theme);
                        }
                        state.renderer.prepare_pane_layout(state.loaded_pane, &grid);
                        let (cx, cy) = grid.cursor_position;
                        let cw = state.cell_metrics.cell_width;
                        let ch = state.cell_metrics.cell_height;
                        if cx < grid.cols && cy < grid.rows {
                            let rect = state.pane_content_rect(state.loaded_pane).unwrap();
                            let px = f64::from(rect.x)
                                + state.renderer.pane_visual_column(state.loaded_pane, cx, cy)
                                    as f64
                                    * cw as f64;
                            let py = f64::from(rect.y) + cy as f64 * ch as f64;
                            state.window.set_ime_cursor_area(
                                winit::dpi::PhysicalPosition::new(px, py),
                                winit::dpi::PhysicalSize::new(cw as f64, ch as f64),
                            );
                        }
                    }
                    Ime::Disabled => {
                        state.pane.preedit = None;
                        state.cancelled_ime_commit = false;
                    }
                    Ime::Enabled => state.cancelled_ime_commit = false,
                }
                state.mark_content_dirty();
                state.request_redraw();
            }

            WindowEvent::MouseInput { state: btn_state, button: win_button, .. } => {
                let route = route_mouse(
                    state.pane.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.pane.exit_status.is_some(),
                );

                state.refresh_link_hover();
                if win_button == MouseButton::Left && btn_state == ElementState::Pressed {
                    state.pane.link_press = crate::link_input::LinkPress::default();
                } else if btn_state == ElementState::Pressed {
                    state.pane.link_press.cancel();
                }
                if win_button == MouseButton::Left
                    && btn_state == ElementState::Released
                    && state.pane.link_press.active()
                {
                    state.pane.link_press.moved(state.pane.mouse_position);
                    let current = state.link_under_pointer();
                    if let Some(target) = state.pane.link_press.release(current.as_ref()) {
                        open_link(&crate::hyperlinks::LinkTarget::new(target));
                    }
                    return;
                }
                if win_button == MouseButton::Right {
                    if btn_state == ElementState::Released && state.pane.link_menu_release {
                        state.pane.link_menu_release = false;
                        return;
                    }
                    if btn_state == ElementState::Pressed {
                        state.pane.link_menu_release = false;
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
                        state.pane.link_press.begin(link, state.pane.mouse_position);
                        state.pane.mouse_pressed = false;
                        state.pane.mouse_press_origin = None;
                        return;
                    }
                    if win_button == MouseButton::Right
                        && crate::link_input::shows_menu(
                            state.modifiers.super_key(),
                            route.is_some(),
                        )
                    {
                        state.pane.link_menu_release = true;
                        // Keep the clicked target while AppKit runs its menu loop.
                        match crate::link_platform::context_menu(
                            &state.window,
                            state.pointer_position,
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
                    state.pane.held_buttons.update(button, btn_state == ElementState::Pressed);
                    state.pane.last_mouse_report = None;
                }

                if let Some(sgr) = route {
                    if let Some(btn) = winit_to_mouse_button(win_button) {
                        let (col, row) = grid_coords_1based(
                            state.pane.mouse_position,
                            &state.cell_metrics,
                            state.pane.terminal.columns(),
                            state.pane.terminal.screen_lines(),
                        );
                        let col = state
                            .renderer
                            .pane_logical_column(
                                state.loaded_pane,
                                (col - 1) as usize,
                                (row - 1) as usize,
                            )
                            .0 as u32
                            + 1;
                        let kind = match btn_state {
                            ElementState::Pressed => mouse_enc::MouseEventKind::Press,
                            ElementState::Released => mouse_enc::MouseEventKind::Release,
                        };
                        let bytes = mouse_enc::encode(sgr, btn, state.modifiers, kind, col, row);
                        if let Err(e) = state.pane.terminal.write_to_pty(&bytes) {
                            log::warn!("PTY mouse write failed: {e}");
                        }
                        state.pane.mouse_pressed = false;
                        state.pane.mouse_press_origin = None;
                    }
                    if matches!(btn_state, ElementState::Released) {
                        state.pane.last_mouse_report = None;
                    }
                    state.mark_content_dirty();
                    state.request_redraw();
                    return;
                }

                match (btn_state, win_button) {
                    (btn_state, MouseButton::Left) => {
                        let (x, y) = state.pane.mouse_position;
                        let cw = state.cell_metrics.cell_width;
                        let ch = state.cell_metrics.cell_height;
                        let cols = state.pane.terminal.columns();
                        let rows = state.pane.terminal.screen_lines();
                        let display_offset = state.pane.terminal.grid().display_offset();
                        let (point, side) =
                            pixel_to_grid_point(x, y, cw, ch, cols, rows, display_offset);
                        let (point, side) = logical_selection_point(
                            &state.renderer,
                            state.loaded_pane,
                            point,
                            side,
                            display_offset,
                        );

                        match btn_state {
                            ElementState::Pressed => {
                                state.pane.mouse_pressed = true;
                                state.pane.mouse_press_origin = Some((x, y));
                                state.pane.terminal.start_selection(point, side);
                            }
                            ElementState::Released => {
                                state.pane.mouse_pressed = false;
                                if state.pane.mouse_press_origin.is_none() {
                                    return;
                                }
                                const CLICK_DRAG_THRESHOLD_PX: f64 = 5.0;
                                let was_drag = state
                                    .pane
                                    .mouse_press_origin
                                    .map(|(ox, oy)| {
                                        let dx = x - ox;
                                        let dy = y - oy;
                                        (dx * dx + dy * dy).sqrt() > CLICK_DRAG_THRESHOLD_PX
                                    })
                                    .unwrap_or(false);
                                state.pane.mouse_press_origin = None;

                                if !was_drag {
                                    state.pane.terminal.clear_selection();

                                    let (cursor_row, cursor_col, scrolled) = {
                                        let grid = state.pane.terminal.grid();
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
                                                state.pane.terminal.write_to_pty(&payload)
                                            {
                                                log::warn!("PTY cursor-move write failed: {e}");
                                            }
                                        }
                                    }
                                } else {
                                    state.pane.primary_selection =
                                        state.pane.terminal.selection_text();
                                }
                            }
                        }
                        state.mark_content_dirty();
                        state.request_redraw();
                    }

                    (ElementState::Pressed, MouseButton::Middle) => {
                        if let Some(text) = state.pane.primary_selection.as_ref() {
                            if let Err(e) = state.pane.terminal.paste(text) {
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
                state.pane.mouse_position = (position.x, position.y);
                state.pane.pointer_inside = true;
                state.pane.link_press.moved(state.pane.mouse_position);
                state.refresh_link_hover();
                if state.pane.link_press.active() {
                    return;
                }

                let route = route_mouse(
                    state.pane.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.pane.exit_status.is_some(),
                );

                if let Some(sgr) = route {
                    let proto = state.pane.terminal.mouse_protocol();
                    if let Some(btn) = state
                        .pane
                        .held_buttons
                        .report_button(proto.report_motion, proto.report_drag)
                    {
                        let (col, row) = grid_coords_1based(
                            state.pane.mouse_position,
                            &state.cell_metrics,
                            state.pane.terminal.columns(),
                            state.pane.terminal.screen_lines(),
                        );
                        let col = state
                            .renderer
                            .pane_logical_column(
                                state.loaded_pane,
                                (col - 1) as usize,
                                (row - 1) as usize,
                            )
                            .0 as u32
                            + 1;
                        if state.pane.last_mouse_report != Some((col, row, btn)) {
                            state.pane.last_mouse_report = Some((col, row, btn));
                            let bytes = mouse_enc::encode(
                                sgr,
                                btn,
                                state.modifiers,
                                mouse_enc::MouseEventKind::Motion,
                                col,
                                row,
                            );
                            if let Err(e) = state.pane.terminal.write_to_pty(&bytes) {
                                log::warn!("PTY mouse motion write failed: {e}");
                            }
                        }
                    }
                    return;
                }

                state.pane.last_mouse_report = None;

                if state.pane.mouse_pressed {
                    let cw = state.cell_metrics.cell_width;
                    let ch = state.cell_metrics.cell_height;
                    let cols = state.pane.terminal.columns();
                    let rows = state.pane.terminal.screen_lines();
                    let display_offset = state.pane.terminal.grid().display_offset();
                    let (point, side) = pixel_to_grid_point(
                        position.x,
                        position.y,
                        cw,
                        ch,
                        cols,
                        rows,
                        display_offset,
                    );
                    let (point, side) = logical_selection_point(
                        &state.renderer,
                        state.loaded_pane,
                        point,
                        side,
                        display_offset,
                    );
                    state.pane.terminal.update_selection(point, side);
                    state.mark_content_dirty();
                    state.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, phase, .. } => {
                state.pane.link_press.cancel();
                let route = route_mouse(
                    state.pane.terminal.mouse_protocol(),
                    self.mouse_tracking,
                    state.modifiers.shift_key(),
                    state.pane.exit_status.is_some(),
                );
                let lines = state.pane.scroll_accumulator.lines(
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
                            state.pane.mouse_position,
                            &state.cell_metrics,
                            state.pane.terminal.columns(),
                            state.pane.terminal.screen_lines(),
                        );
                        let col = state
                            .renderer
                            .pane_logical_column(
                                state.loaded_pane,
                                (col - 1) as usize,
                                (row - 1) as usize,
                            )
                            .0 as u32
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
                            if let Err(e) = state.pane.terminal.write_to_pty(&bytes) {
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
                    state.pane.terminal.scroll_up(lines as usize);
                } else if lines < 0 {
                    state.pane.terminal.scroll_down(lines.unsigned_abs() as usize);
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
                state.pane.pointer_inside = true;
                state.refresh_link_hover();
            }

            WindowEvent::CursorLeft { .. } => {
                state.pane.pointer_inside = false;
                state.pane.link_press.cancel();
                state.refresh_link_hover();
            }

            _ => {}
        }
    }
}

impl App {
    fn route_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: WindowId,
        mut event: WindowEvent,
    ) {
        if self.route_pane_drag(event_loop, id, &event) {
            return;
        }
        if let WindowEvent::KeyboardInput { event: ref key, .. } = event
            && key.state == ElementState::Pressed
            && let Some(modifiers) = self.windows.get(&id).map(|state| state.modifiers)
            && let Some(shortcut) = pane_shortcut(&key.logical_key, modifiers)
        {
            if !key.repeat {
                match shortcut {
                    PaneShortcut::Split(axis) => {
                        self.split_pane(id, axis);
                    }
                    PaneShortcut::CloseWindow => self.close_window(id, event_loop),
                    PaneShortcut::ClosePane => {
                        let pane = self.windows[&id].tree.active();
                        self.close_pane(id, pane, event_loop);
                    }
                    PaneShortcut::Next | PaneShortcut::Previous => {
                        let state = self.windows.get_mut(&id).unwrap();
                        let original = state.tree.active();
                        let pane = if shortcut == PaneShortcut::Next {
                            state.tree.focus_next()
                        } else {
                            state.tree.focus_previous()
                        };
                        state.tree.focus(original);
                        state.focus_pane(pane);
                    }
                    PaneShortcut::Direction(direction) => {
                        let state = self.windows.get_mut(&id).unwrap();
                        let original = state.tree.active();
                        if let Some(pane) = state.tree.focus_direction(direction, &state.layout) {
                            state.tree.focus(original);
                            state.focus_pane(pane);
                        }
                    }
                }
            }
            return;
        }
        let Some(state) = self.windows.get_mut(&id) else {
            return;
        };
        state.load_pane(state.tree.active());
        if matches!(&event, WindowEvent::CursorLeft { .. }) {
            state.pointer_inside = false;
            state.set_divider_hover(None);
            for pane_id in state.tree.pane_ids() {
                state.load_pane(pane_id);
                state.pane.pointer_inside = false;
                state.pane.link_press.cancel();
            }
            state.load_pane(state.tree.active());
        }
        if matches!(&event, WindowEvent::CursorEntered { .. } | WindowEvent::CursorMoved { .. }) {
            state.pointer_inside = true;
        }
        if let WindowEvent::ModifiersChanged(modifiers) = &event
            && state.modifiers != modifiers.state()
        {
            for pane_id in state.tree.pane_ids() {
                state.load_pane(pane_id);
                state.pane.scroll_accumulator.reset();
                state.pane.last_mouse_report = None;
            }
            state.load_pane(state.tree.active());
        }
        if let WindowEvent::CursorMoved { position, .. } = &event {
            state.pointer_position = (position.x, position.y);
            if let Some(divider) = state.divider_drag {
                if state.tree.drag_divider(divider, position.x, position.y, &state.layout) {
                    state.resize_panes();
                    state.request_redraw();
                }
                return;
            }
        }
        if matches!(
            &event,
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            }
        ) && state.divider_drag.take().is_some()
        {
            state.refresh_pointer_hover();
            state.content_dirty = true;
            state.request_redraw();
            return;
        }
        let pointer_event = matches!(
            &event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::CursorEntered { .. }
        ) || matches!(&event, WindowEvent::ModifiersChanged(_))
            && matches!(
                state.layout.hit_test(state.pointer_position.0, state.pointer_position.1),
                Some(Hit::Pane(_))
            );
        if pointer_event {
            let hit = state.layout.hit_test(state.pointer_position.0, state.pointer_position.1);
            let captured = if matches!(
                &event,
                WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. }
            ) {
                state.captured_pane
            } else {
                None
            };
            let target = if let Some(captured) = captured {
                state.set_divider_hover(None);
                Some(captured)
            } else {
                match hit {
                    Some(Hit::Pane(pane)) => {
                        state.set_divider_hover(None);
                        Some(pane)
                    }
                    Some(Hit::Divider(divider)) => {
                        state.set_divider_hover(Some(divider));
                        if matches!(
                            &event,
                            WindowEvent::MouseInput {
                                state: ElementState::Pressed,
                                button: MouseButton::Left,
                                ..
                            }
                        ) {
                            state.divider_drag = Some(divider);
                        }
                        state.refresh_link_hover();
                        None
                    }
                    None => {
                        state.set_divider_hover(None);
                        None
                    }
                }
            };
            let Some(target) = target else {
                return;
            };
            if matches!(&event, WindowEvent::MouseInput { state: ElementState::Pressed, .. }) {
                state.focus_pane(target);
            }
            state.load_pane(target);
            if matches!(&event, WindowEvent::MouseInput { state: ElementState::Pressed, .. }) {
                state.captured_pane = Some(target);
            }
            if let Some(rect) = state.pane_content_rect(target) {
                state.pane.mouse_position = pane_local_position(rect, state.pointer_position);
                if let WindowEvent::CursorMoved { position, .. } = &mut event {
                    position.x = state.pane.mouse_position.0;
                    position.y = state.pane.mouse_position.1;
                }
            }
        }
        self.dispatch_window_event(event_loop, id, event);
        if let Some(state) = self.windows.get_mut(&id) {
            if let Some(captured) = state.captured_pane
                && state.pane_state(captured).is_none_or(|pane| {
                    !pane.mouse_pressed
                        && pane.held_buttons.report_button(false, true).is_none()
                        && !pane.link_press.active()
                        && !pane.link_menu_release
                })
            {
                state.captured_pane = None;
            }
            state.load_pane(state.tree.active());
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        if !self.restore_workspace(event_loop) && self.spawn_window(event_loop, None).is_none() {
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let geometry_event = matches!(
            &event,
            WindowEvent::Resized(_)
                | WindowEvent::Moved(_)
                | WindowEvent::ScaleFactorChanged { .. }
        );
        let layout_input = match &event {
            WindowEvent::KeyboardInput { event, .. } => {
                event.state == ElementState::Pressed
                    && self.windows.get(&id).is_some_and(|state| state.modifiers.super_key())
            }
            WindowEvent::MouseInput { state, .. } => *state == ElementState::Pressed,
            _ => false,
        };
        let changes_layout = self.session_service.store.is_some()
            && (layout_input
                || matches!(&event, WindowEvent::CursorMoved { .. })
                    && self.windows.get(&id).is_some_and(|state| state.divider_drag.is_some()));
        let before = changes_layout
            .then(|| self.windows.get(&id).map(AppState::workspace_snapshot))
            .flatten();
        self.route_window_event(event_loop, id, event);
        if geometry_event {
            self.note_session_change();
        }
        if changes_layout {
            let after = self.windows.get(&id).map(AppState::workspace_snapshot);
            if before != after {
                self.note_session_change();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Explicit quit snapshots all windows; a last close retains its pending
        // nonempty snapshot instead of replacing it with an empty workspace.
        self.note_session_change();
        self.flush_session();
        self.control_service.close_all();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // One bounded parse batch globally per turn, independent of rendering.
        // Requeue continuations rather than self-posting user events: macOS
        // drains those events before it can dispatch native input events.
        self.pump_parser(event_loop);
        let now = Instant::now();
        let mut earliest_deadline: Option<Instant> = None;
        self.service_session_deadline(now, &mut earliest_deadline);
        self.control_service.expire(now, &mut earliest_deadline);

        for state in self.windows.values_mut() {
            let input = AnimationInputs {
                is_alive: state.has_live_pane(),
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
                && state.has_live_pane()
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
            UserEvent::Control(event) => self.dispatch_control(event),
            UserEvent::Search(id, pane_id, session, action) => {
                let Some(state) = self.windows.get_mut(&id) else {
                    return;
                };
                if state.tree.active() != pane_id
                    || !state.load_pane(pane_id)
                    || state.pane.session != session
                    || !state.pane.search.active
                {
                    state.load_pane(state.tree.active());
                    return;
                }
                match action {
                    crate::search_platform::SearchAction::Query(query) => {
                        state.pane.search.set_query(query, &state.pane.terminal)
                    }
                    crate::search_platform::SearchAction::CaseSensitive(enabled) => {
                        state.pane.search.set_case_sensitive(enabled, &state.pane.terminal)
                    }
                    crate::search_platform::SearchAction::Next => {
                        state.pane.search.navigate(&state.pane.terminal, false)
                    }
                    crate::search_platform::SearchAction::Previous => {
                        state.pane.search.navigate(&state.pane.terminal, true)
                    }
                    crate::search_platform::SearchAction::Close => {
                        state.pane.search.active = false;
                        if let Some(panel) = &state.pane.search_panel {
                            panel.close();
                        }
                        state.window.focus_window();
                    }
                }
                if state.pane.search.active {
                    state.reveal_search_match();
                } else {
                    state.mark_content_dirty();
                    state.request_redraw();
                }
                state.load_pane(state.tree.active());
            }
            UserEvent::PtyOutput(id, pane_id, session) => {
                let Some((id, pane_id)) =
                    self.session_routes.get(&session).and_then(|route| route.resolve(id, pane_id))
                else {
                    return;
                };
                if let Some(state) = self.windows.get(&id) {
                    let pane = if state.loaded_pane == pane_id {
                        Some(&state.pane)
                    } else {
                        state.other_panes.get(&pane_id)
                    };
                    if pane
                        .is_some_and(|pane| pane.session == session && pane.exit_status.is_none())
                    {
                        self.pending_parsers.enqueue((id, pane_id, session));
                    }
                }
            }
            UserEvent::NotificationReady(mut notification) => {
                let mut scope = notification.scope;
                if let Some(route) = self.session_routes.get(&scope.session) {
                    (scope.window, scope.pane) = route.current;
                    notification.scope = scope;
                }
                if let Some(state) = self.windows.get(&scope.window) {
                    let pane = if state.loaded_pane == scope.pane {
                        Some(&state.pane)
                    } else {
                        state.other_panes.get(&scope.pane)
                    };
                    if completion_delivery_allowed(
                        state.focused,
                        scope.session,
                        pane.map(|pane| pane.session),
                    ) {
                        self.native_notifications.deliver(&state.window, notification);
                    }
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
fn make_waker_for(
    proxy: &EventLoopProxy<UserEvent>,
    window_id: WindowId,
    pane_id: PaneId,
    session: u64,
) -> PtyWaker {
    let proxy = proxy.clone();
    Arc::new(move || {
        let _ = proxy.send_event(UserEvent::PtyOutput(window_id, pane_id, session));
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
    /// Cmd+Q saves the complete workspace before terminating the application.
    Quit,
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
        matches!(self, Self::SpawnWindow | Self::CloseWindow | Self::Quit)
    }
}

/// Map a Cmd-modified character to a [`CmdShortcut`].
fn cmd_shortcut(c: &str) -> Option<CmdShortcut> {
    match c {
        "n" => Some(CmdShortcut::SpawnWindow),
        "q" => Some(CmdShortcut::Quit),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneShortcut {
    Split(Axis),
    ClosePane,
    CloseWindow,
    Next,
    Previous,
    Direction(Direction),
}

fn pane_shortcut(key: &Key, modifiers: ModifiersState) -> Option<PaneShortcut> {
    if modifiers == ModifiersState::SUPER {
        return match key {
            Key::Character(c) if c.eq_ignore_ascii_case("d") => {
                Some(PaneShortcut::Split(Axis::Vertical))
            }
            Key::Character(c) if c.eq_ignore_ascii_case("w") => Some(PaneShortcut::ClosePane),
            Key::Character(c) if c == "]" => Some(PaneShortcut::Next),
            Key::Character(c) if c == "[" => Some(PaneShortcut::Previous),
            _ => None,
        };
    }
    if modifiers == (ModifiersState::SUPER | ModifiersState::SHIFT) {
        return match key {
            Key::Character(c) if c.eq_ignore_ascii_case("d") => {
                Some(PaneShortcut::Split(Axis::Horizontal))
            }
            Key::Character(c) if c.eq_ignore_ascii_case("w") => Some(PaneShortcut::CloseWindow),
            _ => None,
        };
    }
    if modifiers == (ModifiersState::SUPER | ModifiersState::ALT) {
        return match key {
            Key::Named(NamedKey::ArrowLeft) => Some(PaneShortcut::Direction(Direction::Left)),
            Key::Named(NamedKey::ArrowRight) => Some(PaneShortcut::Direction(Direction::Right)),
            Key::Named(NamedKey::ArrowUp) => Some(PaneShortcut::Direction(Direction::Up)),
            Key::Named(NamedKey::ArrowDown) => Some(PaneShortcut::Direction(Direction::Down)),
            _ => None,
        };
    }
    None
}

fn completion_delivery_allowed(
    focused: bool,
    expected_session: u64,
    current_session: Option<u64>,
) -> bool {
    !focused && current_session == Some(expected_session)
}

fn allocate_session_token(next: &mut u64) -> Option<u64> {
    let session = *next;
    *next = session.checked_add(1)?;
    Some(session)
}

fn pane_local_position(rect: Rect, position: (f64, f64)) -> (f64, f64) {
    (position.0 - f64::from(rect.x), position.1 - f64::from(rect.y))
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

    let dividers: Vec<_> = state
        .layout
        .dividers
        .iter()
        .map(|divider| RenderDivider {
            rect: mechanic_renderer::PaneRect {
                x: divider.rect.x,
                y: divider.rect.y,
                width: divider.rect.width,
                height: divider.rect.height,
            },
            highlighted: state.divider_hover == Some(divider.id)
                || state.divider_drag == Some(divider.id),
        })
        .collect();
    state.renderer.set_pane_dividers(&dividers);
    let handles: Vec<_> = state
        .layout
        .panes
        .iter()
        .map(|pane| {
            let rect = state.pane_header_rect(pane.id).unwrap();
            RenderPaneHandle {
                rect: mechanic_renderer::PaneRect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
                highlighted: state.pane_drag_visual.hovered == Some(pane.id)
                    || state.pane_drag_visual.dragged == Some(pane.id),
            }
        })
        .collect();
    state.renderer.set_pane_handles(&handles);
    state.renderer.set_pane_drop_preview(state.pane_drag_visual.preview.map(|rect| {
        mechanic_renderer::PaneRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
    }));
    let did_animation_render = !state.content_dirty && state.renderer.render_animation(uniforms);

    // A missing cached frame also requires a full render, even when content is clean.
    if !did_animation_render {
        let conversion_started =
            log::log_enabled!(target: "mechanic_render_profile", log::Level::Trace)
                .then(Instant::now);
        let active = state.tree.active();
        for pane_id in state.tree.pane_ids() {
            state.load_pane(pane_id);
            if !state.pane.content_dirty && state.pane.cached_grid.is_some() {
                continue;
            }
            let mut grid = crate::convert::convert_grid(
                &state.pane.terminal,
                &config.theme,
                state.focused && pane_id == active && state.pane.preedit.is_none(),
            );
            state.pane.search.highlight(&mut grid, &state.pane.terminal, &config.theme);
            if let Some(preedit) = &state.pane.preedit {
                preedit.overlay(&mut grid, &config.theme);
            }
            state.pane.cached_grid = Some(grid);
            state.pane.content_dirty = false;
            state.pane.layout_dirty = false;
        }
        state.load_pane(active);
        if let Some(started) = conversion_started {
            let conversion_ns = started.elapsed().as_nanos();
            log::trace!(target: "mechanic_render_profile",
                "render-profile conversion_ns={conversion_ns} cols={} rows={}",
                state.pane.terminal.columns(), state.pane.terminal.screen_lines(),
            );
        }
        let header_height = state.pane_header_height();
        let panes: Vec<_> = state
            .layout
            .panes
            .iter()
            .map(|item| {
                let rect = app_pane_drag::content_rect(item.rect, header_height);
                let pane = if item.id == state.loaded_pane {
                    &state.pane
                } else {
                    &state.other_panes[&item.id]
                };
                RenderPane {
                    id: item.id,
                    rect: mechanic_renderer::PaneRect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                    },
                    grid: pane.cached_grid.as_ref().unwrap(),
                    active: item.id == active,
                }
            })
            .collect();
        state.content_dirty = !state.renderer.render_panes(&panes, uniforms);
        state.refresh_pointer_hover();
    }

    if let Some(t) = state.bloom_start
        && now.saturating_duration_since(t) >= duration
    {
        state.bloom_start = None;
    }

    let shell = state.pane.terminal.shell_integration();
    let shell_running = state.pane.exit_status.is_none() && shell.is_running();
    let integrated_title = shell_window_title(
        state.pane.terminal.title(),
        shell.cwd(),
        shell_running,
        state.pane.exit_status.is_none().then(|| shell.last_exit_status()).flatten(),
    );
    let base_title = integrated_title.as_str();
    let base = if base_title.is_empty() { "Mechanic" } else { base_title };
    let title_string = match state.pane.exit_status {
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
fn respawn_shell(
    state: &mut AppState,
    config: &Config,
    id: WindowId,
    waker: PtyWaker,
    session: u64,
) {
    let size = state.pane.terminal.size();
    let directory = state.pane.directory.clone();
    match Terminal::new_in_directory(config, size, waker, directory.as_deref()) {
        Ok(new_term) => {
            state.invalidate_search();
            state.pane.terminal = new_term;
            state.pane.session = session;
            state.pane.completions.clear();
            state.pane.directory_metadata = (None, None);
            state.pane.cached_grid = None;
            state.pane.search_panel = None;
            state.pane.search.active = false;
            state.pane.link_press.cancel();
            state.pane.preedit = None;
            state.pane.held_buttons.clear();
            state.pane.scroll_accumulator.reset();
            state.pane.last_mouse_report = None;
            state.pane.mouse_pressed = false;
            state.pane.mouse_press_origin = None;
            state.pane.exit_status = None;
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
#[path = "app_pane_smoke.rs"]
#[allow(dead_code, reason = "used by the explicit native pane smoke example")]
mod pane_smoke;

#[cfg(test)]
#[path = "app_pane_drag_smoke.rs"]
#[allow(dead_code, reason = "used by the explicit native pane smoke example")]
mod app_pane_drag_smoke;

#[cfg(test)]
#[path = "app_session_control_smoke.rs"]
#[allow(dead_code, reason = "used by the explicit native session and control smoke example")]
mod app_session_control_smoke;

#[cfg(test)]
#[path = "app_pane_move_smoke.rs"]
#[allow(dead_code, reason = "used by the explicit native live pane movement smoke example")]
mod app_pane_move_smoke;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_shortcuts_require_exact_chords_and_leave_prompt_navigation_available() {
        let command = ModifiersState::SUPER;
        let shifted = command | ModifiersState::SHIFT;
        assert_eq!(
            pane_shortcut(&Key::Character("d".into()), command),
            Some(PaneShortcut::Split(Axis::Vertical))
        );
        assert_eq!(
            pane_shortcut(&Key::Character("D".into()), shifted),
            Some(PaneShortcut::Split(Axis::Horizontal))
        );
        assert_eq!(
            pane_shortcut(&Key::Character("w".into()), command),
            Some(PaneShortcut::ClosePane)
        );
        assert_eq!(
            pane_shortcut(&Key::Character("W".into()), shifted),
            Some(PaneShortcut::CloseWindow)
        );
        for (key, expected) in [
            (Key::Character("]".into()), PaneShortcut::Next),
            (Key::Character("[".into()), PaneShortcut::Previous),
        ] {
            assert_eq!(pane_shortcut(&key, command), Some(expected));
        }
        for modifiers in [
            ModifiersState::empty(),
            ModifiersState::CONTROL,
            ModifiersState::ALT,
            command | ModifiersState::ALT,
            shifted | ModifiersState::CONTROL,
        ] {
            assert_eq!(pane_shortcut(&Key::Character("d".into()), modifiers), None);
        }
        for key in [NamedKey::ArrowUp, NamedKey::ArrowDown] {
            assert_eq!(pane_shortcut(&Key::Named(key), shifted), None);
        }
        for (key, direction) in [
            (NamedKey::ArrowUp, Direction::Up),
            (NamedKey::ArrowDown, Direction::Down),
            (NamedKey::ArrowLeft, Direction::Left),
            (NamedKey::ArrowRight, Direction::Right),
        ] {
            assert_eq!(
                pane_shortcut(&Key::Named(key), command | ModifiersState::ALT),
                Some(PaneShortcut::Direction(direction))
            );
        }
    }

    #[test]
    fn pane_shortcuts_follow_logical_letters_on_non_us_keyboard_layouts() {
        // AZERTY's physical KeyW produces logical z; closing must follow w.
        assert_eq!(pane_shortcut(&Key::Character("z".into()), ModifiersState::SUPER), None);
        assert_eq!(
            pane_shortcut(&Key::Character("w".into()), ModifiersState::SUPER),
            Some(PaneShortcut::ClosePane)
        );
        // Shifted letters and characters remain interpreted by their value.
        assert_eq!(
            pane_shortcut(
                &Key::Character("D".into()),
                ModifiersState::SUPER | ModifiersState::SHIFT
            ),
            Some(PaneShortcut::Split(Axis::Horizontal))
        );
        assert_eq!(pane_shortcut(&Key::Character("{".into()), ModifiersState::SUPER), None);
    }

    #[test]
    fn asynchronous_notification_authorization_accepts_current_frozen_panes_and_rechecks_focus() {
        // A frozen pane retains its session, so delayed authorization must
        // deliver the same completion as an already-authorized native center.
        assert!(completion_delivery_allowed(false, 3, Some(3)));
        assert!(!completion_delivery_allowed(true, 3, Some(3)));
        assert!(!completion_delivery_allowed(false, 3, Some(4)));
        assert!(!completion_delivery_allowed(false, 3, None));
    }

    #[test]
    fn reused_window_and_pane_ids_cannot_reuse_terminal_sessions() {
        let mut next = 1;
        let original = CompletionScope {
            window: WindowId::from(42),
            pane: 1,
            session: allocate_session_token(&mut next).unwrap(),
        };
        // Splits and restarts consume the same sequence as new windows.
        let split_session = allocate_session_token(&mut next).unwrap();
        let restart_session = allocate_session_token(&mut next).unwrap();
        let replacement =
            CompletionScope { session: allocate_session_token(&mut next).unwrap(), ..original };
        assert!(original.session < split_session && split_session < restart_session);
        assert!(restart_session < replacement.session);
        assert!(!completion_delivery_allowed(false, original.session, Some(replacement.session)));
        assert!(completion_delivery_allowed(false, replacement.session, Some(replacement.session)));
        // Find and PTY callbacks also compare the same immutable session token.
        assert_ne!(original, replacement);
    }

    #[test]
    fn session_identity_exhaustion_never_wraps() {
        let mut next = u64::MAX - 1;
        assert_eq!(allocate_session_token(&mut next), Some(u64::MAX - 1));
        assert_eq!(allocate_session_token(&mut next), None);
        assert_eq!(allocate_session_token(&mut next), None);
        assert_eq!(next, u64::MAX);
    }

    #[test]
    fn pointer_coordinates_follow_nested_pane_bounds_without_changing_keyboard_focus() {
        let mut tree = PaneTree::new(1);
        let right = tree.split_active(Axis::Vertical).unwrap();
        let bottom = tree.split_active(Axis::Horizontal).unwrap();
        let layout = tree.layout(
            Rect { x: 0, y: 0, width: 804, height: 404 },
            Size { width: 80, height: 60 },
            4,
        );
        for id in [1, right, bottom] {
            let rect = layout.pane(id).unwrap();
            let position = (f64::from(rect.x) + 15.0, f64::from(rect.y) + 25.0);
            assert_eq!(layout.hit_test(position.0, position.1), Some(Hit::Pane(id)));
            assert_eq!(pane_local_position(rect, position), (15.0, 25.0));
            assert_eq!(tree.active(), bottom);
            let metrics = CellMetrics { cell_width: 10.0, cell_height: 20.0, ascent: 15.0 };
            assert_eq!(
                grid_coords_1based(pane_local_position(rect, position), &metrics, 20, 10),
                (2, 2)
            );
            // A captured drag leaving this pane stays outside for link tests,
            // while terminal mouse coordinates clamp to the edge.
            let outside =
                pane_local_position(rect, (f64::from(rect.x) - 1.0, f64::from(rect.y) + 1.0));
            assert_eq!(link_cell(outside, &metrics, 20, 10), None);
            assert_eq!(grid_coords_1based(outside, &metrics, 20, 10), (1, 1));
        }
    }

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
        assert_eq!(cmd_shortcut("q"), Some(CmdShortcut::Quit));
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
            "b", "d", "e", "f", "g", "h", "i", "j", "l", "m", "o", "p", "r", "s", "t", "u", "x",
            "y", "1", "2", "9", "!", "@", "#", "~", ".", "/", "",
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
        assert!(CmdShortcut::Quit.is_app_level());

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
