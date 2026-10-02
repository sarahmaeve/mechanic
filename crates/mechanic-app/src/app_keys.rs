//! Match key releases to forwarded presses and track sided modifiers.
use super::*;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct KeyTracking {
    forwarded: HashMap<PhysicalKey, (PaneId, u64)>,
    modifiers: HashSet<KeyCode>,
}

fn modifier(code: KeyCode) -> Option<ModifiersState> {
    match code {
        KeyCode::ShiftLeft | KeyCode::ShiftRight => Some(ModifiersState::SHIFT),
        KeyCode::ControlLeft | KeyCode::ControlRight => Some(ModifiersState::CONTROL),
        KeyCode::AltLeft | KeyCode::AltRight => Some(ModifiersState::ALT),
        KeyCode::SuperLeft | KeyCode::SuperRight => Some(ModifiersState::SUPER),
        _ => None,
    }
}

impl KeyTracking {
    pub(super) fn begin(
        &mut self,
        key: PhysicalKey,
        state: ElementState,
        repeat: bool,
        target: (PaneId, u64),
    ) -> bool {
        match state {
            ElementState::Released => self.forwarded.remove(&key) == Some(target),
            ElementState::Pressed => {
                if !repeat {
                    self.forwarded.remove(&key);
                }
                true
            }
        }
    }

    pub(super) fn forwarded(&mut self, key: PhysicalKey, target: (PaneId, u64)) {
        self.forwarded.insert(key, target);
    }

    pub(super) fn repeat_allowed(&self, key: PhysicalKey, target: (PaneId, u64)) -> bool {
        self.forwarded.get(&key) == Some(&target)
    }

    pub(super) fn clear_forwarded(&mut self) {
        self.forwarded.clear();
    }
    pub(super) fn clear(&mut self) {
        self.forwarded.clear();
        self.modifiers.clear();
    }

    pub(super) fn reconcile(&mut self, current: ModifiersState) {
        self.modifiers.retain(|key| modifier(*key).is_some_and(|flag| current.contains(flag)));
    }

    pub(super) fn update_modifiers(
        &mut self,
        key: PhysicalKey,
        state: ElementState,
        mut current: ModifiersState,
    ) -> ModifiersState {
        let PhysicalKey::Code(code) = key else { return current };
        let Some(flag) = modifier(code) else { return current };
        match state {
            ElementState::Pressed => {
                self.modifiers.insert(code);
            }
            ElementState::Released => {
                self.modifiers.remove(&code);
            }
        }
        current.set(flag, self.modifiers.iter().any(|code| modifier(*code) == Some(flag)));
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_requires_matching_press_and_session_not_just_pane_id() {
        let mut keys = KeyTracking::default();
        let key = PhysicalKey::Code(KeyCode::KeyA);
        assert!(keys.begin(key, ElementState::Pressed, false, (1, 2)));
        assert!(!keys.begin(key, ElementState::Released, false, (1, 2)));
        keys.forwarded(key, (1, 2));
        assert!(!keys.repeat_allowed(key, (1, 3)));
        assert!(!keys.begin(key, ElementState::Released, false, (1, 3)));
        keys.forwarded(key, (1, 2));
        assert!(keys.begin(key, ElementState::Released, false, (1, 2)));
        assert!(!keys.begin(key, ElementState::Released, false, (1, 2)));
    }
    #[test]
    fn consumed_shortcut_and_focus_switch_cannot_leak_release() {
        let mut keys = KeyTracking::default();
        let key = PhysicalKey::Code(KeyCode::KeyC);
        keys.forwarded(key, (1, 2));
        keys.begin(key, ElementState::Pressed, false, (1, 2));
        assert!(!keys.begin(key, ElementState::Released, false, (1, 2)));
        keys.forwarded(key, (1, 2));
        keys.clear_forwarded();
        assert!(!keys.repeat_allowed(key, (1, 2)));
        assert!(!keys.begin(key, ElementState::Released, false, (1, 2)));
    }
    #[test]
    fn modifier_release_preserves_other_side_and_reconciles_focus() {
        let mut keys = KeyTracking::default();
        let left = PhysicalKey::Code(KeyCode::ShiftLeft);
        let right = PhysicalKey::Code(KeyCode::ShiftRight);
        assert_eq!(
            keys.update_modifiers(left, ElementState::Pressed, ModifiersState::empty()),
            ModifiersState::SHIFT
        );
        keys.update_modifiers(right, ElementState::Pressed, ModifiersState::SHIFT);
        assert_eq!(
            keys.update_modifiers(left, ElementState::Released, ModifiersState::SHIFT),
            ModifiersState::SHIFT
        );
        assert_eq!(
            keys.update_modifiers(right, ElementState::Released, ModifiersState::SHIFT),
            ModifiersState::empty()
        );
        keys.update_modifiers(left, ElementState::Pressed, ModifiersState::empty());
        keys.reconcile(ModifiersState::empty());
        assert_eq!(
            keys.update_modifiers(right, ElementState::Released, ModifiersState::SHIFT),
            ModifiersState::empty()
        );
    }
}
