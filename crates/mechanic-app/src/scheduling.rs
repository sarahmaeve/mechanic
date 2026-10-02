//! Independent queues for parser work and paced presentation.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const CONTENT_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) struct ParseQueue<T> {
    pending: VecDeque<T>,
}

impl<T: Eq> ParseQueue<T> {
    pub(crate) fn new() -> Self {
        Self { pending: VecDeque::new() }
    }

    pub(crate) fn enqueue(&mut self, id: T) {
        if !self.pending.contains(&id) {
            self.pending.push_back(id);
        }
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        self.pending.pop_front()
    }

    pub(crate) fn remove(&mut self, id: &T) {
        self.pending.retain(|pending| pending != id);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

pub(crate) struct FramePacer {
    last_render: Instant,
    redraw_pending: bool,
    occluded: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameSchedule {
    Idle,
    WaitUntil(Instant),
    Redraw,
}

impl FramePacer {
    pub(crate) fn new(now: Instant) -> Self {
        Self { last_render: now, redraw_pending: false, occluded: false }
    }

    pub(crate) fn request_redraw(&mut self) -> bool {
        if self.occluded {
            return false;
        }
        self.redraw_pending = true;
        true
    }

    pub(crate) fn set_occluded(&mut self, occluded: bool) {
        self.occluded = occluded;
    }

    pub(crate) fn is_occluded(&self) -> bool {
        self.occluded
    }

    pub(crate) fn rendered(&mut self, now: Instant) {
        self.last_render = now;
        self.redraw_pending = false;
    }

    pub(crate) fn last_render(&self) -> Instant {
        self.last_render
    }

    pub(crate) fn schedule(
        &mut self,
        now: Instant,
        dirty: bool,
        animation_deadline: Option<Instant>,
    ) -> FrameSchedule {
        if self.occluded || self.redraw_pending {
            return FrameSchedule::Idle;
        }
        let content_deadline = dirty.then_some(self.last_render + CONTENT_INTERVAL);
        let deadline = match (content_deadline, animation_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match deadline {
            Some(deadline) if now >= deadline => {
                self.request_redraw();
                FrameSchedule::Redraw
            }
            Some(deadline) => FrameSchedule::WaitUntil(deadline),
            None => FrameSchedule::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicated_round_robin_continues_without_presentation() {
        let mut queue = ParseQueue::new();
        queue.enqueue("busy");
        queue.enqueue("quiet");
        queue.enqueue("busy");
        assert_eq!(queue.pop(), Some("busy"));
        queue.enqueue("busy"); // More bytes remain, even without any redraw.
        assert_eq!(queue.pop(), Some("quiet"));
        assert_eq!(queue.pop(), Some("busy"));
        queue.enqueue("busy");
        queue.remove(&"busy"); // Freeze or close.
        assert!(queue.is_empty());
        queue.enqueue("busy"); // Restart schedules the replacement shell.
        assert_eq!(queue.pop(), Some("busy"));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn dirty_deadline_stays_anchored_under_repeated_parser_turns() {
        let now = Instant::now();
        let mut pacer = FramePacer::new(now);
        for ms in 0..16 {
            assert_eq!(
                pacer.schedule(now + Duration::from_millis(ms), true, None),
                FrameSchedule::WaitUntil(now + CONTENT_INTERVAL),
            );
        }
        assert_eq!(pacer.schedule(now + CONTENT_INTERVAL, true, None), FrameSchedule::Redraw);
        // OS-suppressed redraws must not leave an expired wake deadline.
        assert_eq!(
            pacer.schedule(now + Duration::from_secs(1), true, Some(now)),
            FrameSchedule::Idle,
        );
        let rendered = now + Duration::from_secs(2);
        pacer.rendered(rendered);
        assert_eq!(pacer.schedule(rendered, false, None), FrameSchedule::Idle);
        assert_eq!(
            pacer.schedule(rendered, true, None),
            FrameSchedule::WaitUntil(rendered + CONTENT_INTERVAL),
        );
    }

    #[test]
    fn animation_deadlines_and_immediate_input_respect_pending_redraws() {
        let now = Instant::now();
        let mut pacer = FramePacer::new(now);
        let animation = now + Duration::from_millis(33);
        assert_eq!(
            pacer.schedule(now, false, Some(animation)),
            FrameSchedule::WaitUntil(animation)
        );
        assert_eq!(pacer.schedule(animation, false, Some(animation)), FrameSchedule::Redraw);
        pacer.rendered(animation);
        pacer.request_redraw(); // Immediate keyboard/resize request.
        assert_eq!(pacer.schedule(animation, true, Some(animation)), FrameSchedule::Idle);
    }

    #[test]
    fn failed_presentations_retry_at_content_cadence_without_animations() {
        let mut attempted_at = Instant::now();
        let mut pacer = FramePacer::new(attempted_at);
        pacer.request_redraw();
        // Failed full or cached fallback renders leave content dirty. Each
        // delivered RedrawRequested clears its pending request and paces retries.
        for _ in 0..3 {
            pacer.rendered(attempted_at);
            let retry = attempted_at + CONTENT_INTERVAL;
            assert_eq!(pacer.schedule(attempted_at, true, None), FrameSchedule::WaitUntil(retry));
            assert_eq!(
                pacer.schedule(retry - Duration::from_millis(1), true, None),
                FrameSchedule::WaitUntil(retry)
            );
            assert_eq!(pacer.schedule(retry, true, None), FrameSchedule::Redraw);
            assert_eq!(pacer.schedule(retry, true, None), FrameSchedule::Idle);
            attempted_at = retry;
        }
        // Recovery clears dirty content; an otherwise idle window sleeps again.
        pacer.rendered(attempted_at);
        assert_eq!(pacer.schedule(attempted_at, false, None), FrameSchedule::Idle);
        assert_eq!(
            pacer.schedule(attempted_at + Duration::from_secs(1), false, None),
            FrameSchedule::Idle
        );
    }

    #[test]
    fn occluded_windows_keep_parsing_without_presentation_deadlines() {
        let now = Instant::now();
        let mut pacer = FramePacer::new(now);
        let mut parsers = ParseQueue::new();
        pacer.set_occluded(true);
        assert!(!pacer.request_redraw());
        parsers.enqueue("hidden");
        let id = parsers.pop().unwrap();
        parsers.enqueue(id); // Parser continuation needs no presentation.
        let later = now + Duration::from_secs(2);
        assert_eq!(pacer.schedule(later, true, Some(now)), FrameSchedule::Idle);
        assert_eq!(parsers.pop(), Some("hidden"));
        pacer.set_occluded(false);
        assert!(pacer.request_redraw()); // Restoration explicitly requests a frame.
        assert_eq!(pacer.schedule(later, true, Some(now)), FrameSchedule::Idle);
        pacer.rendered(later);
        assert_eq!(pacer.schedule(later, false, None), FrameSchedule::Idle);
    }
}
