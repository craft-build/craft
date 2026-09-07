//! A background Tokio runtime that drives timers for the app.
//!
//! GPUI has its own executor, but the assignment asks for Tokio specifically,
//! so the "agent is thinking" / toast-dismiss delays are genuine
//! `tokio::time::sleep` calls running on a real multi-thread Tokio runtime.
//! GPUI's `cx.spawn` future just awaits a oneshot channel that the Tokio task
//! fires into once its sleep completes, then hands control back to the UI
//! thread to apply the result.

use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

static RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("failed to start the Tokio runtime"));

/// Run long-lived ACP connections and workspace operations without blocking GPUI.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    RUNTIME.spawn(future)
}
