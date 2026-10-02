//! Completion alerts are decided once, when parser-produced completion events arrive.

use std::collections::HashMap;
use std::time::Duration;

use mechanic_config::NotificationsConfig;
use mechanic_core::CommandCompletion;
use winit::window::WindowId;

/// A terminal's identity includes its generation so respawns can restart IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompletionScope {
    pub window: WindowId,
    pub pane: u64,
    pub session: u64,
}

/// A snapshot safe to carry through a native authorization callback.
/// It deliberately has no command text, working directory, or shell output.
#[derive(Debug, Clone)]
pub struct CompletionNotification {
    pub scope: CompletionScope,
    command_id: u64,
    body: String,
}

impl CompletionNotification {
    pub fn title(&self) -> &'static str {
        "Mechanic"
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn identifier(&self) -> String {
        format!(
            "mechanic-command-{}-{}-{}-{}",
            u64::from(self.scope.window),
            self.scope.pane,
            self.scope.session,
            self.command_id
        )
    }
}

pub struct NotificationPolicy {
    enabled: bool,
    minimum: Duration,
    last_completion: HashMap<CompletionScope, u64>,
}

impl NotificationPolicy {
    pub fn new(config: &NotificationsConfig) -> Self {
        Self {
            enabled: config.enabled,
            minimum: config.min_command_duration(),
            last_completion: HashMap::new(),
        }
    }

    /// Consume even suppressed IDs: later focus changes cannot replay a command.
    pub fn consider(
        &mut self,
        scope: CompletionScope,
        focused: bool,
        completion: &CommandCompletion,
    ) -> Option<CompletionNotification> {
        let last = self.last_completion.entry(scope).or_insert(0);
        if completion.id <= *last {
            return None;
        }
        *last = completion.id;
        if !self.enabled || focused || completion.duration < self.minimum {
            return None;
        }
        let elapsed = completion.duration.as_secs();
        let unit = if elapsed == 1 { "second" } else { "seconds" };
        let body = match completion.exit_status {
            Some(0) => format!("Command completed successfully after {elapsed} {unit}."),
            Some(status) => {
                format!("Command exited with status {status} after {elapsed} {unit}.")
            }
            None => format!("Command finished after {elapsed} {unit} (exit status unknown)."),
        };
        Some(CompletionNotification { scope, command_id: completion.id, body })
    }

    /// Release closed panes and old session generations.
    pub fn forget_scope(&mut self, scope: CompletionScope) {
        self.last_completion.remove(&scope);
    }

    pub fn forget_window(&mut self, window: WindowId) {
        self.last_completion.retain(|scope, _| scope.window != window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(window: u64, pane: u64, session: u64) -> CompletionScope {
        CompletionScope { window: WindowId::from(window), pane, session }
    }

    fn completion(id: u64, millis: u64, status: Option<i32>) -> CommandCompletion {
        CommandCompletion {
            id,
            duration: Duration::from_millis(millis),
            exit_status: status,
            cwd: Some("/private/secret-command-token".into()),
            cwd_host: Some("private-host".into()),
        }
    }

    fn policy() -> NotificationPolicy {
        NotificationPolicy::new(&NotificationsConfig {
            enabled: true,
            ..NotificationsConfig::default()
        })
    }

    #[test]
    fn default_opt_out_and_foreground_suppress_alerts() {
        let event = completion(1, 60_000, Some(0));
        let id = scope(1, 0, 0);
        assert!(
            NotificationPolicy::new(&NotificationsConfig::default())
                .consider(id, false, &event)
                .is_none()
        );
        let mut policy = policy();
        assert!(policy.consider(id, true, &event).is_none());
        assert!(policy.consider(id, false, &event).is_none());
    }

    #[test]
    fn background_threshold_includes_exact_boundary() {
        let mut policy = policy();
        let id = scope(1, 0, 0);
        assert!(policy.consider(id, false, &completion(1, 9_999, Some(0))).is_none());
        assert!(policy.consider(id, false, &completion(2, 10_000, Some(0))).is_some());
    }

    #[test]
    fn duplicate_and_out_of_order_ids_are_not_replayed() {
        let mut policy = policy();
        let id = scope(1, 0, 0);
        assert!(policy.consider(id, false, &completion(2, 20_000, Some(0))).is_some());
        assert!(policy.consider(id, false, &completion(2, 20_000, Some(0))).is_none());
        assert!(policy.consider(id, false, &completion(1, 20_000, Some(0))).is_none());
        assert!(policy.consider(id, false, &completion(3, 20_000, Some(0))).is_some());
    }

    #[test]
    fn window_pane_and_session_are_independent_identities() {
        let mut policy = policy();
        let event = completion(1, 20_000, Some(0));
        let scopes = [scope(1, 0, 0), scope(2, 0, 0), scope(1, 1, 0), scope(1, 0, 1)];
        let mut identifiers = Vec::new();
        for id in scopes {
            let alert = policy.consider(id, false, &event).unwrap();
            assert_eq!(alert.scope, id);
            identifiers.push(alert.identifier());
        }
        identifiers.sort();
        identifiers.dedup();
        assert_eq!(identifiers.len(), scopes.len());
        policy.forget_scope(scopes[2]);
        assert_eq!(policy.last_completion.len(), 3);
        policy.forget_window(WindowId::from(1));
        assert_eq!(policy.last_completion.len(), 1);
    }

    #[test]
    fn known_failure_and_unknown_status_have_honest_private_wording() {
        let mut policy = policy();
        let id = scope(1, 0, 0);
        for (command_id, status, expected) in [
            (1, Some(0), "Command completed successfully after 20 seconds."),
            (2, Some(127), "Command exited with status 127 after 20 seconds."),
            (3, None, "Command finished after 20 seconds (exit status unknown)."),
        ] {
            let alert =
                policy.consider(id, false, &completion(command_id, 20_000, status)).unwrap();
            assert_eq!(alert.body(), expected);
            assert_eq!(alert.title(), "Mechanic");
            assert!(!format!("{alert:?}").contains("secret-command-token"));
            assert!(!alert.body().contains("private-host"));
        }
    }
}
