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

use std::io;

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
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
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // Make sure the terminal is restored on panic too.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = drive(&mut terminal, provider).await;

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
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
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

    run_loop(&mut app, &cmd_tx, input_rx, evt_rx, |app| {
        begin_synchronized_output();
        let draw = terminal.draw(|f| ui::draw(f, app));
        end_synchronized_output();
        draw.map(|_| ())
    })
    .await
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
                        app.handle_key(key, cmd_tx);
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

    fn quit() -> Event {
        key(KeyCode::Char('c'), true)
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
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            let mut app = app;
            run_loop(&mut app, &cmd_tx, input_rx, evt_rx, counting_paint(paints)).await
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

        input_tx.send(quit()).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(paints.load(Ordering::SeqCst), 2);
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

        input_tx.send(quit()).unwrap();
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
            run_loop(&mut app, &cmd_tx, input_rx, evt_rx, move |a| {
                p.fetch_add(1, Ordering::SeqCst);
                terminal
                    .draw(|f| ui::draw(f, a))
                    .map(|_| ())
                    .map_err(|e| match e {})
            })
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

        input_tx.send(quit()).unwrap();
        task.await.unwrap().unwrap();
    }
}
