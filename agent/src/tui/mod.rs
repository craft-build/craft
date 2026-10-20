//! Craft's interactive terminal UI: the default surface of the `craft` binary.
//!
//! The UI renders whatever a [`provider::Provider`] streams in; the live
//! backend is [`provider::live::CraftProvider`].

mod animation;
mod app;
mod compat;
mod composer;
mod file_picker;
pub(crate) mod geom;
mod keybindings;
mod modals;
mod notify;
#[cfg(test)]
mod parity;
#[cfg(test)]
mod parity_slt;
mod permission_prompt;
mod plan_form;
mod probe;
pub mod provider;
mod question_form;
mod repaint;
pub(crate) mod search_modal;
mod selection;
mod shell;
mod style;
pub(crate) mod text;
mod ui;

use std::io;

use slt::event::KeyEventKind;
use tokio::sync::mpsc;

use app::App;
use provider::{AgentEvent, Command, Provider, Status};
use ui::theme;

/// Run the terminal UI against `provider` until the user quits.
/// `startup_prompt` (the positional `PROMPT` / piped stdin) is submitted
/// exactly once, after the provider has started up.
///
/// The slt loop owns the terminal session (raw mode, alt screen, mouse,
/// bracketed paste, focus reporting, panic + suspend restore — see
/// `slt::RunConfig` and its terminal module).
pub async fn run<P: Provider>(provider: P, startup_prompt: Option<String>) -> io::Result<()> {
    drive_slt(provider, startup_prompt).await
}

fn write_and_flush(bytes: &str) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout().lock();
    out.write_all(bytes.as_bytes())?;
    out.flush()
}

/// Custom slash commands for the app, honoring the provider's `--no-commands`
/// gate: a disabled session gets an empty list without touching the
/// filesystem. Extracted so the gate is testable without booting the TUI.
fn discover_custom_commands(
    cwd: &std::path::Path,
    enabled: bool,
) -> Vec<crate::command::CustomCommand> {
    if enabled {
        crate::command::discover_commands(cwd)
    } else {
        Vec::new()
    }
}

/// Seed the app's initial mode from the provider (`--mode plan` /
/// `--permission-mode plan`): plan enters with an allocated plan path,
/// exactly as a Tab toggle would. Extracted so the wiring is testable
/// without booting the full TUI loop.
fn apply_initial_mode(app: &mut App, starts_in_plan_mode: bool) {
    if starts_in_plan_mode {
        app.toggle_mode();
    }
}

/// Submit the pending startup prompt, if any: set the composer text and
/// run the normal submit path, so slash commands and bang-mode route the
/// same way a typed prompt would. The `Option::take` makes it fire at most
/// once; `App::submit` no-ops on empty text.
fn submit_startup_prompt(
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    pending: &mut Option<String>,
) {
    if let Some(prompt) = pending.take() {
        app.composer.set_text(prompt);
        app.submit(cmd_tx);
    }
}

/// Whether the base surface owns the keyboard: no modal, search, file
/// picker, permission prompt, or question form is open. Overlays that
/// own the keyboard must swallow keys, including EditInput.
fn base_owns_keyboard(app: &App) -> bool {
    matches!(app.overlays.modal, modals::Modal::None)
        && !app.overlays.search.is_open()
        && !app.overlays.file_picker.is_open()
        && !app.overlays.permission_prompt.is_open()
        && !app.overlays.question_form.is_open()
}

// ---------------------------------------------------------------------------
// The slt run loop
// ---------------------------------------------------------------------------

/// Run the TUI on `slt::run_async_with`: the provider channel is pumped into
/// the render loop's message queue (each send wakes the loop), input events
/// come straight from slt as slt types, and `ui::frame::frame` paints.
///
/// Debug tracing for the render loop (CRAFT_TUI_TRACE=path): appends
/// per-frame lines so runtime behavior can be inspected post-hoc.
fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CRAFT_TUI_TRACE").is_some())
}

