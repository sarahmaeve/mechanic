//! Pane grips, drop previews, and drag gestures. Shells move only on release.
use super::*;
use crate::panes::DropEdge;

#[derive(Default)]
pub(super) struct PaneDragVisual {
    pub(super) hovered: Option<PaneId>,
    pub(super) dragged: Option<PaneId>,
    pub(super) preview: Option<Rect>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_receivers_follow_native_order_instead_of_candidate_enumeration() {
        let behind = (11, WindowId::from(1), (20.0, 30.0));
        let front = (22, WindowId::from(2), (40.0, 50.0));
        let stack = [22, 11];
        for candidates in [[behind, front], [front, behind]] {
            assert_eq!(frontmost_drag_candidate(&stack, &candidates), Some((front.1, front.2)));
        }
        assert_eq!(
            frontmost_drag_candidate(&[11, 22], &[behind, front]),
            Some((behind.1, behind.2))
        );
    }

    #[test]
    fn utility_panels_and_title_bars_block_receivers_behind_them() {
        let receiver = (11, WindowId::from(1), (20.0, 30.0));
        // 99 is a visible utility panel, or a window whose frame contains the
        // pointer but whose terminal content rectangle does not.
        assert_eq!(frontmost_drag_candidate(&[99, 11], &[receiver]), None);
        assert_eq!(frontmost_drag_candidate(&[], &[receiver]), None);
        assert_eq!(frontmost_drag_candidate(&[11], &[receiver]), Some((receiver.1, receiver.2)));
    }
}

pub(super) struct PaneDrag {
    window: WindowId,
    pane: PaneId,
    session: u64,
    start: (f64, f64),
    position: (f64, f64),
    /// Screen coordinates permit mouseDragged events captured by the source
    /// window to preview a pane in a different native window.
    screen_position: Option<(f64, f64)>,
    pointer_window: WindowId,
    started: bool,
}

#[derive(Clone, Copy)]
struct PaneDropTarget {
    window: WindowId,
    pane: PaneId,
    edge: DropEdge,
    preview: Option<Rect>,
}

/// The stack includes utility panels, which must block docking into terminal
/// windows behind them. Only the frontmost native hit can receive a pane.
#[cfg(any(target_os = "macos", test))]
fn frontmost_drag_candidate(
    stack: &[isize],
    candidates: &[(isize, WindowId, (f64, f64))],
) -> Option<(WindowId, (f64, f64))> {
    let front = stack.first()?;
    candidates.iter().find(|(number, _, _)| number == front).map(|(_, id, local)| (*id, *local))
}

#[cfg(target_os = "macos")]
mod native_stack {
    use super::*;
    use objc2::{MainThreadMarker, rc::Retained};
    use objc2_app_kit::{NSApplication, NSView};
    use objc2_foundation::NSPoint;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    fn view(window: &Window) -> Option<Retained<NSView>> {
        let _mtm = MainThreadMarker::new()?;
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
        // SAFETY: winit owns this live NSView for the borrowed Window's
        // lifetime, and the marker above confines access to the main thread.
        unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
    }

    pub(super) fn number(window: &Window) -> Option<isize> {
        Some(view(window)?.window()?.windowNumber())
    }

    fn screen_position(source: &Window, position: (f64, f64)) -> Option<NSPoint> {
        let view = view(source)?;
        let source_window = view.window()?;
        let bounds = view.bounds();
        let x = position.0 / source.scale_factor();
        let y = position.1 / source.scale_factor();
        let point = NSPoint::new(
            bounds.origin.x + x,
            bounds.origin.y + if view.isFlipped() { y } else { bounds.size.height - y },
        );
        Some(source_window.convertPointToScreen(view.convertPoint_toView(point, None)))
    }

    pub(super) fn local_position(window: &Window, screen: NSPoint) -> Option<(f64, f64)> {
        let view = view(window)?;
        let point = view.convertPoint_fromView(view.window()?.convertPointFromScreen(screen), None);
        let bounds = view.bounds();
        let x = point.x - bounds.origin.x;
        let y = point.y - bounds.origin.y;
        Some((
            x * window.scale_factor(),
            (if view.isFlipped() { y } else { bounds.size.height - y }) * window.scale_factor(),
        ))
    }

    pub(super) fn translate_position(
        source: &Window,
        position: (f64, f64),
        destination: &Window,
    ) -> Option<(f64, f64)> {
        local_position(destination, screen_position(source, position)?)
    }

