//! Craft's interactive terminal UI: the default surface of the `craft` binary.
//!
//! The UI renders whatever a [`provider::Provider`] streams in; the live
//! backend is [`provider::live::CraftProvider`].

mod app;
mod composer;
mod modals;
pub mod provider;
mod repaint;
mod selection;
mod ui;

use std::cell::RefCell;
use std::io;
use std::path::Path;

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use app::App;
use provider::{AgentEvent, Command, Provider};
use repaint::{Dirty, IDLE_POLL};

/// Run the terminal UI against `provider` until the user quits.
pub async fn run<P: Provider>(provider: P) -> io::Result<()> {
    enable_raw_mode()?;
    // Mouse capture: terminals then show the standard arrow pointer on hover
    // instead of the I-beam text cursor. Bracketed paste: multi-line pastes
    // arrive as one Paste event instead of per-line Enter keypresses.
    io::stdout()
        .execute(EnterAlternateScreen)?
        .execute(EnableMouseCapture)?
        .execute(EnableBracketedPaste)?;
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
    // Recall the persistent input history from the state dir (best effort).
    if let Ok(dir) = crate::storage::StateDir::resolve() {
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
        || {
            suspend_terminal(&mut terminal.borrow_mut());
            Ok(())
        },
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

/// True for the Alt-O chord that hands the composer text to $VISUAL/$EDITOR.
fn is_open_editor_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('o')
        && key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
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
async fn run_loop(
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    mut input_rx: mpsc::UnboundedReceiver<Event>,
    mut evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut paint: impl FnMut(&mut App) -> io::Result<()>,
    mut edit_composer: impl FnMut(&mut App) -> io::Result<()>,
    mut suspend_ui: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut dirty = Dirty::YES;
    // A closed provider channel just stops being selected on; only the input
    // reader dying ends the session.
    let mut provider_alive = true;

    loop {
        let cadence = app.cadence();
        let sleep = tokio::time::sleep(cadence.frame().unwrap_or(IDLE_POLL));
        tokio::select! {
            ev = input_rx.recv() => {
                match ev {
                    // The input reader stopped: the terminal is gone.
                    None => return Ok(()),
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        // Alt-O hands the composer to $EDITOR; it must run
                        // here, where the terminal is reachable to suspend
                        // and restore the UI around the child process.
                    // Ctrl-Z suspends the process; it must run here, where
                    // the terminal is reachable to tear down and restore the
                    // UI around SIGTSTP. It wins over every other handler,
                    // including open modals (reference semantics).
                    if is_suspend_key(&key) {
                        suspend_ui()?;
                    } else if is_open_editor_key(&key)
                            && matches!(app.modal, modals::Modal::None)
                        {
                            if let Err(e) = edit_composer(app) {
                                eprintln!("warning: could not open editor: {e}");
                            }
                        } else {
                            app.handle_key(key, cmd_tx);
                        }
                    }
                    Some(Event::Paste(text)) => app.insert_paste(&text),
                    Some(Event::Mouse(mouse)) => app.handle_mouse(mouse),
                    Some(_) => {} // Resize etc: still repaints below.
                }
                dirty = Dirty::YES;
            }
            ev = evt_rx.recv(), if provider_alive => {
                match ev {
                    Some(ev) => {
                        app.handle_event(ev);
                        dirty = Dirty::YES;
                    }
                    None => provider_alive = false,
                }
            }
            _ = sleep => dirty |= Dirty::from(cadence.moves()),
        }
        if app.should_quit {
            return Ok(());
        }
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
                move || {
                    suspends.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
        })
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
                || Ok(()),
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
        app.modal = modals::Modal::Palette {
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
    fn only_plain_alt_o_opens_the_editor() {
        assert!(is_open_editor_key(&KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::ALT
        )));
        assert!(!is_open_editor_key(&KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::NONE
        )));
        assert!(!is_open_editor_key(&KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        assert!(!is_open_editor_key(&KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::ALT
        )));
    }
}
