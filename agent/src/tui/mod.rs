//! Craft's interactive terminal UI: the default surface of the `craft` binary.
//!
//! The UI renders whatever a [`provider::Provider`] streams in; the live
//! backend is [`provider::live::CraftProvider`].

mod animation;
mod app;
mod composer;
mod file_picker;
mod hyperlink;
mod keybindings;
mod modals;
mod notify;
mod permission_prompt;
mod plan_form;
pub mod provider;
mod question_form;
mod repaint;
pub(crate) mod search_modal;
mod selection;
mod shell;
mod ui;

use std::cell::RefCell;
use std::io;
use std::path::Path;

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use app::App;
use provider::{AgentEvent, Command, Provider, Status};
use repaint::{Dirty, IDLE_POLL};
use ui::theme;

const EVENT_DRAIN_BUDGET: usize = 256;

/// Run the terminal UI against `provider` until the user quits.
pub async fn run<P: Provider>(provider: P) -> io::Result<()> {
    enable_raw_mode()?;
    // Mouse capture: terminals then show the standard arrow pointer on hover
    // instead of the I-beam text cursor. Bracketed paste: multi-line pastes
    // arrive as one Paste event instead of per-line Enter keypresses. Focus
    // change: bell notifications only fire when the user is not watching.
    io::stdout()
        .execute(EnterAlternateScreen)?
        .execute(EnableMouseCapture)?
        .execute(EnableBracketedPaste)?
        .execute(EnableFocusChange)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // Make sure the terminal is restored on panic too.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = drive(terminal, provider).await;

    disable_raw_mode()?;
    io::stdout()
        .execute(DisableBracketedPaste)?
        .execute(DisableFocusChange)?
        .execute(DisableMouseCapture)?
        .execute(LeaveAlternateScreen)?;
    result
}

/// CSI ?2026h — begin synchronized output. Terminals that support it batch
/// all subsequent writes until the matching end, eliminating the partial-frame
/// flicker a full-buffer diff flush can otherwise cause during rapid
/// redraws. Unknown `?` private modes are ignored by spec, so unsupported
/// terminals are unaffected.
const SYNC_BEGIN: &str = "\u{1b}[?2026h";
const SYNC_END: &str = "\u{1b}[?2026l";

fn write_and_flush(bytes: &str) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout().lock();
    out.write_all(bytes.as_bytes())?;
    out.flush()
}

/// Custom slash commands for the app, honoring the provider's `--no-commands`
/// gate: a disabled session gets an empty list without touching the
/// filesystem. Extracted from `drive` so the gate is testable without
/// booting the full TUI loop.
fn discover_custom_commands(cwd: &Path, enabled: bool) -> Vec<crate::command::CustomCommand> {
    if enabled {
        crate::command::discover_commands(cwd)
    } else {
        Vec::new()
    }
}

/// Seed the app's initial mode from the provider (`--mode plan` /
/// `--permission-mode plan`): plan enters with an allocated plan path,
/// exactly as a Tab toggle would. Extracted from `drive` so the wiring is
/// testable without booting the full TUI loop.
fn apply_initial_mode(app: &mut App, starts_in_plan_mode: bool) {
    if starts_in_plan_mode {
        app.toggle_mode();
    }
}

/// Emits the synchronized-output begin sequence and flushes, so the terminal
/// enters batched-update mode before the next frame diff.
fn begin_synchronized_output() {
    let _ = write_and_flush(SYNC_BEGIN);
}

/// Emits the synchronized-output end sequence and flushes, releasing the
/// batched frame for display.
fn end_synchronized_output() {
    let _ = write_and_flush(SYNC_END);
}

