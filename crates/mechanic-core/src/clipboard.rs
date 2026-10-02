//! Bounded OSC 52 requests. Payloads are never included in debug output.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

pub use alacritty_terminal::term::ClipboardType;

/// Maximum decoded clipboard text held for terminal requests or a read reply.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Bound pending requests even when their payloads are empty.
pub const MAX_CLIPBOARD_REQUESTS: usize = 16;

pub type ClipboardFormatter = Arc<dyn Fn(&str) -> String + Send + Sync + 'static>;

#[derive(Clone)]
pub enum ClipboardRequest {
    Store { target: ClipboardType, text: String },
    Load { target: ClipboardType, formatter: ClipboardFormatter },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOperation {
    Read,
    Write,
}

impl ClipboardRequest {
    pub fn operation(&self) -> ClipboardOperation {
        match self {
            Self::Store { .. } => ClipboardOperation::Write,
            Self::Load { .. } => ClipboardOperation::Read,
        }
    }

    pub fn target(&self) -> ClipboardType {
        match self {
            Self::Store { target, .. } | Self::Load { target, .. } => *target,
        }
    }

    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::Store { text, .. } => text.len(),
            Self::Load { .. } => 0,
        }
    }
}

impl fmt::Debug for ClipboardRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardRequest")
            .field("operation", &self.operation())
            .field("target", &self.target())
            .field("payload_bytes", &self.payload_bytes())
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct ClipboardRequests {
    requests: VecDeque<ClipboardRequest>,
    payload_bytes: usize,
}

impl ClipboardRequests {
    /// Rejected reads must receive an empty formatted response at the caller.
    pub fn push(&mut self, request: ClipboardRequest) -> Result<(), ClipboardRequest> {
        let bytes = request.payload_bytes();
        if self.requests.len() >= MAX_CLIPBOARD_REQUESTS
            || bytes > MAX_CLIPBOARD_BYTES.saturating_sub(self.payload_bytes)
        {
            return Err(request);
        }
        self.payload_bytes += bytes;
        self.requests.push_back(request);
        Ok(())
    }

    pub fn drain(&mut self) -> Vec<ClipboardRequest> {
        self.payload_bytes = 0;
        self.requests.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(bytes: usize) -> ClipboardRequest {
        ClipboardRequest::Store { target: ClipboardType::Clipboard, text: "x".repeat(bytes) }
    }

    #[test]
    fn queue_bounds_count_and_aggregate_payload_and_recovers_after_drain() {
        let mut queue = ClipboardRequests::default();
        queue.push(store(MAX_CLIPBOARD_BYTES)).unwrap();
        assert!(queue.push(store(1)).is_err());
        assert_eq!(queue.drain().len(), 1);
        for _ in 0..MAX_CLIPBOARD_REQUESTS {
            queue.push(store(0)).unwrap();
        }
        assert!(queue.push(store(0)).is_err());
        assert_eq!(queue.drain().len(), MAX_CLIPBOARD_REQUESTS);
        queue.push(store(1)).unwrap();
    }

    #[test]
    fn debug_redacts_clipboard_text() {
        let request = ClipboardRequest::Store {
            target: ClipboardType::Selection,
            text: "private clipboard content".into(),
        };
        assert!(!format!("{request:?}").contains("private"));
    }
}
