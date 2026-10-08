//! Pure shortcut-to-key-event state. No OS keyboard state is read here.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub(super) struct ShortcutState {
    // An ID is admitted only after BindShortcuts confirms that the portal bound it.
    bindings: HashMap<String, Vec<u32>>,
    // Keep the app's cancel mapping even when no physical cancel shortcut was
    // approved. Fault cancellation is local IPC, independent of that binding.
    cancel_keys: Vec<u32>,
    active: HashSet<String>,
    pressed_order: Vec<u32>,
}

impl ShortcutState {
    pub(super) fn new(bindings: HashMap<String, Vec<u32>>, cancel_keys: Vec<u32>) -> Self {
        Self {
            bindings,
            cancel_keys,
            ..Self::default()
        }
    }

    pub(super) fn held(&self) -> HashSet<u32> {
        self.active
            .iter()
            .filter_map(|id| self.bindings.get(id))
            .flatten()
            .copied()
            .collect()
    }

    /// Returns only changes to the synthetic state, in press order / reverse
    /// release order. Shared modifier keys remain down until their last user
    /// deactivates. Repeats and signals for unapproved IDs produce no events.
    pub(super) fn transition(
        &mut self,
        id: &str,
        pressed: bool,
        _timestamp: u64,
    ) -> Vec<(u32, bool)> {
        let Some(keys) = self.bindings.get(id).cloned() else {
            return Vec::new();
        };
        // The portal does not define a timestamp base or require monotonicity.
        // KDE currently emits zero; key repeat is deduplicated by active ID.
        // D-Bus sender/session checks, not timestamp guesses, admit the signal.
        let before = self.held();
        let changed = if pressed {
            self.active.insert(id.to_owned())
        } else {
            self.active.remove(id)
        };
        if !changed {
            return Vec::new();
        }
        let after = self.held();
        let changes: Vec<_> = if pressed {
            keys.into_iter()
                .filter(|vk| !before.contains(vk))
                .map(|vk| (vk, true))
                .collect()
        } else {
            keys.into_iter()
                .rev()
                .filter(|vk| !after.contains(vk))
                .map(|vk| (vk, false))
                .collect()
        };
        for &(vk, down) in &changes {
            if down {
                self.pressed_order.push(vk);
            } else {
                self.pressed_order.retain(|key| *key != vk);
            }
        }
        changes
    }

    /// Release every synthetic key when the session closes or its owner exits.
    pub(super) fn release_all(&mut self) -> Vec<(u32, bool)> {
        self.active.clear();
        self.pressed_order
            .drain(..)
            .rev()
            .map(|vk| (vk, false))
            .collect()
    }

