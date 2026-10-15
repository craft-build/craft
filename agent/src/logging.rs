//! Process-wide diagnostics: tracing events go to the rotating log file
//! (`<logs_dir>/craft.log`), warn level by default, overridable with
//! `RUST_LOG`. Without this every `tracing` event in the crate is dropped —
//! no subscriber is installed otherwise.
//!
//! Kept silent on failure: a missing or unwritable logs dir disables
//! diagnostics rather than the program.

use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

/// Install the global tracing subscriber. Idempotent: a second call (tests,
/// embedded runtimes) is a no-op.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let Some(writer) = writer() else {
        return;
    };
    // try_init, not init: a second call (tests, embedded runtimes) is a
    // no-op rather than a panic.
    let _ = subscriber(writer, filter).try_init();
}

fn writer() -> Option<crate::storage::log::RotatingFileWriter> {
    crate::storage::log::RotatingFileWriter::new(
        crate::storage::log::DEFAULT_MAX_BYTES,
        crate::storage::log::DEFAULT_MAX_FILES,
    )
    .ok()
}

fn subscriber(
    writer: crate::storage::log::RotatingFileWriter,
    filter: EnvFilter,
) -> impl tracing::Subscriber + use<> {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(writer))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        init();
        init();
    }

    /// The subscriber must actually land events in the rotating file: this
    /// is the diagnostics contract everything else relies on. Scoped with
    /// `with_default` so the global subscriber other tests install cannot
    /// swallow the events.
    #[test]
    fn events_land_in_the_log_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer =
            crate::storage::log::RotatingFileWriter::with_limits(dir.path(), 1 << 20, 2)
                .expect("writer");
        let dispatch = tracing::Dispatch::new(subscriber(
            writer,
            EnvFilter::new("craft::logging::tests=trace"),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::warn!(target: "craft::logging::tests", "diagnostics pipeline check");
        });
        // The writer flushes per event; read straight back.
        let logged = std::fs::read_to_string(dir.path().join("craft.log")).expect("log file");
        assert!(
            logged.contains("diagnostics pipeline check"),
            "event missing from log: {logged}"
        );
    }
}
