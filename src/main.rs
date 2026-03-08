mod app;
mod compose;
mod setup;
mod threading;
mod ui;

use std::env;
use std::io;

use crossterm::event::{Event, EventStream, KeyEventKind, MouseEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::prelude::*;
use ratatui_image::picker::{Picker, ProtocolType};

use neverlight_mail_core::config;

use app::{App, AppEvent};

/// RAII guard that restores the terminal on drop (including panics).
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = terminal::disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

fn is_iterm2_like_terminal() -> bool {
    env::var("TERM_PROGRAM").is_ok_and(|v| v.contains("iTerm"))
        || env::var("LC_TERMINAL").is_ok_and(|v| v.contains("iTerm"))
}

fn detect_picker() -> Picker {
    let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

    // In iTerm2 sessions, force iTerm2 protocol.
    // Some environments report kitty capabilities but don't correctly render
    // ratatui-image's kitty stateful path, resulting in an empty image pane.
    if is_iterm2_like_terminal() {
        picker.set_protocol_type(ProtocolType::Iterm2);
    }

    picker
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let accounts = match config::resolve_all_accounts() {
        Ok(accounts) if !accounts.is_empty() => accounts,
        Ok(_) => return Err(anyhow::anyhow!("No accounts configured")),
        Err(needs_input) => {
            match setup::run_setup(needs_input)? {
                setup::SetupResult::Cancelled => return Ok(()),
                setup::SetupResult::Configured => {}
            }
            config::resolve_all_accounts()
                .map_err(|e| anyhow::anyhow!("Config error after setup: {e:?}"))?
        }
    };

    let mut app = App::with_accounts(accounts).await?;

    // Terminal setup
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(crossterm::event::EnableMouseCapture)?;
    let _guard = TermGuard; // restores terminal on drop (including panics)

    // Detect terminal image protocol (sixel/kitty/iterm2/halfblocks).
    // Must happen AFTER alternate screen, BEFORE EventStream.
    let picker = detect_picker();
    app.set_picker(picker);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    run(&mut terminal, &mut app).await
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
                        match mouse.kind {
                            MouseEventKind::Down(_)
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown => {
                                app.handle_mouse(mouse.kind, mouse.column, mouse.row);
                            }
                            _ => continue, // skip redraw for move/drag noise
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        // Wake is enough — draw() at loop top picks up new size.
                    }
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            // Background task results — JMAP fetches land here
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
