//! Command palette actions run on the application's event loop.
use super::*;
use crate::palette::PaletteEntry;
use crate::palette_platform::{PaletteAction, PalettePanel};

impl App {
    fn palette_entries(&self) -> Vec<PaletteEntry> {
        let mut entries: Vec<_> = [
            ("window.new", "New window", "Cmd+N"),
            ("pane.split-right", "Split pane side by side", "Cmd+D"),
            ("pane.split-down", "Split pane above and below", "Cmd+Shift+D"),
            ("pane.zoom", "Toggle pane zoom", "Cmd+Shift+Return"),
            ("pane.detach", "Detach pane into its own window", ""),
            ("pane.next", "Focus next pane", "Cmd+]"),
            ("pane.previous", "Focus previous pane", "Cmd+["),
            ("pane.title", "Set pane title", ""),
            ("pane.text-color", "Set pane text color", ""),
            ("pane.outline-color", "Set pane outline color", ""),
            ("pane.reset", "Reset pane title and colors", ""),
            ("loadout.save", "Save loadout with starting directories", ""),
            ("loadout.save-layout", "Save loadout without directories", ""),
            ("search", "Find in scrollback", "Cmd+F"),
            ("animations", "Toggle all animations", "Cmd+Shift+A"),
            ("pane.close", "Close active pane", "Cmd+W"),
            ("window.close", "Close window", "Cmd+Shift+W"),
        ]
        .into_iter()
        .map(|(id, label, shortcut)| PaletteEntry {
            id: id.into(),
            label: label.into(),
            shortcut: (!shortcut.is_empty()).then(|| shortcut.into()),
        })
        .collect();
        if let Ok(loadouts) = self.list_loadouts() {
            for loadout in loadouts {
                let name = loadout.name;
                entries.push(PaletteEntry {
                    id: format!("loadout.open:{name}"),
                    label: format!("Open loadout: {name}"),
                    shortcut: None,
                });
                entries.push(PaletteEntry {
                    id: format!("loadout.delete:{name}"),
                    label: format!("Delete saved loadout: {name}"),
                    shortcut: None,
                });
            }
        }
        entries
    }

    pub(super) fn show_palette(&mut self, id: WindowId) {
        self.cancel_pane_drag();
        let entries = self.palette_entries();
        let Some(state) = self.windows.get_mut(&id) else { return };
        state.load_pane(state.tree.active());
        state.cancel_preedit();
        state.modifiers = ModifiersState::empty();
        if let Some(panel) = &state.pane.search_panel {
            panel.close();
        }
        if state.palette.is_none() {
            let proxy = self.proxy.clone();
            match PalettePanel::new(&state.window, move |action| {
                let _ = proxy.send_event(UserEvent::Palette(id, action));
            }) {
                Ok(panel) => state.palette = Some(panel),
                Err(error) => {
                    log::warn!("could not open command palette: {error}");
                    return;
                }
            }
        }
        state.palette_target = Some((state.tree.active(), state.pane.session));
        state.palette_open = true;
        let panel = state.palette.as_ref().unwrap();
        panel.set_entries(entries);
        panel.show();
    }

    fn palette_target(&self, id: WindowId) -> Result<PaneId, String> {
        let state = self.windows.get(&id).ok_or("window closed")?;
        let (pane, session) = state.palette_target.ok_or("open the palette again")?;
        if state.pane_state(pane).is_some_and(|state| state.session == session) {
            Ok(pane)
        } else {
            Err("Pane moved or closed; reopen the palette.".into())
        }
    }

    pub(super) fn dispatch_palette(
        &mut self,
        events: &ActiveEventLoop,
        window: WindowId,
        action: PaletteAction,
    ) {
        if !self.windows.get(&window).is_some_and(|state| state.palette_open) {
            return;
        }
        let command = matches!(&action, PaletteAction::Execute(_));
        let result = match action {
            PaletteAction::Close => {
                self.close_palette(window);
                Ok(())
            }
            PaletteAction::Execute(id) => self.execute_palette(events, window, &id),
            PaletteAction::Submit { id, value } => self.submit_palette(window, &id, &value),
        };
        if let Err(error) = result {
            if command && let Some(state) = self.windows.get_mut(&window) {
                state.palette_open = true;
                if let Some(panel) = &state.palette {
                    panel.show();
                }
            }
            if let Some(panel) = self.windows.get(&window).and_then(|s| s.palette.as_ref()) {
                panel.set_status(&error);
            }
            log::warn!("palette: {error}");
        }
    }

