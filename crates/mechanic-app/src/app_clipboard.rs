//! OSC 52 permissions and session-safe clipboard delivery.

use super::*;
use std::collections::VecDeque;

use mechanic_config::{ClipboardConfig, ClipboardPolicy};
use mechanic_core::clipboard::{
    ClipboardOperation, ClipboardRequest, ClipboardType, MAX_CLIPBOARD_BYTES,
    MAX_CLIPBOARD_REQUESTS,
};

struct ScopedRequest {
    session: u64,
    request: ClipboardRequest,
    policy: ClipboardPolicy,
}

struct PendingRequest {
    id: u64,
    scoped: ScopedRequest,
}

enum Work {
    Execute(ScopedRequest, bool),
    Prompt { id: u64, session: u64, operation: ClipboardOperation, target: ClipboardType },
}

#[derive(Default)]
pub(super) struct ClipboardController {
    queue: VecDeque<ScopedRequest>,
    pending: Option<PendingRequest>,
    prompt: Option<crate::clipboard_platform::ClipboardPrompt>,
    prompt_window: Option<WindowId>,
    next_id: u64,
    payload_bytes: usize,
}

impl ClipboardController {
    fn enqueue(
        &mut self,
        session: u64,
        request: ClipboardRequest,
        config: ClipboardConfig,
    ) -> Result<(), ScopedRequest> {
        let mut policy = match request.operation() {
            ClipboardOperation::Read => config.read,
            ClipboardOperation::Write => config.write,
        };
        // Never queue a succession of approval sheets. An ask received while
        // another ask is queued or presented is denied for that request only.
        if policy == ClipboardPolicy::Ask
            && (self.pending.is_some()
                || self.queue.iter().any(|request| request.policy == ClipboardPolicy::Ask))
        {
            policy = ClipboardPolicy::Deny;
        }
        let scoped = ScopedRequest { session, request, policy };
        let bytes = scoped.request.payload_bytes();
        if self.queue.len() + usize::from(self.pending.is_some()) >= MAX_CLIPBOARD_REQUESTS
            || bytes > MAX_CLIPBOARD_BYTES.saturating_sub(self.payload_bytes)
        {
            return Err(scoped);
        }
        self.payload_bytes += bytes;
        self.queue.push_back(scoped);
        Ok(())
    }

    fn next(&mut self) -> Option<Work> {
        if self.pending.is_some() {
            return None;
        }
        let scoped = self.queue.pop_front()?;
        if scoped.policy != ClipboardPolicy::Ask {
            self.payload_bytes -= scoped.request.payload_bytes();
            let allowed = scoped.policy == ClipboardPolicy::Allow;
            return Some(Work::Execute(scoped, allowed));
        }
        let Some(id) = self.next_id.checked_add(1) else {
            self.payload_bytes -= scoped.request.payload_bytes();
            return Some(Work::Execute(scoped, false));
        };
        self.next_id = id;
        let work = Work::Prompt {
            id,
            session: scoped.session,
            operation: scoped.request.operation(),
            target: scoped.request.target(),
        };
        self.pending = Some(PendingRequest { id, scoped });
        Some(work)
    }

    fn resolve(&mut self, id: u64) -> Option<ScopedRequest> {
        if self.pending.as_ref()?.id != id {
            return None;
        }
        let pending = self.pending.take()?;
        self.payload_bytes -= pending.scoped.request.payload_bytes();
        // Disable callbacks before detaching the sheet. Already queued native
        // callbacks are harmless because the request ID is no longer pending.
        self.prompt.take();
        self.prompt_window = None;
        Some(pending.scoped)
    }

    fn cancel_if_invalid(
        &mut self,
        valid: impl FnOnce(u64, Option<WindowId>) -> bool,
    ) -> Option<ScopedRequest> {
        let pending = self.pending.as_ref()?;
        (!valid(pending.scoped.session, self.prompt_window))
            .then_some(pending.id)
            .and_then(|id| self.resolve(id))
    }

