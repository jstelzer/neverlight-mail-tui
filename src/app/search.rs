use super::{App, BgResult, Lane, Phase};

impl App {
    pub(super) fn spawn_search(&mut self) {
        let query = self.search_query.clone();
        let lane_epoch = self.start_lane(Lane::Search);
        self.phase = Phase::Searching;
        let cache = self.cache.clone();
        let tx = self.bg_tx.clone();
        let handle = tokio::spawn(async move {
            let result = cache.search(query).await;
            let _ = tx.send(BgResult::SearchResults {
                lane_epoch,
                result,
            });
        });
        self.register_lane_task(Lane::Search, handle);
    }

    pub(super) fn exit_search(&mut self) {
        self.search_active = false;
        self.search_query.clear();
        // Reload current folder
        self.spawn_load_messages();
    }
}
