use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::compose::{self, ComposeField, ComposeMode, ComposeState};

use super::{App, AppEvent, BgResult};

impl App {
    pub(super) fn start_compose(&mut self, mode: ComposeMode) {
        let mut state = ComposeState::new(mode);
        match mode {
            ComposeMode::New => {}
            ComposeMode::Reply => {
                if let Some(msg) = self.messages.get(self.selected_message) {
                    state.to = msg.from.clone();
                    state.subject = if msg.subject.starts_with("Re: ") {
                        msg.subject.clone()
                    } else {
                        format!("Re: {}", msg.subject)
                    };
                    state.in_reply_to = Some(msg.message_id.clone());
                    state.references = Some(compose::build_references(
                        msg.in_reply_to.as_deref(),
                        &msg.message_id,
                    ));
                    if let Some(body) = &self.body_text {
                        let quoted = compose::quote_body(body, &msg.from, &msg.date);
                        state.body = ratatui_textarea::TextArea::new(
                            std::iter::once(String::new())
                                .chain(std::iter::once(String::new()))
                                .chain(quoted.lines().map(String::from))
                                .collect(),
                        );
                    }
                    state.active_field = ComposeField::Body;
                }
            }
            ComposeMode::Forward => {
                if let Some(msg) = self.messages.get(self.selected_message) {
                    state.subject = if msg.subject.starts_with("Fwd: ") {
                        msg.subject.clone()
                    } else {
                        format!("Fwd: {}", msg.subject)
                    };
                    if let Some(body) = &self.body_text {
                        let fwd = compose::forward_body(body, &msg.from, &msg.date, &msg.subject);
                        state.body = ratatui_textarea::TextArea::new(
                            std::iter::once(String::new())
                                .chain(std::iter::once(String::new()))
                                .chain(fwd.lines().map(String::from))
                                .collect(),
                        );
                    }
                    state.active_field = ComposeField::To;
                }
            }
        }
        self.compose = Some(state);
        self.status = match mode {
            ComposeMode::New => "Compose — Ctrl-S to send, Esc to cancel".into(),
            ComposeMode::Reply => "Reply — Ctrl-S to send, Esc to cancel".into(),
            ComposeMode::Forward => "Forward — Ctrl-S to send, Esc to cancel".into(),
        };
    }

    pub(super) fn spawn_send(&mut self) {
        let state = match self.compose.take() {
            Some(s) => s,
            None => return,
        };
        let acct_config = &self.active().config;
        let from = acct_config
            .email_addresses
            .first()
            .cloned()
            .unwrap_or_else(|| acct_config.username.clone());
        let body_text = state.body.lines().join("\n");
        let email = neverlight_mail_core::smtp::OutgoingEmail {
            from,
            to: state.to,
            subject: state.subject,
            body: body_text,
            in_reply_to: state.in_reply_to,
            references: state.references,
            attachments: Vec::new(),
        };
        let smtp_config = acct_config.smtp.clone();
        let tx = self.bg_tx.clone();
        self.status = "Sending…".into();
        tokio::spawn(async move {
            let result = neverlight_mail_core::smtp::send_email(&smtp_config, &email).await;
            let _ = tx.send(BgResult::SendResult(result));
        });
    }

    pub(super) fn handle_compose_key(&mut self, key: KeyEvent) -> AppEvent {
        // Ctrl-S sends
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.spawn_send();
            return AppEvent::Continue;
        }

        let state = match self.compose.as_mut() {
            Some(s) => s,
            None => return AppEvent::Continue,
        };
        match key.code {
            KeyCode::Esc => {
                self.compose = None;
                self.status = "Compose cancelled.".into();
            }
            KeyCode::Tab => {
                state.active_field = match state.active_field {
                    ComposeField::To => ComposeField::Subject,
                    ComposeField::Subject => ComposeField::Body,
                    ComposeField::Body => ComposeField::To,
                };
            }
            KeyCode::BackTab => {
                state.active_field = match state.active_field {
                    ComposeField::To => ComposeField::Body,
                    ComposeField::Subject => ComposeField::To,
                    ComposeField::Body => ComposeField::Subject,
                };
            }
            _ => {
                match state.active_field {
                    ComposeField::To => match key.code {
                        KeyCode::Backspace => {
                            state.to.pop();
                        }
                        KeyCode::Char(c) => state.to.push(c),
                        _ => {}
                    },
                    ComposeField::Subject => match key.code {
                        KeyCode::Backspace => {
                            state.subject.pop();
                        }
                        KeyCode::Char(c) => state.subject.push(c),
                        _ => {}
                    },
                    ComposeField::Body => {
                        // Forward full KeyEvent to textarea
                        state.body.input(key);
                    }
                }
            }
        }
        AppEvent::Continue
    }
}
