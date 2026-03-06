use neverlight_mail_core::store::{self};
use neverlight_mail_core::{EnvelopeHash, FlagOp, MailboxHash};

use super::{App, BgResult, Lane};

impl App {
    pub(super) fn toggle_read(&mut self) {
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

    pub(super) fn toggle_star(&mut self) {
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

    pub(super) fn move_to_folder(&mut self, target_name: &str) {
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
}
