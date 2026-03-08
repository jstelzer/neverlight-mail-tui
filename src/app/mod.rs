mod actions;
mod compose;
mod images;
mod lanes;
mod navigation;
mod search;
mod sync;
mod watch;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::prelude::Rect;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use neverlight_mail_core::client::JmapClient;
use neverlight_mail_core::config::AccountConfig;
use neverlight_mail_core::models::{AttachmentData, Folder, MessageSummary};
use neverlight_mail_core::session::JmapSession;
use neverlight_mail_core::store::CacheHandle;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::thread::{ResizeRequest, ThreadProtocol};

use crate::compose::ComposeState;

// ---------------------------------------------------------------------------
// Error classification helpers
// ---------------------------------------------------------------------------

fn error_indicates_dead_session(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("deadline has elapsed")
        || lower.contains("not connected")
        || lower.contains("connection refused")
        || lower.contains("eof")
}

fn body_error_indicates_stale_message(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("not found")
        || lower.contains("deleted before you requested")
        || lower.contains("local cache")
}

// ---------------------------------------------------------------------------
// Background task results
// ---------------------------------------------------------------------------

pub enum BgResult {
    Folders {
        account_idx: usize,
        lane_epoch: u64,
        result: Result<Vec<Folder>, String>,
    },
    Messages {
        account_idx: usize,
        lane_epoch: u64,
        folder_idx: usize,
        mailbox_id: String,
        result: Result<Vec<MessageSummary>, String>,
    },
    /// Cached messages arrived (show immediately, JMAP fetch may follow)
    CachedMessages {
        account_idx: usize,
        lane_epoch: u64,
        folder_idx: usize,
        mailbox_id: String,
        result: Result<Vec<MessageSummary>, String>,
    },
    Body {
        account_id: String,
        lane_epoch: u64,
        mailbox_id: String,
        email_id: String,
        result: Result<(String, Vec<AttachmentData>), String>,
    },
    /// Flag operation completed (or failed — revert optimistic update)
    FlagOp {
        account_id: String,
        lane_epoch: u64,
        email_id: String,
        /// Original flags to restore on failure
        was_read: bool,
        was_starred: bool,
        result: Result<(), String>,
    },
    /// Move operation completed (or failed — revert optimistic removal)
    MoveOp {
        account_id: String,
        lane_epoch: u64,
        destination_name: String,
        /// Original message + index for reinsertion on failure
        message: Box<Option<(usize, MessageSummary)>>,
        result: Result<(), String>,
    },
    SearchResults {
        lane_epoch: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    SendResult(Result<(), String>),
    /// Push state changed — trigger refresh
    PushStateChanged {
        account_idx: usize,
        watch_generation: u64,
    },
    /// Push stream ended or errored
    PushEnded {
        account_idx: usize,
        watch_generation: u64,
        error: Option<String>,
    },
    /// Re-spawn push watcher after a delay
    PushRetry {
        account_idx: usize,
    },
    /// Reconnect attempt completed
    Reconnected {
        account_idx: usize,
        result: Result<JmapClient, String>,
    },
}

// ---------------------------------------------------------------------------
// Per-account state
// ---------------------------------------------------------------------------

pub struct AccountState {
    pub config: AccountConfig,
    pub client: Option<JmapClient>,
    pub folders: Vec<Folder>,
    /// Consecutive reconnect failures (reset on success).
    pub reconnect_attempts: u32,
    /// Last error message for diagnostics.
    pub last_error: Option<String>,
    /// Monotonic generation counter for watcher identity.
    pub watch_generation: u64,
}

impl AccountState {
    /// Backoff duration for reconnect retries: 5s, 15s, 30s, 60s cap.
    pub fn reconnect_backoff(&self) -> Duration {
        let secs = match self.reconnect_attempts {
            0 => 5,
            1 => 15,
            2 => 30,
            _ => 60,
        };
        Duration::from_secs(secs)
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

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
    pub phase: Phase,
    pub search_active: bool,
    pub search_query: String,
    pub compose: Option<ComposeState>,
    /// Thread IDs that are collapsed (children hidden)
    pub collapsed_threads: HashSet<String>,
    /// Maps visible row index → actual index in self.messages
    pub visible_indices: Vec<usize>,
    /// Number of messages per thread_id
    pub thread_sizes: HashMap<String, usize>,
    pub bg_rx: mpsc::UnboundedReceiver<BgResult>,
    pub(super) bg_tx: mpsc::UnboundedSender<BgResult>,
    /// Layout rects set by ui::render each frame, used for mouse hit-testing.
    pub layout_rects: LayoutRects,
    /// Terminal image protocol picker (sixel/kitty/halfblocks).
    pub(super) picker: Option<Picker>,
    /// Picker-selected protocol for runtime diagnostics.
    pub picker_protocol: Option<ProtocolType>,
    /// Image protocols for inline image attachments in the current message body.
    pub image_protos: Vec<ThreadProtocol>,
    /// Index of the currently displayed image in the carousel.
    pub image_index: usize,
    /// Channel for image resize requests from ThreadProtocol.
    pub img_resize_rx: mpsc::UnboundedReceiver<ResizeRequest>,
    pub(super) img_resize_tx: mpsc::UnboundedSender<ResizeRequest>,
    /// Attachment summary for the current message (filename, mime_type, size).
    pub attachment_info: Vec<(String, String, usize)>,
    pub(super) lane_epochs: LaneEpochs,
    pub(super) lane_tasks: LaneTasks,
    pub(super) account_lane_epochs: AccountLaneEpochs,
    pub(super) account_lane_tasks: AccountLaneTasks,
    /// At most one delayed reconnect task per account.
    pub(super) reconnect_tasks: HashMap<usize, JoinHandle<()>>,
    pub diagnostics: Diagnostics,
}

/// Cached layout geometry for mouse hit-testing.
#[derive(Default, Clone)]
pub struct LayoutRects {
    pub folders: Rect,
    pub messages: Rect,
    pub body: Rect,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Loading,
    Refreshing,
    Searching,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lane {
    Folder,
    Message,
    Search,
    Flag,
    Mutation,
}

#[derive(Default)]
pub(super) struct LaneEpochs {
    pub folder: u64,
    pub message: u64,
    pub search: u64,
    pub flag: u64,
    pub mutation: u64,
}

#[derive(Default)]
pub(super) struct LaneTasks {
    pub folder: Vec<JoinHandle<()>>,
    pub message: Option<JoinHandle<()>>,
    pub search: Option<JoinHandle<()>>,
    pub flag: Option<JoinHandle<()>>,
    pub mutation: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct AccountLaneEpochs {
    pub flag: HashMap<String, u64>,
    pub mutation: HashMap<String, u64>,
}

#[derive(Default)]
pub(super) struct AccountLaneTasks {
    pub flag: HashMap<String, JoinHandle<()>>,
    pub mutation: HashMap<String, JoinHandle<()>>,
}

#[derive(Default, Clone, Copy)]
pub struct LaneOpIds {
    pub folder: u64,
    pub message: u64,
    pub search: u64,
    pub flag: u64,
    pub mutation: u64,
}

#[derive(Default)]
pub struct Diagnostics {
    pub current_op_ids: LaneOpIds,
    pub toc_drift_count: u64,
    pub refresh_stuck_count: u64,
    pub refresh_timeout_count: u64,
    pub(super) next_op_id: u64,
    pub(super) refresh_started_at: Option<Instant>,
    pub(super) refresh_stuck_reported: bool,
    pub(super) refresh_timeout_reported: bool,
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

impl App {
    /// Convenience accessors for the active account.
    pub(super) fn active(&self) -> &AccountState {
        &self.accounts[self.active_account]
    }

