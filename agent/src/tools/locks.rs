//! Cross-agent path locking.
//!
//! Scheduling claims order calls within one agent. Subagents and concurrent
//! sessions in one process share the workspace, so filesystem tools also
//! serialize through this lock table, keyed by the same normalized keys the
//! scheduling claims use. Writers hold exclusive guards on their claimed
//! keys; readers hold shared guards. A workspace-wide write (the `move`
//! tool's import rewrite) takes the root key exclusively, excluding every
//! other path operation. Guards are always acquired root-first, then in
//! sorted key order, so concurrent acquisitions cannot deadlock. Lock
//! acquisition races the run's cancellation.

use std::collections::BTreeMap;
use std::sync::Arc;

use animist::agent::Cancellation;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::error::{Error, Result};

#[derive(Clone)]
pub(crate) struct PathLocks {
    root: Arc<RwLock<()>>,
    slots: Arc<tokio::sync::Mutex<BTreeMap<String, Arc<RwLock<()>>>>>,
}

impl PathLocks {
    pub(crate) fn new() -> Self {
        Self {
            root: Arc::new(RwLock::new(())),
            slots: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        }
    }

    async fn slot(&self, key: &str) -> Arc<RwLock<()>> {
        let mut slots = self.slots.lock().await;
        Arc::clone(
            slots
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        )
    }

    /// Shared guards for `keys` (sorted internally), under a shared root
    /// guard. Empty `keys` takes the root guard alone, as directory walks do.
    pub(crate) async fn read(&self, keys: &[String], cancel: &Cancellation) -> Result<ReadGuard> {
        let acquisition = async {
            let mut guards = Vec::with_capacity(keys.len() + 1);
            guards.push(self.root.clone().read_owned().await);
            for key in sorted(keys) {
                guards.push(self.slot(&key).await.read_owned().await);
            }
            ReadGuard { _guards: guards }
        };
        race_cancel(acquisition, cancel).await
    }

    /// Exclusive guards for `keys` (sorted internally), under a shared root
    /// guard so a workspace-wide writer still excludes these calls.
    pub(crate) async fn write(&self, keys: &[String], cancel: &Cancellation) -> Result<WriteGuard> {
        let acquisition = async {
            let root = self.root.clone().read_owned().await;
            let mut guards = Vec::with_capacity(keys.len());
            for key in sorted(keys) {
                guards.push(self.slot(&key).await.write_owned().await);
            }
            WriteGuard {
                _root: root,
                _guards: guards,
            }
        };
        race_cancel(acquisition, cancel).await
    }

    /// An exclusive guard on the root key alone: excludes every reader and
    /// writer keyed under this workspace.
    pub(crate) async fn workspace_write(&self, cancel: &Cancellation) -> Result<RootGuard> {
        let acquisition = async {
            RootGuard {
                _guard: self.root.clone().write_owned().await,
            }
        };
        race_cancel(acquisition, cancel).await
    }
}

async fn race_cancel<T>(
    acquisition: impl std::future::Future<Output = T>,
    cancel: &Cancellation,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::LockCancelled),
        guard = acquisition => Ok(guard),
    }
}

fn sorted(keys: &[String]) -> Vec<String> {
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    keys
}

pub(crate) struct ReadGuard {
    _guards: Vec<OwnedRwLockReadGuard<()>>,
}

pub(crate) struct WriteGuard {
    _root: OwnedRwLockReadGuard<()>,
    _guards: Vec<OwnedRwLockWriteGuard<()>>,
}

pub(crate) struct RootGuard {
    _guard: OwnedRwLockWriteGuard<()>,
}
