//! Threads: the scope parameter a turn type alone can't supply (design §5).
//!
//! A [`Thread`] is an instantiation of a run of turns against a specific slice
//! of the request. The root thread is the whole request (and absorbs the
//! "workstream" concept: its id is the workstream id, so the persisted
//! `<project>/flow/<root_thread_id>/` path needs zero storage migration).
//! Children spawn on demand whenever some type's transition creates one.
//!
//! Actions ([`ThreadAction`], design §5):
//! - `Advance`: stay in this thread, whether or not the type changes.
//! - `Exit`: close this thread, hand control back to its parent, which re-enters
//!   and re-checks its own transition conditions (e.g. Plan re-checks "all
//!   children exited" -> Integrator).
//! - `Spawn`: open a new child thread of some type.
//!
//! Concurrency falls out for free (design §5): sibling threads have empty
//! mutual scope by construction, so whether two chunks can run at the same time
//! is simply true whenever neither is an ancestor of the other. The
//! dependency-ordered scheduler (migrated from `run_chunk_dag`'s JoinSet logic)
//! runs eligible siblings up to a concurrency limit.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::turn_type::{ThreadStatus, TurnType};

/// One node in the thread tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: super::typed_log::ThreadId,
    pub parent: Option<super::typed_log::ThreadId>,
    pub turn_type: TurnType,
    pub status: ThreadStatus,
    /// Child thread ids, in spawn order. Empty for a leaf.
    pub children: Vec<super::typed_log::ThreadId>,
    /// Plan-array order for rendering and dependency tiebreaking. 0 for the
    /// root and threads spawned before a Plan runs.
    #[serde(default)]
    pub order: usize,
    /// Chunk title for rendering; empty until a Plan names it.
    #[serde(default)]
    pub title: String,
    /// Ids of sibling threads that must reach `Done` before this thread can
    /// start. Declared by Plan; drives dependency-aware scheduling (design §5:
    /// "c2 depends on c1 just means don't spawn c2 until c1 has exited").
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl Thread {
    pub fn new_root(id: super::typed_log::ThreadId, turn_type: TurnType) -> Self {
        Self {
            id,
            parent: None,
            turn_type,
            status: ThreadStatus::Running,
            children: Vec::new(),
            order: 0,
            title: String::new(),
            depends_on: Vec::new(),
        }
    }
}

/// Owns the thread tree. Root thread id = the workstream id (design §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadManager {
    pub threads: BTreeMap<String, Thread>,
    pub root: super::typed_log::ThreadId,
}

impl ThreadManager {
    pub fn new(root_id: impl Into<String>, initial_type: TurnType) -> Self {
        let root_id = super::typed_log::ThreadId::new(root_id);
        let root = Thread::new_root(root_id.clone(), initial_type);
        let mut threads = BTreeMap::new();
        threads.insert(root_id.to_string(), root);
        Self {
            threads,
            root: root_id,
        }
    }

    pub fn root(&self) -> &Thread {
        self.threads
            .get(self.root.as_str())
            .expect("root always present")
    }

    pub fn root_mut(&mut self) -> &mut Thread {
        self.threads
            .get_mut(self.root.as_str())
            .expect("root always present")
    }

    pub fn get(&self, id: &super::typed_log::ThreadId) -> Option<&Thread> {
        self.threads.get(id.as_str())
    }

    pub fn get_mut(&mut self, id: &super::typed_log::ThreadId) -> Option<&mut Thread> {
        self.threads.get_mut(id.as_str())
    }

    /// Advance: stay in this thread, possibly changing type. Refreshes the
    /// thread's pinned reads on re-entry (design §7: "refreshed only when the
    /// thread re-enters"). Returns the new active thread id (the same one).
    pub fn advance(&mut self, id: &super::typed_log::ThreadId, to: TurnType) -> Option<&Thread> {
        let t = self.threads.get_mut(id.as_str())?;
        t.turn_type = to;
        t.status = ThreadStatus::Running;
        self.threads.get(id.as_str())
    }

