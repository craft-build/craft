use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use event_listener::Event;

struct Shared {
    cancelled: AtomicBool,
    event: Event,
}

const CANCELLED: &str = "cancelled";

impl Shared {
    fn fire(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.event.notify(usize::MAX);
    }
}

#[derive(Clone)]
pub struct CancelToken(Arc<Shared>);

pub struct CancelTrigger(Arc<Shared>);

impl CancelToken {
    pub fn new() -> (CancelTrigger, Self) {
        let shared = Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            event: Event::new(),
        });
        (CancelTrigger(Arc::clone(&shared)), Self(shared))
    }

    pub fn none() -> Self {
        Self(Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            event: Event::new(),
        }))
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn race<T>(&self, future: impl Future<Output = T>) -> Result<T, String> {
        if self.is_cancelled() {
            return Err(CANCELLED.into());
        }
        tokio::select! {
            result = async { Ok(future.await) } => result,
            _ = self.cancelled() => Err(CANCELLED.into()),
        }
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let listener = self.0.event.listen();
            if self.is_cancelled() {
                return;
            }
            listener.await;
        }
    }

    pub fn child(&self) -> (CancelTrigger, Self) {
        let (child_trigger, child_token) = Self::new();
        let parent = self.clone();
        let child_shared = Arc::clone(&child_token.0);
        tokio::spawn(async move {
            tokio::select! {
                _ = parent.cancelled() => child_shared.fire(),
                _ = child_shared.event.listen() => {}
            }
        });
        (child_trigger, child_token)
    }
}

impl CancelTrigger {
    pub fn cancel(self) {
        self.0.fire();
    }
}

impl Drop for CancelTrigger {
    fn drop(&mut self) {
        self.0.fire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_wakes_token() {
        let (trigger, token) = CancelToken::new();
        assert!(!token.is_cancelled());
        trigger.cancel();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_by_parent() {
        let (parent_trigger, parent_token) = CancelToken::new();
        let (_child_trigger, child_token) = parent_token.child();
        parent_trigger.cancel();
        child_token.cancelled().await;
        assert!(child_token.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_by_own_trigger() {
        let (_parent_trigger, parent_token) = CancelToken::new();
        let (child_trigger, child_token) = parent_token.child();
        child_trigger.cancel();
        child_token.cancelled().await;
        assert!(child_token.is_cancelled());
        assert!(!parent_token.is_cancelled());
    }

    #[tokio::test]
    async fn drop_trigger_also_cancels() {
        let (trigger, token) = CancelToken::new();
        drop(trigger);
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn race_returns_value_when_not_cancelled() {
        let (_trigger, token) = CancelToken::new();
        let result = token.race(async { 42 }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn race_returns_error_when_already_cancelled() {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        let result = token.race(std::future::pending::<()>()).await;
        assert!(result.unwrap_err().contains("cancelled"));
    }

    #[tokio::test]
    async fn race_interrupted_by_concurrent_cancel() {
        let (trigger, token) = CancelToken::new();
        tokio::spawn(async move { trigger.cancel() });
        let result = token.race(std::future::pending::<()>()).await;
        assert!(result.is_err());
    }
}
