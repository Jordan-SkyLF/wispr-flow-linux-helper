//! Small pure state machine for approved portal actions and logical IPC keys.
//! Portal timestamps are opaque; activation state, not timestamps, deduplicates.

use super::config::LogicalShortcuts;
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub(super) const PTT: &str = "ptt";
pub(super) const CANCEL: &str = "cancel";
// No physical-state query can detect a lost release in portal mode. A finite
// hold limit cancels safely rather than completing a potentially stuck PTT.
pub(super) const MAX_HOLD: Duration = Duration::from_secs(300);

#[derive(Debug, Default)]
pub(super) struct ShortcutState {
    pub logical: Option<LogicalShortcuts>,
    pub approved: HashSet<String>,
    active: HashSet<String>,
    pressed: Vec<u32>,
    held_since: Option<Instant>,
    pub work_possible: bool,
    pub faulted: bool,
}

impl ShortcutState {
    pub fn held(&self) -> HashSet<u32> {
        self.pressed.iter().copied().collect()
    }
    pub fn busy(&self) -> bool {
        self.work_possible || !self.active.is_empty()
    }
    pub fn expired(&self, now: Instant) -> bool {
        self.held_since
            .is_some_and(|start| now.saturating_duration_since(start) >= MAX_HOLD)
    }
    pub fn transition(&mut self, id: &str, down: bool, now: Instant) -> Vec<(u32, bool)> {
        if self.faulted || !self.approved.contains(id) || self.logical.is_none() {
            return Vec::new();
        }
        let changed = if down {
            self.active.insert(id.into())
        } else {
            self.active.remove(id)
        };
        if !changed {
            return Vec::new();
        }
        if self.active.is_empty() {
            self.held_since = None;
        } else if self.held_since.is_none() {
            self.held_since = Some(now);
        }
        if id == CANCEL {
            return if down {
                let changes = self.cancel_and_release(None);
                self.work_possible = false;
                changes
            } else {
                Vec::new()
            };
        }
        if id != PTT {
            return Vec::new();
        }
        if down {
            if self.active.contains(CANCEL) {
                return Vec::new();
            }
            self.work_possible = true;
            self.pressed = self.logical.as_ref().unwrap().ptt.clone();
            self.pressed.iter().map(|key| (*key, true)).collect()
        } else {
            self.release()
        }
    }
    fn release(&mut self) -> Vec<(u32, bool)> {
        self.pressed
            .drain(..)
            .rev()
            .map(|key| (key, false))
            .collect()
    }
    fn cancel_and_release(&mut self, replacement: Option<&[u32]>) -> Vec<(u32, bool)> {
        let Some(logical) = &self.logical else {
            return self.release();
        };
        let cancel = replacement.unwrap_or(&logical.cancel).to_vec();
        // After an app remap, old held keys can intercept the NEW Dismiss or
        // already contain it. Clear those keys before the new cancel pulse.
        // The pinned app can briefly enter Stopping here, then aborts; the
        // caller MUST latch insertion off before these frames are queued.
        let mut changes = if replacement.is_some() {
            self.release()
        } else {
            Vec::new()
        };
        let added: Vec<_> = cancel
            .iter()
            .copied()
            .filter(|key| !self.pressed.contains(key))
            .collect();
        changes.extend(added.iter().map(|key| (*key, true)));
        changes.extend(added.iter().rev().map(|key| (*key, false)));
        changes.extend(self.release());
        changes
    }
    pub fn fault(&mut self, replacement: Option<&[u32]>) -> Vec<(u32, bool)> {
        if self.faulted {
            return Vec::new();
        }
        self.faulted = true;
        let changes = if self.work_possible || !self.pressed.is_empty() {
            self.cancel_and_release(replacement)
        } else {
            self.release()
        };
        self.approved.clear();
        self.active.clear();
        self.held_since = None;
        changes
    }
    pub fn paste_completed(&mut self) {
        if self.active.is_empty() && self.pressed.is_empty() {
            self.work_possible = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> ShortcutState {
        ShortcutState {
            logical: Some(LogicalShortcuts {
                ptt: vec![162, 91],
                cancel: vec![27],
            }),
            approved: HashSet::from([PTT.into(), CANCEL.into()]),
            ..Default::default()
        }
    }
    #[test]
    fn ordinary_order_duplicates_and_unapproved_actions() {
        let mut s = state();
        let now = Instant::now();
        assert!(s.transition("attacker", true, now).is_empty());
        assert!(s.transition(PTT, false, now).is_empty());
        assert_eq!(s.transition(PTT, true, now), vec![(162, true), (91, true)]);
        assert!(s.transition(PTT, true, now).is_empty());
        assert_eq!(s.held(), HashSet::from([162, 91]));
        assert_eq!(
            s.transition(PTT, false, now),
            vec![(91, false), (162, false)]
        );
        assert!(s.transition(PTT, false, now).is_empty());
    }
    #[test]
    fn fault_cancels_before_ptt_release_and_is_idempotent() {
        let mut s = state();
        s.transition(PTT, true, Instant::now());
        assert_eq!(
            s.fault(None),
            vec![(27, true), (27, false), (91, false), (162, false)]
        );
        assert!(s.held().is_empty());
        assert!(s.fault(None).is_empty());
        assert!(s.transition(PTT, true, Instant::now()).is_empty());
    }
    #[test]
    fn processing_is_not_finished_when_ptt_is_released() {
        let mut s = state();
        let now = Instant::now();
        s.transition(PTT, true, now);
        s.transition(PTT, false, now);
        assert!(s.busy());
        assert_eq!(s.fault(None), vec![(27, true), (27, false)]);
    }
    #[test]
    fn normal_cancel_pulses_and_suppresses_held_physical_ptt() {
        let mut s = state();
        let now = Instant::now();
        s.transition(PTT, true, now);
        assert_eq!(
            s.transition(CANCEL, true, now),
            vec![(27, true), (27, false), (91, false), (162, false)]
        );
        assert!(s.held().is_empty());
        assert!(s.transition(PTT, true, now).is_empty());
        assert!(s.transition(PTT, false, now).is_empty());
        assert!(s.transition(PTT, true, now).is_empty()); // Cancel is still down.
        assert!(s.transition(CANCEL, false, now).is_empty());
        assert!(s.transition(PTT, false, now).is_empty());
        assert_eq!(s.transition(PTT, true, now).len(), 2);
    }
    #[test]
    fn shared_cancel_modifier_is_not_pressed_twice() {
        let mut s = state();
        s.logical.as_mut().unwrap().cancel = vec![162, 27];
        s.transition(PTT, true, Instant::now());
        assert_eq!(
            s.fault(None),
            vec![(27, true), (27, false), (91, false), (162, false)]
        );
    }
    #[test]
    fn new_cancel_mapping_and_subset_rearm_release_every_key() {
        let mut s = state();
        s.transition(PTT, true, Instant::now());
        assert_eq!(
            s.fault(Some(&[164, 27])),
            vec![
                (91, false),
                (162, false),
                (164, true),
                (27, true),
                (27, false),
                (164, false)
            ]
        );
        let mut s = state();
        s.transition(PTT, true, Instant::now());
        assert_eq!(
            s.fault(Some(&[162])),
            vec![(91, false), (162, false), (162, true), (162, false)]
        );
        assert!(s.held().is_empty());
    }
    #[test]
    fn missing_release_has_a_finite_fail_closed_boundary() {
        let mut s = state();
        let now = Instant::now();
        s.transition(PTT, true, now);
        assert!(!s.expired(now + MAX_HOLD - Duration::from_millis(1)));
        assert!(s.expired(now + MAX_HOLD));
        assert_eq!(s.fault(None).len(), 4);
        assert!(!s.expired(now + MAX_HOLD));
    }
    #[test]
    fn missed_cancel_release_is_bounded_without_stuck_logical_keys() {
        let mut s = state();
        let now = Instant::now();
        s.transition(CANCEL, true, now);
        assert!(s.held().is_empty());
        assert!(s.expired(now + MAX_HOLD));
    }
    #[test]
    fn successful_paste_allows_idle_configuration_updates() {
        let mut s = state();
        let now = Instant::now();
        s.transition(PTT, true, now);
        s.paste_completed();
        assert!(s.busy());
        s.transition(PTT, false, now);
        s.paste_completed();
        assert!(!s.busy());
    }
}
