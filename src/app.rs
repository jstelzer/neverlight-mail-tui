use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use tokio::sync::mpsc;

use nevermail_core::config::{AccountConfig, Config};
use nevermail_core::imap::ImapSession;
use nevermail_core::models::{Folder, MessageSummary};
use nevermail_core::store::{self, CacheHandle};
use nevermail_core::{EnvelopeHash, FlagOp, MailboxHash, RefreshEventKind};

use crate::compose::{self, ComposeField, ComposeMode, ComposeState};

// ---------------------------------------------------------------------------
// Background task results
// ---------------------------------------------------------------------------

#[allow(dead_code)]
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
    /// Cached messages arrived (show immediately, IMAP fetch may follow)
    CachedMessages {
        folder_idx: usize,
        result: Result<Vec<MessageSummary>, String>,
    },
    /// Flag operation completed (or failed — revert optimistic update)
    FlagOp {
        envelope_hash: u64,
        /// Original flags to restore on failure
        was_read: bool,
        was_starred: bool,
        result: Result<(), String>,
    },
    /// Move operation completed (or failed — revert optimistic removal)
    MoveOp {
        envelope_hash: u64,
        /// Original message + index for reinsertion on failure
        message: Box<Option<(usize, MessageSummary)>>,
        result: Result<(), String>,
    },
    SearchResults(Result<Vec<MessageSummary>, String>),
    SendResult(Result<(), String>),
    /// IDLE event: server notified of new/changed/removed messages
    ImapEvent {
        account_idx: usize,
        mailbox_hash: u64,
        kind: ImapEventKind,
    },
    /// Watch stream ended or errored
    WatchEnded {
        account_idx: usize,
        error: Option<String>,
    },
}

#[derive(Debug)]
pub enum ImapEventKind {
    NewMail,
    Remove(u64),
    Rescan,
}

// ---------------------------------------------------------------------------
// Per-account state
// ---------------------------------------------------------------------------

pub struct AccountState {
    pub config: AccountConfig,
    pub session: Option<Arc<ImapSession>>,
    pub folders: Vec<Folder>,
    pub folder_map: HashMap<String, u64>,
}