    pub(super) fn active_client(&self) -> Option<JmapClient> {
        self.active().client.clone()
    }

    pub(super) fn active_account_id(&self) -> String {
        self.active().config.id.clone()
    }

    pub(super) fn account_idx_by_id(&self, account_id: &str) -> Option<usize> {
        self.accounts
            .iter()
            .position(|acct| acct.config.id == account_id)
    }

    pub(super) fn account_for_message(&self, msg: &MessageSummary) -> Option<(usize, String)> {
        if !msg.account_id.is_empty() {
            let account_id = msg.account_id.clone();
            self.account_idx_by_id(&account_id)
                .map(|idx| (idx, account_id))
        } else {
            Some((self.active_account, self.active_account_id()))
        }
    }

    pub(super) fn fill_missing_account_ids(
        &self,
        messages: &mut [MessageSummary],
        fallback_account_id: &str,
    ) {
        for msg in messages {
            if msg.account_id.is_empty() {
                msg.account_id = fallback_account_id.to_string();
            }
        }
    }

    pub(super) fn message_identity(msg: &MessageSummary) -> (&str, &str, &str) {
        (&msg.account_id, &msg.mailbox_id, &msg.email_id)
    }

    pub(super) fn selected_message_identity(&self) -> Option<(&str, &str, &str)> {
        self.messages
            .get(self.selected_message)
            .map(Self::message_identity)
    }

