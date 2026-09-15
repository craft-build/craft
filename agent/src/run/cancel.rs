//! Cancellation architecture (reference: `craft-agent/src/cancel.rs`, C.6).
//!
//! The watch-based [`CancelToken`](super::CancelToken)/[`CancelFlag`](super::CancelFlag)
//! pair stays the substrate — the TUI surface, the stream select, and the
//! epoch re-arm semantics all hang off it. This module layers the
//! reference's richer API on top: a cancel-on-drop [`CancelTrigger`],
//! [`CancelToken::child`] propagation, [`CancelToken::race`], and the
//! slotted [`CancelMap`] for per-key grouped cancellation with pre-cancel.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{CancelFlag, CancelToken, cancel_channel};

const CANCELLED: &str = "cancelled";

/// The setting half of a token that fires on drop, so a scope that exits
/// early (panic, `?`, early return) stops what it started.
pub struct CancelTrigger(CancelFlag);

impl CancelTrigger {
    pub fn cancel(self) {
        self.0.set(true);
    }
}

impl Drop for CancelTrigger {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

/// A fresh cancellation pair whose trigger cancels on drop.
pub fn cancel_pair() -> (CancelTrigger, CancelToken) {
    let (flag, token) = cancel_channel();
    (CancelTrigger(flag), token)
}

impl CancelToken {
    /// Resolves once this token is cancelled; resolves immediately when it
    /// already is, and also when the setting half is dropped (it can never
    /// fire again, so waiting would hang).
    pub async fn wait(&self) {
        let mut rx = self.subscribe();
        loop {
            if self.cancelled() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Runs `future` to completion unless cancellation wins the race first.
    pub async fn race<T>(&self, future: impl Future<Output = T>) -> Result<T, String> {
        if self.cancelled() {
            return Err(CANCELLED.into());
        }
        let mut rx = self.subscribe();
        tokio::select! {
            result = future => Ok(result),
            changed = rx.changed() => {
                let _ = changed;
                Err(CANCELLED.into())
            }
        }
    }

    /// A token cancelled when this one cancels (but not vice versa). The
    /// returned trigger drops to cancel the child alone. The link task
    /// holds its own clone of the child's flag, so dropping either half
    /// leaves the link functional; it exits as soon as either side fires.
    pub fn child(&self) -> (CancelTrigger, CancelToken) {
        let (flag, token) = cancel_channel();
        if self.cancelled() {
            flag.set(true);
            return (CancelTrigger(flag), token);
        }
        let link = flag.clone();
        let mut parent_rx = self.subscribe();
        let mut child_rx = token.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                // A change (or the parent's flag dropping — drop cancels
                // here, unlike the long-lived TUI-held flag) stops the child.
                changed = parent_rx.changed() => {
                    let _ = changed;
                    link.set(true);
                }
                _ = child_rx.changed() => {}
            }
        });
        (CancelTrigger(flag), token)
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
    async fn trigger_wakes_token() {
        let (trigger, token) = cancel_pair();
        assert!(!token.cancelled());
        trigger.cancel();
        token.wait().await;
        assert!(token.cancelled());
    }

    #[tokio::test]
    async fn drop_trigger_also_cancels() {
        let (trigger, token) = cancel_pair();
        drop(trigger);
        token.wait().await;
        assert!(token.cancelled());
    }

    #[tokio::test]
    async fn wait_resolves_when_the_flag_half_drops() {
        // A dropped setting half can never cancel, so waiting must not hang.
        let (trigger, token) = cancel_pair();
        drop(trigger);
        token.wait().await;
    }