fn trace_frame(line: String) {
    use std::io::Write;
    if !trace_enabled() {
        return;
    }
    let Ok(path) = std::env::var("CRAFT_TUI_TRACE") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// `TERM` values of terminals that are truecolor (24-bit) but whose name
/// doesn't carry `256color`. SSH forwards `TERM` but not `COLORTERM`, so
/// these are the only signal left on the remote side.
const TRUECOLOR_TERMS: &[&str] = &[
    "alacritty",
    "alacritty-direct",
    "contour",
    "foot",
    "foot-extra",
    "ghostty",
    "kitty",
    "mintty",
    "mlterm",
    "rio",
    "wezterm",
    "wezterm-ssh",
    "xterm-ghostty",
    "xterm-kitty",
];

/// Color depth for the slt run config, resolved in two evidence layers.
///
/// **Env layer** (extends `slt::ColorDepth::detect`): `NO_COLOR` → no
/// color, `COLORTERM=truecolor|24bit` → truecolor, and — closing the
/// first SSH gap — the known-truecolor `TERM` allowlist plus the terminfo
/// `*-direct` convention, since SSH forwards `TERM` but not `COLORTERM`:
/// without them, `xterm-ghostty` and friends collapsed to 16-color Basic
/// and every bundled theme read as "the coloring disappeared".
///
/// **Wire layer** (`tui::probe`, consulted when the env layer alone would
/// downsample): `xterm-256color` is what most modern truecolor terminals
/// report out of the box (iTerm2, Terminal.app, VS Code, tmux users), so
/// an SSH session to one of them cannot be distinguished from an SSH
/// session to a genuinely 256-color terminal by environment alone. Before
/// slt takes over stdin, the startup probe asks the terminal itself for
/// the terminfo `Tc` direct-color capability — every truecolor terminal
/// behind a plain SSH hop answers, and the ack upgrades EightBit/Basic to
/// truecolor. A silent or 256-only terminal keeps the downsampled depth:
/// the probe only ever adds fidelity, it never speculates.
///
/// `NO_COLOR` is an explicit user demand and stays final in both layers.
fn detect_color_depth() -> slt::ColorDepth {
    let var = |name: &str| std::env::var(name).ok();
    color_depth_for(
        var("NO_COLOR").as_deref(),
        var("COLORTERM").as_deref(),
        var("TERM").as_deref(),
        probe::truecolor_ack(),
    )
}

/// Pure core of [`detect_color_depth`] so the precedence chain is testable
/// without racing other tests through the process environment or stdio.
/// `truecolor_ack` is the wire probe's cached outcome (`None` = the probe
/// never ran — muxed session, non-tty stdio, or the env layer already
/// decided the depth).
fn color_depth_for(
    no_color: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
    truecolor_ack: Option<bool>,
) -> slt::ColorDepth {
    // https://no-color.org — any non-empty value disables color.
    if no_color.is_some_and(|v| !v.is_empty()) {
        return slt::ColorDepth::NoColor;
    }
    if let Some(ct) = colorterm.map(str::to_ascii_lowercase) {
        if ct == "truecolor" || ct == "24bit" {
            return slt::ColorDepth::TrueColor;
        }
    }
    let term = term.unwrap_or_default().trim().to_ascii_lowercase();
    let env_depth = if TRUECOLOR_TERMS.contains(&term.as_str()) || term.ends_with("-direct") {
        slt::ColorDepth::TrueColor
    } else if term.contains("256color") {
        slt::ColorDepth::EightBit
    } else {
        slt::ColorDepth::Basic
    };
    // The terminal's own `Tc` ack outranks a boring TERM, but only
    // upgrades depths that lost fidelity; NO_COLOR returned above is
    // already out of reach by then.
    match (env_depth, truecolor_ack) {
        (slt::ColorDepth::Basic | slt::ColorDepth::EightBit, Some(true)) => {
            slt::ColorDepth::TrueColor
        }
        _ => env_depth,
    }
}

/// Terminal color environment for `craft doctor`: the raw color-relevant
/// variables exactly as the process sees them, whether an SSH daemon or a
/// terminal multiplexer produced that environment, and the depth the env
/// layer resolves on its own (doctor runs the chain with `ack=None`: the
/// live wire probe belongs to the TUI's pre-slt startup window, and
/// firing it from doctor would stack a raw-mode session on doctor's own
/// stdout). Everything here is plain env reading — no stdio, no probes.
pub(crate) fn color_env_doctor() -> serde_json::Value {
    let var = |name: &str| std::env::var(name).ok();
    let env_depth = color_depth_for(
        var("NO_COLOR").as_deref(),
        var("COLORTERM").as_deref(),
        var("TERM").as_deref(),
        None,
    );
    let ssh = ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    serde_json::json!({
        "term": var("TERM"),
        "colorterm": var("COLORTERM"),
        "no_color": var("NO_COLOR"),
        "ssh_session": ssh,
        "multiplexer": if std::env::var_os("TMUX").is_some() { Some("tmux") } else { None },
        "color_depth": format!("{env_depth:?}"),
    })
}

/// Startup sequencing (resume gating on the first `StatusChanged(Done)`),
/// the bell/notification state machine, per-frame maintenance (file picker
/// ticks, image loads, draft checkpointing, typewriter reveal), and
/// input-history persistence on exit. The `$EDITOR` handoff (Alt-O) and
/// the plan-file editor (Ctrl-O) are the in-TUI [`EditorForm`] modal; Ctrl-Z
/// suspend/resume is handled by slt's own signal hook.
async fn drive_slt<P: Provider>(provider: P, startup_prompt: Option<String>) -> io::Result<()> {
    // Probe the terminal's graphics protocol (kitty/sixel vs halfblocks)
    // before slt takes over the terminal: the probe reads its replies
    // straight from stdin (F.6).
    ui::image::probe();
    // Same pre-pump window: ask the terminal for its `Tc` direct-color
    // capability. SSH drops COLORTERM, so for `TERM=xterm-256color` — what
    // most truecolor terminals report — the wire ack is the only evidence
    // that can lift the depth above 256-color (probe.rs);
    // `detect_color_depth` reads it back when the run config is built.
    probe::probe_truecolor();

    // B.11: grab the MCP handle before `start` consumes the provider.
    let mcp = provider.mcp();
    // `--no-commands`: read the gate before `start` consumes the provider.
    let custom_commands = provider.custom_commands();
    // `--mode plan` / `--permission-mode plan`: same for the initial mode.
    let starts_in_plan_mode = provider.starts_in_plan_mode();
    // Startup resume (`-s/--session`, `-c/--continue`): same.
    let expects_startup_resume = provider.expects_startup_resume();
    let (cmd_tx, mut evt_rx) = provider.start();

    let mut app = App::new();
    app.mcp = mcp;
    if let Ok(cwd) = std::env::current_dir() {
        app.custom_commands = discover_custom_commands(&cwd, custom_commands);
    }
    apply_initial_mode(&mut app, starts_in_plan_mode);
    if let Ok(config) = crate::config::Config::load().await {
        let entries: Vec<(String, Vec<String>)> = config.keybindings.into_iter().collect();
        let mut warnings = Vec::new();
        app.overlays.keybinds =
            keybindings::KeybindingResolver::from_overlay(&entries, &mut warnings);
        if let Some(w) = warnings.first() {
            app.flash(w.clone());
        }
    }
    // Recall the persistent input history and theme from the state dir (best effort).
    if let Ok(dir) = crate::storage::StateDir::resolve() {
        if let Some(name) = crate::storage::theme::read_theme_name(&dir)
            && theme::load_by_name(&name).is_ok()
        {
            let _ = theme::set_named(&name);
        }
        app.input_history = crate::storage::input_history::InputHistory::load(
            &dir,
            crate::storage::input_history::MAX_ENTRIES,
        );
    }

    // The frame closure owns the app; hand the session's accumulated input
    // history back out through this box on exit so it can be persisted.
    let history_out: std::sync::Arc<std::sync::Mutex<crate::storage::input_history::InputHistory>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            crate::storage::input_history::InputHistory::default(),
        ));

    let mut view = ui::frame::View::default();
    let mut bells = notify::RunNotificationState::default();
    let mut focus = notify::Focus::default();
    let mut startup_prompt = startup_prompt;
    if !expects_startup_resume {
        submit_startup_prompt(&mut app, &cmd_tx, &mut startup_prompt);
    }

    let config = slt::RunConfig::default()
        .title("CraftCode")
        .mouse(true)
        .tick_rate(std::time::Duration::from_millis(80))
        .color_depth(detect_color_depth())
        // The app owns Ctrl-C (tri-state quit chord through the
        // keybinding table); slt must not exit on it.
        .handle_ctrl_c(false);

    let history_in = std::sync::Arc::clone(&history_out);
    let mut app = app;
    let cmd_tx_frame = cmd_tx.clone();
    let handle = slt::run_async_with::<AgentEvent>(config, move |ui, msgs| {
        if trace_enabled() {
            let evs: Vec<String> = ui
                .raw_events()
                .map(|e| match e {
                    slt::Event::Key(k) => format!("{:?}+{:?}", k.code, k.modifiers),
                    other => format!("{other:?}"),
                })
                .collect();
            trace_frame(format!(
                "frame msgs={} ui_ev={evs:?} busy={:?} status={:?} conv_len={} reveal_anim={}",
                msgs.len(),
                app.busy(),
                app.status,
                app.conversation.messages.len(),
                app.conversation.reveal_pending(true),
            ));
        }
        // ── Provider events (bounded burst; slt wakes on each send) ──
        for ev in msgs.drain(..) {
            let was_busy = app.busy();
            let was_waiting = app.status == Status::WaitingApproval;
            let settled_done = matches!(&ev, AgentEvent::StatusChanged(Status::Done));
            if let AgentEvent::AssistantText(text) = &ev
                && was_busy
            {
                // Candidate bell payload for this turn's completion.
                bells.on_turn_complete(text);
            }
            app.handle_event(ev);
            if settled_done {
                // The provider settled (startup complete): a held startup
                // prompt is now safe to submit — after any `SessionLoaded`
                // the same burst carried.
                submit_startup_prompt(&mut app, &cmd_tx_frame, &mut startup_prompt);
            }
            if !was_busy && app.busy() {
                bells.on_new_turn();
            }
            if app.busy() || app.status == Status::WaitingApproval {
                // Still working: nothing can settle yet.
            } else if was_busy || was_waiting {
                let failed = app.status == Status::Failed;
                bells.on_done(failed);
                // No queue to drain: the turn settled the moment the
                // status landed.
                bells.on_drain();
            }
            let attention = (!was_waiting && app.status == Status::WaitingApproval)
                .then_some(notify::Notification::PermissionRequested { tool: None });
            let settled = !app.busy() && app.status != Status::WaitingApproval;
            if let Some(notification) = bells.reconcile(attention, settled)
                && focus.allows(&notification)
            {
                ring();
            }
        }

        // ── Input events ──
        //
        // The app owns the keyboard: every event dispatched below is
        // marked consumed so the focused input widgets (the composer
        // textarea, wave 2b) render + carry the caret but never edit —
        // `App`'s composer state stays the single source of truth (the
        // render mirrors sync from it each frame). Mouse events stay
        // unconsumed so clicks keep their widget-focus meaning.
        //
        // `raw_events` (not `events`): the plain accessor is blocked while
        // a modal was active the previous frame — with the editor modal
        // open, the closure would see no keys at all and Esc/Ctrl-S could
        // never fire. The app is a global input router; the raw stream is
        // the documented bypass. Indices align with the consumed flags
        // (nothing consumes before the closure runs).
        let events: Vec<(usize, slt::Event)> = ui
            .raw_events()
            .enumerate()
            .map(|(i, ev)| (i, ev.clone()))
            .collect();
        for (i, ev) in events {
            match ev {
                slt::Event::Key(key) if key.kind == KeyEventKind::Press => {
                    focus.note_input();
                    // The editor modal owns the keyboard while open (see
                    // `editor_modal_key` for the contract).
                    if view.editor.is_some() {
                        if editor_modal_key(&mut app, &mut view, &key) {
                            ui.consume_event(i);
                        }
                        continue;
                    }
                    // Alt-O hands the composer to the in-TUI editor.
                    if app
                        .overlays
                        .keybinds
                        .matches(keybindings::ActionId::EditInput, &key)
                        && base_owns_keyboard(&app)
                    {
                        view.editor = Some(ui::frame::EditorForm::open(
                            ui::frame::EditorOrigin::ComposerDraft,
                            &app.composer.text,
                        ));
                    } else {
                        app.handle_key(key, &cmd_tx_frame);
                    }
                    ui.consume_event(i);
                    // Ctrl-O plan-editor request: open the plan file in
                    // the in-TUI editor.
                    if let Some(path) = app.take_editor_request() {
                        let text = std::fs::read_to_string(&path).unwrap_or_default();
                        view.editor = Some(ui::frame::EditorForm::open(
                            ui::frame::EditorOrigin::PlanFile(path),
                            &text,
                        ));
                    }
                }
                slt::Event::Paste(text) => {
                    focus.note_input();
                    // While the editor is open the paste belongs to its
                    // textarea (left unconsumed, the widget inserts it
                    // natively); otherwise the composer takes it.
                    if view.editor.is_none() {
                        app.insert_paste(&text);
                        ui.consume_event(i);
                    }
                }
                slt::Event::Mouse(mouse) => {
                    focus.note_input();
                    app.handle_mouse(mouse);
                }
                slt::Event::FocusGained => focus.report(notify::Focus::Focused),
                slt::Event::FocusLost => focus.report(notify::Focus::Unfocused),
                _ => {} // Resize: slt relayouts; the next frame repaints.
            }
        }

        if app.should_quit {
            bells.on_manual_exit();
            *history_in.lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut app.input_history);
            ui.quit();
            return;
        }

        // Per-frame maintenance: the file picker's walker ticks on its own,
        // clipboard/file image loads land here (F.6), the draft gets its
        // checkpoint (F.3), and the typewriter reveal advances.
        app.tick_file_picker();
        app.poll_image_loads();
        app.sync_draft(&cmd_tx_frame);
        app.tick_reveal(std::time::Instant::now());

        ui::frame::frame(ui, &mut app, &mut view);
        let vis = app
            .conversation
            .visible_text(app.conversation.messages.len().saturating_sub(1))
            .map(str::len)
            .unwrap_or(0);
        let (pw, mh) = app
            .view
            .paint
            .lock()
            .map(|p| (p.width, p.msg_area.height))
            .unwrap_or((0, 0));
        if trace_enabled() {
            trace_frame(format!("painted vis_len={vis} paint_w={pw} msg_h={mh}"));
        }
    })
    .map_err(|e| io::Error::other(e.to_string()))?;

    // Pump provider events into the render loop; each send wakes it.
    let sender = handle.sender();
    let forward = tokio::spawn(async move {
        while let Some(ev) = evt_rx.recv().await {
            if sender.send(ev).await.is_err() {
                break;
            }
        }
    });

    let result = handle
        .join()
        .await
        .map_err(|e| io::Error::other(e.to_string()));
    forward.abort();

    // Persist the input history gathered this session (best effort).
    if let Ok(dir) = crate::storage::StateDir::resolve() {
        let history = history_out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = history.save(&dir);
    }
    result
}

