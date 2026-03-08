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
                    if let Some((_, account_id)) = self.account_for_message(msg) {
                        state.account_id = Some(account_id);
                    }
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
                    if let Some((_, account_id)) = self.account_for_message(msg) {
                        state.account_id = Some(account_id);
                    }
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
        let account_idx = state
            .account_id
            .as_deref()
            .and_then(|id| self.account_idx_by_id(id))
            .unwrap_or(self.active_account);

        let client = match self.accounts[account_idx].client.clone() {
            Some(c) => c,
            None => {
                self.status = "No connection — cannot send".into();
                return;
            }
        };

        let acct_config = &self.accounts[account_idx].config;
        let from = acct_config
            .email_addresses
            .first()
            .cloned()
            .unwrap_or_else(|| acct_config.username.clone());
        let to: Vec<String> = state
            .to
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let body_text = state.body.lines().join("\n");

        let tx = self.bg_tx.clone();
        self.status = "Sending…".into();
        tokio::spawn(async move {
            // Resolve identity for sending
            let identities = neverlight_mail_core::submit::get_identities(&client)
                .await
                .unwrap_or_default();
            let identity_id = identities
                .first()
                .map(|i| i.id.clone())
                .unwrap_or_default();

            // Find drafts and sent mailbox IDs
            let folders = neverlight_mail_core::mailbox::fetch_all(&client)
                .await
                .unwrap_or_default();
            let drafts_id = neverlight_mail_core::mailbox::find_by_role(&folders, "drafts")
                .unwrap_or_default();
            let sent_id = neverlight_mail_core::mailbox::find_by_role(&folders, "sent")
                .unwrap_or_default();

            let req = neverlight_mail_core::submit::SendRequest {
                identity_id: &identity_id,
                from: &from,
                to: &to,
                cc: &[],
                subject: &state.subject,
                text_body: &body_text,
                html_body: None,
                drafts_mailbox_id: &drafts_id,
                sent_mailbox_id: &sent_id,
            };

            let result = neverlight_mail_core::submit::send(&client, &req)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
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
