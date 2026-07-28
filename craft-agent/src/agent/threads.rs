//! Threads: the scope parameter a turn type alone can't supply.
//!
//! A [`Thread`] is an instantiation of a run of turns against a specific slice
//! of the request. The root thread is the whole request (and absorbs the
//! "workstream" concept: its id is the workstream id, so the persisted
//! `<project>/flow/<root_thread_id>/` path needs zero storage migration).
//! Children spawn on demand when the model calls the `task` tool, which
//! registers a child [`Thread`] under the parent and runs a fresh
//! shift-enabled loop against the child's [`ThreadId`].
//!
//! There is no orchestrator and no DAG here. Each `Thread`/`Agent` runs the
//! normal `Agent::run_loop` and shifts its *own* `turn_type` (Scout -> Tpm ->
//! Plan -> ...) via the `shift` tool. "Parallel work" is N `task` calls, each
//! its own child thread in the shared tree; start-ordering is the model's job
//! (`task` for sequential, `batch` for concurrent). The typed log records one
//! distilled entry per turn.

#![allow(dead_code)]

use std::collections::BTreeMap;

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
}

impl Thread {
    pub fn new_root(id: super::typed_log::ThreadId, turn_type: TurnType) -> Self {
        Self {
            id,
            parent: None,
            turn_type,
            status: ThreadStatus::Running,
            children: Vec::new(),
        }
    }
}

/// Owns the thread tree. Root thread id = the workstream id.
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
    /// thread's pinned reads on re-entry. Returns the new active thread id
    /// (the same one).
    pub fn advance(&mut self, id: &super::typed_log::ThreadId, to: TurnType) -> Option<&Thread> {
        let t = self.threads.get_mut(id.as_str())?;
        t.turn_type = to;
        t.status = ThreadStatus::Running;
        self.threads.get(id.as_str())
    }

    /// Spawn a child thread of `turn_type` under `parent` with the caller-
    /// supplied `child_id` (derived from the spawning `task` tool_use_id so it
    /// is stable across resume). The child starts `Queued`; the caller flips
    /// it to `Running` by running its loop. Returns the new thread id.
    pub fn spawn(
        &mut self,
        parent: &super::typed_log::ThreadId,
        turn_type: TurnType,
        child_id: impl Into<String>,
    ) -> super::typed_log::ThreadId {
        let child_id = super::typed_log::ThreadId::new(child_id);
        let child = Thread {
            id: child_id.clone(),
            parent: Some(parent.clone()),
            turn_type,
            status: ThreadStatus::Queued,
            children: Vec::new(),
        };
        if let Some(p) = self.threads.get_mut(parent.as_str()) {
            p.children.push(child_id.clone());
        }
        self.threads.insert(child_id.to_string(), child);
        child_id
    }

    /// Exit: close this thread, mark it `Done`, and return the parent's id
    /// (the new active thread).
    pub fn exit(&mut self, id: &super::typed_log::ThreadId) -> Option<super::typed_log::ThreadId> {
        let parent = self.threads.get(id.as_str())?.parent.clone();
        if let Some(t) = self.threads.get_mut(id.as_str()) {
            t.status = ThreadStatus::Done;
        }
        parent
    }

    /// Has every child of `parent` exited (status `Done`)? Used by Plan's
    /// "advance to Integrator once every spawned child has exited" edge:
    /// re-entering Plan after the last child closes naturally re-checks this.
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
        let c1 = m.spawn(&root, TurnType::Req, "c1");
        assert_eq!(m.root().children, vec![c1.clone()]);
        assert_eq!(m.get(&c1).unwrap().turn_type, TurnType::Req);
        assert_eq!(m.get(&c1).unwrap().status, ThreadStatus::Queued);
    }

    #[test]
    fn exit_marks_done_and_returns_parent() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1");
        assert_eq!(m.exit(&c1), Some(root.clone()));
        assert_eq!(m.get(&c1).unwrap().status, ThreadStatus::Done);
    }

    #[test]
    fn all_children_done_false_until_all_exit() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1");
        let c2 = m.spawn(&root, TurnType::Req, "c2");
        assert!(!m.all_children_done(&root));
        m.exit(&c1);
        assert!(!m.all_children_done(&root));
        m.exit(&c2);
        assert!(m.all_children_done(&root));
    }

    #[test]
    fn advance_changes_type_in_place() {
        let mut m = mgr();
        let root = m.root.clone();
        m.advance(&root, TurnType::Scout);
        assert_eq!(m.root().turn_type, TurnType::Scout);
    }

    #[test]
    fn active_children_excludes_done() {
        let mut m = mgr();
        let root = m.root.clone();
        let c1 = m.spawn(&root, TurnType::Req, "c1");
        let c2 = m.spawn(&root, TurnType::Req, "c2");
        assert_eq!(m.active_children(&root).len(), 2);
        m.exit(&c1);
        let active = m.active_children(&root);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, c2);
    }
}
