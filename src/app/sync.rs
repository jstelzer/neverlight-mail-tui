use std::sync::atomic::Ordering;

use super::{App, BgResult, Lane, Phase};

impl App {
    /// Start the background backfill task for a specific account.
    pub(super) fn spawn_backfill(&mut self, account_idx: usize) {
        // Cancel any existing backfill task for this account
        if let Some(handle) = self.backfill_tasks.remove(&account_idx) {
            handle.abort();
        }

        let acct = &self.accounts[account_idx];
        let client = match &acct.client {
            Some(c) => c.clone(),
            None => return,
        };
        let cache = self.cache.clone();
        let account_id = acct.config.id.clone();
        let max_messages = acct.config.max_messages_per_mailbox;
        let pause = acct.backfill_pause.clone();
        let folder_mailbox_ids: Vec<String> =
            acct.folders.iter().map(|f| f.mailbox_id.clone()).collect();
        let tx = self.bg_tx.clone();
        let page_size = neverlight_mail_core::email::DEFAULT_PAGE_SIZE;

        let handle = tokio::spawn(async move {
            let aid = account_id;
            loop {
                // Wait while paused (head sync in progress)
                while pause.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }

                // Get incomplete mailboxes from cache
                let incomplete = match cache.list_backfill_progress(aid.clone()).await {
                    Ok(list) => list,
                    Err(e) => {
                        log::warn!("backfill: failed to list progress: {}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        continue;
                    }
                };

                // Build work list: incomplete + never-started mailboxes
                let has_progress: std::collections::HashSet<String> =
                    incomplete.iter().map(|p| p.mailbox_id.clone()).collect();
                let mut work: Vec<String> =
                    incomplete.into_iter().map(|p| p.mailbox_id).collect();

                for mid in &folder_mailbox_ids {
                    if !has_progress.contains(mid) {
                        let progress = cache
                            .get_backfill_progress(aid.clone(), mid.clone())
                            .await
                            .ok()
                            .flatten();
                        if progress.is_none() {
                            work.push(mid.clone());
                        }
                    }
                }

                if work.is_empty() {
                    let _ = tx.send(BgResult::BackfillComplete {
                        account_idx,
                    });
                    return;
                }

                // Process one batch per mailbox
                for mailbox_id in &work {
                    if pause.load(Ordering::Relaxed) {
                        break;
                    }

                    match neverlight_mail_core::backfill::backfill_batch(
                        &client,
                        &cache,
                        &aid,
                        mailbox_id,
                        page_size,
                        max_messages,
                    )
                    .await
                    {
                        Ok(result) => {
                            let _ = tx.send(BgResult::BackfillProgress {
                                account_idx,
                                mailbox_id: result.mailbox_id,
                                position: result.position,
                                total: result.total,
                                completed: result.completed,
                            });
                        }
                        Err(e) => {
                            log::warn!(
                                "backfill: batch failed for mailbox {}: {}",
                                mailbox_id,
                                e
                            );
                        }
                    }

                    // Throttle between mailboxes
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        });
        self.backfill_tasks.insert(account_idx, handle);
    }

    pub(super) fn spawn_load_folders(&mut self) {
        let client = match self.active_client() {
            Some(c) => c,
            None => return,
        };
        let account_idx = self.active_account;
        let lane_epoch = self.start_lane(Lane::Folder);
        self.phase = Phase::Loading;
        let tx = self.bg_tx.clone();
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let handle = tokio::spawn(async move {
            let result = neverlight_mail_core::sync::sync_mailboxes(&client, &cache, &account_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Folders {
                account_idx,
                lane_epoch,
                result,
            });
        });
        self.register_lane_task(Lane::Folder, handle);
    }

    pub(super) fn spawn_load_messages(&mut self) {
        let acct = &self.accounts[self.active_account];
        if acct.folders.is_empty() {
            return;
        }
        let folder = &acct.folders[self.selected_folder];
        let mailbox_id = folder.mailbox_id.clone();
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

        let client = match self.active_client() {
            Some(c) => c,
            None => return,
        };
        let cache = self.cache.clone();
        let account_id = self.active_account_id();
        let tx = self.bg_tx.clone();

        // Try cache first (fast path)
        let cache2 = cache.clone();
        let account_id2 = account_id.clone();
        let mailbox_id2 = mailbox_id.clone();
        let tx2 = tx.clone();
        let cache_handle = tokio::spawn(async move {
            let result = cache2
                .load_messages(account_id2, mailbox_id2.clone(), 200, 0)
                .await;
            let _ = tx2.send(BgResult::CachedMessages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_id: mailbox_id2,
                result,
            });
        });
        self.register_lane_task(Lane::Folder, cache_handle);

        // JMAP fetch (authoritative — sync_emails handles save + prune + state tokens)
        let jmap_mailbox_id = mailbox_id.clone();
        let jmap_handle = tokio::spawn(async move {
            let result = neverlight_mail_core::sync::sync_emails(
                &client, &cache, &account_id, &jmap_mailbox_id, 200,
            )
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Messages {
                account_idx,
                lane_epoch,
                folder_idx,
                mailbox_id,
                result,
            });
        });
        self.register_lane_task(Lane::Folder, jmap_handle);
    }

    pub(super) fn spawn_load_body(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = match self.messages.get(self.selected_message).cloned() {
            Some(msg) => msg,
            None => {
                log::warn!("spawn_load_body: selected_message {} out of bounds", self.selected_message);
                return;
            }
        };
        let email_id = msg.email_id.clone();
        let mailbox_id = msg.context_mailbox_id.clone();
        let (account_idx, account_id) = match self.account_for_message(&msg) {
            Some(account) => account,
            None => {
                log::warn!("spawn_load_body: no account for message {}", email_id);
                return;
            }
        };
        let lane_epoch = self.start_lane(Lane::Message);
        self.phase = Phase::Loading;
        self.status = "Loading body…".into();

        let client = match self.accounts[account_idx].client.clone() {
            Some(c) => c,
            None => {
                log::warn!("spawn_load_body: no client for account {} (reconnecting?)", account_id);
                self.status = "No connection — waiting for reconnect…".into();
                return;
            }
        };
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let handle = tokio::spawn(async move {
            // Try cache first
            if let Ok(Some((_md_body, plain_body, attachments))) =
                cache.load_body(account_id.clone(), email_id.clone()).await
            {
                let body = if !plain_body.is_empty() {
                    plain_body
                } else {
                    _md_body
                };
                let _ = tx.send(BgResult::Body {
                    account_id: account_id.clone(),
                    lane_epoch,
                    mailbox_id,
                    email_id,
                    result: Ok((body, attachments)),
                });
                return;
            }

            // Cache miss — fetch from JMAP
            let result = neverlight_mail_core::email::get_body(&client, &email_id).await;
            let rendered = result
                .map(|(markdown_body, plain_body, attachments)| {
                    // Save to cache (fire-and-forget)
                    let cache2 = cache.clone();
                    let account_id2 = account_id.clone();
                    let email_id2 = email_id.clone();
                    let md_clone = markdown_body.clone();
                    let plain_clone = plain_body.clone();
                    let att_clone = attachments.clone();
                    tokio::spawn(async move {
                        let _ = cache2
                            .save_body(account_id2, email_id2, md_clone, plain_clone, att_clone)
                            .await;
                    });
                    let body = if !plain_body.is_empty() {
                        plain_body
                    } else {
                        markdown_body
                    };
                    (body, attachments)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Body {
                account_id,
                lane_epoch,
                mailbox_id,
                email_id,
                result: rendered,
            });
        });
        self.register_lane_task(Lane::Message, handle);
    }
}
