use std::sync::Arc;

use futures::StreamExt;
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::RefreshEventKind;

use super::{App, BgResult, ImapEventKind};

impl App {
    pub(super) fn spawn_watchers(&mut self) {
        for idx in 0..self.accounts.len() {
            self.spawn_watcher_for(idx);
        }
    }

    pub(super) fn spawn_watcher_for(&mut self, idx: usize) {
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

    pub(super) fn drop_session_and_reconnect(&mut self, account_idx: usize, reason: &str) {
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

    pub(super) fn spawn_reconnect(&self, account_idx: usize) {
        let acct = &self.accounts[account_idx];
        let config = acct.config.clone();
        let delay = acct.reconnect_backoff();
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
}
