//! A background Tokio runtime that drives timers for the app.
//!
//! GPUI has its own executor, but the assignment asks for Tokio specifically,
//! so the "agent is thinking" / toast-dismiss delays are genuine
//! `tokio::time::sleep` calls running on a real multi-thread Tokio runtime.
//! GPUI's `cx.spawn` future just awaits a oneshot channel that the Tokio task
//! fires into once its sleep completes, then hands control back to the UI
//! thread to apply the result.

use std::time::Duration;

use once_cell::sync::Lazy;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    Runtime::new().expect("failed to start the Tokio runtime")
});

/// Sleep for `duration` on the Tokio runtime, resolving the returned
/// receiver afterwards. Await it from a `cx.spawn` future to bridge back
/// into GPUI.
pub fn delay(duration: Duration) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    RUNTIME.spawn(async move {
        tokio::time::sleep(duration).await;
        let _ = tx.send(());
    });
    rx
}
