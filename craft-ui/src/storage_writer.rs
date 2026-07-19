//! Coalescing write-behind cache with incremental JSONL persistence.
//!
//! Apps post session snapshots keyed by session id; the writer thread drains
//! the newest snapshot of every session per wake and performs O(delta)
//! appends. Deletes run on the same thread, so an append and a delete of the
//! same session can never race: a queued save cannot resurrect deleted files.

use std::collections::HashMap;
use std::io;
use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use color_eyre::Result;
use craft_storage::StateDir;
use craft_storage::id::CraftId;
use craft_storage::sessions::{SESSIONS_DIR, SessionError, SessionLog};
use craft_storage::stats::{CostLedger, CostRecord};
use tracing::warn;

use crate::AppSession;
use crate::agent::shared_queue::lock;

type Pending = Arc<Mutex<HashMap<CraftId, Box<AppSession>>>>;

type DeleteCallback = Box<dyn FnOnce(Result<(), SessionError>) + Send>;

enum Op {
    Flush,
    Delete { id: CraftId, done: DeleteCallback },
}

pub struct StorageWriter {
    pending: Pending,
    ops: flume::Sender<Op>,
    cost_tx: flume::Sender<CostRecord>,
    done_rx: flume::Receiver<()>,
    cost_done_rx: flume::Receiver<()>,
}

impl StorageWriter {
    pub fn new(dir: StateDir) -> Result<Self> {
        let pending: Pending = Arc::default();
        let writer_pending = Arc::clone(&pending);
        let (ops, ops_rx) = flume::unbounded::<Op>();
        let (done_tx, done_rx) = flume::bounded::<()>(1);
        let (cost_tx, cost_rx) = flume::unbounded::<CostRecord>();
        let (cost_done_tx, cost_done_rx) = flume::bounded::<()>(1);

        let cost_dir = dir.clone();
        std::thread::Builder::new()
            .name("cost-writer".into())
            .spawn(move || {
                let ledger = match CostLedger::from_state_dir(&cost_dir) {
                    Ok(l) => l,
                    Err(e) => {
                        warn!(error = %e, "failed to open cost ledger");
                        return;
                    }
                };
                while let Ok(record) = cost_rx.recv() {
                    if let Err(e) = ledger.append(&record) {
                        warn!(error = %e, "cost ledger append failed");
                    }
                }
                let _ = cost_done_tx.send(());
            })
            .map_err(|e| color_eyre::eyre::eyre!("failed to spawn cost-writer thread: {e}"))?;

        std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                let mut logs: HashMap<CraftId, SessionLog> = HashMap::new();
                while let Ok(op) = ops_rx.recv() {
                    match op {
                        Op::Flush => flush(&writer_pending, &mut logs, &dir),
                        Op::Delete { id, done } => {
                            lock(&writer_pending).remove(&id);
                            logs.remove(&id);
                            done(AppSession::delete(id, &dir));
                        }
                    }
                }
                flush(&writer_pending, &mut logs, &dir);
                let _ = done_tx.send(());
            })
            .map_err(|e| color_eyre::eyre::eyre!("failed to spawn storage-writer thread: {e}"))?;

        Ok(Self {
            pending,
            ops,
            cost_tx,
            done_rx,
            cost_done_rx,
        })
    }

    pub fn send(&self, session: Box<AppSession>) {
        let mut pending = lock(&self.pending);
        let was_empty = pending.is_empty();
        pending.insert(session.id.id(), session);
        drop(pending);
        if was_empty {
            let _ = self.ops.send(Op::Flush);
        }
    }

    /// Delete a session's files on the writer thread, discarding any pending
    /// snapshot first. Runs after already-queued flushes; `done` fires on the
    /// writer thread, so callers never block on disk.
    pub fn delete(
        &self,
        id: CraftId,
        done: impl FnOnce(Result<(), SessionError>) + Send + 'static,
    ) {
        let op = Op::Delete {
            id,
            done: Box::new(done),
        };
        if let Err(flume::SendError(Op::Delete { done, .. })) = self.ops.send(op) {
            done(Err(writer_gone()));
        }
    }

    pub fn record_cost(&self, record: CostRecord) {
        let _ = self.cost_tx.send(record);
    }

    pub fn shutdown(self, timeout: Duration) {
        drop(self.ops);
        if self.done_rx.recv_timeout(timeout).is_err() {
            warn!("storage writer did not drain within {timeout:?}");
        }
        drop(self.cost_tx);
        if self.cost_done_rx.recv_timeout(timeout).is_err() {
            warn!("cost writer did not drain within {timeout:?}");
        }
    }
}

