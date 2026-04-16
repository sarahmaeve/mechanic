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
    /// The child shell process has exited.
    ///
    /// The payload carries the real exit status when we have one — the
    /// common case, from `AlacrittyEvent::ChildExit`.  A `None` payload
    /// corresponds to `AlacrittyEvent::Exit`, a library-internal "please
    /// exit" signal that carries no status of its own and is rarely seen
    /// in practice.  Callers usually treat `None` as a successful exit.
    Exit(Option<std::process::ExitStatus>),
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
        // Recovery via `into_inner` is safe: the queue is a plain `Vec` with no
        // partial-state invariants — the only mutations are `push` and `take`, both
        // of which leave it in a consistent (possibly empty) state.
        let mut guard = self.events.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    }

    /// Push a single event onto the queue.
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

// ── alacritty_terminal integration ───────────────────────────────────────────

impl AlacrittyEventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        match event {
            AlacrittyEvent::Title(title) => self.push(TerminalEvent::TitleChanged(title)),
            AlacrittyEvent::ResetTitle => self.push(TerminalEvent::TitleReset),
            AlacrittyEvent::Bell => self.push(TerminalEvent::Bell),
            AlacrittyEvent::Wakeup => self.push(TerminalEvent::Wakeup),
            // `Exit` is a library-internal "please exit" request — no
            // exit code is available.  `ChildExit(status)` is the real
            // shell-exited event and carries the status we need for
            // close-on-zero policy.
            AlacrittyEvent::Exit => self.push(TerminalEvent::Exit(None)),
            AlacrittyEvent::ChildExit(status) => self.push(TerminalEvent::Exit(Some(status))),
            AlacrittyEvent::PtyWrite(text) => self.push(TerminalEvent::PtyWrite(text.into_bytes())),
            // Mouse cursor shape changes, clipboard, colour requests, text-area
            // size requests, and cursor-blink changes are currently ignored.
            // They can be wired up when the renderer needs them.
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
    fn send_event_library_exit_is_none_payload() {
        // AlacrittyEvent::Exit is a library-internal request without a
        // status — we surface it as Exit(None) so callers can tell it
        // apart from a real child exit.
        let proxy = EventProxy::new();
        proxy.send_event(AlacrittyEvent::Exit);
        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], TerminalEvent::Exit(None)));
    }

    #[test]
    fn send_event_child_exit_carries_status() {
        // ChildExit(status) must propagate the status so the app layer
        // can decide whether to close or freeze based on the exit code.
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

        // Seed an event before poisoning so we can verify recovery returns it.
        proxy.push(TerminalEvent::Wakeup);

        // Poison the mutex: spawn a thread that panics while holding the lock.
        let proxy_clone = Arc::clone(&proxy);
        let handle = std::thread::spawn(move || {
            let _guard = proxy_clone.events.lock().unwrap();
            panic!("intentional poison");
        });
        // The join will return an Err because the thread panicked — that's expected.
        let _ = handle.join();

        // `drain` must not panic even though the lock is poisoned, and must
        // return the event that was pushed before the poison occurred.
        let events = proxy.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TerminalEvent::Wakeup));
    }
}
