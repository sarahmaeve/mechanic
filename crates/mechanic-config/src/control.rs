//! Local automation endpoint preferences.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    /// Accept local control requests from the current user.
    pub enabled: bool,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
