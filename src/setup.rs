use std::io;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use neverlight_mail_core::config::{
    AuthBackend, ConfigNeedsInput, FileAccountConfig, MultiAccountFileConfig,
    AccountCapabilities, new_account_id,
};
use neverlight_mail_oauth::{AppInfo, OAuthRedirectHandler};
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
    let mut oauth_status: Option<String> = None;

    loop {
        terminal.draw(|frame| render(frame, model, oauth_status.as_deref()))?;

        if let Event::Key(key) = event::read()? {
            // Ctrl+O: start OAuth flow
            if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let jmap_url = model.jmap_url.trim().to_string();
                if jmap_url.is_empty() || !jmap_url.starts_with("https://") {
                    model.error = Some("JMAP URL required for OAuth".into());
                    continue;
                }

                match run_oauth_flow(terminal, model, &jmap_url) {
                    Ok(()) => return Ok(SetupResult::Configured),
                    Err(e) => {
                        // Re-enter alternate screen (browser flow left it)
                        io::stdout().execute(EnterAlternateScreen)?;
                        terminal::enable_raw_mode()?;
                        oauth_status = Some(format!("OAuth failed: {e}"));
                        model.error = Some(format!("OAuth failed: {e}"));
                    }
                }
                continue;
            }

            let input = match key.code {
                KeyCode::Esc => SetupInput::Cancel,
                KeyCode::Enter => SetupInput::Submit,
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    SetupInput::PrevField
                }
                KeyCode::Tab => SetupInput::NextField,
                KeyCode::BackTab => SetupInput::PrevField,
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
// OAuth flow — leaves TUI, opens browser, returns
// ---------------------------------------------------------------------------

fn run_oauth_flow(
    _terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &SetupModel,
    jmap_url: &str,
) -> anyhow::Result<()> {
    // Leave alternate screen so the user can see the browser prompt
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    eprintln!("Starting OAuth sign-in for {}...", jmap_url);
    eprintln!("A browser window will open for authorization.");
    eprintln!();

    // Run async OAuth flow in a one-shot runtime
    let rt = tokio::runtime::Runtime::new()?;
    let jmap_url_owned = jmap_url.to_string();
    let (flow, token_set) = rt.block_on(async move {
        let redirect = neverlight_mail_oauth::LocalServerRedirect::bind("Neverlight Mail TUI").await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let redirect_uri = redirect.redirect_uri();
        eprintln!("Listening for redirect on {redirect_uri}");

        let app_info = AppInfo {
            client_name: "Neverlight Mail TUI".into(),
            client_uri: "https://github.com/jstelzer/neverlight-mail-tui".into(),
            software_id: "neverlight-mail-tui".into(),
            software_version: env!("CARGO_PKG_VERSION").into(),
            redirect_uri: redirect_uri.clone(),
        };

        let flow = neverlight_mail_oauth::OAuthFlow::discover_and_register(
            &jmap_url_owned,
            &app_info,
            "urn:ietf:params:oauth:scope:mail",
        )
        .await
        .map_err(|e| anyhow::anyhow!("Discovery failed: {e}"))?;

        eprintln!("Opening browser for authorization...");
        redirect.open_browser(&flow.authorization_url())
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        eprintln!("Waiting for authorization (complete the flow in your browser)...");
        let token_set = flow.authorize(&redirect).await
            .map_err(|e| anyhow::anyhow!("Authorization failed: {e}"))?;

        eprintln!("Authorization successful!");

        anyhow::Ok((flow, token_set))
    })?;

    // Save account config
    let username = model.username.trim().to_string();
    let label = if model.label.trim().is_empty() {
        username.clone()
    } else {
        model.label.trim().to_string()
    };
    let email_addresses: Vec<String> = model.email
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let account_id = new_account_id();

    // Store refresh token in keyring
    let refresh_token_plaintext =
        match neverlight_mail_core::keyring::set_oauth_refresh(&account_id, &token_set.refresh_token) {
            Ok(()) => None,
            Err(e) => {
                log::warn!("Keyring unavailable for OAuth ({}), using plaintext", e);
                Some(token_set.refresh_token.clone())
            }
        };

    let fac = FileAccountConfig {
        id: account_id,
        label,
        jmap_url: jmap_url.to_string(),
        username,
        auth: AuthBackend::OAuth {
            issuer: flow.issuer().to_string(),
            client_id: flow.client_id().to_string(),
            resource: flow.resource().to_string(),
            token_endpoint: flow.token_endpoint().to_string(),
            refresh_token_plaintext,
        },
        email_addresses,
        capabilities: AccountCapabilities::default(),
        max_messages_per_mailbox: None,
    };

    let mut multi = MultiAccountFileConfig::load()
        .ok()
        .flatten()
        .unwrap_or(MultiAccountFileConfig { accounts: Vec::new() });
    multi.accounts.push(fac);
    multi.save().map_err(|e| anyhow::anyhow!("Failed to save config: {e}"))?;

    eprintln!("Account configured. Starting mail client...");
    Ok(())
}

// ---------------------------------------------------------------------------
// Render — reads SetupModel fields, zero logic
// ---------------------------------------------------------------------------

fn render(frame: &mut Frame, model: &SetupModel, oauth_status: Option<&str>) {
    let area = frame.area();

    // Center a dialog box
    let is_token_only = matches!(
        model.request,
        neverlight_mail_core::setup::SetupRequest::TokenOnly { .. }
    );
    let dialog_w = 60u16.min(area.width.saturating_sub(4));
    let dialog_h = if is_token_only { 10u16 } else { 22u16 }.min(area.height.saturating_sub(2));
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

    // JMAP fields only
    let text_fields: [(FieldId, &str); 5] = [
        (FieldId::Label, "       Label"),
        (FieldId::JmapUrl, "    JMAP URL"),
        (FieldId::Username, "    Username"),
        (FieldId::Token, "       Token"),
        (FieldId::Email, "  From Email"),
    ];

    for (field, label) in &text_fields {
        render_text_field(&mut lines, model, *field, label, field_w);
    }

    lines.push(Line::from(""));

    // OAuth status
    if let Some(status) = oauth_status {
        lines.push(Line::from(Span::styled(
            format!("  {}", status),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(""));
    }

    // Error message
    if let Some(ref err) = model.error {
        lines.push(Line::from(Span::styled(
            format!("  {}", err),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }

    // Help line
    if !is_token_only {
        lines.push(Line::from(Span::styled(
            "  Ctrl+O: sign in with browser (OAuth)",
            Style::default().fg(Color::Cyan),
        )));
    }
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
