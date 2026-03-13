//! BgResult apply dispatcher — maps background task results to state updates.

use std::sync::atomic::Ordering;

use super::{body_error_indicates_stale_message, error_indicates_dead_session, App, BgResult, Focus, Lane, Phase};

impl App {
    /// Apply a background result to app state.
    pub fn apply(&mut self, result: BgResult) {
        self.check_refresh_watchdog();
        match result {
            BgResult::Folders {
                account_idx,
                lane_epoch,
                result,
            } => self.apply_folders(account_idx, lane_epoch, result),
            BgResult::Messages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_id,
                result,
            } => self.apply_messages(account_idx, lane_epoch, folder_idx, mailbox_id, result),
            BgResult::CachedMessages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_id,
                result,
            } => self.apply_cached_messages(account_idx, lane_epoch, folder_idx, mailbox_id, result),
            BgResult::Body {
                account_id,
                lane_epoch,
                mailbox_id,
                email_id,
                result,
            } => self.apply_body(account_id, lane_epoch, mailbox_id, email_id, result),
            BgResult::FlagOp {
                account_id,
                lane_epoch,
                email_id,
                was_read,
                was_starred,
                result,
            } => self.apply_flag_op(account_id, lane_epoch, email_id, was_read, was_starred, result),
            BgResult::MoveOp {
                account_id,
                lane_epoch,
                destination_name,
                message,
                result,
            } => self.apply_move_op(account_id, lane_epoch, destination_name, message, result),
            BgResult::SearchResults {
                lane_epoch,
                result,
            } => self.apply_search_results(lane_epoch, result),
            BgResult::SendResult(result) => self.apply_send_result(result),
            BgResult::PushStateChanged {
                account_idx,
                watch_generation,
            } => self.apply_push_state_changed(account_idx, watch_generation),
            BgResult::PushEnded {
                account_idx,
                watch_generation,
                error,
            } => self.apply_push_ended(account_idx, watch_generation, error),
            BgResult::PushRetry { account_idx } => self.apply_push_retry(account_idx),
            BgResult::Reconnected {
                account_idx,
                result,
            } => self.apply_reconnected(account_idx, result),
            BgResult::BackfillProgress {
                account_idx,
                mailbox_id,
                position,
                total,
                completed,
            } => self.apply_backfill_progress(account_idx, mailbox_id, position, total, completed),
            BgResult::BackfillComplete { account_idx } => self.apply_backfill_complete(account_idx),
        }
        if self.phase != Phase::Refreshing {
            self.clear_refresh_watchdog();
        }
        self.revalidate_selection();
    }

    fn apply_folders(
        &mut self,
        account_idx: usize,
        lane_epoch: u64,
        result: Result<Vec<neverlight_mail_core::models::Folder>, String>,
    ) {
        if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Folder) {
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
                    // Activate backfill after initial folder sync
                    if !acct.backfill_active && acct.client.is_some() {
                        acct.backfill_active = true;
                        self.spawn_backfill(account_idx);
                    }
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

    fn apply_messages(
        &mut self,
        account_idx: usize,
        lane_epoch: u64,
        folder_idx: usize,
        mailbox_id: String,
        result: Result<Vec<neverlight_mail_core::models::MessageSummary>, String>,
    ) {
        if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Folder) {
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
                // Unpause backfill after head sync completes
                if let Some(acct) = self.accounts.get(account_idx) {
                    acct.backfill_pause.store(false, Ordering::Relaxed);
                }
                // Reconcile sidebar unread count from actual message flags
                let unread = msgs.iter().filter(|m| !m.is_read).count() as u32;
                if let Some(folder) = self.accounts[account_idx].folders.get_mut(folder_idx) {
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

    fn apply_cached_messages(
        &mut self,
        account_idx: usize,
        lane_epoch: u64,
        folder_idx: usize,
        mailbox_id: String,
        result: Result<Vec<neverlight_mail_core::models::MessageSummary>, String>,
    ) {
        if account_idx != self.active_account || lane_epoch != self.lane_epoch(Lane::Folder) {
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

    fn apply_body(
        &mut self,
        account_id: String,
        lane_epoch: u64,
        mailbox_id: String,
        email_id: String,
        result: Result<(String, Vec<neverlight_mail_core::models::AttachmentData>), String>,
    ) {
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
                                self.image_protos.push(ratatui_image::thread::ThreadProtocol::new(
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

    fn apply_flag_op(
        &mut self,
        account_id: String,
        lane_epoch: u64,
        email_id: String,
        was_read: bool,
        was_starred: bool,
        result: Result<(), String>,
    ) {
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

    fn apply_move_op(
        &mut self,
        account_id: String,
        lane_epoch: u64,
        destination_name: String,
        message: Box<Option<(usize, neverlight_mail_core::models::MessageSummary)>>,
        result: Result<(), String>,
    ) {
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

    fn apply_search_results(
        &mut self,
        lane_epoch: u64,
        result: Result<Vec<neverlight_mail_core::models::MessageSummary>, String>,
    ) {
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

    fn apply_send_result(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.phase = Phase::Idle;
                self.compose = None;
                self.status = "Message sent.".into();
            }
            Err(e) => {
                self.phase = Phase::Error;
                self.status = format!("Send error: {e}");
            }
        }
    }

    fn apply_push_state_changed(&mut self, account_idx: usize, watch_generation: u64) {
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
        // Pause backfill during head sync to avoid contention
        if let Some(acct) = self.accounts.get(account_idx) {
            acct.backfill_pause.store(true, Ordering::Relaxed);
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

    fn apply_push_ended(
        &mut self,
        account_idx: usize,
        watch_generation: u64,
        error: Option<String>,
    ) {
        let Some(acct) = self.accounts.get_mut(account_idx) else {
            return;
        };
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

    fn apply_push_retry(&mut self, account_idx: usize) {
        if self.accounts.get(account_idx).and_then(|a| a.client.as_ref()).is_some() {
            log::info!("Re-spawning push watcher for account {}", account_idx);
            self.spawn_watcher_for(account_idx);
        }
    }

    fn apply_backfill_progress(
        &mut self,
        account_idx: usize,
        mailbox_id: String,
        position: u32,
        total: u32,
        completed: bool,
    ) {
        let Some(acct) = self.accounts.get_mut(account_idx) else {
            return;
        };
        if completed {
            acct.backfill_progress.remove(&mailbox_id);
        } else {
            acct.backfill_progress.insert(mailbox_id, (position, total));
        }
    }

    fn apply_backfill_complete(&mut self, account_idx: usize) {
        log::info!("Backfill complete for account {}", account_idx);
        let Some(acct) = self.accounts.get_mut(account_idx) else {
            return;
        };
        acct.backfill_active = false;
        acct.backfill_progress.clear();
    }

    fn apply_reconnected(
        &mut self,
        account_idx: usize,
        result: Result<neverlight_mail_core::client::JmapClient, String>,
    ) {
        self.reconnect_tasks.remove(&account_idx);
        match result {
            Ok(client) => {
                let Some(acct) = self.accounts.get_mut(account_idx) else {
                    return;
                };
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
            Err(e) => {
                let Some(acct) = self.accounts.get_mut(account_idx) else {
                    return;
                };
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
