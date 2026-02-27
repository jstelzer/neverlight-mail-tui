use std::io;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use neverlight_mail_core::config::{
    ConfigNeedsInput, FileAccountConfig, MultiAccountFileConfig, PasswordBackend, SmtpOverrides,
    new_account_id,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupField {
    Server,
    Port,
    Username,
    Password,
    Email,
    Starttls,
}

impl SetupField {
    const ALL: [SetupField; 6] = [
        Self::Server,
        Self::Port,
        Self::Username,
        Self::Password,
        Self::Email,
        Self::Starttls,
    ];

    fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap();
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap();
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

struct SetupState {
    mode: SetupMode,
    server: String,
    port: String,
    username: String,
    password: String,
    email: String,
    starttls: bool,
    active_field: SetupField,
    error: Option<String>,
}

enum SetupMode {
    Full,
    PasswordOnly {
        server: String,
        username: String,
    },
}

pub enum SetupResult {
    Configured,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run_setup(needs: ConfigNeedsInput) -> anyhow::Result<SetupResult> {
    let mut state = match &needs {
        ConfigNeedsInput::FullSetup => SetupState {
            mode: SetupMode::Full,
            server: String::new(),
            port: "993".into(),
            username: String::new(),
            password: String::new(),
            email: String::new(),
            starttls: false,
            active_field: SetupField::Server,
            error: None,
        },
        ConfigNeedsInput::PasswordOnly {
            server,
            port,
            username,
            starttls,
            error,
        } => SetupState {
            mode: SetupMode::PasswordOnly {
                server: server.clone(),
                username: username.clone(),
            },
            server: server.clone(),
            port: port.to_string(),
            username: username.clone(),
            password: String::new(),
            email: String::new(),
            starttls: *starttls,
            active_field: SetupField::Password,
            error: error.clone(),
        },
    };

    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_form(&mut terminal, &mut state);

    io::stdout().execute(LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    result
}

// ---------------------------------------------------------------------------
// Blocking event loop
// ---------------------------------------------------------------------------

fn run_form(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut SetupState,
) -> anyhow::Result<SetupResult> {
    loop {
        terminal.draw(|frame| render(frame, state))?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc => return Ok(SetupResult::Cancelled),
                KeyCode::Enter => {
                    if let Some(result) = try_submit(state) {
                        return result;
                    }
                }
                KeyCode::Tab => {
                    state.active_field = if key.modifiers.contains(KeyModifiers::SHIFT) {
                        state.active_field.prev()
                    } else {
                        state.active_field.next()
                    };
                    // Skip read-only fields in PasswordOnly mode
                    if matches!(state.mode, SetupMode::PasswordOnly { .. }) {
                        while state.active_field != SetupField::Password
                            && state.active_field != SetupField::Starttls
                        {
                            state.active_field = if key.modifiers.contains(KeyModifiers::SHIFT) {
                                state.active_field.prev()
                            } else {
                                state.active_field.next()
                            };
                        }
                    }
                }
                KeyCode::BackTab => {
                    state.active_field = state.active_field.prev();
                    if matches!(state.mode, SetupMode::PasswordOnly { .. }) {
                        while state.active_field != SetupField::Password
                            && state.active_field != SetupField::Starttls
                        {
                            state.active_field = state.active_field.prev();
                        }
                    }
                }
                KeyCode::Char(' ') if state.active_field == SetupField::Starttls => {
                    state.starttls = !state.starttls;
                }
                KeyCode::Char(c) => {
                    if let Some(field) = active_text_field(state) {
                        field.push(c);
                        state.error = None;
                    }
                }
                KeyCode::Backspace => {
                    if let Some(field) = active_text_field(state) {
                        field.pop();
                    }
                }
                _ => {}
            }
        }
    }
}

fn active_text_field(state: &mut SetupState) -> Option<&mut String> {
    // In PasswordOnly mode, only password is editable text
    if matches!(state.mode, SetupMode::PasswordOnly { .. }) {
        return match state.active_field {
            SetupField::Password => Some(&mut state.password),
            _ => None,
        };
    }
    match state.active_field {
        SetupField::Server => Some(&mut state.server),
        SetupField::Port => Some(&mut state.port),
        SetupField::Username => Some(&mut state.username),
        SetupField::Password => Some(&mut state.password),
        SetupField::Email => Some(&mut state.email),
        SetupField::Starttls => None,
    }
}

// ---------------------------------------------------------------------------
// Submit
// ---------------------------------------------------------------------------

fn try_submit(state: &mut SetupState) -> Option<anyhow::Result<SetupResult>> {
    match &state.mode {
        SetupMode::PasswordOnly {
            server,
            username,
        } => {
            if state.password.is_empty() {
                state.error = Some("Password is required".into());
                return None;
            }
            let server = server.clone();
            let username = username.clone();

            // Store password in keyring, fall back to updating config with plaintext
            let password_backend =
                match neverlight_mail_core::keyring::set_password(&username, &server, &state.password) {
                    Ok(()) => PasswordBackend::Keyring,
                    Err(e) => {
                        log::warn!("Keyring unavailable ({}), using plaintext", e);
                        PasswordBackend::Plaintext {
                            value: state.password.clone(),
                        }
                    }
                };

            // Load existing config, update matching account's password backend
            let mut multi = match MultiAccountFileConfig::load() {
                Ok(Some(m)) => m,
                _ => {
                    state.error = Some("Could not load existing config".into());
                    return None;
                }
            };
            if let Some(acct) = multi
                .accounts
                .iter_mut()
                .find(|a| a.server == server && a.username == username)
            {
                acct.password = password_backend;
            }
            if let Err(e) = multi.save() {
                state.error = Some(format!("Failed to save config: {e}"));
                return None;
            }
            Some(Ok(SetupResult::Configured))
        }

        SetupMode::Full => {
            // Validate required fields
            if state.server.trim().is_empty() {
                state.error = Some("Server is required".into());
                return None;
            }
            if state.username.trim().is_empty() {
                state.error = Some("Username is required".into());
                return None;
            }
            if state.password.is_empty() {
                state.error = Some("Password is required".into());
                return None;
            }
            if state.email.trim().is_empty() {
                state.error = Some("Email address is required".into());
                return None;
            }
            let port: u16 = match state.port.trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    state.error = Some("Port must be a number (e.g. 993)".into());
                    return None;
                }
            };

            let server = state.server.trim().to_string();
            let username = state.username.trim().to_string();
            let email = state.email.trim().to_string();
            let account_id = new_account_id();

            // Try keyring, fall back to plaintext
            let password_backend =
                match neverlight_mail_core::keyring::set_password(&username, &server, &state.password) {
                    Ok(()) => {
                        log::info!("Password stored in keyring");
                        PasswordBackend::Keyring
                    }
                    Err(e) => {
                        log::warn!("Keyring unavailable ({}), using plaintext", e);
                        PasswordBackend::Plaintext {
                            value: state.password.clone(),
                        }
                    }
                };

            let fac = FileAccountConfig {
                id: account_id,
                label: username.clone(),
                server,
                port,
                username,
                starttls: state.starttls,
                password: password_backend,
                email_addresses: vec![email],
                smtp: SmtpOverrides::default(),
            };

            let multi = MultiAccountFileConfig {
                accounts: vec![fac],
            };
            if let Err(e) = multi.save() {
                state.error = Some(format!("Failed to save config: {e}"));
                return None;
            }
            Some(Ok(SetupResult::Configured))
        }
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

fn render(frame: &mut Frame, state: &SetupState) {
    let area = frame.area();

    // Center a dialog box
    let dialog_w = 50u16.min(area.width.saturating_sub(4));
    let dialog_h = 16u16.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(dialog_w)) / 2;
    let y = (area.height.saturating_sub(dialog_h)) / 2;
    let dialog = Rect::new(x, y, dialog_w, dialog_h);

    frame.render_widget(Clear, dialog);

    let title = match &state.mode {
        SetupMode::Full => " Account Setup ",
        SetupMode::PasswordOnly { .. } => " Enter Password ",
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let is_password_only = matches!(state.mode, SetupMode::PasswordOnly { .. });

    // Build lines
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    let field_w = inner.width.saturating_sub(16) as usize;

    let fields = [
        (SetupField::Server, "IMAP Server", &state.server, false),
        (SetupField::Port, "       Port", &state.port, false),
        (SetupField::Username, "   Username", &state.username, false),
        (SetupField::Password, "   Password", &state.password, true),
        (SetupField::Email, " From Email", &state.email, false),
    ];

    for (field, label, value, is_secret) in &fields {
        let active = state.active_field == *field;
        let readonly = is_password_only && *field != SetupField::Password;

        let display_val = if *is_secret {
            "*".repeat(value.len())
        } else {
            (*value).clone()
        };

        // Pad/truncate to field width, add cursor if active
        let mut rendered = if active && !readonly {
            format!("{}_", display_val)
        } else {
            display_val
        };
        rendered.truncate(field_w);

        let label_style = if readonly {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        let value_style = if active && !readonly {
            Style::default().fg(Color::Yellow)
        } else if readonly {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("  {}: ", label), label_style),
            Span::styled(rendered, value_style),
        ]));
    }

    // STARTTLS toggle
    {
        let active = state.active_field == SetupField::Starttls;
        let readonly = is_password_only;
        let check = if state.starttls { "x" } else { " " };
        let label_style = if readonly {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        let value_style = if active && !readonly {
            Style::default().fg(Color::Yellow)
        } else if readonly {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled("    STARTTLS: ", label_style),
            Span::styled(format!("[{}]", check), value_style),
            Span::styled(" (Space to toggle)", Style::default().fg(Color::DarkGray)),
        ]));
    }

    lines.push(Line::from(""));

    // Error message
    if let Some(ref err) = state.error {
        lines.push(Line::from(Span::styled(
            format!("  {}", err),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }

    // Help line
    lines.push(Line::from(Span::styled(
        "  Tab: next  Shift-Tab: prev  Enter: save  Esc: quit",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}
