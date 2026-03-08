use neverlight_mail_core::push::EventSourceConfig;
use neverlight_mail_core::session::JmapSession;

use super::{App, BgResult};

impl App {
    pub(super) fn spawn_watchers(&mut self) {
        for idx in 0..self.accounts.len() {
            self.spawn_watcher_for(idx);
        }
    }

    pub(super) fn spawn_watcher_for(&mut self, idx: usize) {
        let client = match &self.accounts[idx].client {
            Some(c) => c.clone(),
            None => return,
        };
        self.accounts[idx].watch_generation = self.accounts[idx].watch_generation.saturating_add(1);
        let generation = self.accounts[idx].watch_generation;
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = neverlight_mail_core::push::listen(
                &client,
                &EventSourceConfig::default(),
                |_state_change| {
                    let _ = tx.send(BgResult::PushStateChanged {
                        account_idx: idx,
                        watch_generation: generation,
                    });
                },
            )
            .await;

            let error = result.err();
            let _ = tx.send(BgResult::PushEnded {
                account_idx: idx,
                watch_generation: generation,
                error,
            });
        });
    }

    pub(super) fn drop_client_and_reconnect(&mut self, account_idx: usize, reason: &str) {
        let acct = &mut self.accounts[account_idx];
        log::warn!(
            "Dropping client for '{}' (reason: {reason})",
            acct.config.label,
        );
        acct.client = None;
        acct.last_error = Some(format!("Session lost: {reason}"));
        acct.reconnect_attempts = acct.reconnect_attempts.saturating_add(1);
        self.spawn_reconnect(account_idx);
    }

    pub(super) fn spawn_reconnect(&mut self, account_idx: usize) {
        if let Some(handle) = self.reconnect_tasks.remove(&account_idx) {
            handle.abort();
        }
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
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let result = JmapSession::connect(&config)
                .await
                .map(|(_session, client)| client)
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Reconnected {
                account_idx,
                result,
            });
        });
        self.reconnect_tasks.insert(account_idx, handle);
    }
}
