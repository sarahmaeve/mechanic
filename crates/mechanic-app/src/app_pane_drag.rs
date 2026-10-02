//! Pane grips, drop previews, and drag gestures. Shells move only on release.
use super::*;
use crate::panes::DropEdge;

#[derive(Default)]
pub(super) struct PaneDragVisual {
    pub(super) hovered: Option<PaneId>,
    pub(super) dragged: Option<PaneId>,
    pub(super) preview: Option<Rect>,
}

pub(super) struct PaneDrag {
    window: WindowId,
    pane: PaneId,
    session: u64,
    start: (f64, f64),
    position: (f64, f64),
    started: bool,
}

pub(super) fn content_rect(mut rect: Rect, header_height: u32) -> Rect {
    let header = header_height.min(rect.height.saturating_sub(1));
    rect.y += header;
    rect.height -= header;
    rect
}

impl AppState {
    pub(super) fn pane_header_height(&self) -> u32 {
        (18.0 * self.window.scale_factor()).round().max(1.0) as u32
    }

    pub(super) fn pane_header_rect(&self, id: PaneId) -> Option<Rect> {
        let mut rect = self.layout.pane(id)?;
        rect.height = self.pane_header_height().min(rect.height.saturating_sub(1));
        Some(rect)
    }

    pub(super) fn pane_content_rect(&self, id: PaneId) -> Option<Rect> {
        Some(content_rect(self.layout.pane(id)?, self.pane_header_height()))
    }

    fn handle_under_pointer(&self) -> Option<PaneId> {
        if !self.pointer_inside || self.captured_pane.is_some() || self.divider_drag.is_some() {
            return None;
        }
        self.layout.panes.iter().find_map(|pane| {
            self.pane_header_rect(pane.id)
                .filter(|rect| rect.contains(self.pointer_position.0, self.pointer_position.1))
                .map(|_| pane.id)
        })
    }

    pub(super) fn refresh_pane_handle_hover(&mut self) -> bool {
        let hovered = self.handle_under_pointer();
        if self.pane_drag_visual.hovered != hovered {
            self.pane_drag_visual.hovered = hovered;
            self.content_dirty = true;
            self.request_redraw();
        }
        if hovered.is_some() || self.pane_drag_visual.dragged.is_some() {
            self.set_divider_hover(None);
            self.refresh_pane_handle_cursor();
            true
        } else {
            false
        }
    }

    pub(super) fn refresh_pane_handle_cursor(&mut self) -> bool {
        let icon = if self.pane_drag_visual.dragged.is_some() {
            CursorIcon::Grabbing
        } else if self.pane_drag_visual.hovered.is_some() {
            CursorIcon::Grab
        } else {
            return false;
        };
        if self.pointer_cursor != Some(icon) {
            self.window.set_cursor(icon);
            self.pointer_cursor = Some(icon);
        }
        if self.hovered_preview.take().is_some() {
            let _ = crate::link_platform::set_hover(&self.window, None);
        }
        true
    }

    fn pane_drop_target(
        &self,
        source: PaneId,
        position: (f64, f64),
    ) -> Option<(PaneId, DropEdge, Rect)> {
        let (target, edge) = self.layout.drop_target(position.0, position.1)?;
        let preview = self.tree.move_preview(source, target, edge, &self.layout)?;
        Some((target, edge, preview))
    }
}

impl App {
    pub(super) fn cancel_pane_drag(&mut self) {
        let Some(drag) = self.pane_drag.take() else { return };
        if let Some(state) = self.windows.get_mut(&drag.window) {
            state.pane_drag_visual.dragged = None;
            state.pane_drag_visual.preview = None;
            state.refresh_pointer_hover();
            state.content_dirty = true;
            state.request_redraw();
        }
    }