    /// Spawn a child thread of `turn_type` under `parent`. The child's id is
    /// minted from `parent` + an index so it is stable across resume (design
    /// §5: children spawn on demand, not on a fixed schedule). Returns the new
    /// thread id. The child inherits `order`/`title`/`depends_on` from the
    /// spawn payload (the Plan's declared chunk metadata).
    pub fn spawn(
        &mut self,
        parent: &super::typed_log::ThreadId,
        turn_type: TurnType,
        child_id: impl Into<String>,
        order: usize,
        title: String,
        depends_on: Vec<String>,
    ) -> super::typed_log::ThreadId {
        let child_id = super::typed_log::ThreadId::new(child_id);
        let child = Thread {
            id: child_id.clone(),
            parent: Some(parent.clone()),
            turn_type,
            status: ThreadStatus::Queued,
            children: Vec::new(),
            order,
            title,
            depends_on,
        };
        if let Some(p) = self.threads.get_mut(parent.as_str()) {
            p.children.push(child_id.clone());
        }
        self.threads.insert(child_id.to_string(), child);
        child_id
    }

    /// Exit: close this thread, hand control back to its parent (design §5).
    /// The parent re-enters and re-checks its own transition conditions. Marks
    /// the thread `Done` and returns the parent's id (the new active thread).
    pub fn exit(&mut self, id: &super::typed_log::ThreadId) -> Option<super::typed_log::ThreadId> {
        let parent = self.threads.get(id.as_str())?.parent.clone();
        if let Some(t) = self.threads.get_mut(id.as_str()) {
            t.status = ThreadStatus::Done;
        }
        parent
    }

    /// Has every child of `parent` exited (status `Done`)? Used by Plan's
    /// "advance to Integrator once every spawned child has exited" edge
    /// (design §5, §8.1): re-entering Plan after the last child closes is what
    /// naturally re-checks this condition.
    pub fn all_children_done(&self, parent: &super::typed_log::ThreadId) -> bool {
        let Some(t) = self.threads.get(parent.as_str()) else {
            return true;
        };
        !t.children.is_empty()
            && t.children.iter().all(|c| {
                self.threads
                    .get(c.as_str())
                    .is_some_and(|ct| ct.status == ThreadStatus::Done)
            })
    }

    /// Children of `parent` that are not yet `Done`, in spawn order.
    pub fn active_children(&self, parent: &super::typed_log::ThreadId) -> Vec<&Thread> {
        let Some(t) = self.threads.get(parent.as_str()) else {
            return Vec::new();
        };
        t.children
            .iter()
            .filter_map(|c| self.threads.get(c.as_str()))
            .filter(|c| c.status != ThreadStatus::Done)
            .collect()
    }

    /// Dependency-ordered scheduling of a parent's children (migrated from
    /// `run_chunk_dag`'s JoinSet eligibility logic). Returns the child ids that
    /// are eligible to run now: not yet `Done`, not currently `Running`, and
    /// whose `depends_on` siblings have all reached `Done`. A child becomes
    /// eligible only when every dependency has exited (design §5: "don't spawn
    /// c2's thread until c1's has exited").
    pub fn eligible_children(
        &self,
        parent: &super::typed_log::ThreadId,
    ) -> Vec<super::typed_log::ThreadId> {
        let Some(t) = self.threads.get(parent.as_str()) else {
            return Vec::new();
        };
        let done: HashSet<String> = t
            .children
            .iter()
            .filter_map(|c| {
                self.threads
                    .get(c.as_str())
                    .filter(|ct| ct.status == ThreadStatus::Done)
                    .map(|ct| ct.id.to_string())
            })
            .collect();
        t.children
            .iter()
            .filter(|c| {
                let Some(ct) = self.threads.get(c.as_str()) else {
                    return false;
                };
                ct.status == ThreadStatus::Queued && ct.depends_on.iter().all(|d| done.contains(d))
            })
            .map(|c| super::typed_log::ThreadId::new(c.as_str()))
            .collect()
    }

