//! Craft's interactive terminal UI: the default surface of the `craft` binary.
//!
//! The UI renders whatever a [`provider::Provider`] streams in; the live
//! backend is [`provider::live::CraftProvider`].

mod app;
mod composer;
mod modals;
pub mod provider;
mod selection;
mod ui;

use std::io;
use std::time::Duration;

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
use provider::{Command, Provider};

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

async fn drive<P: Provider>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    provider: P,
) -> io::Result<()> {
    let (cmd_tx, mut evt_rx): (mpsc::UnboundedSender<Command>, _) = provider.start();

    // Crossterm events are blocking reads -> pump them on a dedicated thread.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut app = App::new();

    // Repaint on a fixed cadence so the status indicator animates even when
    // nothing is streaming (e.g. a long thinking block).
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(ev) = input_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key, &cmd_tx);
                    }
                    Event::Paste(text) => app.insert_paste(&text),
                    Event::Mouse(mouse) => app.handle_mouse(mouse),
                    _ => {} // Resize etc: just fall through to redraw.
                }
            }
            Some(ev) = evt_rx.recv() => app.handle_event(ev),
            _ = tick.tick() => {}
            else => break,
        }
        if app.should_quit {
            break;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
    }
    Ok(())
}