    fn finish_session(
        &mut self,
        session: u64,
        requests: Vec<ClipboardRequest>,
        config: ClipboardConfig,
    ) -> Vec<ScopedRequest> {
        if let Some(id) = self
            .pending
            .as_ref()
            .filter(|pending| pending.scoped.session == session)
            .map(|pending| pending.id)
        {
            self.resolve(id);
        }
        let mut final_stores = Vec::new();
        let mut remaining = VecDeque::new();
        while let Some(scoped) = self.queue.pop_front() {
            if scoped.session != session {
                remaining.push_back(scoped);
                continue;
            }
            self.payload_bytes -= scoped.request.payload_bytes();
            if scoped.policy == ClipboardPolicy::Allow
                && scoped.request.operation() == ClipboardOperation::Write
            {
                final_stores.push(scoped);
            }
        }
        self.queue = remaining;
        // Final output has no live requester to answer or ask. Permitted
        // stores still take effect before the application's close-on-exit
        // decision, even when another session owns the approval sheet.
        if config.write == ClipboardPolicy::Allow {
            final_stores.extend(
                requests
                    .into_iter()
                    .filter(|request| request.operation() == ClipboardOperation::Write)
                    .map(|request| ScopedRequest {
                        session,
                        request,
                        policy: ClipboardPolicy::Allow,
                    }),
            );
        }
        final_stores
    }
}

/// Kept injectable so tests exercise clipboard access without touching AppKit
/// or the user's pasteboard. Errors intentionally carry no clipboard text.
trait ClipboardAccess {
    fn read(&mut self, session: u64, target: ClipboardType) -> Option<String>;
    fn write(&mut self, session: u64, target: ClipboardType, text: String);
    fn reply(&mut self, session: u64, bytes: &[u8]);
}

fn execute_request(access: &mut impl ClipboardAccess, scoped: ScopedRequest, allowed: bool) {
    match scoped.request {
        ClipboardRequest::Store { target, text } => {
            if allowed && text.len() <= MAX_CLIPBOARD_BYTES {
                access.write(scoped.session, target, text);
            }
        }
        ClipboardRequest::Load { target, formatter } => {
            let text = if allowed {
                access.read(scoped.session, target).filter(|text| text.len() <= MAX_CLIPBOARD_BYTES)
            } else {
                None
            };
            access.reply(scoped.session, formatter(text.as_deref().unwrap_or("")).as_bytes());
        }
    }
}

impl App {
    fn clipboard_route(&self, session: u64) -> Option<(WindowId, PaneId)> {
        let (window, pane) = self.session_routes.get(&session)?.current;
        (self.windows.get(&window)?.pane_state(pane)?.session == session).then_some((window, pane))
    }

    fn clipboard_session_live(&self, session: u64) -> bool {
        self.clipboard_route(session)
            .and_then(|(window, pane)| self.windows.get(&window)?.pane_state(pane))
            .is_some_and(|pane| pane.exit_status.is_none())
    }

    pub(super) fn drain_clipboard_requests(
        &mut self,
        window: WindowId,
        pane: PaneId,
        session: u64,
        final_output: bool,
    ) {
        if self.clipboard_route(session) != Some((window, pane)) {
            return;
        }
        let requests = {
            let state = self.windows.get_mut(&window).expect("validated clipboard window");
            let state = if state.loaded_pane == pane {
                &mut state.pane
            } else {
                state.other_panes.get_mut(&pane).expect("validated clipboard pane")
            };
            state.terminal.drain_clipboard_requests()
        };
        if final_output {
            let stores =
                self.clipboard_controller.finish_session(session, requests, self.config.clipboard);
            for scoped in stores {
                execute_request(self, scoped, true);
            }
            self.service_clipboard_requests();
            return;
        }
        for request in requests {
            if let Err(scoped) =
                self.clipboard_controller.enqueue(session, request, self.config.clipboard)
            {
                execute_request(self, scoped, false);
            }
        }
        self.service_clipboard_requests();
    }

