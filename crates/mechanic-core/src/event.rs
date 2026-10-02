//! Terminal event handling.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener as AlacrittyEventListener};

/// A terminal event that the application layer may need to act on.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// The window title was changed by an OSC sequence.
    TitleChanged(String),
    /// The title was reset to the default (empty / application-controlled).
    TitleReset,
    /// The terminal bell was triggered.
    Bell,
    /// New content is ready to render.
    Wakeup,
    /// Exit request or child exit; `None` means no child status is available.
    Exit(Option<std::process::ExitStatus>),
    /// Protocol response bytes to write back to the PTY.
    PtyWrite(Vec<u8>),
}

/// Cloneable queue for events emitted by the terminal parser.
#[derive(Clone)]
pub struct EventProxy {
    events: Arc<Mutex<Vec<TerminalEvent>>>,
}

impl EventProxy {
    pub fn new() -> Self {
        Self { events: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Drain all pending events, returning them in arrival order.
    pub fn drain(&self) -> Vec<TerminalEvent> {
        // Poison recovery is safe: push/take leave the event Vec valid.
        let mut guard = self.events.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    }

    fn push(&self, event: TerminalEvent) {
        let mut guard = self.events.lock().unwrap_or_else(|p| p.into_inner());
        guard.push(event);
    }
}

impl Default for EventProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl AlacrittyEventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        match event {
            AlacrittyEvent::Title(title) => self.push(TerminalEvent::TitleChanged(title)),
            AlacrittyEvent::ResetTitle => self.push(TerminalEvent::TitleReset),
            AlacrittyEvent::Bell => self.push(TerminalEvent::Bell),
            AlacrittyEvent::Wakeup => self.push(TerminalEvent::Wakeup),
            AlacrittyEvent::Exit => self.push(TerminalEvent::Exit(None)),
            AlacrittyEvent::ChildExit(status) => self.push(TerminalEvent::Exit(Some(status))),
            AlacrittyEvent::PtyWrite(text) => self.push(TerminalEvent::PtyWrite(text.into_bytes())),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_is_empty_initially() {
        let proxy = EventProxy::new();
        assert!(proxy.drain().is_empty());
    }

    #[test]
    fn drain_returns_pushed_events_in_order() {
        let proxy = EventProxy::new();
        proxy.push(TerminalEvent::Bell);
        proxy.push(TerminalEvent::Wakeup);
        let events = proxy.drain();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], TerminalEvent::Bell));
        assert!(matches!(events[1], TerminalEvent::Wakeup));
    }

    #[test]
    fn drain_clears_the_queue() {
        let proxy = EventProxy::new();
        proxy.push(TerminalEvent::Bell);
        let _ = proxy.drain();
        assert!(proxy.drain().is_empty());
    }

    #[test]
    fn send_event_title_changed() {
        let proxy = EventProxy::new();
        proxy.send_event(AlacrittyEvent::Title("vim".to_string()));
        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], TerminalEvent::TitleChanged(t) if t == "vim"));
    }

    #[test]
    fn send_event_pty_write_converts_to_bytes() {
        let proxy = EventProxy::new();
        proxy.send_event(AlacrittyEvent::PtyWrite("hi".to_string()));
        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], TerminalEvent::PtyWrite(b) if b == b"hi"));
    }

    #[test]
    fn send_event_library_exit_is_none_payload() {
        let proxy = EventProxy::new();
        proxy.send_event(AlacrittyEvent::Exit);
        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], TerminalEvent::Exit(None)));
    }

    #[test]
    fn send_event_child_exit_carries_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let proxy = EventProxy::new();
        let status = std::process::ExitStatus::from_raw(0);
        proxy.send_event(AlacrittyEvent::ChildExit(status));
        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TerminalEvent::Exit(Some(s)) => assert!(s.success()),
            other => panic!("expected Exit(Some(..)), got {other:?}"),
        }
    }

    #[test]
    fn clone_shares_the_same_queue() {
        let proxy = EventProxy::new();
        let clone = proxy.clone();
        proxy.push(TerminalEvent::Bell);
        let events = clone.drain();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn drain_recovers_after_lock_poison() {
        use std::sync::Arc;

        let proxy = Arc::new(EventProxy::new());

        proxy.push(TerminalEvent::Wakeup);

        let proxy_clone = Arc::clone(&proxy);
        let handle = std::thread::spawn(move || {
            let _guard = proxy_clone.events.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = handle.join();

        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TerminalEvent::Wakeup));
    }
}
