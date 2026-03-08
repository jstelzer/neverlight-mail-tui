use neverlight_mail_core::store;
use neverlight_mail_core::FlagOp;

use super::{App, BgResult, Lane};

impl App {
    pub(super) fn toggle_read(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let idx = self.selected_message;
        let msg_snapshot = match self.messages.get(idx).cloned() {
            Some(msg) => msg,
            None => return,
        };
        let (account_idx, account_id) = match self.account_for_message(&msg_snapshot) {
            Some(account) => account,
            None => return,
        };

        let msg = &mut self.messages[idx];
        let was_read = msg.is_read;
        let was_starred = msg.is_starred;
        let email_id = msg.email_id.clone();

        // Optimistic toggle
        msg.is_read = !was_read;
        let new_read = msg.is_read;

        let flag_op = FlagOp::SetSeen(new_read);

        let client = match self.accounts[account_idx].client.clone() {
            Some(c) => c,
            None => return,
        };
        let lane_epoch = self.start_account_lane(&account_id, Lane::Flag);
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let op_account_id = account_id.clone();
        let task_account_id = account_id.clone();
        let task_email_id = email_id.clone();
        let handle = tokio::spawn(async move {
            // Update cache optimistically
            let flags = store::flags_to_u8(new_read, was_starred);
            let op = if new_read { "mark-read" } else { "mark-unread" };
            let _ = cache
                .update_flags(
                    task_account_id.clone(),
                    task_email_id.clone(),
                    flags,
                    op.to_string(),
                )
                .await;

            // JMAP sync
            let result = neverlight_mail_core::email::set_flag(&client, &task_email_id, &flag_op)
                .await
                .map_err(|e| e.to_string());

            if result.is_ok() {
                let _ = cache
                    .clear_pending_op(task_account_id.clone(), task_email_id.clone(), flags)
                    .await;
            } else {
                let _ = cache
                    .revert_pending_op(task_account_id.clone(), task_email_id.clone())
                    .await;
            }

            let _ = tx.send(BgResult::FlagOp {
                account_id: op_account_id,
                lane_epoch,
                email_id,
                was_read,
                was_starred,
                result,
            });
        });
        self.register_account_lane_task(&account_id, Lane::Flag, handle);
    }

    pub(super) fn toggle_star(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let idx = self.selected_message;
        let msg_snapshot = match self.messages.get(idx).cloned() {
            Some(msg) => msg,
            None => return,
        };
        let (account_idx, account_id) = match self.account_for_message(&msg_snapshot) {
            Some(account) => account,
            None => return,
        };

        let msg = &mut self.messages[idx];
        let was_read = msg.is_read;
        let was_starred = msg.is_starred;
        let email_id = msg.email_id.clone();

        // Optimistic toggle
        msg.is_starred = !was_starred;
        let new_starred = msg.is_starred;

        let flag_op = FlagOp::SetFlagged(new_starred);

        let client = match self.accounts[account_idx].client.clone() {
            Some(c) => c,
            None => return,
        };
        let lane_epoch = self.start_account_lane(&account_id, Lane::Flag);
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let op_account_id = account_id.clone();
        let task_account_id = account_id.clone();
        let task_email_id = email_id.clone();
        let handle = tokio::spawn(async move {
            let flags = store::flags_to_u8(was_read, new_starred);
            let op = if new_starred { "star" } else { "unstar" };
            let _ = cache
                .update_flags(
                    task_account_id.clone(),
                    task_email_id.clone(),
                    flags,
                    op.to_string(),
                )
                .await;

            let result = neverlight_mail_core::email::set_flag(&client, &task_email_id, &flag_op)
                .await
                .map_err(|e| e.to_string());

            if result.is_ok() {
                let _ = cache
                    .clear_pending_op(task_account_id.clone(), task_email_id.clone(), flags)
                    .await;
            } else {
                let _ = cache
                    .revert_pending_op(task_account_id.clone(), task_email_id.clone())
                    .await;
            }

            let _ = tx.send(BgResult::FlagOp {
                account_id: op_account_id,
                lane_epoch,
                email_id,
                was_read,
                was_starred,
                result,
            });
        });
        self.register_account_lane_task(&account_id, Lane::Flag, handle);
    }

    pub(super) fn move_to_folder(&mut self, target_role: &str) {
        if self.messages.is_empty() {
            return;
        }

        // Find target mailbox by role
        let dest_mailbox_id = neverlight_mail_core::mailbox::find_by_role(
            &self.active().folders,
            target_role,
        );
        let dest_mailbox_id = match dest_mailbox_id {
            Some(id) => id,
            None => {
                self.status = format!("No {target_role} folder found");
                return;
            }
        };

        let idx = self.selected_message;
        let msg_snapshot = match self.messages.get(idx).cloned() {
            Some(msg) => msg,
            None => return,
        };
        let (account_idx, account_id) = match self.account_for_message(&msg_snapshot) {
            Some(account) => account,
            None => return,
        };

        let client = match self.accounts[account_idx].client.clone() {
            Some(c) => c,
            None => return,
        };

        let msg = self.messages.remove(idx);
        let email_id = msg.email_id.clone();
        let source_mailbox_id = msg.mailbox_id.clone();

        // Adjust selection
        if self.selected_message >= self.messages.len() && !self.messages.is_empty() {
            self.selected_message = self.messages.len() - 1;
        }
        if self.messages.is_empty() {
            self.body_text = None;
        }
        let lane_epoch = self.start_account_lane(&account_id, Lane::Mutation);
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let saved_msg = msg.clone();
        let destination_name = target_role.to_string();
        let op_account_id = account_id.clone();
        let task_account_id = account_id.clone();
        let task_email_id = email_id.clone();
        let handle = tokio::spawn(async move {
            // JMAP move is atomic — no postcondition check needed
            let result = neverlight_mail_core::email::move_to(
                &client,
                &task_email_id,
                &source_mailbox_id,
                &dest_mailbox_id,
            )
            .await
            .map_err(|e| e.to_string());

            if result.is_ok() {
                let _ = cache
                    .remove_message(task_account_id.clone(), task_email_id.clone())
                    .await;
            } else {
                let _ = cache
                    .revert_pending_op(task_account_id.clone(), task_email_id.clone())
                    .await;
            }

            let _ = tx.send(BgResult::MoveOp {
                account_id: op_account_id,
                lane_epoch,
                destination_name,
                message: Box::new(if result.is_err() {
                    Some((idx, saved_msg))
                } else {
                    None
                }),
                result,
            });
        });
        self.register_account_lane_task(&account_id, Lane::Mutation, handle);
    }
}