impl AccountState {
    fn rebuild_folder_map(&mut self) {
        self.folder_map = self
            .folders
            .iter()
            .map(|f| (f.path.clone(), f.mailbox_hash))
            .collect();
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct App {
    pub accounts: Vec<AccountState>,
    pub active_account: usize,
    pub cache: CacheHandle,
    pub messages: Vec<MessageSummary>,
    pub body_text: Option<String>,
    pub body_scroll: u16,
    pub selected_folder: usize,
    pub selected_message: usize,
    pub focus: Focus,
    pub status: String,
    pub search_active: bool,
    pub search_query: String,
    pub compose: Option<ComposeState>,
    /// Thread IDs that are collapsed (children hidden)
    pub collapsed_threads: HashSet<u64>,
    /// Maps visible row index → actual index in self.messages
    pub visible_indices: Vec<usize>,
    /// Number of messages per thread_id
    pub thread_sizes: HashMap<u64, usize>,
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
    /// Convenience accessors for the active account.
    fn active(&self) -> &AccountState {
        &self.accounts[self.active_account]
    }
    #[allow(dead_code)]
    fn active_mut(&mut self) -> &mut AccountState {
        &mut self.accounts[self.active_account]
    }
    fn active_session(&self) -> Option<Arc<ImapSession>> {
        self.active().session.clone()
    }
    fn active_account_id(&self) -> String {
        self.active().config.id.clone()
    }
    pub fn active_folders(&self) -> &[Folder] {
        &self.active().folders
    }

    pub async fn new() -> anyhow::Result<Self> {
        let configs = Config::resolve_all_accounts()
            .map_err(|e| anyhow::anyhow!("Config error: {e:?}"))?;
        if configs.is_empty() {
            return Err(anyhow::anyhow!("No accounts configured"));
        }

        let cache = CacheHandle::open().map_err(|e| anyhow::anyhow!("{e}"))?;
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();

        let mut account_states = Vec::new();
        for config in configs {
            let imap_config = config.to_imap_config();
            let session = ImapSession::connect(imap_config).await.ok();
            account_states.push(AccountState {
                config,
                session,
                folders: Vec::new(),
                folder_map: HashMap::new(),
            });
        }

        let app = App {
            accounts: account_states,
            active_account: 0,
            cache,
            messages: Vec::new(),
            body_text: None,
            body_scroll: 0,
            selected_folder: 0,
            selected_message: 0,
            focus: Focus::Folders,
            status: "Connecting…".into(),
            search_active: false,
            search_query: String::new(),
            compose: None,
            collapsed_threads: HashSet::new(),
            visible_indices: Vec::new(),
            thread_sizes: HashMap::new(),
            bg_rx,
            bg_tx,
        };

        app.spawn_load_folders();
        app.spawn_watchers();
        Ok(app)
    }

    fn spawn_watchers(&self) {
        for (idx, acct) in self.accounts.iter().enumerate() {
            let session = match &acct.session {
                Some(s) => Arc::clone(s),
                None => continue,
            };
            let tx = self.bg_tx.clone();
            tokio::spawn(async move {
                let stream = match session.watch().await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(BgResult::WatchEnded {
                            account_idx: idx,
                            error: Some(e),
                        });
                        return;
                    }
                };
                futures::pin_mut!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(nevermail_core::BackendEvent::Refresh(rev)) => {
                            let mailbox_hash = rev.mailbox_hash.0;
                            let kind = match rev.kind {
                                RefreshEventKind::Create(_) => ImapEventKind::NewMail,
                                RefreshEventKind::Remove(hash) => {
                                    ImapEventKind::Remove(hash.0)
                                }
                                RefreshEventKind::Rescan => ImapEventKind::Rescan,
                                _ => continue,
                            };
                            let _ = tx.send(BgResult::ImapEvent {
                                account_idx: idx,
                                mailbox_hash,
                                kind,
                            });
                        }
                        Err(_) => continue,
                        _ => {}
                    }
                }
                let _ = tx.send(BgResult::WatchEnded {
                    account_idx: idx,
                    error: None,
                });
            });
        }
    }

    // -----------------------------------------------------------------------
    // Threading
    // -----------------------------------------------------------------------

    pub fn recompute_visible(&mut self) {
        // Build thread sizes
        self.thread_sizes.clear();
        for msg in &self.messages {
            if let Some(tid) = msg.thread_id {
                *self.thread_sizes.entry(tid).or_insert(0) += 1;
            }
        }

        // Build visible indices: show all roots + children of non-collapsed threads
        self.visible_indices.clear();
        for (i, msg) in self.messages.iter().enumerate() {
            let dominated = msg.thread_depth > 0;
            if !dominated {
                // Root or standalone — always visible
                self.visible_indices.push(i);
            } else if let Some(tid) = msg.thread_id {
                // Child — visible only if thread is not collapsed
                if !self.collapsed_threads.contains(&tid) {
                    self.visible_indices.push(i);
                }
            } else {
                // No thread_id but has depth — show anyway
                self.visible_indices.push(i);
            }
        }
    }

    fn toggle_thread_collapse(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &self.messages[self.selected_message];
        let tid = match msg.thread_id {
            Some(t) => t,
            None => return,
        };
        let size = self.thread_sizes.get(&tid).copied().unwrap_or(1);
        if size <= 1 {
            return; // No children to collapse
        }
        if self.collapsed_threads.contains(&tid) {
            self.collapsed_threads.remove(&tid);
        } else {
            self.collapsed_threads.insert(tid);
            // If selected was a child, jump to thread root
            if msg.thread_depth > 0 {
                if let Some(root_idx) = self
                    .messages
                    .iter()
                    .position(|m| m.thread_id == Some(tid) && m.thread_depth == 0)
                {
                    self.selected_message = root_idx;
                }
            }
        }
        self.recompute_visible();
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
                let acct = &mut self.accounts[self.active_account];
                acct.folders = folders;
                acct.rebuild_folder_map();
                if !acct.folders.is_empty() {
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
                    Ok(mut msgs) => {
                        msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        let name = &self.active().folders[folder_idx].name;
                        self.status = format!("{name} — {} messages", msgs.len());
                        self.messages = msgs;
                        self.selected_message = 0;
                        self.body_text = None;
                        self.collapsed_threads.clear();
                        self.recompute_visible();
                    }
                    Err(e) => self.status = format!("Fetch error: {e}"),
                }
            }
            BgResult::CachedMessages { folder_idx, result } => {
                if folder_idx != self.selected_folder {
                    return;
                }
                // Only apply cached results if we haven't already loaded from IMAP
                if !self.messages.is_empty() {
                    return;
                }
                if let Ok(mut msgs) = result {
                    if !msgs.is_empty() {
                        msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        let name = &self.active().folders[folder_idx].name;
                        self.status = format!("{name} — {} cached, syncing…", msgs.len());
                        self.messages = msgs;
                        self.selected_message = 0;
                        self.body_text = None;
                        self.recompute_visible();
                    }
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
                        self.body_scroll = 0;
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
            BgResult::FlagOp {
                envelope_hash,
                was_read,
                was_starred,
                result,
            } => {
                if let Err(e) = result {
                    // Revert optimistic flag toggle
                    if let Some(msg) = self
                        .messages
                        .iter_mut()
                        .find(|m| m.envelope_hash == envelope_hash)
                    {
                        msg.is_read = was_read;
                        msg.is_starred = was_starred;
                    }
                    self.status = format!("Flag error: {e}");
                }
            }
            BgResult::MoveOp {
                envelope_hash: _,
                message,
                result,
            } => {
                if let Err(e) = result {
                    // Revert optimistic removal — reinsert the message
                    if let Some((idx, msg)) = *message {
                        let insert_at = idx.min(self.messages.len());
                        self.messages.insert(insert_at, msg);
                    }
                    self.status = format!("Move error: {e}");
                }
            }
            BgResult::SearchResults(result) => match result {
                Ok(mut msgs) => {
                    msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                    self.status = format!("Search: {} results", msgs.len());
                    self.messages = msgs;
                    self.selected_message = 0;
                    self.body_text = None;
                    self.collapsed_threads.clear();
                    self.recompute_visible();
                }
                Err(e) => self.status = format!("Search error: {e}"),
            },
            BgResult::SendResult(result) => match result {
                Ok(()) => {
                    self.compose = None;
                    self.status = "Message sent.".into();
                }
                Err(e) => self.status = format!("Send error: {e}"),
            },
            BgResult::ImapEvent {
                account_idx,
                mailbox_hash,
                kind,
            } => {
                // Only act if event is for the active account and current folder
                let is_active = account_idx == self.active_account;
                let current_mbox = self
                    .active()
                    .folders
                    .get(self.selected_folder)
                    .map(|f| f.mailbox_hash);
                match kind {
                    ImapEventKind::NewMail | ImapEventKind::Rescan => {
                        if is_active && current_mbox == Some(mailbox_hash) {
                            self.status = "New mail — refreshing…".into();
                            self.spawn_load_messages();
                        }
                    }
                    ImapEventKind::Remove(envelope_hash) => {
                        if is_active && current_mbox == Some(mailbox_hash) {
                            self.messages
                                .retain(|m| m.envelope_hash != envelope_hash);
                            if self.selected_message >= self.messages.len()
                                && !self.messages.is_empty()
                            {
                                self.selected_message = self.messages.len() - 1;
                            }
                        }
                    }
                }
            }
            BgResult::WatchEnded { account_idx, error } => {
                if let Some(e) = error {
                    log::warn!("Watch ended for account {account_idx}: {e}");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Spawn background IMAP tasks
    // -----------------------------------------------------------------------

    fn spawn_load_folders(&self) {
        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let tx = self.bg_tx.clone();
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        tokio::spawn(async move {
            let result = session
                .fetch_folders()
                .await
                .map_err(|e| e.to_string());
            if let Ok(ref folders) = result {
                let _ = cache
                    .save_folders(account_id, folders.clone())
                    .await;
            }
            let _ = tx.send(BgResult::Folders(result));
        });
    }

    fn spawn_load_messages(&mut self) {
        let acct = &self.accounts[self.active_account];
        if acct.folders.is_empty() {
            return;
        }
        let folder = &acct.folders[self.selected_folder];
        let mbox_hash_raw = folder.mailbox_hash;
        let mbox_hash = MailboxHash(mbox_hash_raw);
        let folder_idx = self.selected_folder;
        self.status = format!("Loading {}…", folder.name);
        // Clear messages so cached results can display
        self.messages.clear();
        self.body_text = None;

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let tx = self.bg_tx.clone();

        // Try cache first (fast path)
        let cache2 = cache.clone();
        let account_id2 = account_id.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let result = cache2
                .load_messages(account_id2, mbox_hash_raw, 200, 0)
                .await;
            let _ = tx2.send(BgResult::CachedMessages {
                folder_idx,
                result,
            });
        });

        // IMAP fetch (authoritative, overwrites cache)
        tokio::spawn(async move {
            let result = session
                .fetch_messages(mbox_hash)
                .await
                .map_err(|e| e.to_string());
            if let Ok(ref msgs) = result {
                let _ = cache
                    .save_messages(account_id, mbox_hash_raw, msgs.clone())
                    .await;
            }
            let _ = tx.send(BgResult::Messages { folder_idx, result });
        });
    }

    fn spawn_load_body(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &self.messages[self.selected_message];
        let env_hash_raw = msg.envelope_hash;
        let env_hash = nevermail_core::EnvelopeHash(env_hash_raw);
        let message_idx = self.selected_message;
        self.status = "Loading body…".into();

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            // Try cache first
            if let Ok(Some((md_body, plain_body, _attachments))) =
                cache.load_body(env_hash_raw).await
            {
                let body = if !plain_body.is_empty() {
                    plain_body
                } else {
                    md_body
                };
                let _ = tx.send(BgResult::Body {
                    message_idx,
                    result: Ok(body),
                });
                return;
            }

            // Cache miss — fetch from IMAP
            let result = session.fetch_body(env_hash).await;
            let rendered = result
                .map(|(text_plain, text_html, attachments)| {
                    let rendered = nevermail_core::mime::render_body(
                        if text_plain.is_empty() {
                            None
                        } else {
                            Some(text_plain.as_str())
                        },
                        if text_html.is_empty() {
                            None
                        } else {
                            Some(text_html.as_str())
                        },
                    );
                    // Save to cache (fire-and-forget)
                    let cache2 = cache.clone();
                    tokio::spawn(async move {
                        let _ = cache2
                            .save_body(env_hash_raw, text_plain, text_html, attachments)
                            .await;
                    });
                    rendered
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Body {
                message_idx,
                result: rendered,
            });
        });
    }

    // -----------------------------------------------------------------------
    // Flag and move operations
    // -----------------------------------------------------------------------

    fn toggle_read(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &mut self.messages[self.selected_message];
        let was_read = msg.is_read;
        let was_starred = msg.is_starred;
        let envelope_hash = msg.envelope_hash;
        let mailbox_hash = msg.mailbox_hash;

        // Optimistic toggle
        msg.is_read = !was_read;
        let new_read = msg.is_read;

        let flag_op = if new_read {
            FlagOp::Set(nevermail_core::Flag::SEEN)
        } else {
            FlagOp::UnSet(nevermail_core::Flag::SEEN)
        };

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            // Update cache optimistically
            let flags = store::flags_to_u8(new_read, was_starred);
            let op = if new_read { "mark-read" } else { "mark-unread" };
            let _ = cache
                .update_flags(envelope_hash, flags, op.to_string())
                .await;

            // IMAP sync
            let result = session
                .set_flags(
                    EnvelopeHash(envelope_hash),
                    MailboxHash(mailbox_hash),
                    vec![flag_op],
                )
                .await;

            if result.is_ok() {
                let _ = cache.clear_pending_op(envelope_hash, flags).await;
            } else {
                let _ = cache.revert_pending_op(envelope_hash).await;
            }

            let _ = tx.send(BgResult::FlagOp {
                envelope_hash,
                was_read,
                was_starred,
                result,
            });
        });
    }

    fn toggle_star(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &mut self.messages[self.selected_message];
        let was_read = msg.is_read;
        let was_starred = msg.is_starred;
        let envelope_hash = msg.envelope_hash;
        let mailbox_hash = msg.mailbox_hash;

        // Optimistic toggle
        msg.is_starred = !was_starred;
        let new_starred = msg.is_starred;

        let flag_op = if new_starred {
            FlagOp::Set(nevermail_core::Flag::FLAGGED)
        } else {
            FlagOp::UnSet(nevermail_core::Flag::FLAGGED)
        };

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let flags = store::flags_to_u8(was_read, new_starred);
            let op = if new_starred { "star" } else { "unstar" };
            let _ = cache
                .update_flags(envelope_hash, flags, op.to_string())
                .await;

            let result = session
                .set_flags(
                    EnvelopeHash(envelope_hash),
                    MailboxHash(mailbox_hash),
                    vec![flag_op],
                )
                .await;

            if result.is_ok() {
                let _ = cache.clear_pending_op(envelope_hash, flags).await;
            } else {
                let _ = cache.revert_pending_op(envelope_hash).await;
            }

            let _ = tx.send(BgResult::FlagOp {
                envelope_hash,
                was_read,
                was_starred,
                result,
            });
        });
    }

    fn move_to_folder(&mut self, target_name: &str) {
        if self.messages.is_empty() {
            return;
        }

        // Look up target folder hash
        let fm = &self.active().folder_map;
        let dest_hash = fm
            .get(target_name)
            .or_else(|| fm.get(&format!("INBOX.{target_name}")))
            .copied();
        let dest_hash = match dest_hash {
            Some(h) => h,
            None => {
                self.status = format!("No {target_name} folder found");
                return;
            }
        };

        let idx = self.selected_message;
        let msg = self.messages.remove(idx);
        let envelope_hash = msg.envelope_hash;
        let source_hash = msg.mailbox_hash;

        // Adjust selection
        if self.selected_message >= self.messages.len() && !self.messages.is_empty() {
            self.selected_message = self.messages.len() - 1;
        }
        if self.messages.is_empty() {
            self.body_text = None;
        }

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let saved_msg = msg.clone();
        tokio::spawn(async move {
            // Cache: mark as pending move
            let _ = cache
                .update_flags(
                    envelope_hash,
                    store::flags_to_u8(saved_msg.is_read, saved_msg.is_starred),
                    format!("move:{dest_hash}"),
                )
                .await;

            let result = session
                .move_messages(
                    EnvelopeHash(envelope_hash),
                    MailboxHash(source_hash),
                    MailboxHash(dest_hash),
                )
                .await;

            if result.is_ok() {
                let _ = cache.remove_message(envelope_hash).await;
            } else {
                let _ = cache.revert_pending_op(envelope_hash).await;
            }

            let _ = tx.send(BgResult::MoveOp {
                envelope_hash,
                message: Box::new(if result.is_err() {
                    Some((idx, saved_msg))
                } else {
                    None
                }),
                result,
            });
        });
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    fn spawn_search(&self) {
        let query = self.search_query.clone();
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = cache.search(query).await;
            let _ = tx.send(BgResult::SearchResults(result));
        });
    }

    fn exit_search(&mut self) {
        self.search_active = false;
        self.search_query.clear();
        // Reload current folder
        self.spawn_load_messages();
    }

    // -----------------------------------------------------------------------
    // Compose
    // -----------------------------------------------------------------------

    fn start_compose(&mut self, mode: ComposeMode) {
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
                        state.body = tui_textarea::TextArea::new(
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
                        let fwd =
                            compose::forward_body(body, &msg.from, &msg.date, &msg.subject);
                        state.body = tui_textarea::TextArea::new(
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

    fn spawn_send(&mut self) {
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
        let email = nevermail_core::smtp::OutgoingEmail {
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
            let result = nevermail_core::smtp::send_email(&smtp_config, &email).await;
            let _ = tx.send(BgResult::SendResult(result));
        });
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> AppEvent {
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
                        KeyCode::Backspace => { state.to.pop(); }
                        KeyCode::Char(c) => state.to.push(c),
                        _ => {}
                    },
                    ComposeField::Subject => match key.code {
                        KeyCode::Backspace => { state.subject.pop(); }
                        KeyCode::Char(c) => state.subject.push(c),
                        _ => {}
                    },
                    ComposeField::Body => {
                        // Forward full KeyEvent to tui-textarea
                        state.body.input(key);
                    }
                }
            }
        }
        AppEvent::Continue
    }

    // -----------------------------------------------------------------------
    // Key handling (instant — no awaits)
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> AppEvent {
        // Compose mode gets priority
        if self.compose.is_some() {
            return self.handle_compose_key(key);
        }
        if self.search_active {
            return self.handle_search_key(key.code);
        }
        match key.code {
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
            KeyCode::Char('s') => self.toggle_star(),
            KeyCode::Char('R') => self.toggle_read(),
            KeyCode::Char('d') => self.move_to_folder("Trash"),
            KeyCode::Char('a') => self.move_to_folder("Archive"),
            KeyCode::Char('/') => {
                self.search_active = true;
                self.search_query.clear();
                self.status = "Search: ".into();
            }
            KeyCode::Char(' ') => self.toggle_thread_collapse(),
            KeyCode::Char('c') => self.start_compose(ComposeMode::New),
            KeyCode::Char('r') => self.start_compose(ComposeMode::Reply),
            KeyCode::Char('f') => self.start_compose(ComposeMode::Forward),
            KeyCode::Char(n @ '1'..='9') => {
                let idx = (n as usize) - ('1' as usize);
                if idx < self.accounts.len() && idx != self.active_account {
                    self.active_account = idx;
                    self.selected_folder = 0;
                    self.selected_message = 0;
                    self.body_text = None;
                    self.messages.clear();
                    if self.active().folders.is_empty() {
                        self.spawn_load_folders();
                    } else {
                        self.spawn_load_messages();
                    }
                    self.status = format!(
                        "Account: {}",
                        self.active().config.label
                    );
                }
            }
            _ => {}
        }
        AppEvent::Continue
    }

    fn handle_search_key(&mut self, key: KeyCode) -> AppEvent {
        match key {
            KeyCode::Esc => self.exit_search(),
            KeyCode::Enter => {
                if self.search_query.is_empty() {
                    self.exit_search();
                } else {
                    self.status = format!("Searching: {}…", self.search_query);
                    self.spawn_search();
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.status = format!("Search: {}", self.search_query);
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.status = format!("Search: {}", self.search_query);
            }
            _ => {}
        }
        AppEvent::Continue
    }

    /// Navigate messages using visible_indices when threading is active.
    fn visible_nav(&self, direction: i32) -> Option<usize> {
        if self.visible_indices.is_empty() {
            // No threading — simple navigation
            let new = self.selected_message as i32 + direction;
            if new >= 0 && (new as usize) < self.messages.len() {
                return Some(new as usize);
            }
            return None;
        }
        // Find current position in visible_indices
        let cur_pos = self
            .visible_indices
            .iter()
            .position(|&i| i == self.selected_message)
            .unwrap_or(0);
        let new_pos = cur_pos as i32 + direction;
        if new_pos >= 0 && (new_pos as usize) < self.visible_indices.len() {
            Some(self.visible_indices[new_pos as usize])
        } else {
            None
        }
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
                if let Some(idx) = self.visible_nav(-1) {
                    self.selected_message = idx;
                }
            }
            Focus::Body => {
                self.body_scroll = self.body_scroll.saturating_sub(1);
            }
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder + 1 < self.active().folders.len() {
                    self.selected_folder += 1;
                    self.spawn_load_messages();
                }
            }
            Focus::Messages => {
                if let Some(idx) = self.visible_nav(1) {
                    self.selected_message = idx;
                }
            }
            Focus::Body => {
                self.body_scroll = self.body_scroll.saturating_add(1);
            }
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