    pub(super) fn service_clipboard_requests(&mut self) {
        let windows = &self.windows;
        let routes = &self.session_routes;
        let cancelled = self.clipboard_controller.cancel_if_invalid(|session, prompt_window| {
            let Some((window, pane)) = routes.get(&session).map(|route| route.current) else {
                return false;
            };
            windows.get(&window).and_then(|state| state.pane_state(pane)).is_some_and(|pane| {
                pane.session == session
                    && pane.exit_status.is_none()
                    && prompt_window.is_none_or(|parent| parent == window)
            })
        });
        if let Some(scoped) = cancelled
            && self.clipboard_session_live(scoped.session)
        {
            // A moved pane receives an empty denial in its new window. A
            // frozen/dropped session has no requester left to answer.
            execute_request(self, scoped, false);
        }
        while let Some(work) = self.clipboard_controller.next() {
            match work {
                Work::Execute(scoped, allowed) => {
                    if self.clipboard_route(scoped.session).is_some()
                        && (scoped.request.operation() == ClipboardOperation::Write
                            || self.clipboard_session_live(scoped.session))
                    {
                        execute_request(self, scoped, allowed);
                    }
                }
                Work::Prompt { id, session, operation, target } => {
                    let Some((window, pane)) = self.clipboard_route(session) else {
                        self.clipboard_controller.resolve(id);
                        continue;
                    };
                    if !self.clipboard_session_live(session) {
                        self.clipboard_controller.resolve(id);
                        continue;
                    }
                    let proxy = self.proxy.clone();
                    let native_window = &self.windows[&window].window;
                    match crate::clipboard_platform::ClipboardPrompt::new(
                        native_window,
                        pane,
                        session,
                        operation,
                        target,
                        move |allowed| {
                            let _ = proxy.send_event(UserEvent::ClipboardDecision(id, allowed));
                        },
                    ) {
                        Ok(prompt) => {
                            self.clipboard_controller.prompt = Some(prompt);
                            self.clipboard_controller.prompt_window = Some(window);
                        }
                        Err(_) => {
                            // Missing native UI cannot authorize a read/write.
                            if let Some(scoped) = self.clipboard_controller.resolve(id) {
                                execute_request(self, scoped, false);
                            }
                            continue;
                        }
                    }
                    break;
                }
            }
        }
    }

    pub(super) fn dispatch_clipboard_decision(&mut self, id: u64, allowed: bool) {
        self.service_clipboard_requests();
        if let Some(scoped) = self.clipboard_controller.resolve(id)
            && self.clipboard_session_live(scoped.session)
        {
            execute_request(self, scoped, allowed);
        }
        self.service_clipboard_requests();
    }
}

impl ClipboardAccess for App {
    fn read(&mut self, session: u64, target: ClipboardType) -> Option<String> {
        let (window, pane) = self.clipboard_route(session)?;
        let state = self.windows.get_mut(&window)?;
        match target {
            ClipboardType::Clipboard => state.clipboard.as_mut()?.get_text().ok(),
            ClipboardType::Selection => state.pane_state(pane)?.primary_selection.clone(),
        }
    }

    fn write(&mut self, session: u64, target: ClipboardType, text: String) {
        let Some((window, pane)) = self.clipboard_route(session) else { return };
        let state = self.windows.get_mut(&window).expect("validated clipboard window");
        match target {
            ClipboardType::Clipboard => {
                if let Some(clipboard) = &mut state.clipboard {
                    let _ = clipboard.set_text(text);
                }
            }
            ClipboardType::Selection => {
                let pane = if state.loaded_pane == pane {
                    &mut state.pane
                } else {
                    state.other_panes.get_mut(&pane).expect("validated clipboard pane")
                };
                pane.primary_selection = Some(text);
            }
        }
    }

