//! Runs syntect on one dedicated thread to bound its regex cache memory.
//!
//! The cache is not syntect's own, it is regex-automata's `Pool<Cache>`. When
//! a thread wants to highlight while another already holds the cache, it gets
//! a fresh set of its own, one per compiled `Regex` in the syntax, and that
//! memory is never given back, not even when the thread exits. Single-
//! threading every highlight keeps the bill to one set.
//!
//! Ported from the reference `craft-highlight/src/pool.rs`, with std
//! mpsc channels in place of `flume` (the only channel user in this crate).

use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, channel, sync_channel};
use std::thread;

const UNKNOWN_PANIC: &str = "<non-string panic payload>";
const THREAD_GONE: &str = "the highlight thread is gone";

type Job = Box<dyn FnOnce() + Send + 'static>;

static JOBS: OnceLock<Sender<Job>> = OnceLock::new();

fn jobs() -> &'static Sender<Job> {
    JOBS.get_or_init(|| {
        let (tx, rx): (Sender<Job>, Receiver<Job>) = channel();
        thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                ON_HIGHLIGHT_THREAD.set(true);
                while let Ok(job) = rx.recv() {
                    if let Err(payload) = catch_unwind(AssertUnwindSafe(job)) {
                        eprintln!("highlight job panicked: {}", panic_message(&*payload));
                    }
                }
            })
            .expect("failed to spawn the highlight thread");
        tx
    })
}

thread_local! {
    static ON_HIGHLIGHT_THREAD: Cell<bool> = const { Cell::new(false) };
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or(UNKNOWN_PANIC)
}

/// Runs `f` on the highlight thread and waits for its result.
///
/// Nested calls run inline because markdown highlights its own code blocks.
pub fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    if ON_HIGHLIGHT_THREAD.get() {
        return f();
    }
    let (done_tx, done_rx) = sync_channel(1);
    spawn(move || {
        let _ = done_tx.send(catch_unwind(AssertUnwindSafe(f)));
    });
    match done_rx.recv() {
        Ok(Ok(value)) => value,
        Ok(Err(payload)) => resume_unwind(payload),
        Err(_) => panic!("{THREAD_GONE}"),
    }
}

pub fn spawn(f: impl FnOnce() + Send + 'static) {
    let _ = jobs().send(Box::new(f));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const PAYLOAD: usize = 41;
    const WAITERS: usize = 8;
    const BOOM: &str = "boom";

    #[test]
    fn runs_job_and_returns_value() {
        assert_eq!(run(|| PAYLOAD + 1), PAYLOAD + 1);
    }

    #[test]
    fn every_job_runs_on_one_thread() {
        let waiters: Vec<_> = (0..WAITERS)
            .map(|_| thread::spawn(|| run(|| thread::current().id())))
            .collect();
        let ids: Vec<_> = waiters
            .into_iter()
            .map(|w| w.join().expect("waiter panicked"))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "jobs ran on multiple threads: {ids:?}"
        );
    }

    #[test]
    fn nested_run_does_not_deadlock() {
        assert_eq!(run(|| run(|| PAYLOAD)), PAYLOAD);
    }

    #[test]
    fn many_waiters_all_make_progress() {
        let done = Arc::new(AtomicUsize::new(0));
        let waiters: Vec<_> = (0..WAITERS)
            .map(|_| {
                let done = Arc::clone(&done);
                thread::spawn(move || run(move || done.fetch_add(1, Ordering::AcqRel)))
            })
            .collect();
        for w in waiters {
            w.join().expect("waiter panicked");
        }
        assert_eq!(done.load(Ordering::SeqCst), WAITERS);
    }

    #[test]
    fn survives_a_panicking_job() {
        spawn(|| panic!("{BOOM}"));
        assert_eq!(run(|| PAYLOAD), PAYLOAD);
    }

    #[test]
    fn run_resumes_the_job_panic() {
        let payload = catch_unwind(|| run::<()>(|| panic!("{BOOM}"))).expect_err("job must panic");
        assert_eq!(panic_message(&*payload), BOOM);
    }
}
