use std::collections::{HashMap, HashSet};

use neverlight_mail_core::models::MessageSummary;

/// Compute thread sizes and visible indices from a message list and collapsed set.
///
/// Returns `(thread_sizes, visible_indices)`.
pub fn compute_visible(
    messages: &[MessageSummary],
    collapsed: &HashSet<u64>,
) -> (HashMap<u64, usize>, Vec<usize>) {
    let mut thread_sizes: HashMap<u64, usize> = HashMap::new();
    for msg in messages {
        if let Some(tid) = msg.thread_id {
            *thread_sizes.entry(tid).or_insert(0) += 1;
        }
    }

    let mut visible = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.thread_depth == 0 {
            // Root or standalone — always visible
            visible.push(i);
        } else if let Some(tid) = msg.thread_id {
            // Child — visible only if thread is not collapsed
            if !collapsed.contains(&tid) {
                visible.push(i);
            }
        } else {
            // No thread_id but has depth — show anyway
            visible.push(i);
        }
    }

    (thread_sizes, visible)
}

/// Navigate through visible indices, returning the new message index.
///
/// When `visible_indices` is empty (no threading), navigates by raw index.
pub fn visible_nav(
    visible_indices: &[usize],
    messages_len: usize,
    selected: usize,
    direction: i32,
) -> Option<usize> {
    if visible_indices.is_empty() {
        let new = selected as i32 + direction;
        if new >= 0 && (new as usize) < messages_len {
            return Some(new as usize);
        }
        return None;
    }
    let cur_pos = visible_indices
        .iter()
        .position(|&i| i == selected)
        .unwrap_or(0);
    let new_pos = cur_pos as i32 + direction;
    if new_pos >= 0 && (new_pos as usize) < visible_indices.len() {
        Some(visible_indices[new_pos as usize])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(thread_id: Option<u64>, depth: u32) -> MessageSummary {
        MessageSummary {
            uid: 0,
            subject: String::new(),
            from: String::new(),
            to: String::new(),
            date: String::new(),
            is_read: false,
            is_starred: false,
            has_attachments: false,
            thread_id,
            envelope_hash: 0,
            timestamp: 0,
            mailbox_hash: 0,
            message_id: String::new(),
            in_reply_to: None,
            reply_to: None,
            thread_depth: depth,
        }
    }

    // -------------------------------------------------------------------
    // compute_visible
    // -------------------------------------------------------------------

    #[test]
    fn standalone_messages_all_visible() {
        let messages = vec![msg(None, 0), msg(None, 0), msg(None, 0)];
        let (sizes, visible) = compute_visible(&messages, &HashSet::new());
        assert_eq!(visible, vec![0, 1, 2]);
        assert!(sizes.is_empty());
    }

    #[test]
    fn thread_expanded_shows_all() {
        // Root + 2 children in thread 42
        let messages = vec![msg(Some(42), 0), msg(Some(42), 1), msg(Some(42), 1)];
        let (sizes, visible) = compute_visible(&messages, &HashSet::new());
        assert_eq!(visible, vec![0, 1, 2]);
        assert_eq!(sizes[&42], 3);
    }

    #[test]
    fn thread_collapsed_hides_children() {
        let messages = vec![msg(Some(42), 0), msg(Some(42), 1), msg(Some(42), 1)];
        let collapsed = HashSet::from([42]);
        let (_sizes, visible) = compute_visible(&messages, &collapsed);
        // Only root is visible
        assert_eq!(visible, vec![0]);
    }

    #[test]
    fn mixed_threads_and_standalone() {
        let messages = vec![
            msg(Some(1), 0), // 0: thread 1 root
            msg(Some(1), 1), // 1: thread 1 child
            msg(None, 0),    // 2: standalone
            msg(Some(2), 0), // 3: thread 2 root
            msg(Some(2), 1), // 4: thread 2 child
        ];
        // Collapse thread 1 only
        let collapsed = HashSet::from([1]);
        let (sizes, visible) = compute_visible(&messages, &collapsed);
        assert_eq!(visible, vec![0, 2, 3, 4]);
        assert_eq!(sizes[&1], 2);
        assert_eq!(sizes[&2], 2);
    }

    #[test]
    fn orphan_depth_without_thread_id_stays_visible() {
        let messages = vec![msg(None, 0), msg(None, 1)];
        let (_sizes, visible) = compute_visible(&messages, &HashSet::new());
        assert_eq!(visible, vec![0, 1]);
    }

    #[test]
    fn empty_messages() {
        let (sizes, visible) = compute_visible(&[], &HashSet::new());
        assert!(visible.is_empty());
        assert!(sizes.is_empty());
    }

    // -------------------------------------------------------------------
    // visible_nav
    // -------------------------------------------------------------------

    #[test]
    fn nav_no_threading_moves_by_one() {
        assert_eq!(visible_nav(&[], 5, 2, 1), Some(3));
        assert_eq!(visible_nav(&[], 5, 2, -1), Some(1));
    }

    #[test]
    fn nav_no_threading_clamps_at_bounds() {
        assert_eq!(visible_nav(&[], 5, 0, -1), None);
        assert_eq!(visible_nav(&[], 5, 4, 1), None);
    }

    #[test]
    fn nav_no_threading_empty_list() {
        assert_eq!(visible_nav(&[], 0, 0, 1), None);
        assert_eq!(visible_nav(&[], 0, 0, -1), None);
    }

    #[test]
    fn nav_with_threading_skips_collapsed() {
        // visible_indices maps visible rows to message indices
        // e.g. thread collapsed: [0, 3, 4] means messages 1,2 are hidden
        let vis = vec![0, 3, 4];
        assert_eq!(visible_nav(&vis, 5, 0, 1), Some(3));
        assert_eq!(visible_nav(&vis, 5, 3, 1), Some(4));
        assert_eq!(visible_nav(&vis, 5, 4, -1), Some(3));
        assert_eq!(visible_nav(&vis, 5, 3, -1), Some(0));
    }

    #[test]
    fn nav_with_threading_clamps_at_bounds() {
        let vis = vec![0, 3, 4];
        assert_eq!(visible_nav(&vis, 5, 0, -1), None);
        assert_eq!(visible_nav(&vis, 5, 4, 1), None);
    }

    #[test]
    fn nav_selected_not_in_visible_falls_back_to_zero() {
        // selected_message=2 isn't in visible_indices — unwrap_or(0) kicks in
        let vis = vec![0, 3, 4];
        // From position 0 (fallback), move down → position 1 → index 3
        assert_eq!(visible_nav(&vis, 5, 2, 1), Some(3));
    }
}
