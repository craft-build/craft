//! A set of spawned tool tasks whose panics become error strings.
//!
//! Ported from the reference's `task_set.rs`: each future is double-spawned
//! so a panic inside a tool never unwinds into the run loop — it surfaces as
//! `Err(String)` (payload + backtrace), and a cancelled task as
//! `"task was cancelled"`. `join_all` returns results in spawn order.

use std::backtrace::Backtrace;
use std::future::Future;

const CANCELLED: &str = "task was cancelled";

pub(super) struct TaskSet<T> {
    tasks: Vec<tokio::task::JoinHandle<Result<T, String>>>,
}

impl<T: Send + 'static> TaskSet<T> {
    pub(super) fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    pub(super) fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        self.tasks.push(tokio::spawn(async move {
            match tokio::spawn(future).await {
                Ok(result) => Ok(result),
                Err(e) if e.is_panic() => Err(panic_to_string(e.into_panic())),
                Err(_) => Err(CANCELLED.into()),
            }
        }));
    }

    pub(super) async fn join_all(self) -> Vec<Result<T, String>> {
        let mut results = Vec::with_capacity(self.tasks.len());
        for task in self.tasks {
            results.push(match task.await {
                Ok(inner) => inner,
                Err(e) if e.is_panic() => Err(panic_to_string(e.into_panic())),
                Err(_) => Err(CANCELLED.into()),
            });
        }
        results
    }
}

impl<T> Default for TaskSet<T> {
    fn default() -> Self {
        Self { tasks: Vec::new() }
    }
}

fn panic_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    };
    let bt = Backtrace::force_capture();
    format!("{msg}\n\nBacktrace:\n{bt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mixed_panic_and_ok() {
        let mut set: TaskSet<i32> = TaskSet::new();
        set.spawn(async { 42 });
        set.spawn(async { panic!("oops") });
        set.spawn(async { 7 });
        let results = set.join_all().await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap(), &42);
        let err = results[1].as_ref().unwrap_err();
        assert!(err.starts_with("oops"), "unexpected error: {err}");
        assert!(err.contains("Backtrace:"), "missing backtrace in: {err}");
        assert_eq!(results[2].as_ref().unwrap(), &7);
    }
}
