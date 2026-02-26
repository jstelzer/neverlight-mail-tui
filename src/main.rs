mod app;
mod ui;

use std::io;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::prelude::*;

use app::{App, AppEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut app = App::new().await?;

    // Terminal setup
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app).await;

    // Terminal restore (always, even on error)
    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    let mut reader = EventStream::new();

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        tokio::select! {
            // Input events — wake immediately on keypress
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match app.handle_key(key.code) {
                            AppEvent::Continue => {}
                            AppEvent::Quit => break,
                        }
                    }
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            // Background task results — IMAP fetches land here
            Some(result) = app.recv() => {
                app.apply(result);
            }
        }
    }
    Ok(())
}
