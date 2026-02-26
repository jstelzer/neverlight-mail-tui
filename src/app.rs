use std::sync::Arc;

use crossterm::event::KeyCode;

use nevermail_core::config::{AccountConfig, Config};
use nevermail_core::imap::ImapSession;
use nevermail_core::models::{Folder, MessageSummary};
use nevermail_core::store::CacheHandle;
use nevermail_core::MailboxHash;

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
        let account = accounts.into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("No accounts configured"))?;

        let imap_config = account.to_imap_config();
        let session = ImapSession::connect(imap_config)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let cache = CacheHandle::open()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut app = App {
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
        };

        app.load_folders().await;
        Ok(app)
    }

    // -----------------------------------------------------------------------
    // Data loading
    // -----------------------------------------------------------------------

    async fn load_folders(&mut self) {
        self.status = "Loading folders…".into();
        match self.session.fetch_folders().await {
            Ok(folders) => {
                self.folders = folders;
                self.status = format!("{} folders", self.folders.len());
                if !self.folders.is_empty() {
                    self.load_messages().await;
                }
            }
            Err(e) => self.status = format!("Folder error: {e}"),
        }
    }

    async fn load_messages(&mut self) {
        if self.folders.is_empty() {
            return;
        }
        let folder = &self.folders[self.selected_folder];
        let mbox_hash = MailboxHash(folder.mailbox_hash);
        self.status = format!("Loading {}…", folder.name);

        match self.session.fetch_messages(mbox_hash).await {
            Ok(msgs) => {
                self.status = format!("{} — {} messages", folder.name, msgs.len());
                self.messages = msgs;
                self.selected_message = 0;
                self.body_text = None;
            }
            Err(e) => self.status = format!("Fetch error: {e}"),
        }
    }

    async fn load_body(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &self.messages[self.selected_message];
        let env_hash = nevermail_core::EnvelopeHash(msg.envelope_hash);
        self.status = "Loading body…".into();

        match self.session.fetch_body(env_hash).await {
            Ok((text_plain, text_html, _attachments)) => {
                let plain = if text_plain.is_empty() { None } else { Some(text_plain.as_str()) };
                let html = if text_html.is_empty() { None } else { Some(text_html.as_str()) };
                self.body_text = Some(nevermail_core::mime::render_body(plain, html));
                self.status = msg.subject.clone();
            }
            Err(e) => {
                self.body_text = Some(format!("Error: {e}"));
                self.status = format!("Body error: {e}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub async fn handle_key(&mut self, key: KeyCode) -> AppEvent {
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
            KeyCode::Up | KeyCode::Char('k') => self.move_up().await,
            KeyCode::Down | KeyCode::Char('j') => self.move_down().await,
            KeyCode::Enter => self.select().await,
            _ => {}
        }
        AppEvent::Continue
    }

    async fn move_up(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder > 0 {
                    self.selected_folder -= 1;
                    self.load_messages().await;
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

    async fn move_down(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder + 1 < self.folders.len() {
                    self.selected_folder += 1;
                    self.load_messages().await;
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

    async fn select(&mut self) {
        match self.focus {
            Focus::Folders => self.load_messages().await,
            Focus::Messages => self.load_body().await,
            Focus::Body => {}
        }
    }

    // -----------------------------------------------------------------------
    // Tick (drive pending async work)
    // -----------------------------------------------------------------------

    pub async fn tick(&mut self) {
        // Placeholder for background tasks (IDLE watch, etc.)
    }
}