    pub(super) fn route_pane_drag(
        &mut self,
        events: &ActiveEventLoop,
        id: WindowId,
        event: &WindowEvent,
    ) -> bool {
        if let WindowEvent::MouseInput { button, state, .. } = event {
            let captured = self.pane_header_buttons.contains(button);
            self.pane_header_buttons.retain(|held| held != button);
            if *state == ElementState::Pressed && self.pane_drag.is_some() {
                self.pane_header_buttons.push(*button);
            }
            if captured
                && *state == ElementState::Released
                && !(*button == MouseButton::Left && self.pane_drag.is_some())
            {
                return true;
            }
        }
        if let Some(drag) = &self.pane_drag {
            let valid = self
                .windows
                .get(&drag.window)
                .and_then(|state| state.pane_state(drag.pane))
                .is_some_and(|pane| pane.session == drag.session);
            let escape = matches!(event, WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && event.logical_key == Key::Named(NamedKey::Escape));
            let lost_window = id == drag.window
                && matches!(
                    event,
                    WindowEvent::Focused(false)
                        | WindowEvent::CloseRequested
                        | WindowEvent::Destroyed
                );
            if !valid || escape || lost_window {
                self.cancel_pane_drag();
                if escape {
                    return true;
                }
                if matches!(
                    event,
                    WindowEvent::MouseInput {
                        button: MouseButton::Left,
                        state: ElementState::Released,
                        ..
                    }
                ) {
                    return true;
                }
            }
        }

        if self.pane_drag.is_some() {
            match event {
                WindowEvent::CursorMoved { position, .. } => {
                    let drag = self.pane_drag.as_mut().unwrap();
                    // macOS mouseDragged stays attached to the window where the grab began.
                    if id != drag.window {
                        return false;
                    }
                    if !position.x.is_finite() || !position.y.is_finite() {
                        return true;
                    }
                    drag.position = (position.x, position.y);
                    let state = self.windows.get_mut(&drag.window).unwrap();
                    state.pointer_position = drag.position;
                    let size = state.window.inner_size();
                    state.pointer_inside =
                        Rect { x: 0, y: 0, width: size.width, height: size.height }
                            .contains(position.x, position.y);
                    drag.started |= (position.x - drag.start.0).hypot(position.y - drag.start.1)
                        >= 5.0 * state.window.scale_factor();
                    let preview = drag
                        .started
                        .then(|| state.pane_drop_target(drag.pane, drag.position))
                        .flatten()
                        .map(|(_, _, rect)| rect);
                    if state.pane_drag_visual.preview != preview {
                        state.pane_drag_visual.preview = preview;
                        state.content_dirty = true;
                        state.request_redraw();
                    }
                    state.refresh_pane_handle_cursor();
                    return true;
                }
                WindowEvent::MouseInput {
                    button: MouseButton::Left,
                    state: ElementState::Released,
                    ..
                } => {
                    let drag = self.pane_drag.take().unwrap();
                    let state = self.windows.get_mut(&drag.window).unwrap();
                    let target = state.pane_drop_target(drag.pane, drag.position);
                    let size = state.window.inner_size();
                    let outside = !Rect { x: 0, y: 0, width: size.width, height: size.height }
                        .contains(drag.position.0, drag.position.1);
                    let pane_rect = state.layout.pane(drag.pane).unwrap();
                    let position = state.window.outer_position().ok().map(|origin| {
                        PhysicalPosition::new(
                            (f64::from(origin.x) + f64::from(pane_rect.x) + drag.position.0
                                - drag.start.0)
                                .round() as i32,
                            (f64::from(origin.y) + f64::from(pane_rect.y) + drag.position.1
                                - drag.start.1)
                                .round() as i32,
                        )
                    });
                    state.pane_drag_visual.dragged = None;
                    state.pane_drag_visual.preview = None;
                    state.refresh_pointer_hover();
                    state.content_dirty = true;
                    state.request_redraw();
                    if drag.started {
                        if outside {
                            self.detach_pane(events, drag.window, drag.pane, position);
                        } else if let Some((target, edge, _)) = target
                            && state.tree.move_pane(drag.pane, target, edge)
                        {
                            state.resize_panes();
                            state.request_redraw();
                            self.note_session_change();
                        }
                    }
                    return true;
                }
                WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => return true,
                _ => {}
            }
        }

        let Some(state) = self.windows.get_mut(&id) else { return false };
        if matches!(event, WindowEvent::CursorLeft { .. }) {
            state.pointer_inside = false;
            state.refresh_pane_handle_hover();
        }
        if matches!(event, WindowEvent::CursorEntered { .. }) {
            state.pointer_inside = true;
            state.refresh_pane_handle_hover();
        }
        if let WindowEvent::CursorMoved { position, .. } = event {
            state.pointer_position = (position.x, position.y);
            state.pointer_inside = true;
        }
        let pointer_event = matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
        );
        if !pointer_event {
            return false;
        }
        if !state.refresh_pane_handle_hover() {
            return false;
        }
        if let WindowEvent::MouseInput { button, state: ElementState::Pressed, .. } = event {
            self.pane_header_buttons.push(*button);
        }
        if matches!(
            event,
            WindowEvent::MouseInput { button: MouseButton::Left, state: ElementState::Pressed, .. }
        ) {
            let Some(pane) = state.pane_drag_visual.hovered else { return false };
            state.focus_pane(pane);
            state.cancel_preedit();
            state.pane.link_press.cancel();
            state.pane_drag_visual.dragged = Some(pane);
            state.refresh_pane_handle_cursor();
            self.pane_drag = Some(PaneDrag {
                window: id,
                pane,
                session: state.pane.session,
                start: state.pointer_position,
                position: state.pointer_position,
                started: false,
            });
        }
        true
    }
}