async fn drive<P: Provider>(
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    provider: P,
) -> io::Result<()> {
    // Probe the terminal's graphics protocol (kitty/sixel vs halfblocks)
    // before the input reader thread starts: the probe reads its replies
    // straight from stdin (F.6).
    ui::image::probe();

    // B.11: grab the MCP handle before `start` consumes the provider.
    let mcp = provider.mcp();
    // `--no-commands`: read the gate before `start` consumes the provider.
    let custom_commands = provider.custom_commands();
    // `--mode plan` / `--permission-mode plan`: same for the initial mode.
    let starts_in_plan_mode = provider.starts_in_plan_mode();
    let (cmd_tx, evt_rx): (mpsc::UnboundedSender<Command>, _) = provider.start();

    // Crossterm events are blocking reads -> pump them on a dedicated thread.
    let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut app = App::new();
    // B.11: the session's MCP client (snapshot reads + Toggle/Reconnect
    // commands from the `/mcp` screen). Providers without one leave it None.
    app.mcp = mcp;
    // Custom slash commands (J.5): discovered from the working directory's
    // project ancestors and the user's global config dirs. `--no-commands`
    // (via the provider gate) skips discovery entirely.
    if let Ok(cwd) = std::env::current_dir() {
        app.custom_commands = discover_custom_commands(&cwd, custom_commands);
    }
    apply_initial_mode(&mut app, starts_in_plan_mode);
    // Data-driven keybindings (F.1): apply the user's config overlay on top
    // of the compile-time defaults; surface the first problem as a flash.
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

    let terminal = RefCell::new(terminal);
    let result = run_loop(
        &mut app,
        &cmd_tx,
        input_rx,
        evt_rx,
        |app| {
            begin_synchronized_output();
            let draw = {
                let mut term = terminal.borrow_mut();
                term.draw(|f| ui::draw(f, app)).map(|_| ())
            };
            end_synchronized_output();
            draw
        },
        |app| {
            let edited = edit_temp_content(&app.composer.text)?;
            app.composer.set_text(edited);
            // A full clear: the alternate screen came back with whatever the
            // editor left in the diff buffers.
            terminal.borrow_mut().clear().map_err(io::Error::other)
        },
        |path: &std::path::Path| {
            // Plan editor handoff (Ctrl-O): park the UI, run the editor on
            // the plan file, then rebuild the alternate screen.
            open_in_editor(path).map_err(io::Error::other)?;
            terminal.borrow_mut().clear().map_err(io::Error::other)
        },
        || {
            suspend_terminal(&mut terminal.borrow_mut());
            Ok(())
        },
        || {
            // Bell: crossterm's `bell()` writes exactly this sequence.
            let _ = write_and_flush("\u{7}");
        },
        notify::Focus::default(),
    )
    .await;

    // Persist the input history gathered this session (best effort).
    if let Ok(dir) = crate::storage::StateDir::resolve() {
        let _ = app.input_history.save(&dir);
    }
    result
}

// ---------------------------------------------------------------------------
// $EDITOR handoff (Alt-O)
// ---------------------------------------------------------------------------

/// True for the Ctrl-Z chord that suspends the process (Unix only). Like
/// the reference, this binding always wins: it is checked before any modal
/// or composer handling, and it is not remappable.
fn is_suspend_key(key: &KeyEvent) -> bool {
    cfg!(unix) && key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL)
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

/// Write `content` to a temp file, open it in the user's editor, and return
/// the edited text. Ported from the reference `terminal::edit_temp_content`.
fn edit_temp_content(content: &str) -> Result<String, io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("craft-input-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| io::Error::other(format!("failed to create temp file: {e}")))?;
    std::fs::write(tmp.path(), content)
        .map_err(|e| io::Error::other(format!("failed to write temp file: {e}")))?;

    open_in_editor(tmp.path()).map_err(io::Error::other)?;

    std::fs::read_to_string(tmp.path())
        .map_err(|e| io::Error::other(format!("failed to read edited content: {e}")))
}

/// $VISUAL beats $EDITOR (reference order); the value may carry arguments
/// (e.g. `code --wait`), split with basic quote awareness.
fn editor_command() -> Result<Vec<String>, String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .map_err(|_| "set $VISUAL or $EDITOR to edit in an editor".to_string())?;
    parse_editor_args(&editor)
}

/// Split an editor spec on whitespace, honoring single/double quotes so
/// `EDITOR="my editor -x"` works.
fn parse_editor_args(editor: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for c in editor.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    has_token = true;
                }
                c if c.is_whitespace() => {
                    if has_token {
                        args.push(std::mem::take(&mut cur));
                        has_token = false;
                    }
                }
                c => {
                    cur.push(c);
                    has_token = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("unbalanced quote in $VISUAL or $EDITOR".to_string());
    }
    if has_token {
        args.push(cur);
    }
    if args.is_empty() {
        return Err("empty $VISUAL or $EDITOR".to_string());
    }
    Ok(args)
}

