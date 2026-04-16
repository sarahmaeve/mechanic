//! Terminal event handling.
//!
//! [`EventListener`] implements the `alacritty_terminal` event-listener trait
//! and funnels terminal events into a [`crossbeam_channel`] so the main thread
//! can poll them without blocking.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener as AlacrittyEventListener};

// ── Public event type ─────────────────────────────────────────────────────────

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
    /// The child process has exited.
    Exit,
    /// Bytes that the terminal wants written back to the PTY
    /// (e.g. responses to OSC colour queries).
    PtyWrite(Vec<u8>),
}

// ── EventProxy ────────────────────────────────────────────────────────────────

/// Shared state stored inside the event proxy.
///
/// We keep the pending event list behind an `Arc<Mutex<…>>` so that
/// `EventProxy` is `Clone + Send + Sync`, matching what `alacritty_terminal`
/// requires.
#[derive(Clone)]
pub struct EventProxy {
    events: Arc<Mutex<Vec<TerminalEvent>>>,
}

impl EventProxy {
    /// Create a new, empty event proxy.
    pub fn new() -> Self {
        Self { events: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Drain all pending events, returning them in arrival order.
    pub fn drain(&self) -> Vec<TerminalEvent> {
        let mut guard = self.events.lock().expect("EventProxy lock poisoned");
        std::mem::take(&mut *guard)
    }

    /// Push a single event onto the queue.
    fn push(&self, event: TerminalEvent) {
        let mut guard = self.events.lock().expect("EventProxy lock poisoned");
        guard.push(event);
    }
}

impl Default for EventProxy {
    fn default() -> Self {
        Self::new()
    }
}

// ── alacritty_terminal integration ───────────────────────────────────────────

impl AlacrittyEventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        match event {
            AlacrittyEvent::Title(title) => self.push(TerminalEvent::TitleChanged(title)),
            AlacrittyEvent::ResetTitle => self.push(TerminalEvent::TitleReset),
            AlacrittyEvent::Bell => self.push(TerminalEvent::Bell),
            AlacrittyEvent::Wakeup => self.push(TerminalEvent::Wakeup),
            AlacrittyEvent::Exit => self.push(TerminalEvent::Exit),
            AlacrittyEvent::PtyWrite(text) => self.push(TerminalEvent::PtyWrite(text.into_bytes())),
            // Mouse cursor shape changes, clipboard, colour requests, text-area
            // size requests, and cursor-blink changes are currently ignored.
            // They can be wired up when the renderer needs them.
            AlacrittyEvent::ChildExit(_) => self.push(TerminalEvent::Exit),
            _ => {}
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
    fn clone_shares_the_same_queue() {
        let proxy = EventProxy::new();
        let clone = proxy.clone();
        proxy.push(TerminalEvent::Bell);
        let events = clone.drain();
        assert_eq!(events.len(), 1);
    }
}
