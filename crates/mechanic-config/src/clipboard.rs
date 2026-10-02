//! Independent permissions for terminal-initiated clipboard reads and writes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardPolicy {
    Ask,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub read: ClipboardPolicy,
    pub write: ClipboardPolicy,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self { read: ClipboardPolicy::Ask, write: ClipboardPolicy::Allow }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_require_read_approval_and_allow_copy() {
        assert_eq!(ClipboardConfig::default().read, ClipboardPolicy::Ask);
        assert_eq!(ClipboardConfig::default().write, ClipboardPolicy::Allow);
        let config: crate::Config = toml::from_str("[clipboard]\nwrite = 'deny'").unwrap();
        assert_eq!(config.clipboard.read, ClipboardPolicy::Ask);
        assert_eq!(config.clipboard.write, ClipboardPolicy::Deny);
    }

    #[test]
    fn all_policy_combinations_roundtrip() {
        for read in [ClipboardPolicy::Ask, ClipboardPolicy::Allow, ClipboardPolicy::Deny] {
            for write in [ClipboardPolicy::Ask, ClipboardPolicy::Allow, ClipboardPolicy::Deny] {
                let original = ClipboardConfig { read, write };
                let encoded = toml::to_string(&original).unwrap();
                assert_eq!(toml::from_str::<ClipboardConfig>(&encoded).unwrap(), original);
            }
        }
    }
}
