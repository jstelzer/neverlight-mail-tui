use std::sync::Arc;

use crossterm::event::KeyCode;
use tokio::sync::mpsc;

use nevermail_core::config::{AccountConfig, Config};
use nevermail_core::imap::ImapSession;
use nevermail_core::models::{Folder, MessageSummary};
use nevermail_core::store::CacheHandle;
use nevermail_core::MailboxHash;

// ---------------------------------------------------------------------------
// Background task results
// ---------------------------------------------------------------------------

pub enum BgResult {
    Folders(Result<Vec<Folder>, String>),
    Messages {
        folder_idx: usize,
        result: Result<Vec<MessageSummary>, String>,
    },
    Body {
        message_idx: usize,
        result: Result<String, String>,
    },
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct App {
    pub account: AccountConfig,
    pub session: Arc<ImapSession>,
    pub cache: CacheHandle,
    pub folders: Vec<Folder>,
    pub messages: Vec<MessageSummary>,
    pub body_text: Option<String>,
    pub selected_folder: usize,
    pub selected_message: usize,
    pub focus: Focus,
    pub status: String,
    bg_rx: mpsc::UnboundedReceiver<BgResult>,
    bg_tx: mpsc::UnboundedSender<BgResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Folders,
    Messages,
    Body,
}

pub enum AppEvent {
    Continue,
    Quit,
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

impl App {
    pub async fn new() -> anyhow::Result<Self> {
        let accounts = Config::resolve_all_accounts()
            .map_err(|e| anyhow::anyhow!("Config error: {e:?}"))?;
        let account = accounts
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No accounts configured"))?;

        let imap_config = account.to_imap_config();
        let session = ImapSession::connect(imap_config)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let cache = CacheHandle::open().map_err(|e| anyhow::anyhow!("{e}"))?;

        let (bg_tx, bg_rx) = mpsc::unbounded_channel();

        let app = App {
            account,
            session,
            cache,
            folders: Vec::new(),
            messages: Vec::new(),
            body_text: None,
            selected_folder: 0,
            selected_message: 0,
            focus: Focus::Folders,
            status: "Connecting…".into(),
            bg_rx,
            bg_tx,
        };

        app.spawn_load_folders();
        Ok(app)
    }

    // -----------------------------------------------------------------------
    // Channel interface (called from main loop)
    // -----------------------------------------------------------------------

    /// Receive the next background result, if any.
    pub async fn recv(&mut self) -> Option<BgResult> {
        self.bg_rx.recv().await
    }

    /// Apply a background result to app state.
    pub fn apply(&mut self, result: BgResult) {
        match result {
            BgResult::Folders(Ok(folders)) => {
                self.status = format!("{} folders", folders.len());
                self.folders = folders;
                if !self.folders.is_empty() {
                    self.spawn_load_messages();
                }
            }
            BgResult::Folders(Err(e)) => {
                self.status = format!("Folder error: {e}");
            }
            BgResult::Messages { folder_idx, result } => {
                // Only apply if user hasn't navigated away
                if folder_idx != self.selected_folder {
                    return;
                }
                match result {
                    Ok(msgs) => {
                        let name = &self.folders[folder_idx].name;
                        self.status = format!("{name} — {} messages", msgs.len());
                        self.messages = msgs;
                        self.selected_message = 0;
                        self.body_text = None;
                    }
                    Err(e) => self.status = format!("Fetch error: {e}"),
                }
            }
            BgResult::Body {
                message_idx,
                result,
            } => {
                if message_idx != self.selected_message {
                    return;
                }
                match result {
                    Ok(body) => {
                        self.body_text = Some(body);
                        if let Some(msg) = self.messages.get(message_idx) {
                            self.status = msg.subject.clone();
                        }
                    }
                    Err(e) => {
                        self.body_text = Some(format!("Error: {e}"));
                        self.status = format!("Body error: {e}");
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Spawn background IMAP tasks
    // -----------------------------------------------------------------------

    fn spawn_load_folders(&self) {
        let session = Arc::clone(&self.session);
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = session
                .fetch_folders()
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Folders(result));
        });
    }

    fn spawn_load_messages(&mut self) {
        if self.folders.is_empty() {
            return;
        }
        let folder = &self.folders[self.selected_folder];
        let mbox_hash = MailboxHash(folder.mailbox_hash);
        let folder_idx = self.selected_folder;
        self.status = format!("Loading {}…", folder.name);

        let session = Arc::clone(&self.session);
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = session
                .fetch_messages(mbox_hash)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Messages { folder_idx, result });
        });
    }

    fn spawn_load_body(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &self.messages[self.selected_message];
        let env_hash = nevermail_core::EnvelopeHash(msg.envelope_hash);
        let message_idx = self.selected_message;
        self.status = "Loading body…".into();

        let session = Arc::clone(&self.session);
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = session.fetch_body(env_hash).await;
            let rendered = result
                .map(|(text_plain, text_html, _attachments)| {
                    let plain = if text_plain.is_empty() {
                        None
                    } else {
                        Some(text_plain.as_str())
                    };
                    let html = if text_html.is_empty() {
                        None
                    } else {
                        Some(text_html.as_str())
                    };
                    nevermail_core::mime::render_body(plain, html)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Body {
                message_idx,
                result: rendered,
            });
        });
    }

    // -----------------------------------------------------------------------
    // Key handling (instant — no awaits)
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyCode) -> AppEvent {
        match key {
            KeyCode::Char('q') => return AppEvent::Quit,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Folders => Focus::Messages,
                    Focus::Messages => Focus::Body,
                    Focus::Body => Focus::Folders,
                };
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Folders => Focus::Body,
                    Focus::Messages => Focus::Folders,
                    Focus::Body => Focus::Messages,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Enter => self.select(),
            _ => {}
        }
        AppEvent::Continue
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder > 0 {
                    self.selected_folder -= 1;
                    self.spawn_load_messages();
                }
            }
            Focus::Messages => {
                if self.selected_message > 0 {
                    self.selected_message -= 1;
                }
            }
            Focus::Body => {} // scroll later
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder + 1 < self.folders.len() {
                    self.selected_folder += 1;
                    self.spawn_load_messages();
                }
            }
            Focus::Messages => {
                if self.selected_message + 1 < self.messages.len() {
                    self.selected_message += 1;
                }
            }
            Focus::Body => {} // scroll later
        }
    }

    fn select(&mut self) {
        match self.focus {
            Focus::Folders => self.spawn_load_messages(),
            Focus::Messages => self.spawn_load_body(),
            Focus::Body => {}
        }
    }
}