fn writer_gone() -> SessionError {
    craft_storage::StorageError::Io(io::Error::other("storage writer unavailable")).into()
}

fn flush(pending: &Pending, logs: &mut HashMap<CraftId, SessionLog>, dir: &StateDir) {
    let batch = mem::take(&mut *lock(pending));
    if batch.is_empty() {
        return;
    }
    let sessions_dir = match dir.ensure_subdir(SESSIONS_DIR) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "failed to ensure sessions dir");
            return;
        }
    };
    for session in batch.into_values() {
        write_session(&sessions_dir, logs, &session);
    }
}

fn write_session(
    sessions_dir: &Path,
    logs: &mut HashMap<CraftId, SessionLog>,
    session: &AppSession,
) {
    let id = session.id.id();
    if let Some(log) = logs.get_mut(&id) {
        if !append_or_compact(log, sessions_dir, session) {
            logs.remove(&id);
        }
        return;
    }
    let mut log = match open_or_create_log(sessions_dir, session) {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, id = %id, "session log open failed");
            return;
        }
    };
    if append_or_compact(&mut log, sessions_dir, session) {
        logs.insert(id, log);
    }
}

/// False means the log's cursors are unusable and it must not stay cached.
fn append_or_compact(log: &mut SessionLog, sessions_dir: &Path, session: &AppSession) -> bool {
    let id = session.id.id();
    match log.append(session) {
        Ok(()) => true,
        Err(SessionError::CursorAhead { .. }) => match log.compact(sessions_dir, session) {
            Ok(()) => true,
            Err(e) => {
                warn!(error = %e, id = %id, "compact fallback failed");
                false
            }
        },
        Err(e) => {
            warn!(error = %e, id = %id, "append failed");
            true
        }
    }
}

fn open_or_create_log(
    sessions_dir: &Path,
    session: &AppSession,
) -> Result<SessionLog, craft_storage::sessions::SessionError> {
    let id = session.id.id();
    let jsonl_path = sessions_dir.join(format!("{id}.jsonl"));
    if jsonl_path.exists() {
        let (_loaded, log) = SessionLog::open::<
            craft_providers::Message,
            craft_providers::TokenUsage,
            craft_agent::ToolOutput,
        >(sessions_dir, id)?;
        Ok(log)
    } else {
        AppSession::migrate_to_jsonl(sessions_dir, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    /// Snapshots must coalesce per session id, not into one `latest` slot:
    /// two racing sessions used to silently drop one.
    #[test]
    fn shutdown_drains_newest_snapshot_of_every_session() {
        let (_tmp, dir) = state_dir();
        let writer = StorageWriter::new(dir.clone()).unwrap();
        let a = AppSession::new("test-model", "/tmp/a");
        let mut b = AppSession::new("test-model", "/tmp/b");
        let (a_id, b_id) = (a.id.id(), b.id.id());
        writer.send(Box::new(a));
        writer.send(Box::new(b.clone()));
        b.title = "renamed".into();
        writer.send(Box::new(b));
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(AppSession::load(a_id, &dir).is_ok());
        assert_eq!(AppSession::load(b_id, &dir).unwrap().title, "renamed");
    }

    #[test]
    fn delete_discards_pending_snapshot() {
        let (_tmp, dir) = state_dir();
        let writer = StorageWriter::new(dir.clone()).unwrap();
        let session = AppSession::new("test-model", "/tmp/c");
        let id = session.id.id();
        writer.send(Box::new(session));
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(id, move |res| {
            let _ = done_tx.send(res);
        });
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(done_rx.recv().unwrap().is_ok());
        assert!(AppSession::load(id, &dir).is_err());
    }
}
