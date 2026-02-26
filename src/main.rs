mod app;
mod ui;

use std::io;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
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

async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        // Poll for keyboard events with a timeout so async tasks can progress
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.handle_key(key.code).await {
                    AppEvent::Continue => {}
                    AppEvent::Quit => break,
                }
            }
        }

        // Drive any pending async work
        app.tick().await;
    }
    Ok(())
}
