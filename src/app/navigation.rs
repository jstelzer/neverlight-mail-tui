use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};

use crate::compose::ComposeMode;

use super::{App, AppEvent, Focus, Phase};

impl App {
    pub(super) fn recompute_visible(&mut self) {
        let (sizes, visible) =
            crate::threading::compute_visible(&self.messages, &self.collapsed_threads);
        self.thread_sizes = sizes;
        self.visible_indices = visible;
    }

    pub(super) fn toggle_thread_collapse(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let msg = &self.messages[self.selected_message];
        let tid = match msg.thread_id {
            Some(t) => t,
            None => return,
        };
        let size = self.thread_sizes.get(&tid).copied().unwrap_or(1);
        if size <= 1 {
            return; // No children to collapse
        }
        if self.collapsed_threads.contains(&tid) {
            self.collapsed_threads.remove(&tid);
        } else {
            self.collapsed_threads.insert(tid);
            // If selected was a child, jump to thread root
            if msg.thread_depth > 0 {
                if let Some(root_idx) = self
                    .messages
                    .iter()
                    .position(|m| m.thread_id == Some(tid) && m.thread_depth == 0)
                {
                    self.selected_message = root_idx;
                }
            }
        }
        self.recompute_visible();
    }

    /// Navigate messages using visible_indices when threading is active.
    fn visible_nav(&self, direction: i32) -> Option<usize> {
        crate::threading::visible_nav(
            &self.visible_indices,
            self.messages.len(),
            self.selected_message,
            direction,
        )
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder > 0 {
                    self.selected_folder -= 1;
                    self.spawn_load_messages();
                }
            }
            Focus::Messages => {
                if let Some(idx) = self.visible_nav(-1) {
                    self.selected_message = idx;
                }
            }
            Focus::Body => {
                self.body_scroll = self.body_scroll.saturating_sub(1);
            }
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Folders => {
                if self.selected_folder + 1 < self.active().folders.len() {
                    self.selected_folder += 1;
                    self.spawn_load_messages();
                }
            }
            Focus::Messages => {
                if let Some(idx) = self.visible_nav(1) {
                    self.selected_message = idx;
                }
            }
            Focus::Body => {
                self.body_scroll = self.body_scroll.saturating_add(1);
            }
        }
    }

    fn select(&mut self) {
        match self.focus {
            Focus::Folders => self.spawn_load_messages(),
            Focus::Messages => self.spawn_load_body(),
            Focus::Body => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AppEvent {
        // Compose mode gets priority
        if self.compose.is_some() {
            return self.handle_compose_key(key);
        }
        if self.search_active {
            return self.handle_search_key(key.code);
        }
        match key.code {
            KeyCode::Char('q') => return AppEvent::Quit,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Folders => Focus::Messages,
                    Focus::Messages => Focus::Body,
                    Focus::Body => Focus::Folders,
                };
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Folders => Focus::Body,
                    Focus::Messages => Focus::Folders,
                    Focus::Body => Focus::Messages,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Enter => self.select(),
            KeyCode::Char('s') => self.toggle_star(),
            KeyCode::Char('R') => self.toggle_read(),
            KeyCode::Char('d') => self.move_to_folder("Trash"),
            KeyCode::Char('a') => self.move_to_folder("Archive"),
            KeyCode::Char('/') => {
                self.search_active = true;
                self.search_query.clear();
                self.status = "Search: ".into();
            }
            KeyCode::Char(' ') => self.toggle_thread_collapse(),
            KeyCode::Char('c') => self.start_compose(ComposeMode::New),
            KeyCode::Char('r') => self.start_compose(ComposeMode::Reply),
            KeyCode::Char('f') => self.start_compose(ComposeMode::Forward),
            KeyCode::Char(n @ '1'..='9') => {
                let idx = (n as usize) - ('1' as usize);
                if idx < self.accounts.len() && idx != self.active_account {
                    self.active_account = idx;
                    self.selected_folder = 0;
                    self.selected_message = 0;
                    self.body_text = None;
                    self.messages.clear();
                    if self.active().folders.is_empty() {
                        self.spawn_load_folders();
                    } else {
                        self.spawn_load_messages();
                    }
                    self.phase = Phase::Loading;
                    self.status = format!("Account: {}", self.active().config.label);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if self.focus == Focus::Body && self.image_protos.len() > 1 && self.image_index > 0
                {
                    self.image_index -= 1;
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if self.focus == Focus::Body
                    && self.image_protos.len() > 1
                    && self.image_index + 1 < self.image_protos.len()
                {
                    self.image_index += 1;
                }
            }
            _ => {}
        }
        AppEvent::Continue
    }

    pub(super) fn handle_search_key(&mut self, key: KeyCode) -> AppEvent {
        match key {
            KeyCode::Esc => self.exit_search(),
            KeyCode::Enter => {
                if self.search_query.is_empty() {
                    self.exit_search();
                } else {
                    self.phase = Phase::Searching;
                    self.status = format!("Searching: {}…", self.search_query);
                    self.spawn_search();
                    // Exit search input mode so user can navigate results
                    self.search_active = false;
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.status = format!("Search: {}", self.search_query);
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.status = format!("Search: {}", self.search_query);
            }
            _ => {}
        }
        AppEvent::Continue
    }

    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        if self.compose.is_some() || self.search_active {
            return;
        }

        let lr = &self.layout_rects;
        let in_rect =
            |r: &ratatui::prelude::Rect| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height;

        match kind {
            MouseEventKind::Down(_) => {
                if in_rect(&lr.folders.clone()) {
                    self.focus = Focus::Folders;
                    // Row within the pane (subtract border top)
                    let local_row = row.saturating_sub(lr.folders.y + 1) as usize;
                    if local_row < self.active().folders.len() && local_row != self.selected_folder
                    {
                        self.selected_folder = local_row;
                        self.spawn_load_messages();
                    }
                } else if in_rect(&lr.messages.clone()) {
                    self.focus = Focus::Messages;
                    let local_row = row.saturating_sub(lr.messages.y + 1) as usize;
                    // Map visible row to actual message index
                    let vis = if self.visible_indices.is_empty() {
                        if local_row < self.messages.len() {
                            Some(local_row)
                        } else {
                            None
                        }
                    } else if local_row < self.visible_indices.len() {
                        Some(self.visible_indices[local_row])
                    } else {
                        None
                    };
                    if let Some(idx) = vis {
                        self.selected_message = idx;
                        self.spawn_load_body();
                    }
                } else if in_rect(&lr.body.clone()) {
                    self.focus = Focus::Body;
                }
            }
            MouseEventKind::ScrollUp => {
                if in_rect(&lr.body.clone()) {
                    self.body_scroll = self.body_scroll.saturating_sub(3);
                } else if in_rect(&lr.folders.clone()) {
                    if self.selected_folder > 0 {
                        self.selected_folder -= 1;
                        self.spawn_load_messages();
                    }
                } else if in_rect(&lr.messages.clone()) {
                    if let Some(idx) = self.visible_nav(-1) {
                        self.selected_message = idx;
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if in_rect(&lr.body.clone()) {
                    self.body_scroll = self.body_scroll.saturating_add(3);
                } else if in_rect(&lr.folders.clone()) {
                    if self.selected_folder + 1 < self.active().folders.len() {
                        self.selected_folder += 1;
                        self.spawn_load_messages();
                    }
                } else if in_rect(&lr.messages.clone()) {
                    if let Some(idx) = self.visible_nav(1) {
                        self.selected_message = idx;
                    }
                }
            }
            _ => {}
        }
    }
}
