//! Cancellation architecture.
//!
//! animist's watch-based [`Cancellation`] is the substrate: clones share
//! one monotonic flag, and `cancelled()`/`race` do the waiting. On top: a
//! cancel-on-drop [`CancelTrigger`] and the slotted [`CancelMap`] for
//! per-key grouped cancellation with pre-cancel. Parent↔child
//! propagation is the caller's awaited select (see `subagent::run_linked`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use animist::agent::Cancellation;

/// The setting half of a cancellation that fires on drop, so a scope that
/// exits early (panic, `?`, early return) stops what it started.
pub struct CancelTrigger(Cancellation);

impl CancelTrigger {
    pub fn new(cancellation: Cancellation) -> Self {
        Self(cancellation)
    }

    /// A clone sharing this trigger's state, for the run to await.
    pub fn cancellation(&self) -> Cancellation {
        self.0.clone()
    }

    pub fn cancel(self) {}
}

impl Drop for CancelTrigger {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Names one registration inside a key's list so its owner can retire it
/// without disturbing the others registered under the same key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CancelSlot(u64);

struct Slotted {
    slot: CancelSlot,
    trigger: Option<CancelTrigger>,
}

#[derive(Default)]
struct Entry {
    registrations: Vec<Slotted>,
    cancelled: bool,
}

pub struct CancelMap<K> {
    entries: Mutex<HashMap<K, Entry>>,
    next_slot: AtomicU64,
}

impl<K: Eq + std::hash::Hash> Default for CancelMap<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + std::hash::Hash> CancelMap<K> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_slot: AtomicU64::new(0),
        }
    }

    /// Registers `trigger` under `id`, alongside any already there, and
    /// returns the slot to hand back to [`retire`](Self::retire).
    pub fn insert(&self, id: K, trigger: CancelTrigger) -> CancelSlot {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let slot = CancelSlot(self.next_slot.fetch_add(1, Ordering::Relaxed));
        let entry = map.entry(id).or_default();
        let trigger = if entry.cancelled {
            drop(trigger);
            None
        } else {
            Some(trigger)
        };
        entry.registrations.push(Slotted { slot, trigger });
        slot
    }

    /// Retires one registration, dropping its trigger when it is still
    /// active and leaving its siblings alone.
    pub fn retire(&self, id: &K, slot: CancelSlot) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = map.get_mut(id) else {
            return;
        };
        entry
            .registrations
            .retain(|registration| registration.slot != slot);
        if entry.registrations.is_empty() {
            map.remove(id);
        }
    }

    /// Cancels everything under `id` and marks later siblings cancelled.
    /// The entry stays until every registered sibling retires.
    pub fn cancel_or_precancel(&self, id: K) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(id).or_default();
        entry.cancelled = true;
        for registration in &mut entry.registrations {
            drop(registration.trigger.take());
        }
    }

    pub fn remove(&self, id: &K) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    pub fn cancel_all(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain();
    }

    #[cfg(test)]
    fn has_key(&self, id: &K) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_cancels_its_cancellation() {
        let cancellation = Cancellation::default();
        let trigger = CancelTrigger::new(cancellation.clone());
        assert!(!cancellation.is_cancelled());
        trigger.cancel();
        cancellation.cancelled().await;
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn drop_trigger_also_cancels() {
        let cancellation = Cancellation::default();
        let trigger = CancelTrigger::new(cancellation.clone());
        drop(trigger);
        cancellation.cancelled().await;
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_accessor_shares_state() {
        let cancellation = Cancellation::default();
        let trigger = CancelTrigger::new(cancellation);
        let shared = trigger.cancellation();
        assert!(!shared.is_cancelled());
        drop(trigger);
        assert!(shared.is_cancelled());
    }

    #[test]
    fn cancel_map_insert_and_cancel() {
        let map = CancelMap::new();
        let cancellation = Cancellation::default();
        map.insert("t1".to_owned(), CancelTrigger::new(cancellation.clone()));
        assert!(!cancellation.is_cancelled());
        map.cancel_or_precancel("t1".to_owned());
        assert!(cancellation.is_cancelled());
    }

    /// One tool call can open several subagents.
    #[test]
    fn cancel_map_keeps_siblings_under_one_key() {
        let map = CancelMap::new();
        let c1 = Cancellation::default();
        let c2 = Cancellation::default();
        map.insert("x".to_owned(), CancelTrigger::new(c1.clone()));
        map.insert("x".to_owned(), CancelTrigger::new(c2.clone()));
        assert!(!c1.is_cancelled(), "a sibling must not evict the first");
        assert!(!c2.is_cancelled());

        map.cancel_or_precancel("x".to_owned());
        assert!(c1.is_cancelled(), "cancelling the key stops them all");
        assert!(c2.is_cancelled());
    }

    #[test]
    fn cancel_map_retire_leaves_siblings_running() {
        let map = CancelMap::new();
        let c1 = Cancellation::default();
        let c2 = Cancellation::default();
        let slot1 = map.insert("x".to_owned(), CancelTrigger::new(c1.clone()));
        map.insert("x".to_owned(), CancelTrigger::new(c2.clone()));

        map.retire(&"x".to_owned(), slot1);
        assert!(c1.is_cancelled(), "retiring drops that trigger");
        assert!(!c2.is_cancelled(), "the sibling keeps running");

        map.cancel_or_precancel("x".to_owned());
        assert!(c2.is_cancelled());
    }

    /// The last one out clears the key so it can be reused.
    #[test]
    fn cancel_map_retiring_the_last_registration_clears_the_key() {
        let map = CancelMap::new();
        let slot = map.insert("x".to_owned(), CancelTrigger::new(Cancellation::default()));
        assert!(map.has_key(&"x".to_owned()));

        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()), "empty key must be dropped");
    }

    /// Cancelling before anything registers has to catch every session the
    /// tool call goes on to open, not just the first one through the door.
    #[test]
    fn cancel_map_precancel_catches_every_later_sibling() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());

        let c1 = Cancellation::default();
        let c2 = Cancellation::default();
        let slot1 = map.insert("x".to_owned(), CancelTrigger::new(c1.clone()));
        let slot2 = map.insert("x".to_owned(), CancelTrigger::new(c2.clone()));
        assert!(c1.is_cancelled());
        assert!(c2.is_cancelled(), "the mark must outlive the first insert");

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));
    }

    /// Pressing esc while a fan-out is running must also stop the sibling
    /// that starts a moment later.
    #[test]
    fn cancel_map_cancel_catches_a_sibling_registered_after() {
        let map = CancelMap::new();
        let c1 = Cancellation::default();
        let slot1 = map.insert("x".to_owned(), CancelTrigger::new(c1.clone()));

        map.cancel_or_precancel("x".to_owned());
        assert!(c1.is_cancelled());

        let c2 = Cancellation::default();
        let slot2 = map.insert("x".to_owned(), CancelTrigger::new(c2.clone()));
        assert!(c2.is_cancelled(), "cancel left no mark for the sibling");

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));

        let c3 = Cancellation::default();
        map.insert("x".to_owned(), CancelTrigger::new(c3.clone()));
        assert!(
            !c3.is_cancelled(),
            "the completed call must not poison a reused tool id"
        );
    }

    #[test]
    fn cancel_map_insert_into_cancelled_returns_retirement_slot() {
        let map = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());
        let cancellation = Cancellation::default();
        let slot = map.insert("x".to_owned(), CancelTrigger::new(cancellation.clone()));
        assert!(cancellation.is_cancelled());
        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()));
    }

    #[test]
    fn cancel_map_remove_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.remove(&"t1".to_owned());
        let cancellation = Cancellation::default();
        map.insert("t1".to_owned(), CancelTrigger::new(cancellation.clone()));
        assert!(
            !cancellation.is_cancelled(),
            "remove should clear cancellation"
        );
    }

    #[test]
    fn cancel_map_cancel_all() {
        let map = CancelMap::new();
        let c1 = Cancellation::default();
        let c2 = Cancellation::default();
        map.insert("a".to_owned(), CancelTrigger::new(c1.clone()));
        map.insert("b".to_owned(), CancelTrigger::new(c2.clone()));
        map.cancel_all();
        assert!(c1.is_cancelled());
        assert!(c2.is_cancelled());
    }

    #[test]
    fn cancel_map_cancel_all_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.cancel_all();
        let cancellation = Cancellation::default();
        map.insert("t1".to_owned(), CancelTrigger::new(cancellation.clone()));
        assert!(
            !cancellation.is_cancelled(),
            "cancel_all should clear cancelled entries"
        );
    }
}