    fn close_palette(&mut self, id: WindowId) {
        if let Some(state) = self.windows.get_mut(&id) {
            state.palette_open = false;
            if let Some(panel) = &state.palette {
                panel.close();
            }
            state.modifiers = ModifiersState::empty();
        }
    }

    fn execute_palette(
        &mut self,
        events: &ActiveEventLoop,
        window: WindowId,
        id: &str,
    ) -> Result<(), String> {
        if let Some(name) = id.strip_prefix("loadout.open:") {
            self.close_palette(window);
            self.open_loadout(events, name, true).map_err(|e| e.to_string())?;
            return Ok(());
        }
        if let Some(name) = id.strip_prefix("loadout.delete:") {
            self.delete_loadout(name).map_err(|e| e.to_string())?;
            self.show_palette(window);
            return Ok(());
        }
        let pane = if id.starts_with("pane.") {
            self.palette_target(window)?
        } else {
            self.windows.get(&window).ok_or("window closed")?.tree.active()
        };
        if matches!(
            id,
            "pane.title"
                | "pane.text-color"
                | "pane.outline-color"
                | "loadout.save"
                | "loadout.save-layout"
        ) {
            let state = &self.windows[&window];
            let appearance = &state.pane_state(pane).unwrap().appearance;
            let (title, placeholder, value) = match id {
                "pane.title" => (
                    "Pane title",
                    "Title; leave blank to reset",
                    appearance.title.as_deref().unwrap_or(""),
                ),
                "pane.text-color" => (
                    "Pane text color",
                    "#RRGGBB; leave blank to reset",
                    appearance.text_color.as_deref().unwrap_or(""),
                ),
                "pane.outline-color" => (
                    "Pane outline color",
                    "#RRGGBB; leave blank to reset",
                    appearance.outline_color.as_deref().unwrap_or(""),
                ),
                _ => ("Save loadout", "Loadout name (an existing name is replaced)", ""),
            };
            state.palette.as_ref().unwrap().set_prompt(id, title, placeholder, value);
            return Ok(());
        }
        self.close_palette(window);
        self.windows.get_mut(&window).unwrap().focus_pane(pane);
        match id {
            "window.new" => {
                let directory = self.windows[&window].pane.directory.clone();
                self.spawn_window(events, directory.as_deref()).ok_or("Could not create window")?;
            }
            "pane.split-right" | "pane.split-down" => {
                let axis = if id == "pane.split-right" { Axis::Vertical } else { Axis::Horizontal };
                self.split_pane(window, axis)
                    .ok_or("Not enough space to split, or shell could not start")?;
            }
            "pane.zoom" => {
                self.windows.get_mut(&window).unwrap().toggle_pane_zoom();
            }
            "pane.detach" => {
                self.detach_pane(events, window, pane, None).ok_or("Could not detach pane")?;
            }
            "pane.next" | "pane.previous" => {
                let state = self.windows.get_mut(&window).unwrap();
                let original = state.tree.active();
                let next = if id == "pane.next" {
                    state.tree.focus_next()
                } else {
                    state.tree.focus_previous()
                };
                state.tree.focus(original);
                state.focus_pane(next);
            }
            "pane.reset" => self.set_pane_appearance(window, pane, Default::default())?,
            "search" => self.windows.get_mut(&window).unwrap().show_search(&self.proxy, window),
            "animations" => self.toggle_animations(),
            "pane.close" => self.close_pane(window, pane, events),
            "window.close" => self.close_window(window, events),
            _ => return Err("Unknown command".into()),
        }
        self.note_session_change();
        Ok(())
    }

    fn submit_palette(&mut self, window: WindowId, id: &str, value: &str) -> Result<(), String> {
        if matches!(id, "loadout.save" | "loadout.save-layout") {
            self.save_loadout(value, id == "loadout.save").map_err(|e| e.to_string())?;
            self.show_palette(window);
            if let Some(panel) = self.windows[&window].palette.as_ref() {
                panel.set_status("Loadout saved");
            }
            return Ok(());
        }
        let pane = self.palette_target(window)?;
        let mut appearance = self.windows[&window].pane_state(pane).unwrap().appearance.clone();
        match id {
            "pane.title" => appearance.title = (!value.is_empty()).then(|| value.to_owned()),
            "pane.text-color" | "pane.outline-color" => {
                let color = if value.is_empty() {
                    None
                } else {
                    Some(crate::session::normalize_color(value)?)
                };
                if id == "pane.text-color" {
                    appearance.text_color = color;
                } else {
                    appearance.outline_color = color;
                }
            }
            _ => return Err("Unknown prompt".into()),
        }
        self.set_pane_appearance(window, pane, appearance)?;
        self.close_palette(window);
        Ok(())
    }
}