    #[tokio::test]
    async fn race_returns_value_when_not_cancelled() {
        let (_trigger, token) = cancel_pair();
        let result = token.race(async { 42 }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn race_returns_error_when_already_cancelled() {
        let (trigger, token) = cancel_pair();
        trigger.cancel();
        let result = token.race(std::future::pending::<()>()).await;
        assert!(result.unwrap_err().contains("cancelled"));
    }

    #[tokio::test]
    async fn race_interrupted_by_concurrent_cancel() {
        let (trigger, token) = cancel_pair();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            trigger.cancel();
        });
        let result = token.race(std::future::pending::<()>()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn child_cancelled_by_parent() {
        let (parent_trigger, parent_token) = cancel_pair();
        let (_child_trigger, child_token) = parent_token.child();
        parent_trigger.cancel();
        child_token.wait().await;
        assert!(child_token.cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_by_own_trigger() {
        let (_parent_trigger, parent_token) = cancel_pair();
        let (child_trigger, child_token) = parent_token.child();
        child_trigger.cancel();
        child_token.wait().await;
        assert!(child_token.cancelled());
        assert!(!parent_token.cancelled());
    }

    #[tokio::test]
    async fn child_of_cancelled_parent_starts_cancelled() {
        let (parent_trigger, parent_token) = cancel_pair();
        parent_trigger.cancel();
        let (_child_trigger, child_token) = parent_token.child();
        assert!(child_token.cancelled());
    }

    #[test]
    fn cancel_map_insert_and_cancel() {
        let map = CancelMap::new();
        let (trigger, token) = cancel_pair();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.cancelled());
        map.cancel_or_precancel("t1".to_owned());
        assert!(token.cancelled());
    }

    /// One tool call can open several subagents. They used to evict each
    /// other, so the first died the moment the second registered.
    #[test]
    fn cancel_map_keeps_siblings_under_one_key() {
        let map = CancelMap::new();
        let (t1, tok1) = cancel_pair();
        let (t2, tok2) = cancel_pair();
        map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);
        assert!(!tok1.cancelled(), "a sibling must not evict the first");
        assert!(!tok2.cancelled());

        map.cancel_or_precancel("x".to_owned());
        assert!(tok1.cancelled(), "cancelling the key stops them all");
        assert!(tok2.cancelled());
    }

    #[test]
    fn cancel_map_retire_leaves_siblings_running() {
        let map = CancelMap::new();
        let (t1, tok1) = cancel_pair();
        let (t2, tok2) = cancel_pair();
        let slot1 = map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);

        map.retire(&"x".to_owned(), slot1);
        assert!(tok1.cancelled(), "retiring drops that trigger");
        assert!(!tok2.cancelled(), "the sibling keeps running");

        map.cancel_or_precancel("x".to_owned());
        assert!(tok2.cancelled());
    }

    /// The last one out clears the key so it can be reused.
    #[test]
    fn cancel_map_retiring_the_last_registration_clears_the_key() {
        let map = CancelMap::new();
        let (t1, _tok1) = cancel_pair();
        let slot = map.insert("x".to_owned(), t1);
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

        let (t1, tok1) = cancel_pair();
        let (t2, tok2) = cancel_pair();
        let slot1 = map.insert("x".to_owned(), t1);
        let slot2 = map.insert("x".to_owned(), t2);
        assert!(tok1.cancelled());
        assert!(tok2.cancelled(), "the mark must outlive the first insert");

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
        let (t1, tok1) = cancel_pair();
        let slot1 = map.insert("x".to_owned(), t1);

        map.cancel_or_precancel("x".to_owned());
        assert!(tok1.cancelled());

        let (t2, tok2) = cancel_pair();
        let slot2 = map.insert("x".to_owned(), t2);
        assert!(tok2.cancelled(), "cancel left no mark for the sibling");

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));

        let (t3, tok3) = cancel_pair();
        map.insert("x".to_owned(), t3);
        assert!(
            !tok3.cancelled(),
            "the completed call must not poison a reused tool id"
        );
    }

    #[test]
    fn cancel_map_insert_into_cancelled_returns_retirement_slot() {
        let map = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());
        let (trigger, token) = cancel_pair();
        let slot = map.insert("x".to_owned(), trigger);
        assert!(token.cancelled());
        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()));
    }

    #[test]
    fn cancel_map_remove_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.remove(&"t1".to_owned());
        let (trigger, token) = cancel_pair();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.cancelled(), "remove should clear cancellation");
    }

    #[test]
    fn cancel_map_cancel_all() {
        let map = CancelMap::new();
        let (t1, tok1) = cancel_pair();
        let (t2, tok2) = cancel_pair();
        map.insert("a".to_owned(), t1);
        map.insert("b".to_owned(), t2);
        map.cancel_all();
        assert!(tok1.cancelled());
        assert!(tok2.cancelled());
    }

    #[test]
    fn cancel_map_cancel_all_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.cancel_all();
        let (trigger, token) = cancel_pair();
        map.insert("t1".to_owned(), trigger);
        assert!(
            !token.cancelled(),
            "cancel_all should clear cancelled entries"
        );
    }
}
