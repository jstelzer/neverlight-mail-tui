use std::time::Duration;

use super::{App, Lane, Phase};

impl App {
    pub(super) fn bump_lane_epoch(&mut self, lane: Lane) -> u64 {
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

    pub(super) fn lane_epoch(&self, lane: Lane) -> u64 {
        match lane {
            Lane::Folder => self.lane_epochs.folder,
            Lane::Message => self.lane_epochs.message,
            Lane::Search => self.lane_epochs.search,
            Lane::Flag => self.lane_epochs.flag,
            Lane::Mutation => self.lane_epochs.mutation,
        }
    }

    pub(super) fn lane_name(lane: Lane) -> &'static str {
        match lane {
            Lane::Folder => "folder",
            Lane::Message => "message",
            Lane::Search => "search",
            Lane::Flag => "flag",
            Lane::Mutation => "mutation",
        }
    }

    pub(super) fn set_current_op_id(&mut self, lane: Lane, op_id: u64) {
        match lane {
            Lane::Folder => self.diagnostics.current_op_ids.folder = op_id,
            Lane::Message => self.diagnostics.current_op_ids.message = op_id,
            Lane::Search => self.diagnostics.current_op_ids.search = op_id,
            Lane::Flag => self.diagnostics.current_op_ids.flag = op_id,
            Lane::Mutation => self.diagnostics.current_op_ids.mutation = op_id,
        }
    }

    pub(super) fn begin_refresh_watchdog(&mut self) {
        self.diagnostics.refresh_started_at = Some(std::time::Instant::now());
        self.diagnostics.refresh_stuck_reported = false;
        self.diagnostics.refresh_timeout_reported = false;
    }

    pub(super) fn clear_refresh_watchdog(&mut self) {
        self.diagnostics.refresh_started_at = None;
        self.diagnostics.refresh_stuck_reported = false;
        self.diagnostics.refresh_timeout_reported = false;
    }

    pub(super) fn check_refresh_watchdog(&mut self) {
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

    pub(super) fn cancel_lane(&mut self, lane: Lane) {
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

    pub(super) fn start_lane(&mut self, lane: Lane) -> u64 {
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

    pub(super) fn register_lane_task(&mut self, lane: Lane, handle: tokio::task::JoinHandle<()>) {
        match lane {
            Lane::Folder => self.lane_tasks.folder.push(handle),
            Lane::Message => self.lane_tasks.message = Some(handle),
            Lane::Search => self.lane_tasks.search = Some(handle),
            Lane::Flag => self.lane_tasks.flag = Some(handle),
            Lane::Mutation => self.lane_tasks.mutation = Some(handle),
        }
    }

    pub(super) fn account_lane_epoch(&self, account_id: &str, lane: Lane) -> u64 {
        match lane {
            Lane::Flag => *self.account_lane_epochs.flag.get(account_id).unwrap_or(&0),
            Lane::Mutation => *self
                .account_lane_epochs
                .mutation
                .get(account_id)
                .unwrap_or(&0),
            _ => self.lane_epoch(lane),
        }
    }

    pub(super) fn start_account_lane(&mut self, account_id: &str, lane: Lane) -> u64 {
        match lane {
            Lane::Flag => {
                if let Some(handle) = self.account_lane_tasks.flag.remove(account_id) {
                    handle.abort();
                }
                self.diagnostics.next_op_id = self.diagnostics.next_op_id.saturating_add(1);
                self.set_current_op_id(Lane::Flag, self.diagnostics.next_op_id);
                let slot = self
                    .account_lane_epochs
                    .flag
                    .entry(account_id.to_string())
                    .or_insert(0);
                *slot = slot.saturating_add(1);
                *slot
            }
            Lane::Mutation => {
                if let Some(handle) = self.account_lane_tasks.mutation.remove(account_id) {
                    handle.abort();
                }
                self.diagnostics.next_op_id = self.diagnostics.next_op_id.saturating_add(1);
                self.set_current_op_id(Lane::Mutation, self.diagnostics.next_op_id);
                let slot = self
                    .account_lane_epochs
                    .mutation
                    .entry(account_id.to_string())
                    .or_insert(0);
                *slot = slot.saturating_add(1);
                *slot
            }
            _ => self.start_lane(lane),
        }
    }

    pub(super) fn register_account_lane_task(
        &mut self,
        account_id: &str,
        lane: Lane,
        handle: tokio::task::JoinHandle<()>,
    ) {
        match lane {
            Lane::Flag => {
                self.account_lane_tasks
                    .flag
                    .insert(account_id.to_string(), handle);
            }
            Lane::Mutation => {
                self.account_lane_tasks
                    .mutation
                    .insert(account_id.to_string(), handle);
            }
            _ => self.register_lane_task(lane, handle),
        }
    }

    pub(super) fn revalidate_selection(&mut self) {
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
}