    pub fn active_folders(&self) -> &[Folder] {
        &self.active().folders
    }

    pub async fn with_accounts(accounts: Vec<AccountConfig>) -> anyhow::Result<Self> {
        let cache = CacheHandle::open("tui").map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut account_states = Vec::new();
        for config in accounts {
            let client = JmapSession::connect(&config).await.ok().map(|(_, c)| c);
            let state = AccountState {
                config,
                client,
                folders: Vec::new(),
                reconnect_attempts: 0,
                last_error: None,
                watch_generation: 0,
            };
            account_states.push(state);
        }

        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let (img_resize_tx, img_resize_rx) = mpsc::unbounded_channel();

        let mut app = App {
            accounts: account_states,
            active_account: 0,
            cache,
            messages: Vec::new(),
            body_text: None,
            body_scroll: 0,
            selected_folder: 0,
            selected_message: 0,
            focus: Focus::Folders,
            status: "Starting…".into(),
            phase: Phase::Loading,
            search_active: false,
            search_query: String::new(),
            compose: None,
            collapsed_threads: HashSet::new(),
            visible_indices: Vec::new(),
            thread_sizes: HashMap::new(),
            bg_rx,
            bg_tx,
            layout_rects: LayoutRects::default(),
            picker: None,
            picker_protocol: None,
            image_protos: Vec::new(),
            image_index: 0,
            img_resize_rx,
            img_resize_tx,
            attachment_info: Vec::new(),
            lane_epochs: LaneEpochs::default(),
            lane_tasks: LaneTasks::default(),
            account_lane_epochs: AccountLaneEpochs::default(),
            account_lane_tasks: AccountLaneTasks::default(),
            reconnect_tasks: HashMap::new(),
            diagnostics: Diagnostics::default(),
        };

        app.spawn_load_folders();
        app.spawn_watchers();
        Ok(app)
    }

