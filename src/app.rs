use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use futures::StreamExt;
use ratatui::prelude::Rect;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use neverlight_mail_core::config::AccountConfig;
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::models::{AttachmentData, Folder, MessageSummary};
use neverlight_mail_core::store::{self, CacheHandle};
use neverlight_mail_core::{EnvelopeHash, FlagOp, MailboxHash, RefreshEventKind};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocolType;
use ratatui_image::thread::{ResizeRequest, ThreadProtocol};

use crate::compose::{self, ComposeField, ComposeMode, ComposeState};

// ---------------------------------------------------------------------------
// Error classification helpers
// ---------------------------------------------------------------------------

fn error_indicates_dead_session(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("timed out")
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

#[allow(dead_code)]
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
        mailbox_hash: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    /// Cached messages arrived (show immediately, IMAP fetch may follow)
    CachedMessages {
        account_idx: usize,
        lane_epoch: u64,
        folder_idx: usize,
        mailbox_hash: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    Body {
        account_idx: usize,
        lane_epoch: u64,
        envelope_hash: u64,
        result: Result<(String, Vec<AttachmentData>), String>,
    },
    /// Flag operation completed (or failed — revert optimistic update)
    FlagOp {
        account_idx: usize,
        lane_epoch: u64,
        envelope_hash: u64,
        /// Original flags to restore on failure
        was_read: bool,
        was_starred: bool,
        result: Result<(), String>,
    },
    /// Move operation completed (or failed — revert optimistic removal)
    MoveOp {
        account_idx: usize,
        lane_epoch: u64,
        envelope_hash: u64,
        source_mailbox_hash: u64,
        destination_name: String,
        reconciled_source_toc: Option<Vec<MessageSummary>>,
        retryable: bool,
        postcondition_failed: bool,
        /// Original message + index for reinsertion on failure
        message: Box<Option<(usize, MessageSummary)>>,
        result: Result<(), String>,
    },
    SearchResults {
        account_idx: usize,
        lane_epoch: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    SendResult(Result<(), String>),
    /// IDLE event: server notified of new/changed/removed messages
    ImapEvent {
        account_idx: usize,
        watch_generation: u64,
        mailbox_hash: u64,
        kind: ImapEventKind,
    },
    /// Watch stream ended or errored
    WatchEnded {
        account_idx: usize,
        watch_generation: u64,
        error: Option<String>,
    },
    /// Reconnect attempt completed
    Reconnected {
        account_idx: usize,
        result: Result<Arc<ImapSession>, String>,
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
    pub phase: Phase,
    pub search_active: bool,
    pub search_query: String,
    pub compose: Option<ComposeState>,
    /// Thread IDs that are collapsed (children hidden)
    pub collapsed_threads: HashSet<u64>,
    /// Maps visible row index → actual index in self.messages
    pub visible_indices: Vec<usize>,
    /// Number of messages per thread_id
    pub thread_sizes: HashMap<u64, usize>,
    pub bg_rx: mpsc::UnboundedReceiver<BgResult>,
    bg_tx: mpsc::UnboundedSender<BgResult>,
    /// Layout rects set by ui::render each frame, used for mouse hit-testing.
    pub layout_rects: LayoutRects,
    /// Terminal image protocol picker (sixel/kitty/halfblocks).
    picker: Option<Picker>,
    /// Picker-selected protocol for runtime diagnostics.
    pub picker_protocol: Option<ProtocolType>,
    /// Image protocols for inline image attachments in the current message body.
    pub image_protos: Vec<ThreadProtocol>,
    /// Index of the currently displayed image in the carousel.
    pub image_index: usize,
    /// Channel for image resize requests from ThreadProtocol.
    pub img_resize_rx: mpsc::UnboundedReceiver<ResizeRequest>,
    img_resize_tx: mpsc::UnboundedSender<ResizeRequest>,
    /// Attachment summary for the current message (filename, mime_type, size).
    pub attachment_info: Vec<(String, String, usize)>,
    lane_epochs: LaneEpochs,
    lane_tasks: LaneTasks,
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
enum Lane {
    Folder,
    Message,
    Search,
    Flag,
    Mutation,
}

#[derive(Default)]
struct LaneEpochs {
    folder: u64,
    message: u64,
    search: u64,
    flag: u64,
    mutation: u64,
}

#[derive(Default)]
struct LaneTasks {
    folder: Vec<JoinHandle<()>>,
    message: Option<JoinHandle<()>>,
    search: Option<JoinHandle<()>>,
    flag: Option<JoinHandle<()>>,
    mutation: Option<JoinHandle<()>>,
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
    pub postcondition_failure_count: u64,
    pub refresh_stuck_count: u64,
    pub refresh_timeout_count: u64,
    next_op_id: u64,
    refresh_started_at: Option<Instant>,
    refresh_stuck_reported: bool,
    refresh_timeout_reported: bool,
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

    pub async fn with_accounts(accounts: Vec<AccountConfig>) -> anyhow::Result<Self> {
        let cache = CacheHandle::open("tui").map_err(|e| anyhow::anyhow!("{e}"))?;
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let (img_tx, img_rx) = mpsc::unbounded_channel();

        let mut account_states = Vec::new();
        for config in accounts {
            let imap_config = config.to_imap_config();
            let session = ImapSession::connect(imap_config).await.ok();
            account_states.push(AccountState {
                config,
                session,
                folders: Vec::new(),
                folder_map: HashMap::new(),
                reconnect_attempts: 0,
                last_error: None,
                watch_generation: 0,
            });
        }

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
            status: "Connecting…".into(),
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
            img_resize_rx: img_rx,
            img_resize_tx: img_tx,
            attachment_info: Vec::new(),
            lane_epochs: LaneEpochs::default(),
            lane_tasks: LaneTasks::default(),
            diagnostics: Diagnostics::default(),
        };

        app.spawn_load_folders();
        app.spawn_watchers();
        Ok(app)
    }

    fn spawn_watchers(&mut self) {
        for idx in 0..self.accounts.len() {
            self.spawn_watcher_for(idx);
        }
    }

    fn spawn_watcher_for(&mut self, idx: usize) {
        let session = match &self.accounts[idx].session {
            Some(s) => Arc::clone(s),
            None => return,
        };
        self.accounts[idx].watch_generation =
            self.accounts[idx].watch_generation.saturating_add(1);
        let generation = self.accounts[idx].watch_generation;
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let stream = match session.watch().await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(BgResult::WatchEnded {
                        account_idx: idx,
                        watch_generation: generation,
                        error: Some(e),
                    });
                    return;
                }
            };
            futures::pin_mut!(stream);
            while let Some(event) = stream.next().await {
                match event {
                    Ok(neverlight_mail_core::BackendEvent::Refresh(rev)) => {
                        let mailbox_hash = rev.mailbox_hash.0;
                        let kind = match rev.kind {
                            RefreshEventKind::Create(_) => ImapEventKind::NewMail,
                            RefreshEventKind::Remove(hash) => ImapEventKind::Remove(hash.0),
                            RefreshEventKind::Rescan => ImapEventKind::Rescan,
                            _ => continue,
                        };
                        let _ = tx.send(BgResult::ImapEvent {
                            account_idx: idx,
                            watch_generation: generation,
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
                watch_generation: generation,
                error: None,
            });
        });
    }

    fn drop_session_and_reconnect(&mut self, account_idx: usize, reason: &str) {
        let acct = &mut self.accounts[account_idx];
        log::warn!(
            "Dropping session for '{}' (reason: {reason})",
            acct.config.label,
        );
        acct.session = None;
        acct.last_error = Some(format!("Session lost: {reason}"));
        acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
        self.spawn_reconnect(account_idx);
    }

    fn spawn_reconnect(&self, account_idx: usize) {
        let acct = &self.accounts[account_idx];
        let delay = acct.reconnect_backoff();
        let config = acct.config.clone();
        let tx = self.bg_tx.clone();
        log::info!(
            "Scheduling reconnect for '{}' in {}s (attempt {})",
            config.label,
            delay.as_secs(),
            acct.reconnect_attempts,
        );
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let imap_config = config.to_imap_config();
            let result = ImapSession::connect(imap_config).await;
            let _ = tx.send(BgResult::Reconnected {
                account_idx,
                result,
            });
        });
    }

    // -----------------------------------------------------------------------
    // Threading
    // -----------------------------------------------------------------------

    pub fn recompute_visible(&mut self) {
        let (sizes, visible) =
            crate::threading::compute_visible(&self.messages, &self.collapsed_threads);
        self.thread_sizes = sizes;
        self.visible_indices = visible;
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

    fn bump_lane_epoch(&mut self, lane: Lane) -> u64 {
        let slot = match lane {
            Lane::Folder => &mut self.lane_epochs.folder,
            Lane::Message => &mut self.lane_epochs.message,
            Lane::Search => &mut self.lane_epochs.search,
            Lane::Flag => &mut self.lane_epochs.flag,
            Lane::Mutation => &mut self.lane_epochs.mutation,
        };
        *slot = slot.saturating_add(1);
        *slot
    }

    fn lane_epoch(&self, lane: Lane) -> u64 {
        match lane {
            Lane::Folder => self.lane_epochs.folder,
            Lane::Message => self.lane_epochs.message,
            Lane::Search => self.lane_epochs.search,
            Lane::Flag => self.lane_epochs.flag,
            Lane::Mutation => self.lane_epochs.mutation,
        }
    }

    fn lane_name(lane: Lane) -> &'static str {
        match lane {
            Lane::Folder => "folder",
            Lane::Message => "message",
            Lane::Search => "search",
            Lane::Flag => "flag",
            Lane::Mutation => "mutation",
        }
    }

    fn set_current_op_id(&mut self, lane: Lane, op_id: u64) {
        match lane {
            Lane::Folder => self.diagnostics.current_op_ids.folder = op_id,
            Lane::Message => self.diagnostics.current_op_ids.message = op_id,
            Lane::Search => self.diagnostics.current_op_ids.search = op_id,
            Lane::Flag => self.diagnostics.current_op_ids.flag = op_id,
            Lane::Mutation => self.diagnostics.current_op_ids.mutation = op_id,
        }
    }

    fn begin_refresh_watchdog(&mut self) {
        self.diagnostics.refresh_started_at = Some(Instant::now());
        self.diagnostics.refresh_stuck_reported = false;
        self.diagnostics.refresh_timeout_reported = false;
    }

    fn clear_refresh_watchdog(&mut self) {
        self.diagnostics.refresh_started_at = None;
        self.diagnostics.refresh_stuck_reported = false;
        self.diagnostics.refresh_timeout_reported = false;
    }

    fn check_refresh_watchdog(&mut self) {
        const REFRESH_STUCK_AFTER: Duration = Duration::from_secs(10);
        const REFRESH_TIMEOUT_AFTER: Duration = Duration::from_secs(20);
        if self.phase != Phase::Refreshing {
            return;
        }
        let Some(started) = self.diagnostics.refresh_started_at else {
            return;
        };
        let elapsed = started.elapsed();
        if elapsed >= REFRESH_STUCK_AFTER && !self.diagnostics.refresh_stuck_reported {
            self.diagnostics.refresh_stuck_count =
                self.diagnostics.refresh_stuck_count.saturating_add(1);
            self.diagnostics.refresh_stuck_reported = true;
            log::warn!(
                "refresh-stuck count={}",
                self.diagnostics.refresh_stuck_count
            );
        }
        if elapsed >= REFRESH_TIMEOUT_AFTER && !self.diagnostics.refresh_timeout_reported {
            self.diagnostics.refresh_timeout_count =
                self.diagnostics.refresh_timeout_count.saturating_add(1);
            self.diagnostics.refresh_timeout_reported = true;
            log::warn!(
                "refresh-timeout count={}",
                self.diagnostics.refresh_timeout_count
            );
        }
    }

    fn cancel_lane(&mut self, lane: Lane) {
        match lane {
            Lane::Folder => {
                for handle in self.lane_tasks.folder.drain(..) {
                    handle.abort();
                }
            }
            Lane::Message => {
                if let Some(handle) = self.lane_tasks.message.take() {
                    handle.abort();
                }
            }
            Lane::Search => {
                if let Some(handle) = self.lane_tasks.search.take() {
                    handle.abort();
                }
            }
            Lane::Flag => {
                if let Some(handle) = self.lane_tasks.flag.take() {
                    handle.abort();
                }
            }
            Lane::Mutation => {
                if let Some(handle) = self.lane_tasks.mutation.take() {
                    handle.abort();
                }
            }
        }
    }

    fn start_lane(&mut self, lane: Lane) -> u64 {
        self.cancel_lane(lane);
        self.diagnostics.next_op_id = self.diagnostics.next_op_id.saturating_add(1);
        let op_id = self.diagnostics.next_op_id;
        self.set_current_op_id(lane, op_id);
        let epoch = self.bump_lane_epoch(lane);
        log::debug!(
            "lane-start lane={} epoch={} op_id={}",
            Self::lane_name(lane),
            epoch,
            op_id
        );
        epoch
    }

    fn register_lane_task(&mut self, lane: Lane, handle: JoinHandle<()>) {
        match lane {
            Lane::Folder => self.lane_tasks.folder.push(handle),
            Lane::Message => self.lane_tasks.message = Some(handle),
            Lane::Search => self.lane_tasks.search = Some(handle),
            Lane::Flag => self.lane_tasks.flag = Some(handle),
            Lane::Mutation => self.lane_tasks.mutation = Some(handle),
        }
    }

    fn current_selected_envelope_hash(&self) -> Option<u64> {
        self.messages
            .get(self.selected_message)
            .map(|m| m.envelope_hash)
    }

    fn revalidate_selection(&mut self) {
        let folder_len = self.active().folders.len();
        if folder_len == 0 {
            self.selected_folder = 0;
            self.messages.clear();
            self.selected_message = 0;
            self.body_text = None;
            self.collapsed_threads.clear();
            self.visible_indices.clear();
            self.thread_sizes.clear();
            return;
        }

        if self.selected_folder >= folder_len {
            self.selected_folder = folder_len - 1;
        }

        if self.messages.is_empty() {
            self.selected_message = 0;
            self.body_text = None;
            self.attachment_info.clear();
            self.image_protos.clear();
            self.image_index = 0;
            self.collapsed_threads.clear();
            self.visible_indices.clear();
            self.thread_sizes.clear();
            return;
        }

        if self.selected_message >= self.messages.len() {
            self.selected_message = self.messages.len() - 1;
        }

        self.recompute_visible();
        if !self.visible_indices.is_empty()
            && !self.visible_indices.contains(&self.selected_message)
        {
            self.selected_message = self.visible_indices[0];
        }
    }

    // -----------------------------------------------------------------------
    // Channel interface (called from main loop)
    // -----------------------------------------------------------------------

    pub fn set_picker(&mut self, picker: Picker) {
        self.picker_protocol = Some(picker.protocol_type());
        self.picker = Some(picker);
    }

    fn protocol_type_name(protocol_type: ProtocolType) -> &'static str {
        match protocol_type {
            ProtocolType::Halfblocks => "halfblocks",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Kitty => "kitty",
            ProtocolType::Iterm2 => "iterm2",
        }
    }

    fn stateful_protocol_name(protocol_type: &StatefulProtocolType) -> &'static str {
        match protocol_type {
            StatefulProtocolType::Halfblocks(_) => "halfblocks",
            StatefulProtocolType::Sixel(_) => "sixel",
            StatefulProtocolType::Kitty(_) => "kitty",
            StatefulProtocolType::ITerm2(_) => "iterm2",
        }
    }

    pub fn image_protocol_label(&self) -> String {
        let picker = self
            .picker_protocol
            .map(Self::protocol_type_name)
            .unwrap_or("none");
        let render = match self.image_protos.get(self.image_index) {
            Some(proto) => proto
                .protocol_type()
                .map(Self::stateful_protocol_name)
                .unwrap_or("pending"),
            None => "none",
        };
        format!("picker:{picker} render:{render}")
    }

    /// Apply a completed image resize (from ThreadProtocol background work).
    pub fn apply_image_resize(&mut self, request: ResizeRequest) {
        if let Ok(resized) = request.resize_encode() {
            if let Some(proto) = self.image_protos.get_mut(self.image_index) {
                proto.update_resized_protocol(resized);
            }
        }
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
                        acct.rebuild_folder_map();
                        if !acct.folders.is_empty() {
                            self.spawn_load_messages();
                        }
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        self.status = format!("Folder error: {e}");
                        log::error!("Folder sync failed for '{}': {e} — dropping session", self.accounts[account_idx].config.label);
                        let acct = &mut self.accounts[account_idx];
                        acct.last_error = Some(e);
                        acct.session = None;
                        acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
                        self.spawn_reconnect(account_idx);
                    }
                }
            }
            BgResult::Messages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_hash,
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
                    .map(|f| f.mailbox_hash)
                    != Some(mailbox_hash)
                {
                    self.diagnostics.toc_drift_count =
                        self.diagnostics.toc_drift_count.saturating_add(1);
                    return;
                }
                match result {
                    Ok(mut msgs) => {
                        msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        let name = &self.active().folders[folder_idx].name;
                        self.status = format!("{name} — {} messages", msgs.len());
                        self.phase = Phase::Idle;
                        // Reconcile sidebar unread count from actual message flags
                        let unread = msgs.iter().filter(|m| !m.is_read).count() as u32;
                        if let Some(folder) = self.accounts[account_idx]
                            .folders
                            .get_mut(folder_idx)
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
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        self.status = format!("Fetch error: {e}");
                        log::error!("Message sync failed for '{}': {e} — dropping session", self.accounts[account_idx].config.label);
                        let acct = &mut self.accounts[account_idx];
                        acct.last_error = Some(e);
                        acct.session = None;
                        acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
                        self.spawn_reconnect(account_idx);
                    }
                }
            }
            BgResult::CachedMessages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_hash,
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
                    .map(|f| f.mailbox_hash)
                    != Some(mailbox_hash)
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
                account_idx,
                lane_epoch,
                envelope_hash,
                result,
            } => {
                if account_idx != self.active_account
                    || lane_epoch != self.lane_epoch(Lane::Message)
                    || self.current_selected_envelope_hash() != Some(envelope_hash)
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
                            .find(|m| m.envelope_hash == envelope_hash)
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
                                envelope_hash,
                            );
                            if let Some(pos) = self
                                .messages
                                .iter()
                                .position(|m| m.envelope_hash == envelope_hash)
                            {
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
                            let account_id = self.active_account_id();
                            tokio::spawn(async move {
                                if let Err(e) =
                                    cache.remove_message(account_id, envelope_hash).await
                                {
                                    log::warn!(
                                        "Failed to evict stale message from cache: {e}"
                                    );
                                }
                            });
                            self.body_text = None;
                            self.attachment_info.clear();
                            self.image_protos.clear();
                            self.image_index = 0;
                            self.status =
                                "Message no longer exists on server".into();
                            self.spawn_load_messages();
                            return;
                        }
                        self.phase = Phase::Error;
                        self.body_text = Some(format!("Error: {e}"));
                        self.attachment_info.clear();
                        self.image_protos.clear();
                        self.image_index = 0;
                        self.status = format!("Body error: {e}");
                    }
                }
            }
            BgResult::FlagOp {
                account_idx,
                lane_epoch,
                envelope_hash,
                was_read,
                was_starred,
                result,
            } => {
                if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Flag) {
                    return;
                }
                if let Err(e) = result {
                    self.phase = Phase::Error;
                    if let Some(msg) = self
                        .messages
                        .iter_mut()
                        .find(|m| m.envelope_hash == envelope_hash)
                    {
                        msg.is_read = was_read;
                        msg.is_starred = was_starred;
                    }
                    self.status = format!("Flag error: {e}");
                    if self.accounts[account_idx].session.is_none()
                        || error_indicates_dead_session(&e)
                    {
                        self.drop_session_and_reconnect(account_idx, "flag-failed");
                    }
                }
            }
            BgResult::MoveOp {
                account_idx,
                lane_epoch,
                envelope_hash: _,
                source_mailbox_hash,
                destination_name,
                reconciled_source_toc,
                retryable,
                postcondition_failed,
                message,
                result,
            } => {
                if account_idx != self.active_account
                    || lane_epoch != self.lane_epoch(Lane::Mutation)
                {
                    return;
                }
                match result {
                    Ok(()) => {
                        self.phase = Phase::Idle;
                        self.status = format!("Moved to {destination_name}.");
                    }
                    Err(e) => {
                        self.phase = Phase::Error;
                        if let Some(mut msgs) = reconciled_source_toc {
                            let selected_mailbox_hash = self
                                .active()
                                .folders
                                .get(self.selected_folder)
                                .map(|f| f.mailbox_hash);
                            if selected_mailbox_hash == Some(source_mailbox_hash) {
                                msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                                self.messages = msgs;
                                self.selected_message = 0;
                                self.body_text = None;
                                self.collapsed_threads.clear();
                            }
                        } else if let Some((idx, msg)) = *message {
                            let insert_at = idx.min(self.messages.len());
                            self.messages.insert(insert_at, msg);
                        }

                        if postcondition_failed {
                            self.diagnostics.postcondition_failure_count = self
                                .diagnostics
                                .postcondition_failure_count
                                .saturating_add(1);
                        }
                        if retryable {
                            self.status = format!("Move error (retryable): {e}");
                        } else {
                            self.status = format!("Move error: {e}");
                        }
                        if self.accounts[account_idx].session.is_none()
                            || error_indicates_dead_session(&e)
                        {
                            self.drop_session_and_reconnect(account_idx, "move-failed");
                        }
                    }
                }
            }
            BgResult::SearchResults {
                account_idx,
                lane_epoch,
                result,
            } => {
                if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Search)
                {
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
            BgResult::ImapEvent {
                account_idx,
                watch_generation,
                mailbox_hash,
                kind,
            } => {
                // Stale watcher — ignore events from a superseded watch stream.
                if let Some(acct) = self.accounts.get(account_idx) {
                    if watch_generation != acct.watch_generation {
                        log::debug!(
                            "Ignoring stale ImapEvent for '{}' (gen {} != current {})",
                            acct.config.label,
                            watch_generation,
                            acct.watch_generation,
                        );
                        return;
                    }
                }
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
                            self.phase = Phase::Refreshing;
                            self.begin_refresh_watchdog();
                            self.status = "New mail — refreshing…".into();
                            self.spawn_load_messages();
                        }
                    }
                    ImapEventKind::Remove(envelope_hash) => {
                        if is_active && current_mbox == Some(mailbox_hash) {
                            self.messages.retain(|m| m.envelope_hash != envelope_hash);
                            if self.selected_message >= self.messages.len()
                                && !self.messages.is_empty()
                            {
                                self.selected_message = self.messages.len() - 1;
                            }
                        }
                    }
                }
            }
            BgResult::WatchEnded { account_idx, watch_generation, error } => {
                if let Some(acct) = self.accounts.get_mut(account_idx) {
                    // Stale watcher — a newer watcher has been spawned since this one started.
                    if watch_generation != acct.watch_generation {
                        log::debug!(
                            "Ignoring stale WatchEnded for '{}' (gen {} != current {})",
                            acct.config.label,
                            watch_generation,
                            acct.watch_generation,
                        );
                        return;
                    }
                    match &error {
                        Some(e) => log::warn!("Watch ended for '{}': {e}", acct.config.label),
                        None => log::info!("Watch stream ended for '{}'", acct.config.label),
                    }
                    let msg = error.unwrap_or_else(|| "Connection lost".into());
                    acct.last_error = Some(msg);
                    acct.session = None;
                    acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
                    self.spawn_reconnect(account_idx);
                }
            }
            BgResult::Reconnected { account_idx, result } => {
                match result {
                    Ok(session) => {
                        if let Some(acct) = self.accounts.get_mut(account_idx) {
                            // If already connected (a prior reconnect won the race), drop
                            // this duplicate session silently.
                            if acct.session.is_some() {
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
                            acct.session = Some(session);
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

    // -----------------------------------------------------------------------
    // Spawn background IMAP tasks
    // -----------------------------------------------------------------------

    fn spawn_load_folders(&mut self) {
        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Folder);
        self.phase = Phase::Loading;
        let tx = self.bg_tx.clone();
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let handle = tokio::spawn(async move {
            let result = session.fetch_folders().await.map_err(|e| e.to_string());
            if let Ok(ref folders) = result {
                let _ = cache.save_folders(account_id, folders.clone()).await;
            }
            let _ = tx.send(BgResult::Folders {
                account_idx,
                lane_epoch,
                result,
            });
        });
        self.register_lane_task(Lane::Folder, handle);
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
        let folder_name = folder.name.clone();
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Folder);
        self.phase = if self.messages.is_empty() {
            Phase::Loading
        } else {
            Phase::Refreshing
        };
        if self.phase == Phase::Refreshing {
            self.begin_refresh_watchdog();
        } else {
            self.clear_refresh_watchdog();
        }
        self.status = format!("Loading {folder_name}…");
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
        let cache_handle = tokio::spawn(async move {
            let result = cache2
                .load_messages(account_id2, mbox_hash_raw, 200, 0)
                .await;
            let _ = tx2.send(BgResult::CachedMessages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_hash: mbox_hash_raw,
                result,
            });
        });
        self.register_lane_task(Lane::Folder, cache_handle);

        // IMAP fetch (authoritative, overwrites cache)
        let imap_handle = tokio::spawn(async move {
            let result = session
                .fetch_messages(mbox_hash)
                .await
                .map_err(|e| e.to_string());
            if let Ok(ref msgs) = result {
                let _ = cache
                    .save_messages(account_id, mbox_hash_raw, msgs.clone())
                    .await;
            }
            let _ = tx.send(BgResult::Messages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_hash: mbox_hash_raw,
                result,
            });
        });
        self.register_lane_task(Lane::Folder, imap_handle);
    }

    fn spawn_load_body(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &self.messages[self.selected_message];
        let env_hash_raw = msg.envelope_hash;
        let env_hash = neverlight_mail_core::EnvelopeHash(env_hash_raw);
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Message);
        self.phase = Phase::Loading;
        self.status = "Loading body…".into();

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let tx = self.bg_tx.clone();
        let handle = tokio::spawn(async move {
            // Try cache first
            if let Ok(Some((md_body, plain_body, attachments))) =
                cache.load_body(account_id.clone(), env_hash_raw).await
            {
                let body = if !plain_body.is_empty() {
                    plain_body
                } else {
                    md_body
                };
                let _ = tx.send(BgResult::Body {
                    account_idx,
                    lane_epoch,
                    envelope_hash: env_hash_raw,
                    result: Ok((body, attachments)),
                });
                return;
            }

            // Cache miss — fetch from IMAP
            let result = session.fetch_body(env_hash).await;
            let rendered = result
                .map(|(text_plain, text_html, attachments)| {
                    let rendered = neverlight_mail_core::mime::render_body(
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
                    let account_id2 = account_id.clone();
                    let att_clone = attachments.clone();
                    tokio::spawn(async move {
                        let _ = cache2
                            .save_body(account_id2, env_hash_raw, text_plain, text_html, att_clone)
                            .await;
                    });
                    (rendered, attachments)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Body {
                account_idx,
                lane_epoch,
                envelope_hash: env_hash_raw,
                result: rendered,
            });
        });
        self.register_lane_task(Lane::Message, handle);
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
            FlagOp::Set(neverlight_mail_core::Flag::SEEN)
        } else {
            FlagOp::UnSet(neverlight_mail_core::Flag::SEEN)
        };

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Flag);
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let tx = self.bg_tx.clone();
        let handle = tokio::spawn(async move {
            // Update cache optimistically
            let flags = store::flags_to_u8(new_read, was_starred);
            let op = if new_read { "mark-read" } else { "mark-unread" };
            let _ = cache
                .update_flags(account_id.clone(), envelope_hash, flags, op.to_string())
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
                let _ = cache
                    .clear_pending_op(account_id.clone(), envelope_hash, flags)
                    .await;
            } else {
                let _ = cache
                    .revert_pending_op(account_id.clone(), envelope_hash)
                    .await;
            }

            let _ = tx.send(BgResult::FlagOp {
                account_idx,
                lane_epoch,
                envelope_hash,
                was_read,
                was_starred,
                result,
            });
        });
        self.register_lane_task(Lane::Flag, handle);
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
            FlagOp::Set(neverlight_mail_core::Flag::FLAGGED)
        } else {
            FlagOp::UnSet(neverlight_mail_core::Flag::FLAGGED)
        };

        let session = match self.active_session() {
            Some(s) => s,
            None => return,
        };
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Flag);
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let tx = self.bg_tx.clone();
        let handle = tokio::spawn(async move {
            let flags = store::flags_to_u8(was_read, new_starred);
            let op = if new_starred { "star" } else { "unstar" };
            let _ = cache
                .update_flags(account_id.clone(), envelope_hash, flags, op.to_string())
                .await;

            let result = session
                .set_flags(
                    EnvelopeHash(envelope_hash),
                    MailboxHash(mailbox_hash),
                    vec![flag_op],
                )
                .await;

            if result.is_ok() {
                let _ = cache
                    .clear_pending_op(account_id.clone(), envelope_hash, flags)
                    .await;
            } else {
                let _ = cache
                    .revert_pending_op(account_id.clone(), envelope_hash)
                    .await;
            }

            let _ = tx.send(BgResult::FlagOp {
                account_idx,
                lane_epoch,
                envelope_hash,
                was_read,
                was_starred,
                result,
            });
        });
        self.register_lane_task(Lane::Flag, handle);
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
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Mutation);
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let tx = self.bg_tx.clone();
        let saved_msg = msg.clone();
        let destination_name = target_name.to_string();
        let handle = tokio::spawn(async move {
            // Cache: mark as pending move
            let _ = cache
                .update_flags(
                    account_id.clone(),
                    envelope_hash,
                    store::flags_to_u8(saved_msg.is_read, saved_msg.is_starred),
                    format!("move:{dest_hash}"),
                )
                .await;

            let move_result = session
                .move_messages(
                    EnvelopeHash(envelope_hash),
                    MailboxHash(source_hash),
                    MailboxHash(dest_hash),
                )
                .await;

            let mut reconciled_source_toc = None;
            let mut retryable = false;
            let mut postcondition_failed = false;
            let result = if move_result.is_ok() {
                match session.fetch_messages(MailboxHash(source_hash)).await {
                    Ok(source_msgs) => {
                        let still_in_source =
                            source_msgs.iter().any(|m| m.envelope_hash == envelope_hash);
                        if still_in_source {
                            retryable = true;
                            postcondition_failed = true;
                            let _ = cache
                                .save_messages(account_id.clone(), source_hash, source_msgs.clone())
                                .await;
                            let _ = cache
                                .revert_pending_op(account_id.clone(), envelope_hash)
                                .await;
                            reconciled_source_toc = Some(source_msgs);
                            Err(
                                "Postcondition failed: source mailbox still contains message"
                                    .into(),
                            )
                        } else {
                            let _ = cache
                                .remove_message(account_id.clone(), envelope_hash)
                                .await;
                            Ok(())
                        }
                    }
                    Err(e) => {
                        retryable = true;
                        let _ = cache
                            .revert_pending_op(account_id.clone(), envelope_hash)
                            .await;
                        Err(format!("Move verification failed: {e}"))
                    }
                }
            } else {
                let _ = cache
                    .revert_pending_op(account_id.clone(), envelope_hash)
                    .await;
                move_result
            };

            let _ = tx.send(BgResult::MoveOp {
                account_idx,
                lane_epoch,
                envelope_hash,
                source_mailbox_hash: source_hash,
                destination_name,
                reconciled_source_toc,
                retryable,
                postcondition_failed,
                message: Box::new(if result.is_err() {
                    Some((idx, saved_msg))
                } else {
                    None
                }),
                result,
            });
        });
        self.register_lane_task(Lane::Mutation, handle);
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    fn spawn_search(&mut self) {
        let query = self.search_query.clone();
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Search);
        self.phase = Phase::Searching;
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let handle = tokio::spawn(async move {
            let result = cache.search(query).await;
            let _ = tx.send(BgResult::SearchResults {
                account_idx,
                lane_epoch,
                result,
            });
        });
        self.register_lane_task(Lane::Search, handle);
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
                    self.phase = Phase::Loading;
                    self.status = format!("Account: {}", self.active().config.label);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if self.focus == Focus::Body && self.image_protos.len() > 1 && self.image_index > 0
                {
                    self.image_index -= 1;
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if self.focus == Focus::Body
                    && self.image_protos.len() > 1
                    && self.image_index + 1 < self.image_protos.len()
                {
                    self.image_index += 1;
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
                    self.phase = Phase::Searching;
                    self.status = format!("Searching: {}…", self.search_query);
                    self.spawn_search();
                    // Exit search input mode so user can navigate results
                    self.search_active = false;
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
        crate::threading::visible_nav(
            &self.visible_indices,
            self.messages.len(),
            self.selected_message,
            direction,
        )
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

    // -------------------------------------------------------------------
    // Mouse handling
    // -------------------------------------------------------------------

    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        if self.compose.is_some() || self.search_active {
            return;
        }

        let lr = &self.layout_rects;
        let in_rect =
            |r: &Rect| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height;

        match kind {
            MouseEventKind::Down(_) => {
                if in_rect(&lr.folders.clone()) {
                    self.focus = Focus::Folders;
                    // Row within the pane (subtract border top)
                    let local_row = row.saturating_sub(lr.folders.y + 1) as usize;
                    if local_row < self.active().folders.len() && local_row != self.selected_folder
                    {
                        self.selected_folder = local_row;
                        self.spawn_load_messages();
                    }
                } else if in_rect(&lr.messages.clone()) {
                    self.focus = Focus::Messages;
                    let local_row = row.saturating_sub(lr.messages.y + 1) as usize;
                    // Map visible row to actual message index
                    let vis = if self.visible_indices.is_empty() {
                        if local_row < self.messages.len() {
                            Some(local_row)
                        } else {
                            None
                        }
                    } else if local_row < self.visible_indices.len() {
                        Some(self.visible_indices[local_row])
                    } else {
                        None
                    };
                    if let Some(idx) = vis {
                        self.selected_message = idx;
                        self.spawn_load_body();
                    }
                } else if in_rect(&lr.body.clone()) {
                    self.focus = Focus::Body;
                }
            }
            MouseEventKind::ScrollUp => {
                if in_rect(&lr.body.clone()) {
                    self.body_scroll = self.body_scroll.saturating_sub(3);
                } else if in_rect(&lr.folders.clone()) {
                    if self.selected_folder > 0 {
                        self.selected_folder -= 1;
                        self.spawn_load_messages();
                    }
                } else if in_rect(&lr.messages.clone()) {
                    if let Some(idx) = self.visible_nav(-1) {
                        self.selected_message = idx;
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if in_rect(&lr.body.clone()) {
                    self.body_scroll = self.body_scroll.saturating_add(3);
                } else if in_rect(&lr.folders.clone()) {
                    if self.selected_folder + 1 < self.active().folders.len() {
                        self.selected_folder += 1;
                        self.spawn_load_messages();
                    }
                } else if in_rect(&lr.messages.clone()) {
                    if let Some(idx) = self.visible_nav(1) {
                        self.selected_message = idx;
                    }
                }
            }
            _ => {}
        }
    }
}