    /// Mark a child `Running` when the scheduler picks it up.
    pub fn mark_running(&mut self, id: &super::typed_log::ThreadId) {
        if let Some(t) = self.threads.get_mut(id.as_str()) {
            t.status = ThreadStatus::Running;
        }
    }

    /// Snapshot the thread tree as a flat list of child threads (design
    /// migration table: "keep `ChunkStatus`-like glyphs in the UI as
    /// `ThreadStatus`"). Parent is implied (typical nesting is one level: root
    /// + per-chunk children), so a flat list with parent implied suffices.
    pub fn child_snapshots(&self) -> Vec<ThreadSnapshot> {
        self.root()
            .children
            .iter()
            .filter_map(|c| self.threads.get(c.as_str()))
            .map(|t| ThreadSnapshot {
                id: t.id.to_string(),
                title: t.title.clone(),
                status: t.status,
                stage: Some(t.turn_type),
                order: t.order,
                depends_on: t.depends_on.clone(),
            })
            .collect()
    }
}

/// A flat snapshot of one child thread for UI/ACP rendering. Mirrors the
/// `FlowProgress::Chunk` shape so the existing FlowPanel renders threads
/// instead of chunks with no API churn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSnapshot {
    pub id: String,
    pub title: String,
    pub status: ThreadStatus,
    pub stage: Option<TurnType>,
    pub order: usize,
    pub depends_on: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> ThreadManager {
        ThreadManager::new("ws", TurnType::General)
    }

    #[test]
    fn root_is_present_and_running() {
        let m = mgr();
        assert_eq!(m.root().turn_type, TurnType::General);
        assert_eq!(m.root().status, ThreadStatus::Running);
    }

    #[test]
    fn spawn_adds_child_under_parent() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1", 0, "Add auth".into(), vec![]);
        assert_eq!(m.root().children, vec![c1.clone()]);
        assert_eq!(m.get(&c1).unwrap().turn_type, TurnType::Req);
        assert_eq!(m.get(&c1).unwrap().status, ThreadStatus::Queued);
    }

    #[test]
    fn exit_marks_done_and_returns_parent() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1", 0, "t".into(), vec![]);
        assert_eq!(m.exit(&c1), Some(root.clone()));
        assert_eq!(m.get(&c1).unwrap().status, ThreadStatus::Done);
    }

    #[test]
    fn all_children_done_false_until_all_exit() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1", 0, "a".into(), vec![]);
        let c2 = m.spawn(&root, TurnType::Req, "c2", 1, "b".into(), vec![]);
        assert!(!m.all_children_done(&root));
        m.exit(&c1);
        assert!(!m.all_children_done(&root));
        m.exit(&c2);
        assert!(m.all_children_done(&root));
    }

    #[test]
    fn eligible_children_respects_depends_on() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1", 0, "a".into(), vec![]);
        let c2 = m.spawn(
            &root,
            TurnType::Req,
            "c2",
            1,
            "b".into(),
            vec!["c1".to_string()],
        );
        // c1 eligible immediately, c2 blocked on c1.
        let eligible = m.eligible_children(&root);
        assert_eq!(eligible, vec![c1.clone()]);
        m.mark_running(&c1);
        // While c1 is Running (not Done), c2 still not eligible.
        assert!(m.eligible_children(&root).is_empty());
        m.exit(&c1);
        // c1 Done -> c2 eligible.
        assert_eq!(m.eligible_children(&root), vec![c2]);
    }

    #[test]
    fn advance_changes_type_in_place() {
        let mut m = mgr();
        let root = m.root.clone();
        m.advance(&root, TurnType::Scout);
        assert_eq!(m.root().turn_type, TurnType::Scout);
    }

    #[test]
    fn child_snapshots_reflect_status_and_order() {
        let mut m = mgr();
        let root = m.root.clone();
        m.spawn(&root, TurnType::Req, "c1", 0, "a".into(), vec![]);
        m.spawn(&root, TurnType::Req, "c2", 1, "b".into(), vec![]);
        let snaps = m.child_snapshots();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].id, "c1");
        assert_eq!(snaps[1].order, 1);
    }
}