    /// Apply a background result to app state.
    pub fn apply(&mut self, result: BgResult) {
        self.check_refresh_watchdog();
        match result {
            BgResult::Folders {
                account_idx,
                lane_epoch,
                result,
            } => {
                if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Folder)
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    log::debug!(
                        "stale-drop lane=folder active_account={} event_account={} expected_epoch={} got_epoch={} drift_count={}",
                        self.active_account,
                        account_idx,
                        self.lane_epoch(Lane::Folder),
                        lane_epoch,
                        self.diagnostics.toc_drift_count
                    );
                    return;
                }
                match result {
                    Ok(folders) => {
                        self.status = format!("{} folders", folders.len());
                        self.phase = Phase::Idle;
                        let acct = &mut self.accounts[account_idx];
                        acct.folders = folders;
                        if !acct.folders.is_empty() {
                            self.spawn_load_messages();
                        }
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        self.status = format!("Folder error: {e}");
                        log::error!(
                            "Folder sync failed for '{}': {e} — dropping client",
                            self.accounts[account_idx].config.label
                        );
                        let acct = &mut self.accounts[account_idx];
                        acct.last_error = Some(e);
                        acct.client = None;
                        acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
                        self.spawn_reconnect(account_idx);
                    }
                }
            }
            BgResult::Messages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_id,
                result,
            } => {
                if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Folder)
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                if folder_idx != self.selected_folder {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                if self
                    .active()
                    .folders
                    .get(folder_idx)
                    .map(|f| f.mailbox_id.as_str())
                    != Some(mailbox_id.as_str())
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                match result {
                    Ok(mut msgs) => {
                        let account_id = self.accounts[account_idx].config.id.clone();
                        self.fill_missing_account_ids(&mut msgs, &account_id);
                        msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        let name = &self.active().folders[folder_idx].name;
                        self.status = format!("{name} — {} messages", msgs.len());
                        self.phase = Phase::Idle;
                        // Reconcile sidebar unread count from actual message flags
                        let unread = msgs.iter().filter(|m| !m.is_read).count() as u32;
                        if let Some(folder) = self.accounts[account_idx].folders.get_mut(folder_idx)
                        {
                            if folder.unread_count != unread {
                                log::debug!(
                                    "Reconciling unread for '{}': {} → {}",
                                    folder.name,
                                    folder.unread_count,
                                    unread,
                                );
                                folder.unread_count = unread;
                            }
                        }
                        self.messages = msgs;
                        self.selected_message = 0;
                        self.body_text = None;
                        self.collapsed_threads.clear();
                        self.recompute_visible();
                        // Auto-focus message list when messages arrive
                        if !self.messages.is_empty() {
                            self.focus = Focus::Messages;
                        }
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        self.status = format!("Fetch error: {e}");
                        log::error!(
                            "Message sync failed for '{}': {e} — dropping client",
                            self.accounts[account_idx].config.label
                        );
                        let acct = &mut self.accounts[account_idx];
                        acct.last_error = Some(e);
                        acct.client = None;
                        acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
                        self.spawn_reconnect(account_idx);
                    }
                }
            }
            BgResult::CachedMessages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_id,
                result,
            } => {
                if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Folder)
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                if folder_idx != self.selected_folder {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                if self
                    .active()
                    .folders
                    .get(folder_idx)
                    .map(|f| f.mailbox_id.as_str())
                    != Some(mailbox_id.as_str())
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                if !self.messages.is_empty() {
                    return;
                }
                if let Ok(mut msgs) = result {
                    if !msgs.is_empty() {
                        let account_id = self.accounts[account_idx].config.id.clone();
                        self.fill_missing_account_ids(&mut msgs, &account_id);
                        msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        let name = &self.active().folders[folder_idx].name;
                        self.status = format!("{name} — {} cached, syncing…", msgs.len());
                        self.messages = msgs;
                        self.selected_message = 0;
                        self.body_text = None;
                        self.recompute_visible();
                        self.focus = Focus::Messages;
                    }
                }
            }
            BgResult::Body {
                account_id,
                lane_epoch,
                mailbox_id,
                email_id,
                result,
            } => {
                if lane_epoch != self.lane_epoch(Lane::Message)
                    || self.selected_message_identity()
                        != Some((account_id.as_str(), mailbox_id.as_str(), email_id.as_str()))
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                match result {
                    Ok((body, attachments)) => {
                        self.phase = Phase::Idle;
                        self.body_text = Some(body);
                        self.body_scroll = 0;
                        self.attachment_info = attachments
                            .iter()
                            .map(|a| (a.filename.clone(), a.mime_type.clone(), a.data.len()))
                            .collect();
                        self.image_protos.clear();
                        self.image_index = 0;
                        if let Some(picker) = &self.picker {
                            for att in &attachments {
                                if att.is_image() {
                                    if let Ok(img) = image::load_from_memory(&att.data) {
                                        let proto = picker.new_resize_protocol(img);
                                        self.image_protos.push(ThreadProtocol::new(
                                            self.img_resize_tx.clone(),
                                            Some(proto),
                                        ));
                                    }
                                }
                            }
                        }
                        if let Some(msg) = self
                            .messages
                            .iter()
                            .find(|m| m.email_id == email_id)
                        {
                            self.status = msg.subject.clone();
                        }
                    }
                    Err(e) => {
                        // Stale message: cached TOC has it but server doesn't.
                        // Evict from the list and trigger a refresh to reconcile.
                        if body_error_indicates_stale_message(&e) {
                            log::warn!(
                                "Evicting stale message {} (body error: {e})",
                                email_id,
                            );
                            if let Some(pos) = self.messages.iter().position(|m| {
                                Self::message_identity(m)
                                    == (account_id.as_str(), mailbox_id.as_str(), email_id.as_str())
                            }) {
                                self.messages.remove(pos);
                                if self.selected_message >= self.messages.len()
                                    && !self.messages.is_empty()
                                {
                                    self.selected_message = self.messages.len() - 1;
                                }
                                self.recompute_visible();
                            }
                            // Evict from cache
                            let cache = self.cache.clone();
                            let evict_account_id = account_id.clone();
                            let evict_email_id = email_id.clone();
                            tokio::spawn(async move {
                                if let Err(e) =
                                    cache.remove_message(evict_account_id, evict_email_id).await
                                {
                                    log::warn!("Failed to evict stale message from cache: {e}");
                                }
                            });
                            self.body_text = None;
                            self.attachment_info.clear();
                            self.image_protos.clear();
                            self.image_index = 0;
                            self.status = "Message no longer exists on server".into();
                            self.spawn_load_messages();
                            return;
                        }
                        self.phase = Phase::Error;
                        self.body_text = Some(format!("Error: {e}"));
                        self.attachment_info.clear();
                        self.image_protos.clear();
                        self.image_index = 0;
                        self.status = format!("Body error: {e}");
                        if let Some(account_idx) = self.account_idx_by_id(&account_id) {
                            if self.accounts[account_idx].client.is_none()
                                || error_indicates_dead_session(&e)
                            {
                                self.drop_client_and_reconnect(account_idx, "body-failed");
                            }
                        }
                    }
                }
            }
            BgResult::FlagOp {
                account_id,
                lane_epoch,
                email_id,
                was_read,
                was_starred,
                result,
            } => {
                if lane_epoch != self.account_lane_epoch(&account_id, Lane::Flag) {
                    return;
                }
                if let Err(e) = result {
                    self.phase = Phase::Error;
                    if let Some(msg) = self.messages.iter_mut().find(|m| {
                        m.account_id == account_id && m.email_id == email_id
                    }) {
                        msg.is_read = was_read;
                        msg.is_starred = was_starred;
                    }
                    self.status = format!("Flag error: {e}");
                    if let Some(account_idx) = self.account_idx_by_id(&account_id) {
                        if self.accounts[account_idx].client.is_none()
                            || error_indicates_dead_session(&e)
                        {
                            self.drop_client_and_reconnect(account_idx, "flag-failed");
                        }
                    }
                }
            }
            BgResult::MoveOp {
                account_id,
                lane_epoch,
                destination_name,
                message,
                result,
            } => {
                if lane_epoch != self.account_lane_epoch(&account_id, Lane::Mutation) {
                    return;
                }
                match result {
                    Ok(()) => {
                        self.phase = Phase::Idle;
                        self.status = format!("Moved to {destination_name}.");
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        // Re-insert the message on failure
                        if let Some((idx, msg)) = *message {
                            let insert_at = idx.min(self.messages.len());
                            self.messages.insert(insert_at, msg);
                        }
                        self.status = format!("Move error: {e}");
                        if let Some(account_idx) = self.account_idx_by_id(&account_id) {
                            if self.accounts[account_idx].client.is_none()
                                || error_indicates_dead_session(&e)
                            {
                                self.drop_client_and_reconnect(account_idx, "move-failed");
                            }
                        }
                    }
                }
            }
            BgResult::SearchResults {
                lane_epoch,
                result,
            } => {
                if lane_epoch != self.lane_epoch(Lane::Search) {
                    return;
                }
                match result {
                    Ok(mut msgs) => {
                        msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        self.phase = Phase::Idle;
                        self.status = format!("Search: {} results", msgs.len());
                        self.messages = msgs;
                        self.selected_message = 0;
                        self.body_text = None;
                        self.collapsed_threads.clear();
                        self.recompute_visible();
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        self.status = format!("Search error: {e}");
                    }
                }
            }
            BgResult::SendResult(result) => match result {
                Ok(()) => {
                    self.phase = Phase::Idle;
                    self.compose = None;
                    self.status = "Message sent.".into();
                }
                Err(e) => {
                    self.phase = Phase::Error;
                    self.status = format!("Send error: {e}");
                }
            },
            BgResult::PushStateChanged {
                account_idx,
                watch_generation,
            } => {
                // Stale watcher — ignore events from a superseded push stream.
                if let Some(acct) = self.accounts.get(account_idx) {
                    if watch_generation != acct.watch_generation {
                        log::debug!(
                            "Ignoring stale PushStateChanged for '{}' (gen {} != current {})",
                            acct.config.label,
                            watch_generation,
                            acct.watch_generation,
                        );
                        return;
                    }
                }
                // Refresh the active account's current folder
                let is_active = account_idx == self.active_account;
                if is_active {
                    self.phase = Phase::Refreshing;
                    self.begin_refresh_watchdog();
                    self.status = "State changed — refreshing…".into();
                    self.spawn_load_messages();
                }
            }
            BgResult::PushEnded {
                account_idx,
                watch_generation,
                error,
            } => {
                if let Some(acct) = self.accounts.get_mut(account_idx) {
                    // Stale watcher — a newer watcher has been spawned since this one started.
                    if watch_generation != acct.watch_generation {
                        log::debug!(
                            "Ignoring stale PushEnded for '{}' (gen {} != current {})",
                            acct.config.label,
                            watch_generation,
                            acct.watch_generation,
                        );
                        return;
                    }
                    match &error {
                        Some(e) => log::warn!("Push ended for '{}': {e}", acct.config.label),
                        None => log::info!("Push stream ended for '{}'", acct.config.label),
                    }
                    acct.last_error = error.or_else(|| Some("Push stream ended".into()));
                    // Don't null the API client — push failures don't mean the
                    // API is dead. Just re-spawn the watcher after a delay.
                    let account_idx_copy = account_idx;
                    let tx = self.bg_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let _ = tx.send(BgResult::PushRetry { account_idx: account_idx_copy });
                    });
                }
            }
            BgResult::PushRetry { account_idx } => {
                if self.accounts.get(account_idx).and_then(|a| a.client.as_ref()).is_some() {
                    log::info!("Re-spawning push watcher for account {}", account_idx);
                    self.spawn_watcher_for(account_idx);
                }
            }
            BgResult::Reconnected {
                account_idx,
                result,
            } => {
                self.reconnect_tasks.remove(&account_idx);
                match result {
                    Ok(client) => {
                        if let Some(acct) = self.accounts.get_mut(account_idx) {
                            // If already connected (a prior reconnect won the race), drop
                            // this duplicate client silently.
                            if acct.client.is_some() {
                                log::debug!(
                                    "Ignoring duplicate reconnect for '{}' (already connected)",
                                    acct.config.label,
                                );
                                return;
                            }
                            log::info!(
                                "Reconnected '{}' after {} attempt(s)",
                                acct.config.label,
                                acct.reconnect_attempts,
                            );
                            acct.client = Some(client);
                            acct.reconnect_attempts = 0;
                            acct.last_error = None;
                            self.spawn_watcher_for(account_idx);
                            if account_idx == self.active_account {
                                self.spawn_load_folders();
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(acct) = self.accounts.get_mut(account_idx) {
                            acct.last_error = Some(e.clone());
                            acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
                            log::error!(
                                "Reconnect failed for '{}' (attempt {}): {}",
                                acct.config.label,
                                acct.reconnect_attempts,
                                e,
                            );
                            self.spawn_reconnect(account_idx);
                        }
                    }
                }
            }
        }
        if self.phase != Phase::Refreshing {
            self.clear_refresh_watchdog();
        }
        self.revalidate_selection();
    }
}
