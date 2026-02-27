mod app;
mod compose;
mod setup;
mod threading;
mod ui;

use std::io;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::prelude::*;
use ratatui_image::picker::Picker;

use neverlight_mail_core::config::Config;

use app::{App, AppEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let accounts = match Config::resolve_all_accounts() {
        Ok(accounts) if !accounts.is_empty() => accounts,
        Ok(_) => return Err(anyhow::anyhow!("No accounts configured")),
        Err(needs_input) => {
            match setup::run_setup(needs_input)? {
                setup::SetupResult::Cancelled => return Ok(()),
                setup::SetupResult::Configured => {}
            }
            Config::resolve_all_accounts()
                .map_err(|e| anyhow::anyhow!("Config error after setup: {e:?}"))?
        }
    };

    let mut app = App::with_accounts(accounts).await?;

    // Terminal setup
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(crossterm::event::EnableMouseCapture)?;

    // Detect terminal image protocol (sixel/kitty/iterm2/halfblocks).
    // Must happen AFTER alternate screen, BEFORE EventStream.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    app.set_picker(picker);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app).await;

    // Terminal restore (always, even on error)
    io::stdout().execute(crossterm::event::DisableMouseCapture)?;
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
                        match app.handle_key(key) {
                            AppEvent::Continue => {}
                            AppEvent::Quit => break,
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        app.handle_mouse(mouse.kind, mouse.column, mouse.row);
                    }
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            // Background task results — IMAP fetches land here
            Some(result) = app.bg_rx.recv() => {
                app.apply(result);
            }
            // Image resize requests from ThreadProtocol
            Some(request) = app.img_resize_rx.recv() => {
                app.apply_image_resize(request);
            }
        }
    }
    Ok(())
}
