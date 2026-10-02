//! Gesture checks layered over real hidden windows and live PTYs.
use super::*;
use crate::panes::DropEdge;
use winit::event::DeviceId;

fn check(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

impl App {
    pub(crate) fn smoke_exercise_pane_drags(
        &mut self,
        events: &ActiveEventLoop,
    ) -> Result<(), String> {
        let window = *self.windows.keys().next().ok_or("drag fixture has no window")?;
        check(self.windows[&window].tree.len() == 1, "drag fixture needs one pane")?;
        let first = self.windows[&window].tree.active();
        let second = self.split_pane(window, Axis::Vertical).ok_or("second drag pane failed")?;
        let third = self.split_pane(window, Axis::Vertical).ok_or("third drag pane failed")?;
        let sessions: Vec<_> = self.windows[&window]
            .tree
            .pane_ids()
            .into_iter()
            .map(|id| (id, self.windows[&window].pane_state(id).unwrap().session))
            .collect();
        {
            let state = self.windows.get_mut(&window).unwrap();
            let snapshot = state.tree.snapshot();
            let original_layout = state.layout.panes.clone();
            check(state.toggle_pane_zoom(), "pane zoom did not enable")?;
            check(
                state.tree.snapshot() == snapshot
                    && state.layout.panes.len() == 1
                    && state.layout.panes[0].id == third
                    && state.layout.dividers.is_empty(),
                "zoom modified tree or exposed inactive panes",
            )?;
            state.focus_pane(first);
            check(
                state.layout.panes.len() == 1 && state.layout.panes[0].id == first,
                "zoom did not follow active-pane focus",
            )?;
            state.focus_pane(third);
            check(!state.toggle_pane_zoom(), "pane zoom did not disable")?;
            check(
                state.tree.snapshot() == snapshot && state.layout.panes == original_layout,
                "unzoom did not restore exact pane layout",
            )?;
        }
        let pointer = |app: &mut App, x, y| {
            app.window_event(
                events,
                window,
                WindowEvent::CursorMoved {
                    device_id: DeviceId::dummy(),
                    position: PhysicalPosition::new(x, y),
                },
            )
        };
        let button = |app: &mut App, state| {
            app.window_event(
                events,
                window,
                WindowEvent::MouseInput {
                    device_id: DeviceId::dummy(),
                    button: MouseButton::Left,
                    state,
                },
            )
        };
        let grip = self.windows[&window].pane_header_rect(third).unwrap();
        let grip_x = f64::from(grip.x) + f64::from(grip.width) / 2.0;
        let grip_y = f64::from(grip.y) + f64::from(grip.height) / 2.0;
        pointer(self, grip_x, grip_y);
        check(
            self.windows[&window].pointer_cursor == Some(CursorIcon::Grab),
            "pane grip has no grab cursor",
        )?;
        for other_button in [MouseButton::Right, MouseButton::Middle] {
            pointer(self, grip_x, grip_y);
            self.window_event(
                events,
                window,
                WindowEvent::MouseInput {
                    device_id: DeviceId::dummy(),
                    button: other_button,
                    state: ElementState::Pressed,
                },
            );
            let content = self.windows[&window].pane_content_rect(third).unwrap();
            pointer(self, grip_x, f64::from(content.y) + 10.0);
            check(
                self.route_pane_drag(
                    events,
                    window,
                    &WindowEvent::MouseInput {
                        device_id: DeviceId::dummy(),
                        button: other_button,
                        state: ElementState::Released,
                    },
                ),
                "header button release leaked to terminal content",
            )?;
            check(self.pane_header_buttons.is_empty(), "header button capture was not released")?;
        }
        pointer(self, grip_x, grip_y);
        button(self, ElementState::Pressed);
        self.window_event(events, window, WindowEvent::CursorLeft { device_id: DeviceId::dummy() });
        pointer(self, grip_x + 8.0, grip_y);
        button(self, ElementState::Released);
        check(
            self.windows[&window].pointer_inside
                && self.windows[&window].pane_drag_visual.hovered == Some(third)
                && self.windows[&window].pointer_cursor == Some(CursorIcon::Grab),
            "out-and-back drag lost header hover without CursorEntered",
        )?;
        pointer(self, grip_x, grip_y);
        let before_click = self.windows[&window].tree.snapshot();
        button(self, ElementState::Pressed);
        pointer(self, grip_x + 1.0, grip_y);
        button(self, ElementState::Released);
        check(
            self.windows[&window].tree.snapshot() == before_click,
            "small grip click rearranged panes",
        )?;
        let target = self.windows[&window].layout.pane(second).unwrap();
        let target_x = f64::from(target.x) + f64::from(target.width) / 2.0;
        let target_y = f64::from(target.y + target.height) - 10.0;
        pointer(self, grip_x, grip_y);
        button(self, ElementState::Pressed);
        pointer(self, target_x, target_y);
        let preview = self.windows[&window]
            .pane_drag_visual
            .preview
            .ok_or("pane drag produced no preview")?;
        check(self.windows[&window].tree.snapshot() == before_click, "pane moved before release")?;
        button(self, ElementState::Released);
        let state = &self.windows[&window];
        let a = state.layout.pane(first).unwrap();
        let b = state.layout.pane(second).unwrap();
        let c = state.layout.pane(third).unwrap();
        check(c == preview, "actual dropped pane differs from preview")?;
        check(
            a.height > b.height && b.x == c.x && b.y < c.y,
            "drop did not produce a large left pane and two stacked right panes",
        )?;
        for (pane, session) in &sessions {
            check(
                state.pane_state(*pane).unwrap().session == *session,
                "rearrangement replaced a shell",
            )?;
        }
        let grip = state.pane_header_rect(third).unwrap();
        let x = f64::from(grip.x) + f64::from(grip.width) / 2.0;
        let y = f64::from(grip.y) + f64::from(grip.height) / 2.0;
        let before_cancel = state.tree.snapshot();
        pointer(self, x, y);
        button(self, ElementState::Pressed);
        pointer(self, -80.0, y);
        self.window_event(events, window, WindowEvent::Focused(false));
        button(self, ElementState::Released);
        check(
            self.pane_drag.is_none()
                && self.windows.len() == 1
                && self.windows[&window].tree.snapshot() == before_cancel,
            "focus loss did not cancel pane drag",
        )?;
        self.window_event(events, window, WindowEvent::Focused(true));
        pointer(self, x, y);
        button(self, ElementState::Pressed);
        pointer(self, -80.0, y);
        button(self, ElementState::Released);
        check(self.windows.len() == 2, "outside drop did not create a window")?;
        let moved_session = sessions.iter().find(|(id, _)| *id == third).unwrap().1;
        let (destination_id, destination) =
            self.windows.iter().find(|(id, _)| **id != window).unwrap();
        let destination_id = *destination_id;
        check(
            destination.tree.len() == 1 && destination.pane.session == moved_session,
            "outside drop did not preserve the live pane",
        )?;
        check(self.windows[&window].tree.len() == 2, "detached pane remained in source layout")?;
        let incoming = destination.tree.active();
        let grip = destination.pane_header_rect(incoming).unwrap();
        self.window_event(
            events,
            destination_id,
            WindowEvent::CursorMoved {
                device_id: DeviceId::dummy(),
                position: PhysicalPosition::new(
                    f64::from(grip.x + grip.width / 2),
                    f64::from(grip.y + grip.height / 2),
                ),
            },
        );
        self.window_event(
            events,
            destination_id,
            WindowEvent::MouseInput {
                device_id: DeviceId::dummy(),
                button: MouseButton::Left,
                state: ElementState::Pressed,
            },
        );
        let target = self.windows[&window].layout.pane(first).unwrap();
        pointer(self, f64::from(target.x + target.width / 2), f64::from(target.y + 2));
        let preview = self.windows[&window]
            .pane_drag_visual
            .preview
            .ok_or("cross-window docking produced no receiving preview")?;
        check(
            self.windows[&destination_id].pane_drag_visual.preview.is_none(),
            "cross-window preview remained on the source window",
        )?;
        button(self, ElementState::Released);
        check(
            !self.windows.contains_key(&destination_id) && self.windows[&window].tree.len() == 3,
            "cross-window drop did not close the empty source and dock its pane",
        )?;
        let docked = self.windows[&window].tree.active();
        check(
            self.windows[&window].pane_state(docked).unwrap().session == moved_session
                && self.windows[&window].layout.pane(docked) == Some(preview),
            "docked live pane changed session or differed from its preview",
        )?;
        // A no-op target and exhausted capacity are validated by geometry tests;
        // the live path must also preserve state when its requested pane is absent.
        let tree = self.windows[&window].tree.snapshot();
        check(
            self.dock_pane(window, docked, window, docked, DropEdge::Left).is_none()
                && self.windows[&window].tree.snapshot() == tree,
            "rejected live docking changed the tree",
        )?;
        let receiver = self
            .detach_pane(events, window, docked, None)
            .ok_or("captured-drag receiving window failed")?;
        let source_origin =
            self.windows[&window].window.inner_position().map_err(|error| error.to_string())?;
        let receiver_size = self.windows[&receiver].window.inner_size();
        self.windows[&receiver].window.set_outer_position(PhysicalPosition::new(
            source_origin.x - receiver_size.width as i32 - 200,
            source_origin.y,
        ));
        let receiver_origin =
            self.windows[&receiver].window.inner_position().map_err(|error| error.to_string())?;
        let incoming_session = self.windows[&window].pane_state(first).unwrap().session;
        let grip = self.windows[&window].pane_header_rect(first).unwrap();
        pointer(self, f64::from(grip.x + grip.width / 2), f64::from(grip.y + grip.height / 2));
        button(self, ElementState::Pressed);
        // Native macOS mouseDragged continues reporting source-local physical
        // coordinates. Convert a receiving-window edge into those coordinates.
        let captured_x = f64::from(receiver_origin.x - source_origin.x) + 2.0;
        let captured_y =
            f64::from(receiver_origin.y - source_origin.y) + f64::from(receiver_size.height) / 2.0;
        check(captured_x < 0.0, "captured-drag fixture windows overlap")?;
        pointer(self, captured_x, captured_y);
        let preview = self.windows[&receiver]
            .pane_drag_visual
            .preview
            .ok_or("source-captured drag produced no receiving preview")?;
        check(
            self.windows[&window].pane_drag_visual.preview.is_none(),
            "source-captured drag showed receiving preview in the wrong window",
        )?;
        button(self, ElementState::Released);
        let received = self.windows[&receiver].tree.active();
        check(
            self.windows[&window].tree.len() == 1
                && self.windows[&receiver].tree.len() == 2
                && self.windows[&receiver].pane_state(received).unwrap().session
                    == incoming_session
                && self.windows[&receiver].layout.pane(received) == Some(preview),
            "source-captured docking detached a new window or changed its live pane",
        )?;
        Ok(())
    }
}
