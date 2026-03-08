mod actions;
mod apply;
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

}
