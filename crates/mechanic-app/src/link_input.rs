use alacritty_terminal::term::cell::Hyperlink;

#[derive(Default)]
pub struct LinkPress {
    pressed: Option<(Hyperlink, (f64, f64))>,
    cancelled: bool,
}

impl LinkPress {
    pub fn begin(&mut self, target: Hyperlink, position: (f64, f64)) {
        self.pressed = Some((target, position));
        self.cancelled = false;
    }

    pub fn active(&self) -> bool {
        self.pressed.is_some()
    }

    pub fn moved(&mut self, position: (f64, f64)) {
        if let Some((_, origin)) = &self.pressed {
            let dx = position.0 - origin.0;
            let dy = position.1 - origin.1;
            self.cancelled |= !dx.is_finite() || !dy.is_finite() || dx * dx + dy * dy > 25.0;
        }
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn release(&mut self, current: Option<&Hyperlink>) -> Option<Hyperlink> {
        let (pressed, _) = self.pressed.take()?;
        (!self.cancelled && current == Some(&pressed)).then_some(pressed)
    }
}

/// Link activation never takes an ordinary selection click or a TUI's right-click.
pub fn opens_link(command: bool, can_open: bool) -> bool {
    command && can_open
}

pub fn shows_menu(command: bool, terminal_routes_mouse: bool) -> bool {
    command || !terminal_routes_mouse
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: &str) -> Hyperlink {
        Hyperlink::new(Some(id), "https://example.com/".into())
    }

    #[test]
    fn activation_requires_same_link_and_retains_drag_cancellation() {
        let target = link("first");
        let mut press = LinkPress::default();
        press.begin(target.clone(), (10.0, 10.0));
        press.moved((12.0, 11.0));
        assert_eq!(press.release(Some(&target)), Some(target.clone()));
        assert!(!press.active());
        press.begin(target.clone(), (10.0, 10.0));
        press.moved((30.0, 10.0));
        press.moved((10.0, 10.0));
        assert!(press.release(Some(&target)).is_none());
        press.begin(target, (10.0, 10.0));
        assert!(press.release(Some(&link("replaced"))).is_none());
    }

    #[test]
    fn cancelled_press_still_owns_its_release() {
        let target = link("target");
        let mut press = LinkPress::default();
        press.begin(target.clone(), (0.0, 0.0));
        press.cancel();
        assert!(press.active());
        assert!(press.release(Some(&target)).is_none());
        assert!(!press.active());
    }

    #[test]
    fn selection_and_tui_clicks_keep_their_routes() {
        assert!(!opens_link(false, true));
        assert!(!opens_link(true, false));
        assert!(opens_link(true, true));
        assert!(!shows_menu(false, true));
        assert!(shows_menu(true, true));
        assert!(shows_menu(false, false));
    }
}