/// Park the UI, run the editor synchronously, restore the UI. Ported from
/// the reference `terminal::open_in_editor` teardown/resume cycle.
fn open_in_editor(path: &Path) -> Result<(), String> {
    let args = editor_command()?;

    disable_raw_mode().ok();
    let _ = io::stdout()
        .execute(DisableBracketedPaste)
        .and_then(|o| o.execute(DisableMouseCapture))
        .and_then(|o| o.execute(LeaveAlternateScreen));

    let result = std::process::Command::new(&args[0])
        .args(&args[1..])
        .arg(path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    enable_raw_mode().ok();
    let _ = io::stdout()
        .execute(EnterAlternateScreen)
        .and_then(|o| o.execute(EnableMouseCapture))
        .and_then(|o| o.execute(EnableBracketedPaste));

    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("editor exited with {status}")),
        Err(e) => Err(format!("failed to open {}: {e}", args[0])),
    }
}

/// Teardown, SIGTSTP, resume: the classic terminal-app suspend cycle. The
/// foreground job gets the TTY back, the process parks until `fg`, and the
/// alternate screen is rebuilt from scratch afterwards. Ported from the
/// reference `terminal::suspend`.
fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    disable_raw_mode().ok();
    let _ = io::stdout()
        .execute(DisableBracketedPaste)
        .and_then(|o| o.execute(DisableMouseCapture))
        .and_then(|o| o.execute(LeaveAlternateScreen));
    #[cfg(unix)]
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    enable_raw_mode().ok();
    let _ = io::stdout()
        .execute(EnterAlternateScreen)
        .and_then(|o| o.execute(EnableMouseCapture))
        .and_then(|o| o.execute(EnableBracketedPaste));
    // The shell and any job-control echo wrote to the primary screen while
    // we were away; the alternate screen's diff buffers are stale.
    let _ = terminal.clear();
}