    fn reply(&mut self, session: u64, bytes: &[u8]) {
        let Some((window, pane)) = self.clipboard_route(session) else { return };
        let state = self.windows.get_mut(&window).expect("validated clipboard window");
        let pane = if state.loaded_pane == pane {
            &mut state.pane
        } else {
            state.other_panes.get_mut(&pane).expect("validated clipboard pane")
        };
        let _ = pane.terminal.write_clipboard_reply(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeClipboard {
        clipboard: String,
        primary: HashMap<u64, String>,
        reads: usize,
        writes: usize,
        replies: Vec<(u64, Vec<u8>)>,
    }

    impl ClipboardAccess for FakeClipboard {
        fn read(&mut self, session: u64, target: ClipboardType) -> Option<String> {
            self.reads += 1;
            match target {
                ClipboardType::Clipboard => Some(self.clipboard.clone()),
                ClipboardType::Selection => self.primary.get(&session).cloned(),
            }
        }
        fn write(&mut self, session: u64, target: ClipboardType, text: String) {
            self.writes += 1;
            match target {
                ClipboardType::Clipboard => self.clipboard = text,
                ClipboardType::Selection => {
                    self.primary.insert(session, text);
                }
            }
        }
        fn reply(&mut self, session: u64, bytes: &[u8]) {
            self.replies.push((session, bytes.to_vec()));
        }
    }

    fn load(target: ClipboardType) -> ClipboardRequest {
        ClipboardRequest::Load { target, formatter: Arc::new(|text| format!("reply:{text}")) }
    }

    fn store(target: ClipboardType, text: &str) -> ClipboardRequest {
        ClipboardRequest::Store { target, text: text.into() }
    }

    fn run(controller: &mut ClipboardController, clipboard: &mut FakeClipboard) {
        while let Some(Work::Execute(scoped, allowed)) = controller.next() {
            execute_request(clipboard, scoped, allowed);
        }
    }

    #[test]
    fn denied_read_returns_empty_without_reading_and_denied_write_does_not_touch_clipboard() {
        let mut clipboard = FakeClipboard { clipboard: "secret".into(), ..Default::default() };
        let mut controller = ClipboardController::default();
        let config = ClipboardConfig { read: ClipboardPolicy::Deny, write: ClipboardPolicy::Deny };
        assert!(controller.enqueue(7, load(ClipboardType::Clipboard), config).is_ok());
        assert!(
            controller.enqueue(7, store(ClipboardType::Clipboard, "replacement"), config).is_ok()
        );
        run(&mut controller, &mut clipboard);
        assert_eq!((clipboard.reads, clipboard.writes), (0, 0));
        assert_eq!(clipboard.clipboard, "secret");
        assert_eq!(clipboard.replies, [(7, b"reply:".to_vec())]);
    }

    #[test]
    fn approval_is_once_source_ordered_and_other_asks_are_denied_without_a_prompt_flood() {
        let mut clipboard = FakeClipboard { clipboard: "before".into(), ..Default::default() };
        let mut controller = ClipboardController::default();
        let config = ClipboardConfig::default();
        assert!(controller.enqueue(7, load(ClipboardType::Clipboard), config).is_ok());
        let Some(Work::Prompt { id, .. }) = controller.next() else { panic!("expected approval") };
        assert!(controller.enqueue(7, store(ClipboardType::Clipboard, "after"), config).is_ok());
        assert!(controller.enqueue(8, load(ClipboardType::Clipboard), config).is_ok());
        assert!(controller.next().is_none());
        execute_request(&mut clipboard, controller.resolve(id).unwrap(), true);
        assert!(controller.resolve(id).is_none());
        run(&mut controller, &mut clipboard);
        assert_eq!(clipboard.reads, 1);
        assert_eq!(clipboard.clipboard, "after");
        assert_eq!(clipboard.replies, [(7, b"reply:before".to_vec()), (8, b"reply:".to_vec())]);
        assert!(controller.enqueue(7, load(ClipboardType::Clipboard), config).is_ok());
        assert!(matches!(controller.next(), Some(Work::Prompt { .. })));
    }

    #[test]
    fn allowed_reads_and_writes_use_actual_injected_backend_and_keep_selection_local() {
        let mut clipboard = FakeClipboard::default();
        let mut controller = ClipboardController::default();
        let config =
            ClipboardConfig { read: ClipboardPolicy::Allow, write: ClipboardPolicy::Allow };
        for (session, request) in [
            (7, store(ClipboardType::Clipboard, "system")),
            (7, store(ClipboardType::Selection, "primary")),
            (7, load(ClipboardType::Clipboard)),
            (7, load(ClipboardType::Selection)),
            (8, load(ClipboardType::Selection)),
        ] {
            assert!(controller.enqueue(session, request, config).is_ok());
        }
        run(&mut controller, &mut clipboard);
        assert_eq!((clipboard.reads, clipboard.writes), (3, 2));
        assert_eq!(
            clipboard.replies,
            [
                (7, b"reply:system".to_vec()),
                (7, b"reply:primary".to_vec()),
                (8, b"reply:".to_vec()),
            ]
        );
    }

    #[test]
    fn queues_and_reply_text_are_bounded() {
        let mut controller = ClipboardController::default();
        let config = ClipboardConfig::default();
        for _ in 0..MAX_CLIPBOARD_REQUESTS {
            assert!(controller.enqueue(1, load(ClipboardType::Clipboard), config).is_ok());
        }
        assert!(controller.enqueue(1, load(ClipboardType::Clipboard), config).is_err());
        let mut clipboard =
            FakeClipboard { clipboard: "x".repeat(MAX_CLIPBOARD_BYTES + 1), ..Default::default() };
        execute_request(
            &mut clipboard,
            ScopedRequest {
                session: 1,
                request: load(ClipboardType::Clipboard),
                policy: ClipboardPolicy::Allow,
            },
            true,
        );
        assert_eq!(clipboard.replies, [(1, b"reply:".to_vec())]);
        let mut controller = ClipboardController::default();
        assert!(
            controller
                .enqueue(
                    1,
                    store(ClipboardType::Clipboard, &"x".repeat(MAX_CLIPBOARD_BYTES)),
                    config
                )
                .is_ok()
        );
        assert!(controller.enqueue(1, store(ClipboardType::Clipboard, "x"), config).is_err());
    }

    #[test]
    fn cancelling_stale_pending_request_invalidates_queued_approval() {
        let mut controller = ClipboardController::default();
        assert!(
            controller
                .enqueue(1, load(ClipboardType::Clipboard), ClipboardConfig::default())
                .is_ok()
        );
        let Some(Work::Prompt { id, .. }) = controller.next() else { panic!("expected prompt") };
        controller.resolve(id);
        assert!(controller.resolve(id).is_none());
        assert!(controller.pending.is_none());
        assert_eq!(controller.payload_bytes, 0);
    }

    #[test]
    fn frozen_or_dropped_session_cancels_nonempty_write_approval_and_unblocks_queue() {
        for session_still_live in [false, true] {
            let mut controller = ClipboardController::default();
            let ask = ClipboardConfig { read: ClipboardPolicy::Ask, write: ClipboardPolicy::Ask };
            assert!(
                controller
                    .enqueue(7, store(ClipboardType::Clipboard, "private pending text"), ask)
                    .is_ok()
            );
            let Some(Work::Prompt { id, .. }) = controller.next() else {
                panic!("expected approval")
            };
            assert!(
                controller
                    .enqueue(
                        8,
                        store(ClipboardType::Clipboard, "other live session"),
                        ClipboardConfig::default()
                    )
                    .is_ok()
            );
            let cancelled = controller.cancel_if_invalid(|session, _| {
                // Covers an exited pane retained in the tree and a removed
                // pane; either invalidates this request's original identity.
                session_still_live && session != 7
            });
            assert!(cancelled.is_some());
            assert!(controller.resolve(id).is_none());
            let mut clipboard =
                FakeClipboard { clipboard: "untouched".into(), ..Default::default() };
            run(&mut controller, &mut clipboard);
            assert_eq!(clipboard.clipboard, "other live session");
            assert_eq!(clipboard.writes, 1);
            assert_eq!(controller.payload_bytes, 0);
        }
    }

    #[test]
    fn moved_approval_is_cancelled_and_denied_in_same_live_session() {
        let mut controller = ClipboardController::default();
        assert!(
            controller
                .enqueue(7, load(ClipboardType::Clipboard), ClipboardConfig::default())
                .is_ok()
        );
        let Some(Work::Prompt { id, .. }) = controller.next() else { panic!("expected approval") };
        let old_window = WindowId::from(1);
        let new_window = WindowId::from(2);
        controller.prompt_window = Some(old_window);
        let cancelled = controller
            .cancel_if_invalid(|session, parent| session == 7 && parent == Some(new_window))
            .unwrap();
        let mut clipboard = FakeClipboard { clipboard: "secret".into(), ..Default::default() };
        execute_request(&mut clipboard, cancelled, false);
        assert_eq!(clipboard.reads, 0);
        assert_eq!(clipboard.replies, [(7, b"reply:".to_vec())]);
        assert!(controller.resolve(id).is_none());
    }

    #[test]
    fn final_output_executes_queued_and_new_stores_despite_own_or_other_pending_approval() {
        for dying_session in [7, 8] {
            let mut controller = ClipboardController::default();
            let config = ClipboardConfig::default();
            assert!(controller.enqueue(7, load(ClipboardType::Clipboard), config).is_ok());
            let Some(Work::Prompt { id, .. }) = controller.next() else {
                panic!("expected approval")
            };
            assert!(
                controller
                    .enqueue(
                        dying_session,
                        store(ClipboardType::Clipboard, "queued before EOF"),
                        config
                    )
                    .is_ok()
            );
            let stores = controller.finish_session(
                dying_session,
                vec![
                    load(ClipboardType::Clipboard),
                    store(ClipboardType::Clipboard, "final output"),
                ],
                config,
            );
            let mut clipboard =
                FakeClipboard { clipboard: "original".into(), ..Default::default() };
            for scoped in stores {
                execute_request(&mut clipboard, scoped, true);
            }
            assert_eq!(clipboard.clipboard, "final output");
            assert_eq!((clipboard.reads, clipboard.writes), (0, 2));
            assert!(clipboard.replies.is_empty());
            assert!(controller.queue.is_empty());
            assert_eq!(controller.pending.is_some(), dying_session != 7);
            if dying_session == 7 {
                assert!(controller.resolve(id).is_none());
            }
        }
    }
}