    /// AppKit supplies its own front-to-back application window order without
    /// inspecting other applications or requiring screen-recording access.
    pub(super) fn hits(source: &Window, position: (f64, f64)) -> Option<(NSPoint, Vec<isize>)> {
        let mtm = MainThreadMarker::new()?;
        let screen = screen_position(source, position)?;
        let stack = NSApplication::sharedApplication(mtm)
            .orderedWindows()
            .iter()
            .filter(|window| window.isVisible() && !window.isMiniaturized())
            .filter(|window| {
                let frame = window.frame();
                screen.x >= frame.origin.x
                    && screen.y >= frame.origin.y
                    && screen.x < frame.origin.x + frame.size.width
                    && screen.y < frame.origin.y + frame.size.height
            })
            .map(|window| window.windowNumber())
            .collect();
        Some((screen, stack))
    }
}

pub(super) fn content_rect(mut rect: Rect, header_height: u32) -> Rect {
    let header = header_height.min(rect.height.saturating_sub(1));
    rect.y += header;
    rect.height -= header;
    rect
}

impl AppState {
    pub(super) fn unzoomed_layout(&self) -> Layout {
        let size = self.window.inner_size();
        self.tree.layout(
            Rect { x: 0, y: 0, width: size.width, height: size.height },
            self.minimum_pane_size(),
            (6.0 * self.window.scale_factor()).round().max(1.0) as u32,
        )
    }

    pub(super) fn toggle_pane_zoom(&mut self) -> bool {
        self.cancel_preedit();
        self.pane.link_press.cancel();
        self.pane_zoomed = !self.pane_zoomed && self.tree.len() > 1;
        self.divider_drag = None;
        self.set_divider_hover(None);
        self.resize_panes();
        self.request_redraw();
        self.pane_zoomed
    }

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
}

