use super::{App, BgResult, Lane, Phase};

impl App {
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
        let mailbox_id = msg.mailbox_id.clone();
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
