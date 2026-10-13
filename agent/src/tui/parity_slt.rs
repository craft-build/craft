//! Golden-fixture parity for the `slt` paint path (wave 2a).
//!
//! Renders the exact same scenes as [`crate::tui::parity`] through
//! `ui::frame::frame` on `slt::TestBackend` and compares against the SAME
//! fixture files. While wave 2a is in flight this test documents the
//! remaining gap per scene; it must be byte-identical before the ratatui
//! path is deleted and the run loop swaps to slt.

#![cfg(test)]

use crate::tui::app::App;
use crate::tui::parity;
use crate::tui::ui::frame::{self, frame};

/// Render a scene twice (prev-frame layout stabilizes scroll/bounds reads)
/// and dump the finished buffer. `status_tick` is reset between the renders
/// so per-frame mutations from the stabilizing pass don't leak into the
/// compared frame — the legacy path renders once from a fresh app, so the
/// final render must start from the same tick 0.
fn render_slt(app: &mut App) -> String {
    let mut view = frame::View::default();
    let mut tb = slt::TestBackend::new(u32::from(parity::W), u32::from(parity::H));
    tb.render(|ui| frame(ui, app, &mut view));
    app.status_tick = 0;
    tb.render(|ui| frame(ui, app, &mut view));
    parity::normalize(parity::buffer_rows(tb.buffer()))
}

#[test]
fn golden_fixture_parity_slt() {
    let update = std::env::var("UPDATE_FIXTURES").is_ok_and(|v| v == "1");
    let update_slt = std::env::var("UPDATE_FIXTURES_SLT").is_ok_and(|v| v == "1");
    let mut failures = Vec::new();
    for (name, mut app) in parity::scenes() {
        let rendered = render_slt(&mut app);
        let path = parity::fixture_path(name);
        if update && !update_slt {
            // Capturing only the ratatui side via `UPDATE_FIXTURES=1`
            // shouldn't silently bless a drifted slt paint; opt in with
            // `UPDATE_FIXTURES=1 UPDATE_FIXTURES_SLT=1` to overwrite from
            // the slt path instead.
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("missing fixture {name} (capture with UPDATE_FIXTURES=1): {e}")
        });
        if rendered != expected {
            let diff_rows = rendered
                .lines()
                .zip(expected.lines().chain(std::iter::repeat("")))
                .enumerate()
                .filter_map(|(i, (got, want))| {
                    (got != want).then(|| format!("  row {i:>2}: got {got:?} want {want:?}"))
                })
                .take(8)
                .collect::<Vec<_>>()
                .join("\n");
            failures.push(format!(
                "scene `{name}` slt paint drifted ({}):\n{diff_rows}",
                path.display(),
            ));
        }
    }
    assert!(failures.is_empty(), "\n\n{}\n", failures.join("\n\n"));
}
