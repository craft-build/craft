mod app;
mod provider;
mod ui;

use std::io;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::App;
use provider::mock::MockProvider;
use provider::{Command, Provider};

#[tokio::main]
async fn main() -> io::Result<()> {
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

    let result = run(&mut terminal).await;

    disable_raw_mode()?;
    io::stdout().execute(DisableBracketedPaste)?.execute(DisableMouseCapture)?.execute(LeaveAlternateScreen)?;
    result
}

async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let (cmd_tx, mut evt_rx): (mpsc::UnboundedSender<Command>, _) = MockProvider.start();

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
            else => break,
        }
        if app.should_quit {
            break;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
    }
    Ok(())
}