/// Keys for the open editor modal: `true` when the key was consumed.
/// Ctrl-S saves (apply to the origin and close), Esc cancels.
/// While the editor modal is open: does the focused `TextareaState` edit
/// on this key (so the run-loop closure must leave it unconsumed)? Mirrors
/// the widget's own consumption set — everything else is swallowed by the
/// editor (it owns the keyboard) except Esc (cancel) and Ctrl-S (save).
fn editor_key_edits_widget(key: &slt::event::KeyEvent) -> bool {
    use slt::event::{KeyCode, KeyModifiers};

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let global = ctrl
        || key.modifiers.contains(KeyModifiers::SUPER)
        || key.modifiers.contains(KeyModifiers::META);
    match key.code {
        KeyCode::Char('z' | 'y' | 'k') if ctrl => true, // undo / redo / kill line
        KeyCode::Left | KeyCode::Right if ctrl || alt => true, // word jumps
        KeyCode::Char(_) => !global,
        KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete => true,
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => true,
        KeyCode::Home | KeyCode::End => true,
        _ => false,
    }
}

/// Route one keypress while the editor modal is open. Returns whether the
/// run loop must consume the event: `false` only for the widget's own edit
/// keys (see [`editor_key_edits_widget`]), which the focused `TextareaState`
/// handles natively during render.
///
/// Esc cancels; Ctrl-S saves (composer draft / plan file); Ctrl-A/E keep
/// their readline meaning by moving the state cursor; everything else is
/// swallowed — the editor owns the keyboard, the 2a `handle_editor_key`
/// contract.
fn editor_modal_key(
    app: &mut crate::tui::app::App,
    view: &mut ui::frame::View,
    key: &slt::event::KeyEvent,
) -> bool {
    use slt::event::{KeyCode, KeyModifiers};

    if key.code == KeyCode::Esc {
        view.editor = None;
        return true;
    }
    if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let editor = view.editor.take().expect("editor open");
        let text = editor.text();
        match editor.origin {
            ui::frame::EditorOrigin::ComposerDraft => {
                app.composer.set_text(text);
            }
            ui::frame::EditorOrigin::PlanFile(path) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("warning: could not write plan file: {e}");
                }
            }
        }
        return true;
    }
    // Readline home/end (the widget has no Ctrl-A/E).
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(editor) = view.editor.as_mut() {
            match key.code {
                KeyCode::Char('a') => editor.state.cursor_col = 0,
                KeyCode::Char('e') => {
                    editor.state.cursor_col =
                        editor.state.lines[editor.state.cursor_row].chars().count();
                }
                _ => {}
            }
        }
        return true;
    }
    // The widget's own edit keys stay unconsumed; everything else is
    // swallowed so no other handler sees keys while the editor is open.
    !editor_key_edits_widget(key)
}

