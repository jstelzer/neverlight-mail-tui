use std::io;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use neverlight_mail_core::config::ConfigNeedsInput;
use neverlight_mail_core::setup::{FieldId, SetupInput, SetupModel, SetupTransition};

pub use neverlight_mail_core::setup::SetupOutcome as SetupResult;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run_setup(needs: ConfigNeedsInput) -> anyhow::Result<SetupResult> {
    let mut model = SetupModel::from_config_needs(&needs);

    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_form(&mut terminal, &mut model);

    io::stdout().execute(LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    result
}

// ---------------------------------------------------------------------------
// Blocking event loop — maps crossterm keys to SetupInput
// ---------------------------------------------------------------------------

fn run_form(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &mut SetupModel,
) -> anyhow::Result<SetupResult> {
    loop {
        terminal.draw(|frame| render(frame, model))?;

        if let Event::Key(key) = event::read()? {
            let input = match key.code {
                KeyCode::Esc => SetupInput::Cancel,
                KeyCode::Enter => SetupInput::Submit,
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    SetupInput::PrevField
                }
                KeyCode::Tab => SetupInput::NextField,
                KeyCode::BackTab => SetupInput::PrevField,
                KeyCode::Char(' ') if model.active_field.is_toggle() => {
                    SetupInput::Toggle
                }
                KeyCode::Char(c) => SetupInput::InsertChar(c),
                KeyCode::Backspace => SetupInput::Backspace,
                _ => continue,
            };

            match model.update(input) {
                SetupTransition::Continue => {}
                SetupTransition::Finished(outcome) => return Ok(outcome),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Render — reads SetupModel fields, zero logic
// ---------------------------------------------------------------------------

fn render(frame: &mut Frame, model: &SetupModel) {
    let area = frame.area();

    // Center a dialog box
    let is_password_only = matches!(model.request, neverlight_mail_core::setup::SetupRequest::PasswordOnly { .. });
    let dialog_w = 60u16.min(area.width.saturating_sub(4));
    let dialog_h = if is_password_only { 10u16 } else { 28u16 }.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(dialog_w)) / 2;
    let y = (area.height.saturating_sub(dialog_h)) / 2;
    let dialog = Rect::new(x, y, dialog_w, dialog_h);

    frame.render_widget(Clear, dialog);

    let title = format!(" {} ", model.title());
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    let field_w = inner.width.saturating_sub(16) as usize;

    let text_fields: [(FieldId, &str); 6] = [
        (FieldId::Label, "       Label"),
        (FieldId::Server, " IMAP Server"),
        (FieldId::Port, "        Port"),
        (FieldId::Username, "    Username"),
        (FieldId::Password, "    Password"),
        (FieldId::Email, "  From Email"),
    ];

    for (field, label) in &text_fields {
        render_text_field(&mut lines, model, *field, label, field_w);
    }

    // STARTTLS toggle
    render_toggle_field(&mut lines, model, FieldId::Starttls, "STARTTLS", model.starttls);

    // SMTP overrides section (only for Full/Edit)
    if !is_password_only {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  SMTP (optional — blank = use IMAP)",
            Style::default().fg(Color::DarkGray),
        )));

        let smtp_fields: [(FieldId, &str); 4] = [
            (FieldId::SmtpServer, " SMTP Server"),
            (FieldId::SmtpPort, "   SMTP Port"),
            (FieldId::SmtpUsername, "   SMTP User"),
            (FieldId::SmtpPassword, "   SMTP Pass"),
        ];
        for (field, label) in &smtp_fields {
            render_text_field(&mut lines, model, *field, label, field_w);
        }

        render_toggle_field(&mut lines, model, FieldId::SmtpStarttls, "SMTP TLS", model.smtp_starttls);
    }

    lines.push(Line::from(""));

    // Error message
    if let Some(ref err) = model.error {
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

// ---------------------------------------------------------------------------
// Render helpers
// ---------------------------------------------------------------------------

fn render_text_field<'a>(
    lines: &mut Vec<Line<'a>>,
    model: &SetupModel,
    field: FieldId,
    label: &'a str,
    field_w: usize,
) {
    let active = model.active_field == field;
    let readonly = model.is_readonly(field);
    let value = model.field_value(field);

    let display_val = if field.is_secret() {
        "*".repeat(value.len())
    } else {
        value.to_string()
    };

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

fn render_toggle_field<'a>(
    lines: &mut Vec<Line<'a>>,
    model: &SetupModel,
    field: FieldId,
    label: &'a str,
    value: bool,
) {
    let active = model.active_field == field;
    let readonly = model.is_readonly(field);
    let check = if value { "x" } else { " " };

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
        Span::styled(format!("    {}: ", label), label_style),
        Span::styled(format!("[{}]", check), value_style),
        Span::styled(
            " (Space to toggle)",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
}
