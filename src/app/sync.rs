use neverlight_mail_core::MailboxHash;

use super::{App, BgResult, Lane, Phase};

impl App {
    pub(super) fn spawn_load_folders(&mut self) {
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

    pub(super) fn spawn_load_messages(&mut self) {
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

    pub(super) fn spawn_load_body(&mut self) {
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
}