impl App {
    fn pane_drag_allows_hidden_windows(&self) -> bool {
        #[cfg(test)]
        {
            self.hidden_windows
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn pane_drag_window_available(&self, window: &Window) -> bool {
        window.is_minimized() != Some(true)
            && (self.pane_drag_allows_hidden_windows() || window.is_visible() != Some(false))
    }

    fn pane_drag_destination(&self, drag: &PaneDrag) -> Option<(WindowId, (f64, f64))> {
        let source = self.windows.get(&drag.window)?;
        #[cfg(target_os = "macos")]
        {
            let native = native_stack::hits(&source.window, drag.position);
            if !self.pane_drag_allows_hidden_windows()
                || native.as_ref().is_some_and(|(_, stack)| !stack.is_empty())
            {
                let (screen, stack) = native?;
                let candidates: Vec<_> = self
                    .windows
                    .iter()
                    .filter_map(|(id, state)| {
                        if !self.pane_drag_window_available(&state.window) {
                            return None;
                        }
                        let local = native_stack::local_position(&state.window, screen)?;
                        let size = state.window.inner_size();
                        Rect { x: 0, y: 0, width: size.width, height: size.height }
                            .contains(local.0, local.1)
                            .then(|| Some((native_stack::number(&state.window)?, *id, local)))
                            .flatten()
                    })
                    .collect();
                return frontmost_drag_candidate(&stack, &candidates);
            }
        }
        if drag.pointer_window != drag.window
            && let Some(screen) = drag.screen_position
            && let Some(state) = self.windows.get(&drag.pointer_window)
            && self.pane_drag_window_available(&state.window)
            && let Ok(origin) = state.window.inner_position()
        {
            let local = (screen.0 - f64::from(origin.x), screen.1 - f64::from(origin.y));
            let size = state.window.inner_size();
            if (Rect { x: 0, y: 0, width: size.width, height: size.height })
                .contains(local.0, local.1)
            {
                return Some((drag.pointer_window, local));
            }
        }
        let size = source.window.inner_size();
        let source_bounds = Rect { x: 0, y: 0, width: size.width, height: size.height };
        if self.pane_drag_window_available(&source.window)
            && source_bounds.contains(drag.position.0, drag.position.1)
        {
            Some((drag.window, drag.position))
        } else {
            let screen = drag.screen_position?;
            // Platforms without a native stacking query use pointer delivery
            // and focus, with a stable ID tie-break instead of HashMap order.
            self.windows
                .iter()
                .filter(|(id, _)| **id != drag.window)
                .filter(|(_, state)| self.pane_drag_window_available(&state.window))
                .filter_map(|(id, state)| {
                    let origin = state.window.inner_position().ok()?;
                    let local = (screen.0 - f64::from(origin.x), screen.1 - f64::from(origin.y));
                    let size = state.window.inner_size();
                    Rect { x: 0, y: 0, width: size.width, height: size.height }
                        .contains(local.0, local.1)
                        .then_some((*id, state.focused, local))
                })
                .max_by_key(|(id, focused, _)| (*id == drag.pointer_window, *focused, *id))
                .map(|(id, _, local)| (id, local))
        }
    }

    fn pane_drag_target(&self, drag: &PaneDrag) -> Option<PaneDropTarget> {
        let (window, position) = self.pane_drag_destination(drag)?;
        let state = self.windows.get(&window)?;
        let (pane, edge) = state.layout.drop_target(position.0, position.1)?;
        let layout = state.unzoomed_layout();
        let preview = if window == drag.window {
            state.tree.move_preview(drag.pane, pane, edge, &layout)
        } else {
            state.tree.insert_preview(pane, edge, &layout)
        };
        Some(PaneDropTarget { window, pane, edge, preview })
    }

    fn clear_pane_drag_visuals(&mut self) {
        for state in self.windows.values_mut() {
            let changed = state.pane_drag_visual.dragged.take().is_some()
                | state.pane_drag_visual.preview.take().is_some();
            if changed {
                state.refresh_pointer_hover();
                state.content_dirty = true;
                state.request_redraw();
            }
        }
    }

    fn update_pane_drag_preview(&mut self) {
        let target = self
            .pane_drag
            .as_ref()
            .filter(|drag| drag.started)
            .and_then(|drag| self.pane_drag_target(drag));
        for (window, state) in &mut self.windows {
            let preview = target.filter(|target| target.window == *window).and_then(|t| t.preview);
            if state.pane_drag_visual.preview != preview {
                state.pane_drag_visual.preview = preview;
                state.content_dirty = true;
                state.request_redraw();
            }
        }
    }

    pub(super) fn cancel_pane_drag(&mut self) {
        let Some(_) = self.pane_drag.take() else { return };
        self.clear_pane_drag_visuals();
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
                .filter(|state| state.layout.pane(drag.pane).is_some())
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
                    if !position.x.is_finite() || !position.y.is_finite() {
                        return true;
                    }
                    let screen = self.windows.get(&id).and_then(|state| {
                        state.window.inner_position().ok().map(|origin| {
                            (f64::from(origin.x) + position.x, f64::from(origin.y) + position.y)
                        })
                    });
                    let drag = self.pane_drag.as_mut().unwrap();
                    let local = if id == drag.window {
                        Some((position.x, position.y))
                    } else {
                        #[cfg(target_os = "macos")]
                        {
                            self.windows.get(&id).and_then(|state| {
                                native_stack::translate_position(
                                    &state.window,
                                    (position.x, position.y),
                                    &self.windows[&drag.window].window,
                                )
                            })
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            screen.and_then(|screen| {
                                self.windows[&drag.window].window.inner_position().ok().map(
                                    |origin| {
                                        (
                                            screen.0 - f64::from(origin.x),
                                            screen.1 - f64::from(origin.y),
                                        )
                                    },
                                )
                            })
                        }
                    };
                    let Some(local) = local else { return true };
                    drag.position = local;
                    drag.screen_position = screen;
                    drag.pointer_window = id;
                    let state = self.windows.get_mut(&drag.window).unwrap();
                    state.pointer_position = drag.position;
                    let size = state.window.inner_size();
                    state.pointer_inside =
                        Rect { x: 0, y: 0, width: size.width, height: size.height }
                            .contains(local.0, local.1);
                    drag.started |= (local.0 - drag.start.0).hypot(local.1 - drag.start.1)
                        >= 5.0 * state.window.scale_factor();
                    state.refresh_pane_handle_cursor();
                    self.update_pane_drag_preview();
                    return true;
                }
                WindowEvent::MouseInput {
                    button: MouseButton::Left,
                    state: ElementState::Released,
                    ..
                } => {
                    let drag = self.pane_drag.take().unwrap();
                    let target = self.pane_drag_target(&drag);
                    let destination = self.pane_drag_destination(&drag);
                    let state = &self.windows[&drag.window];
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
                    self.clear_pane_drag_visuals();
                    if drag.started {
                        if outside && destination.is_none() {
                            self.detach_pane(events, drag.window, drag.pane, position);
                        } else if let Some(target) =
                            target.filter(|target| target.preview.is_some())
                        {
                            self.dock_pane(
                                drag.window,
                                drag.pane,
                                target.window,
                                target.pane,
                                target.edge,
                            );
                        }
                    }
                    return true;
                }
                WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::KeyboardInput { .. }
                | WindowEvent::Ime(_) => return true,
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
                screen_position: state.window.inner_position().ok().map(|origin| {
                    (
                        f64::from(origin.x) + state.pointer_position.0,
                        f64::from(origin.y) + state.pointer_position.1,
                    )
                }),
                pointer_window: id,
                started: false,
            });
        }
        true
    }
}