    /// A lost portal must cancel active work before releasing PTT: a release
    /// alone tells Wispr to stop, transcribe and paste. These are app IPC keys,
    /// not OS input. Use the configured Dismiss mapping, or Wispr's Escape
    /// fallback when no Dismiss mapping was configured. Cancel even after PTT
    /// release: Wispr may still be processing or recording hands-free.
    pub(super) fn cancel_and_release_all(&mut self) -> Vec<(u32, bool)> {
        let held = self.held();
        let mut changes = Vec::new();
        if !self.bindings.is_empty() {
            let added: Vec<_> = self
                .cancel_keys
                .iter()
                .copied()
                .filter(|vk| !held.contains(vk))
                .collect();
            changes.extend(added.iter().map(|vk| (*vk, true)));
            changes.extend(added.iter().rev().map(|vk| (*vk, false)));
        }
        changes.extend(self.release_all());
        // SessionGuard and the worker both clean up; cancel only once. No
        // further action is admitted until a fresh state is installed.
        self.bindings.clear();
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ShortcutState {
        ShortcutState::new(
            HashMap::from([
                ("dictate".into(), vec![162, 91]),
                ("command".into(), vec![162, 164, 32]),
            ]),
            vec![27],
        )
    }

    #[test]
    fn press_and_release_preserve_chord_order() {
        let mut state = state();
        assert_eq!(
            state.transition("dictate", true, 10),
            vec![(162, true), (91, true)]
        );
        assert_eq!(state.held(), HashSet::from([162, 91]));
        assert_eq!(
            state.transition("dictate", false, 20),
            vec![(91, false), (162, false)]
        );
        assert!(state.held().is_empty());
    }

    #[test]
    fn autorepeat_is_not_a_second_press() {
        let mut state = state();
        state.transition("dictate", true, 10);
        assert!(state.transition("dictate", true, 11).is_empty());
        assert_eq!(state.held().len(), 2);
    }

    #[test]
    fn duplicate_release_is_ignored() {
        let mut state = state();
        state.transition("dictate", true, 1);
        state.transition("dictate", false, 2);
        assert!(state.transition("dictate", false, 3).is_empty());
    }

    #[test]
    fn unapproved_shortcuts_cannot_emit_keys() {
        let mut state = state();
        assert!(state.transition("unapproved", true, 10).is_empty());
        assert!(state.held().is_empty());
    }

    #[test]
    fn shared_modifiers_are_reference_counted() {
        let mut state = state();
        state.transition("dictate", true, 10);
        assert_eq!(
            state.transition("command", true, 11),
            vec![(164, true), (32, true)]
        );
        assert_eq!(state.transition("dictate", false, 12), vec![(91, false)]);
        assert!(state.held().contains(&162));
        assert_eq!(
            state.transition("command", false, 13),
            vec![(32, false), (164, false), (162, false)]
        );
    }

    #[test]
    fn timestamps_may_be_zero_equal_or_decrease() {
        let mut state = state();
        for (press, release) in [(0, 0), (42, 0), (u64::MAX, 1)] {
            assert_eq!(state.transition("dictate", true, press).len(), 2);
            assert_eq!(state.transition("dictate", false, release).len(), 2);
            assert!(state.held().is_empty());
        }
    }

    #[test]
    fn owner_loss_clears_stale_key_state() {
        let mut state = state();
        state.transition("dictate", true, 10);
        state.transition("command", true, 11);
        let released = state.release_all();
        assert_eq!(released.len(), 4);
        assert!(released.iter().all(|(_, pressed)| !pressed));
        assert!(state.held().is_empty());
        assert!(state.release_all().is_empty());
    }

    #[test]
    fn session_cleanup_releases_in_reverse_press_order() {
        let mut state = state();
        state.transition("dictate", true, 0);
        assert_eq!(state.release_all(), vec![(91, false), (162, false)]);
    }

    #[test]
    fn fault_cleanup_cancels_before_releasing_ptt() {
        let mut state = state();
        state.transition("dictate", true, 0);
        assert_eq!(
            state.cancel_and_release_all(),
            vec![(27, true), (27, false), (91, false), (162, false),]
        );
        assert!(state.held().is_empty());
        assert!(state.cancel_and_release_all().is_empty());
    }

    #[test]
    fn fault_cleanup_uses_configured_cancel_chord_without_repressing_held_modifiers() {
        let mut state = ShortcutState::new(
            HashMap::from([
                ("dictate".into(), vec![162, 91]),
                ("dismiss".into(), vec![162, 27]),
            ]),
            vec![162, 27],
        );
        state.transition("dictate", true, 0);
        assert_eq!(
            state.cancel_and_release_all(),
            vec![(27, true), (27, false), (91, false), (162, false),]
        );
    }

    #[test]
    fn fault_cleanup_cancels_after_ptt_release_and_only_once() {
        let mut state = state();
        state.transition("dictate", true, 0);
        state.transition("dictate", false, 0);
        assert_eq!(
            state.cancel_and_release_all(),
            vec![(27, true), (27, false)]
        );
        assert!(state.cancel_and_release_all().is_empty());
        assert!(state.transition("dictate", true, 0).is_empty());
    }

    #[test]
    fn fault_cleanup_keeps_an_unbound_custom_cancel_mapping() {
        let mut state = ShortcutState::new(
            HashMap::from([("dictate".into(), vec![162, 91])]),
            vec![17, 27],
        );
        state.transition("dictate", true, 0);
        assert_eq!(
            state.cancel_and_release_all(),
            vec![
                (17, true),
                (27, true),
                (27, false),
                (17, false),
                (91, false),
                (162, false),
            ]
        );
    }

    #[test]
    fn release_without_activation_is_ignored() {
        assert!(state().transition("dictate", false, 0).is_empty());
    }
}
