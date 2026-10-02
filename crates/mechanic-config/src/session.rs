//! Session restoration preferences. Session data lives separately from config.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Restore window layouts and directories with fresh configured shells.
    pub restore: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { restore: true }
    }
}