/// The terminal bell (writes exactly the classic `\a` sequence).
fn ring() {
    let _ = write_and_flush("\u{7}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::Status;

    /// Wave 2b routing contract: while the editor modal is open,
    /// `editor_modal_key` consumes everything except the textarea's own
    /// edit keys; Esc cancels, Ctrl-S saves into the composer.
    #[test]
    fn editor_modal_key_routes_save_cancel_and_swallows() {
        use crate::tui::compat::slt_key;
        use crate::tui::ui::frame::{EditorForm, EditorOrigin, View};
        use slt::event::{KeyCode, KeyModifiers};

        let mut app = App::new();
        let mut view = View::default();
        view.editor = Some(EditorForm::open(EditorOrigin::ComposerDraft, "d"));

        // Typing stays unconsumed (the widget edits during render).
        assert!(!editor_modal_key(
            &mut app,
            &mut view,
            &slt_key(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
        assert!(view.editor.is_some(), "typing does not close the editor");

        // Stray chords are swallowed, not leaked to the app.
        assert!(editor_modal_key(
            &mut app,
            &mut view,
            &slt_key(KeyCode::F(4), KeyModifiers::NONE)
        ));
        assert!(editor_modal_key(
            &mut app,
            &mut view,
            &slt_key(KeyCode::Char('r'), KeyModifiers::CONTROL)
        ));

        // Ctrl-S saves and closes; the composer gets the text + end cursor.
        assert!(editor_modal_key(
            &mut app,
            &mut view,
            &slt_key(KeyCode::Char('s'), KeyModifiers::CONTROL)
        ));
        assert!(view.editor.is_none());
        assert_eq!(app.composer.text, "d");
        assert_eq!(app.composer.cursor, 1);

        // Esc cancels without touching the composer.
        view.editor = Some(EditorForm::open(EditorOrigin::ComposerDraft, "nope"));
        assert!(editor_modal_key(
            &mut app,
            &mut view,
            &slt_key(KeyCode::Esc, KeyModifiers::NONE)
        ));
        assert!(view.editor.is_none());
        assert_eq!(app.composer.text, "d");
    }

    /// The widget-edit predicate matches exactly the textarea's own
    /// consumption set (see `editor_key_edits_widget`).
    #[test]
    fn editor_edit_keys_match_the_widget_set() {
        use crate::tui::compat::slt_key;
        use slt::event::{KeyCode, KeyModifiers};

        for (code, mods, edits) in [
            (KeyCode::Char('a'), KeyModifiers::NONE, true),
            (KeyCode::Enter, KeyModifiers::NONE, true),
            (KeyCode::Backspace, KeyModifiers::NONE, true),
            (KeyCode::Up, KeyModifiers::NONE, true),
            (KeyCode::Home, KeyModifiers::NONE, true),
            (KeyCode::Left, KeyModifiers::CONTROL, true),
            (KeyCode::Char('z'), KeyModifiers::CONTROL, true),
            (KeyCode::Char('s'), KeyModifiers::CONTROL, false),
            (KeyCode::Esc, KeyModifiers::NONE, false),
            (KeyCode::Char('a'), KeyModifiers::CONTROL, false),
            (KeyCode::F(4), KeyModifiers::NONE, false),
        ] {
            assert_eq!(
                editor_key_edits_widget(&slt_key(code.clone(), mods)),
                edits,
                "{code:?} + {mods:?}"
            );
        }
    }

    #[test]
    fn base_owns_keyboard_tracks_overlays() {
        let mut app = App::new();
        assert!(base_owns_keyboard(&app));
        app.overlays.modal = modals::Modal::Help;
        assert!(!base_owns_keyboard(&app));
        app.overlays.modal = modals::Modal::None;
        app.overlays.file_picker.open("");
        assert!(!base_owns_keyboard(&app));
    }

    /// The SSH case: `TERM` arrives (truecolor terminal, no `256color` in
    /// the name) but `COLORTERM` does not — the depth must still resolve to
    /// truecolor instead of collapsing the theme into 16 ANSI colors.
    /// The ack is `None`: the env layer already decides, so the wire probe
    /// never fires on allowlisted terminals.
    #[test]
    fn truecolor_term_survives_missing_colorterm_over_ssh() {
        for term in [
            "xterm-ghostty",
            "XTerm-Ghostty",
            "xterm-kitty",
            "alacritty",
            "wezterm",
            "wezterm-ssh",
            "foot",
            "xterm-direct",
        ] {
            assert_eq!(
                color_depth_for(None, None, Some(term), Some(false)),
                slt::ColorDepth::TrueColor,
                "TERM={term}"
            );
        }
    }

    /// slt's original precedence must survive the extension: NO_COLOR wins,
    /// COLORTERM confirms truecolor, `*256color` stays 256-color, and an
    /// unknown TERM still falls back to 16-color Basic.
    #[test]
    fn color_depth_precedence_chain_is_preserved() {
        assert_eq!(
            color_depth_for(Some("1"), Some("truecolor"), Some("xterm-ghostty"), Some(true)),
            slt::ColorDepth::NoColor
        );
        assert_eq!(
            color_depth_for(None, Some("24bit"), Some("xterm"), Some(false)),
            slt::ColorDepth::TrueColor
        );
        assert_eq!(
            color_depth_for(None, None, Some("xterm-256color"), None),
            slt::ColorDepth::EightBit
        );
        assert_eq!(
            color_depth_for(None, None, Some("tmux-256color"), None),
            slt::ColorDepth::EightBit
        );
        assert_eq!(
            color_depth_for(None, None, Some("vt220"), None),
            slt::ColorDepth::Basic
        );
        assert_eq!(
            color_depth_for(None, None, None, None),
            slt::ColorDepth::Basic
        );
    }

    /// The wire-probe upgrade (tui::probe): an SSH session with
    /// `TERM=xterm-256color` — what most truecolor terminals report — stays
    /// downsampled when the terminal is silent, and lifts to truecolor when
    /// it acks the XTGETTCAP `Tc` query. The unprobed case (`None`) keeps
    /// the pre-probe behavior.
    #[test]
    fn probe_ack_lifts_a_downsampled_ssh_session_to_truecolor() {
        assert_eq!(
            color_depth_for(None, None, Some("xterm-256color"), Some(true)),
            slt::ColorDepth::TrueColor,
            "an acked truecolor terminal behind SSH renders all colors"
        );
        assert_eq!(
            color_depth_for(None, None, Some("xterm-256color"), Some(false)),
            slt::ColorDepth::EightBit,
            "a silent or genuinely 256-color terminal keeps the 256-color depth"
        );
        assert_eq!(
            color_depth_for(None, None, Some("xterm-256color"), None),
            slt::ColorDepth::EightBit,
            "no probe (muxed, non-tty, or a session that skipped it): env stands"
        );
    }

    /// The upgrade also rescues unknown TERM values (a remote shell that
    /// rewrote a fancy terminal's TERM to something plain) — and still never
    /// escapes an explicit NO_COLOR.
    #[test]
    fn probe_ack_rescues_unknown_terms_but_not_no_color() {
        assert_eq!(
            color_depth_for(None, None, Some("vt220"), Some(true)),
            slt::ColorDepth::TrueColor
        );
        assert_eq!(
            color_depth_for(None, None, Some("vt220"), Some(false)),
            slt::ColorDepth::Basic
        );
        assert_eq!(
            color_depth_for(Some("1"), None, Some("xterm-256color"), Some(true)),
            slt::ColorDepth::NoColor,
            "NO_COLOR is an explicit user demand; even an ack cannot override it"
        );
    }

    /// Digest a provider burst exactly as `drive_slt`'s frame closure does:
    /// hand each event to the app, and submit a held startup prompt when
    /// the burst carries the provider's first `Done` (the resume settle).
    async fn digest_burst(
        app: &mut App,
        cmd_tx: &mpsc::UnboundedSender<Command>,
        pending: &mut Option<String>,
        events: Vec<AgentEvent>,
    ) {
        for ev in events {
            let settled_done = matches!(&ev, AgentEvent::StatusChanged(Status::Done));
            app.handle_event(ev);
            if settled_done {
                submit_startup_prompt(app, cmd_tx, pending);
            }
        }
    }

    /// Startup sequencing with a resume expected: the prompt waits for the
    /// provider's first `Done` — which a successful resume always sends after
    /// `SessionLoaded` — then submits exactly once, in the mode the session
    /// restored. A second `Done` in the same burst must not double-submit.
    #[tokio::test]
    async fn startup_prompt_waits_for_resume_then_submits_once_in_loaded_mode() {
        use crate::tui::provider::LoadedMessage;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        let mut pending = Some("do x".to_string());
        digest_burst(
            &mut app,
            &cmd_tx,
            &mut pending,
            vec![
                AgentEvent::SessionLoaded {
                    messages: vec![LoadedMessage::User("earlier".into())],
                    draft: String::new(),
                    mode: Some(crate::tui::provider::SessionMode::Plan),
                },
                AgentEvent::StatusChanged(Status::Done),
                AgentEvent::StatusChanged(Status::Done),
            ],
        )
        .await;
        assert!(
            app.mode == app::Mode::Plan,
            "the loaded session's mode wins over the CLI default"
        );
        let sent = sent_messages(&mut cmd_rx);
        assert_eq!(sent.len(), 1, "one submission, two Dones notwithstanding");
        assert!(
            matches!(sent[0].1, crate::run::AgentMode::Plan(_)),
            "the first turn after resume runs in the restored plan mode"
        );
    }

    /// A failed resume (notices, no `SessionLoaded`) still settles with a
    /// `Done`: the held prompt submits exactly once, in the CLI-seeded mode.
    #[tokio::test]
    async fn startup_prompt_submits_once_after_failed_resume_with_cli_mode() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        apply_initial_mode(&mut app, true);
        let mut pending = Some("do x".to_string());
        digest_burst(
            &mut app,
            &cmd_tx,
            &mut pending,
            vec![
                AgentEvent::Notice {
                    tone: provider::Tone::Warning,
                    text: "resume failed".into(),
                },
                AgentEvent::StatusChanged(Status::Done),
            ],
        )
        .await;
        assert!(
            app.mode == app::Mode::Plan,
            "no stored mode keeps the CLI seed"
        );
        let sent = sent_messages(&mut cmd_rx);
        assert_eq!(sent.len(), 1);
        assert!(
            matches!(sent[0].1, crate::run::AgentMode::Plan(_)),
            "the CLI-seeded plan mode carries the failed-resume first turn"
        );
    }
}

/// `--no-commands`: the discovery gate yields zero custom commands without
/// touching the filesystem, even with commands installed on disk.
#[test]
fn no_commands_gate_yields_zero_discovered_commands() {
    let dir = tempfile::tempdir().unwrap();
    let cmd_dir = dir.path().join(".craft/commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::write(cmd_dir.join("ship.md"), "Ship it: $ARGUMENTS").unwrap();
    assert!(
        !discover_custom_commands(dir.path(), true).is_empty(),
        "the fixture command is discovered when enabled"
    );
    assert!(
        discover_custom_commands(dir.path(), false).is_empty(),
        "--no-commands must yield an empty command list"
    );
}

/// `--mode plan` / `--permission-mode plan` seed the app's initial mode:
/// plan enters with an allocated plan path (as Tab would), build stays put.
#[test]
fn initial_mode_seeds_the_app() {
    let mut app = App::new();
    apply_initial_mode(&mut app, true);
    assert!(
        app.mode == app::Mode::Plan,
        "plan flag starts the TUI in plan mode"
    );
    let mut app = App::new();
    apply_initial_mode(&mut app, false);
    assert!(app.mode == app::Mode::Build);
}

/// Collect the SendMessage commands a channel carries, dropping the other
/// commands (SetDraft housekeeping and friends) the loop emits.
#[cfg(test)]
fn sent_messages(
    rx: &mut mpsc::UnboundedReceiver<Command>,
) -> Vec<(String, crate::run::AgentMode)> {
    let mut sent = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let Command::SendMessage(text, mode, images) = cmd {
            assert!(images.is_empty(), "a startup prompt carries no images");
            sent.push((text, mode));
        }
    }
    sent
}

/// A plan-seeded app submits the startup prompt in plan mode: exactly one
/// SendMessage, carrying `AgentMode::Plan` with the app-allocated plan path
/// (the no-accidental-Build-writes guard). A second submission never fires.
#[test]
fn startup_prompt_submits_in_plan_mode_exactly_once() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    apply_initial_mode(&mut app, true);
    let mut pending = Some("do x".to_string());
    submit_startup_prompt(&mut app, &cmd_tx, &mut pending);
    let sent = sent_messages(&mut cmd_rx);
    assert_eq!(sent.len(), 1, "the startup prompt submits exactly once");
    assert_eq!(sent[0].0, "do x");
    assert!(
        matches!(sent[0].1, crate::run::AgentMode::Plan(_)),
        "a plan-seeded session's first turn must run in plan mode"
    );
    assert!(app.mode == app::Mode::Plan);
    assert!(
        app.composer.text.is_empty(),
        "the submitted prompt leaves the composer empty"
    );
    // The pending option is consumed: a second settle submits nothing.
    submit_startup_prompt(&mut app, &cmd_tx, &mut pending);
    assert!(sent_messages(&mut cmd_rx).is_empty());
}

/// A build-mode app submits in build mode; a `None` startup prompt (no
/// positional, interactive stdin) submits nothing at all.
#[test]
fn startup_prompt_submits_build_mode_and_none_submits_nothing() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    let mut pending = Some("do x".to_string());
    submit_startup_prompt(&mut app, &cmd_tx, &mut pending);
    let sent = sent_messages(&mut cmd_rx);
    assert_eq!(sent.len(), 1);
    assert!(matches!(sent[0].1, crate::run::AgentMode::Build));

    let mut pending: Option<String> = None;
    submit_startup_prompt(&mut app, &cmd_tx, &mut pending);
    assert!(sent_messages(&mut cmd_rx).is_empty());
}