/// The dirty-flag event loop: paint only when a frame is owed, and sleep one
/// cadence frame per turn instead of a fixed interval, so a spinner costs
/// ~12 paints a second and a settled session none.
///
/// Real events always owe a frame (handlers are not asked to prove they
/// changed something); a pure timeout owes one only when the current cadence
/// moves pixels on its own (spinner). The first frame always paints.
#[allow(clippy::too_many_arguments)] // closures + focus seed; event-loop wiring
async fn run_loop(
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    mut input_rx: mpsc::UnboundedReceiver<Event>,
    mut evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut paint: impl FnMut(&mut App) -> io::Result<()>,
    mut edit_composer: impl FnMut(&mut App) -> io::Result<()>,
    mut open_plan_file: impl FnMut(&std::path::Path) -> io::Result<()>,
    mut suspend_ui: impl FnMut() -> io::Result<()>,
    mut ring: impl FnMut(),
    focus: notify::Focus,
) -> io::Result<()> {
    let mut dirty = Dirty::YES;
    // A closed provider channel just stops being selected on; only the input
    // reader dying ends the session.
    let mut provider_alive = true;
    // Bell bookkeeping: only rings when the user is not already watching.
    let mut focus = focus;
    let mut bells = notify::RunNotificationState::default();

    loop {
        // The file picker's walker is the one change nothing else announces.
        if app.tick_file_picker() {
            dirty = Dirty::YES;
        }
        // Clipboard/file image loads land the same way (F.6).
        if app.poll_image_loads() {
            dirty = Dirty::YES;
        }
        // F.3 draft preservation: per-frame checkpoint of the composer text.
        app.sync_draft(cmd_tx);
        let cadence = app.cadence();
        let sleep = tokio::time::sleep(cadence.frame().unwrap_or(IDLE_POLL));
        tokio::select! {
            ev = input_rx.recv() => {
                match ev {
                    // The input reader stopped: the terminal is gone.
                    None => return Ok(()),
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        focus.note_input();
                        // Alt-O hands the composer to $EDITOR; it must run
                        // here, where the terminal is reachable to suspend
                        // and restore the UI around the child process.
                    // Ctrl-Z suspends the process; it must run here, where
                    // the terminal is reachable to tear down and restore the
                    // UI around SIGTSTP. It wins over every other handler,
                    // including open modals (reference semantics).
                    if is_suspend_key(&key) {
                        suspend_ui()?;
                        focus.on_resume();
                    } else if app
                        .overlays.keybinds
                        .matches(keybindings::ActionId::EditInput, key)
                        && base_owns_keyboard(app)
                    {
                            if let Err(e) = edit_composer(app) {
                                eprintln!("warning: could not open editor: {e}");
                            }
                            // The editor ate the focus reports we would have
                            // seen around it.
                            focus.on_resume();
                        } else {
                            app.handle_key(key, cmd_tx);
                        }
                        // Ctrl-O plan-editor handoff: the child process
                        // needs the terminal, so it runs here, not in
                        // handle_key (like the Alt-O composer handoff).
                        if let Some(path) = app.take_editor_request() {
                            if let Err(e) = open_plan_file(&path) {
                                eprintln!("warning: could not open editor: {e}");
                            }
                            // The editor ate the focus reports we would
                            // have seen around it.
                            focus.on_resume();
                        }
                    }
                    Some(Event::Paste(text)) => {
                        focus.note_input();
                        app.insert_paste(&text);
                    }
                    Some(Event::Mouse(mouse)) => {
                        focus.note_input();
                        app.handle_mouse(mouse);
                    }
                    Some(Event::FocusGained) => focus.report(notify::Focus::Focused),
                    Some(Event::FocusLost) => focus.report(notify::Focus::Unfocused),
                    Some(_) => {} // Resize etc: still repaints below.
                }
                dirty = Dirty::YES;
            }
            ev = evt_rx.recv(), if provider_alive => {
                if let Some(first) = ev {
                    // Consume a bounded burst before painting, without letting
                    // a continuously ready provider starve terminal input.
                    let pending = std::iter::once(first)
                        .chain(std::iter::from_fn(|| evt_rx.try_recv().ok()))
                        .take(EVENT_DRAIN_BUDGET);
                    for ev in pending {
                        let was_busy = app.busy();
                        let was_waiting = app.status == Status::WaitingApproval;
                        if let AgentEvent::AssistantText(text) = &ev
                            && was_busy
                        {
                            // Candidate bell payload for this turn's
                            // completion.
                            bells.on_turn_complete(text);
                        }
                        app.handle_event(ev);
                        if !was_busy && app.busy() {
                            bells.on_new_turn();
                        }
                        if app.busy() || app.status == Status::WaitingApproval {
                            // Still working: nothing can settle yet.
                        } else if was_busy || was_waiting {
                            let failed = app.status == Status::Failed;
                            bells.on_done(failed);
                            // No queue to drain: the turn settled the moment
                            // the status landed.
                            bells.on_drain();
                        }
                        let attention =
                            (!was_waiting && app.status == Status::WaitingApproval).then_some(
                                notify::Notification::PermissionRequested { tool: None },
                            );
                        let settled = !app.busy() && app.status != Status::WaitingApproval;
                        let fired = bells.reconcile(attention, settled);
                        if let Some(notification) = fired
                            && focus.allows(&notification)
                        {
                            ring();
                        }
                        dirty = Dirty::YES;
                    }
                } else {
                    provider_alive = false;
                }
            }
            _ = sleep => dirty |= Dirty::from(cadence.moves()),
        }
        if app.should_quit {
            bells.on_manual_exit();
            return Ok(());
        }
        dirty |= Dirty::from(app.tick_reveal(std::time::Instant::now()));
        if dirty.take() {
            paint(app)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::Status;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn sync_sequences_match_spec() {
        assert_eq!(SYNC_BEGIN, "\u{1b}[?2026h");
        assert_eq!(SYNC_END, "\u{1b}[?2026l");
    }

    #[test]
    fn sync_sequences_are_private_mode_csi() {
        assert!(SYNC_BEGIN.starts_with("\u{1b}[?"));
        assert!(SYNC_BEGIN.ends_with('h'));
        assert!(SYNC_END.ends_with('l'));
        assert_eq!(&SYNC_BEGIN[3..SYNC_BEGIN.len() - 1], "2026");
    }

    fn key(code: KeyCode, ctrl: bool) -> Event {
        let modifiers = if ctrl {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn quit(input_tx: &mpsc::UnboundedSender<Event>) {
        // Ctrl-C is a tri-state (clear input → cancel turn → quit), so keep
        // pressing until the loop actually exits.
        for _ in 0..3 {
            input_tx.send(key(KeyCode::Char('c'), true)).unwrap();
        }
    }

    /// Paints into a TestBackend while counting frames, so the loop tests can
    /// observe exactly when a frame was owed.
    fn counting_paint(paints: Arc<AtomicUsize>) -> impl FnMut(&mut App) -> io::Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        move |app| {
            paints.fetch_add(1, Ordering::SeqCst);
            terminal
                .draw(|f| ui::draw(f, app))
                .map(|_| ())
                .map_err(|e| match e {})
        }
    }

    fn spawn_loop(
        app: App,
        cmd_tx: &mpsc::UnboundedSender<Command>,
        input_rx: mpsc::UnboundedReceiver<Event>,
        paints: Arc<AtomicUsize>,
    ) -> tokio::task::JoinHandle<io::Result<()>> {
        spawn_loop_with_suspend(app, cmd_tx, input_rx, paints, Arc::new(AtomicUsize::new(0)))
    }

    fn spawn_loop_with_suspend(
        app: App,
        cmd_tx: &mpsc::UnboundedSender<Command>,
        input_rx: mpsc::UnboundedReceiver<Event>,
        paints: Arc<AtomicUsize>,
        suspends: Arc<AtomicUsize>,
    ) -> tokio::task::JoinHandle<io::Result<()>> {
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            let mut app = app;
            let suspends = suspends.clone();
            run_loop(
                &mut app,
                &cmd_tx,
                input_rx,
                evt_rx,
                counting_paint(paints),
                // The test editor stub: refuse to edit anything.
                |_| Err(io::Error::other("no editor in tests")),
                |_| Err(io::Error::other("no editor in tests")),
                move || {
                    suspends.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || (),
                notify::Focus::default(),
            )
            .await
        })
    }

    #[tokio::test]
    async fn provider_bursts_are_drained_in_bounded_ordered_frames() {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel();
        let deltas = EVENT_DRAIN_BUDGET * 2 + 3;
        for _ in 0..deltas {
            evt_tx.send(AgentEvent::AssistantDelta("x".into())).unwrap();
        }
        evt_tx.send(AgentEvent::AssistantEnd).unwrap();
        evt_tx
            .send(AgentEvent::StatusChanged(Status::Done))
            .unwrap();
        let mut app = App::new();
        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        let mut frames = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            run_loop(
                &mut app,
                &cmd_tx,
                input_rx,
                evt_rx,
                |app| {
                    let Some(app::Message::Assistant(text)) = app.conversation.messages.last()
                    else {
                        panic!("assistant deltas must stay in one message");
                    };
                    frames.push(text.len());
                    if app.status == Status::Done {
                        app.should_quit = true;
                        input_tx.send(Event::Resize(80, 24)).unwrap();
                    }
                    Ok(())
                },
                |_| Ok(()),
                |_| Ok(()),
                || Ok(()),
                || (),
                notify::Focus::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(frames, [EVENT_DRAIN_BUDGET, EVENT_DRAIN_BUDGET * 2, deltas]);
        assert!(!app.conversation.assistant_open);
        assert_eq!(app.status, Status::Done);
    }

    #[tokio::test]
    async fn a_single_unicode_burst_reveals_across_clock_frames() {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel();
        let text = "🌸".repeat(40);
        evt_tx
            .send(AgentEvent::AssistantDelta(text.clone()))
            .unwrap();
        let mut app = App::new();
        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        let mut visible_lengths = Vec::new();
        let mut completion_sent = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            run_loop(
                &mut app,
                &cmd_tx,
                input_rx,
                evt_rx,
                |app| {
                    let visible = app.conversation.visible_text(0).unwrap_or(&text);
                    assert!(text.starts_with(visible));
                    visible_lengths.push(visible.len());
                    let visible_chars = visible.chars().count();
                    let fully_visible = visible == text;
                    terminal.draw(|f| ui::draw(f, app)).unwrap();
                    let painted_chars = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .filter(|cell| cell.symbol() == "🌸")
                        .count();
                    assert_eq!(painted_chars, visible_chars);
                    if fully_visible && !completion_sent {
                        completion_sent = true;
                        evt_tx.send(AgentEvent::AssistantEnd).unwrap();
                        evt_tx
                            .send(AgentEvent::StatusChanged(Status::Done))
                            .unwrap();
                    }
                    if app.status == Status::Done {
                        app.should_quit = true;
                        input_tx.send(Event::Resize(80, 24)).unwrap();
                    }
                    Ok(())
                },
                |_| Ok(()),
                |_| Ok(()),
                || Ok(()),
                || (),
                notify::Focus::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            visible_lengths.len() > 2,
            "text must animate without new deltas"
        );
        assert!(visible_lengths.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(visible_lengths.last(), Some(&text.len()));
        assert_eq!(app.cadence(), repaint::Cadence::IDLE);
    }

    /// A settled session owes only the first frame: idle timeouts repaint
    /// nothing, but a real event still does.
    #[tokio::test]
    async fn settled_app_on_timeout_paints_nothing() {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        let mut app = App::new();
        app.handle_event(provider::AgentEvent::StatusChanged(Status::Done));
        assert_eq!(app.cadence(), repaint::Cadence::IDLE);
        let paints = Arc::new(AtomicUsize::new(0));
        let task = spawn_loop(app, &cmd_tx, input_rx, Arc::clone(&paints));

        // Several IDLE_POLL timeouts fire with nothing to show for them.
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(paints.load(Ordering::SeqCst), 1, "no frame owed while idle");

        input_tx.send(key(KeyCode::Char('a'), false)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            paints.load(Ordering::SeqCst),
            2,
            "an event owes exactly one"
        );

        quit(&input_tx);
        task.await.unwrap().unwrap();
        // The first quit press clears the composer ('a' is still there),
        // which owes one repaint; the second quits.
        assert_eq!(paints.load(Ordering::SeqCst), 3);
    }

    /// While a spinner status animates, each SPINNER_FRAME timeout moves the
    /// glyph and so owes a frame.
    #[tokio::test]
    async fn spinner_status_repaints_on_cadence() {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        let mut app = App::new();
        app.handle_event(provider::AgentEvent::StatusChanged(Status::Running));
        assert_eq!(app.cadence(), repaint::Cadence::SPINNER);
        let paints = Arc::new(AtomicUsize::new(0));
        let task = spawn_loop(app, &cmd_tx, input_rx, Arc::clone(&paints));

        // ~5 spinner frames in 450ms; the initial frame plus at least three
        // cadence repaints, but nowhere near the ~28 a fixed 16ms loop would.
        tokio::time::sleep(Duration::from_millis(450)).await;
        let count = paints.load(Ordering::SeqCst);
        assert!(count >= 4, "spinner must animate, saw {count} frames");
        assert!(
            count <= 12,
            "spinner must not pin the frame rate, saw {count}"
        );

        quit(&input_tx);
        task.await.unwrap().unwrap();
    }

    /// A closed provider channel stops owing frames but the session keeps
    /// serving input until the user quits.
    #[tokio::test]
    async fn closed_provider_channel_keeps_serving_input() {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mut app = App::new();
        let paints = Arc::new(AtomicUsize::new(0));
        let p = Arc::clone(&paints);
        let task = tokio::spawn(async move {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            run_loop(
                &mut app,
                &cmd_tx,
                input_rx,
                evt_rx,
                move |a| {
                    p.fetch_add(1, Ordering::SeqCst);
                    terminal
                        .draw(|f| ui::draw(f, a))
                        .map(|_| ())
                        .map_err(|e| match e {})
                },
                |_| Err(io::Error::other("no editor in tests")),
                |_| Err(io::Error::other("no editor in tests")),
                || Ok(()),
                || (),
                notify::Focus::default(),
            )
            .await
        });

        // Provider goes away; the loop must keep running and repainting input.
        drop(evt_tx);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let before = paints.load(Ordering::SeqCst);
        input_tx.send(key(KeyCode::Char('a'), false)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            paints.load(Ordering::SeqCst),
            before + 1,
            "input still repaints after the provider ended"
        );

        quit(&input_tx);
        task.await.unwrap().unwrap();
    }

    /// Ctrl-Z suspends instead of reaching the composer, and the session
    /// survives the resume.
    #[tokio::test]
    async fn ctrl_z_suspends_without_quitting_or_editing_composer() {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        let mut app = App::new();
        app.composer.set_text("draft".to_string());
        let suspends = Arc::new(AtomicUsize::new(0));
        let paints = Arc::new(AtomicUsize::new(0));
        let task = spawn_loop_with_suspend(app, &cmd_tx, input_rx, paints, Arc::clone(&suspends));

        input_tx.send(key(KeyCode::Char('z'), true)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            suspends.load(Ordering::SeqCst),
            1,
            "Ctrl-Z ran the suspend hook"
        );

        // Still running: Ctrl-C needs its full tri-state (input is intact).
        quit(&input_tx);
        let _ = task.await;
    }

    /// Reference semantics: suspend always wins, even under an open modal
    /// (the palette would otherwise own the keyboard).
    #[cfg(unix)]
    #[tokio::test]
    async fn ctrl_z_wins_over_open_modal() {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        let mut app = App::new();
        app.overlays.modal = modals::Modal::Palette {
            query: String::new(),
            selected: 0,
        };
        let suspends = Arc::new(AtomicUsize::new(0));
        let paints = Arc::new(AtomicUsize::new(0));
        let task = spawn_loop_with_suspend(app, &cmd_tx, input_rx, paints, Arc::clone(&suspends));

        input_tx.send(key(KeyCode::Char('z'), true)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            suspends.load(Ordering::SeqCst),
            1,
            "suspend fires before modal key handling"
        );

        // Under the palette Ctrl-C is captured, so just drop the loop.
        task.abort();
    }

    #[test]
    fn only_ctrl_z_suspends() {
        assert!(is_suspend_key(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_suspend_key(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::NONE
        )));
        assert!(!is_suspend_key(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::ALT
        )));
        assert!(!is_suspend_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn editor_args_split_on_whitespace_and_quotes() {
        assert_eq!(parse_editor_args("vim").unwrap(), vec!["vim".to_string()]);
        assert_eq!(
            parse_editor_args("code --wait").unwrap(),
            vec!["code".to_string(), "--wait".to_string()]
        );
        assert_eq!(
            parse_editor_args("\"my editor\"  -x ").unwrap(),
            vec!["my editor".to_string(), "-x".to_string()]
        );
        assert!(parse_editor_args("").is_err());
        assert!(parse_editor_args("   ").is_err());
        assert!(parse_editor_args("\"unclosed").is_err());
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

    /// Drive `run_loop` with a fixed initial focus and a scripted turn,
    /// returning how many bells rang. The initial focus is a parameter (not
    /// a focus event) so the two channels cannot interleave the setup away.
    async fn bells_for(focus: notify::Focus, script: Vec<AgentEvent>) -> usize {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let rings = Arc::new(AtomicUsize::new(0));
        let r = Arc::clone(&rings);
        let task = tokio::spawn(async move {
            let mut app = App::new();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            run_loop(
                &mut app,
                &cmd_tx,
                input_rx,
                evt_rx,
                move |a| {
                    terminal
                        .draw(|f| ui::draw(f, a))
                        .map(|_| ())
                        .map_err(|e| match e {})
                },
                |_| Err(io::Error::other("no editor in tests")),
                |_| Err(io::Error::other("no editor in tests")),
                || Ok(()),
                move || {
                    r.fetch_add(1, Ordering::SeqCst);
                },
                focus,
            )
            .await
        });

        for ev in script {
            evt_tx.send(ev).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        let count = rings.load(Ordering::SeqCst);
        quit(&input_tx);
        task.await.unwrap().unwrap();
        count
    }

    fn turn(reply: &str) -> Vec<AgentEvent> {
        vec![
            AgentEvent::StatusChanged(Status::Running),
            AgentEvent::AssistantText(reply.into()),
            AgentEvent::StatusChanged(Status::Done),
        ]
    }

    #[tokio::test]
    async fn a_watched_completion_stays_silent() {
        let count = bells_for(notify::Focus::Focused, turn("all done")).await;
        assert_eq!(count, 0, "a focused user sees the finish: no bell");
    }

    #[tokio::test]
    async fn an_unwatched_completion_rings() {
        let count = bells_for(notify::Focus::Unfocused, turn("finished")).await;
        assert_eq!(count, 1, "an unfocused terminal hears the finish");
    }

    #[tokio::test]
    async fn a_permission_prompt_rings_even_while_watched() {
        let script = vec![
            AgentEvent::StatusChanged(Status::Running),
            AgentEvent::StatusChanged(Status::WaitingApproval),
        ];
        let count = bells_for(notify::Focus::Focused, script).await;
        assert_eq!(count, 1, "a blocking prompt outranks focus");
    }

    #[tokio::test]
    async fn consecutive_turns_each_ring_their_completion() {
        // The reference suppresses a superseded completion via its explicit
        // QueueDrained event; without a queue, a turn settles the moment its
        // Done status lands, so each completion rings.
        let mut script = turn("first");
        script.extend(turn("second"));
        let count = bells_for(notify::Focus::Unfocused, script).await;
        assert_eq!(count, 2, "each completed turn rings once");
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
