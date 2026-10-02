//! Transfer live pane state without replacing its PTY or execution identity.
use super::*;

pub(super) struct WindowResources {
    pub(super) window: Arc<Window>,
    pub(super) renderer: Renderer,
    pub(super) font_size: f32,
}

/// The reader callback remains bound to its original window and pane. Only the
/// destination changes, so queued and future wakes follow the same live PTY.
#[derive(Clone, Copy)]
pub(super) struct SessionRoute {
    origin: (WindowId, PaneId),
    pub(super) current: (WindowId, PaneId),
}

impl SessionRoute {
    pub(super) fn new(window: WindowId, pane: PaneId) -> Self {
        Self { origin: (window, pane), current: (window, pane) }
    }

    pub(super) fn resolve(&self, window: WindowId, pane: PaneId) -> Option<(WindowId, PaneId)> {
        (self.origin == (window, pane)).then_some(self.current)
    }
}

impl App {
    pub(super) fn create_window_resources(
        &self,
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
        font_size: f32,
    ) -> Option<WindowResources> {
        #[cfg(test)]
        let attributes = attributes.with_visible(!self.hidden_windows);
        // Normal startup attributes are already fully specified. Test visibility
        // is the only shared override needed by detachment's window constructor.
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                log::error!("failed to create window: {error}");
                return None;
            }
        };
        let size = window.inner_size();
        let mut font = self.config.font.clone();
        font.size = font_size;
        let renderer = match pollster::block_on(Renderer::new(
            window.clone(),
            (size.width, size.height),
            window.scale_factor() as f32,
            &self.config.theme,
            font,
        )) {
            Ok(renderer) => renderer,
            Err(error) => {
                log::error!("failed to create renderer: {error}");
                return None;
            }
        };
        Some(WindowResources { window, renderer, font_size })
    }

    /// Detach one live pane. Construct the destination before modifying the
    /// source, so window/renderer failures preserve its complete original state.
    pub(super) fn detach_pane(
        &mut self,
        event_loop: &ActiveEventLoop,
        window: WindowId,
        pane: PaneId,
        screen_position: Option<PhysicalPosition<i32>>,
    ) -> Option<WindowId> {
        let source = self.windows.get(&window)?;
        source.pane_state(pane)?;
        if source.tree.len() == 1 {
            if let Some(position) = screen_position {
                source.window.set_outer_position(position);
            }
            source.window.focus_window();
            self.note_session_change();
            return Some(window);
        }
        if self.windows.len() >= crate::session::MAX_WINDOWS {
            log::warn!("cannot detach pane: window limit reached");
            return None;
        }
        let rect = source.layout.pane(pane)?;
        let scale = source.window.scale_factor();
        let font_size = source.current_font_size;
        let mut attributes = WindowAttributes::default()
            .with_title("Mechanic")
            .with_inner_size(LogicalSize::new(
                f64::from(rect.width) / scale,
                f64::from(rect.height) / scale,
            ))
            .with_transparent(true);
        if let Some(position) = screen_position {
            attributes = attributes.with_position(position);
        }
        let resources = self.create_window_resources(event_loop, attributes, font_size)?;
        let destination = resources.window.id();
        let moved = self.take_live_pane(window, pane)?;
        let session = moved.session;
        let destination_pane = 1;
        let mut state = self.state_for_window(
            resources,
            destination_pane,
            moved,
            PaneTree::new(destination_pane),
            HashMap::new(),
        );
        state.resize_panes();
        state.request_redraw();
        state.window.focus_window();
        self.windows.insert(destination, state);
        self.retarget_live_session(session, window, pane, destination, destination_pane);
        self.note_session_change();
        log::info!("detached live session {session} into window {destination:?}");
        Some(destination)
    }

    fn take_live_pane(&mut self, window: WindowId, pane: PaneId) -> Option<PaneState> {
        let state = self.windows.get_mut(&window)?;
        if !state.tree.pane_ids().contains(&pane) {
            return None;
        }
        state.load_pane(pane);
        state.cancel_preedit();
        if let Some(panel) = state.pane.search_panel.take() {
            panel.close();
        }
        state.pane.link_press.cancel();
        state.pane.hovered_link = None;
        state.pane.link_menu_release = false;
        state.pane.mouse_pressed = false;
        state.pane.held_buttons.clear();
        state.pane.mouse_press_origin = None;
        state.pane.pointer_inside = false;
        state.pane.last_mouse_report = None;
        state.pane.scroll_accumulator.reset();
        state.pane.cached_grid = None;
        state.pane.content_dirty = true;
        state.pane.layout_dirty = true;
        if state.captured_pane == Some(pane) {
            state.captured_pane = None;
        }
        if state.tree.len() == 1 {
            let state = self.windows.remove(&window)?;
            self.notification_policy.forget_window(window);
            return Some(state.pane);
        }
        state.tree.close(pane);
        state.load_pane(state.tree.active());
        let moved = state.other_panes.remove(&pane)?;
        state.resize_panes();
        state.request_redraw();
        Some(moved)
    }

    fn retarget_live_session(
        &mut self,
        session: u64,
        source_window: WindowId,
        source_pane: PaneId,
        destination_window: WindowId,
        destination_pane: PaneId,
    ) {
        let destination = (destination_window, destination_pane);
        self.session_routes
            .entry(session)
            .or_insert_with(|| SessionRoute::new(source_window, source_pane))
            .current = destination;
        self.pending_parsers.remove(&(source_window, source_pane, session));
        if self
            .windows
            .get(&destination_window)
            .and_then(|state| state.pane_state(destination_pane))
            .is_some_and(|pane| pane.exit_status.is_none())
        {
            self.pending_parsers.enqueue((destination_window, destination_pane, session));
        }
        self.notification_policy.forget_scope(CompletionScope {
            window: source_window,
            pane: source_pane,
            session,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_original_wakes_follow_live_moves_without_accepting_stale_origins() {
        let original = WindowId::from(1);
        let destination = WindowId::from(2);
        let final_window = WindowId::from(3);
        let mut route = SessionRoute::new(original, 7);
        route.current = (destination, 1);
        assert_eq!(route.resolve(original, 7), Some((destination, 1)));
        assert_eq!(route.resolve(destination, 1), None);
        route.current = (final_window, 4);
        assert_eq!(route.resolve(original, 7), Some((final_window, 4)));
        assert_eq!(route.resolve(original, 8), None);
        assert_eq!(route.resolve(WindowId::from(8), 7), None);
    }
}
