use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::StatefulImage;

use crate::app::{App, Focus, LayoutRects};
use crate::compose::ComposeField;

pub fn render(frame: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let main_area = outer[0];
    let status_area = outer[1];

    // Three-column layout: folders | messages | body
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(30),
            Constraint::Percentage(50),
        ])
        .split(main_area);

    // Save layout rects for mouse hit-testing
    app.layout_rects = LayoutRects {
        folders: columns[0],
        messages: columns[1],
        body: columns[2],
    };

    render_folders(frame, app, columns[0]);
    render_messages(frame, app, columns[1]);
    render_body(frame, app, columns[2]);
    render_status(frame, app, status_area);

    // Compose overlay (renders on top)
    if app.compose.is_some() {
        render_compose(frame, app, main_area);
    }
}

fn render_folders(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .active_folders()
        .iter()
        .map(|f| {
            let label = if f.unread_count > 0 {
                format!("{} ({})", f.name, f.unread_count)
            } else {
                f.name.clone()
            };
            ListItem::new(label)
        })
        .collect();

    let border_style = if app.focus == Focus::Folders {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Folders ")
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    state.select(Some(app.selected_folder));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_messages(frame: &mut Frame, app: &App, area: Rect) {
    // Use visible_indices if available, otherwise show all messages.
    // Filter out stale indices that exceed current messages length
    // (can happen during cache→IMAP transition).
    let msg_len = app.messages.len();
    let indices: Vec<usize> = if app.visible_indices.is_empty() {
        (0..msg_len).collect()
    } else {
        app.visible_indices
            .iter()
            .copied()
            .filter(|&i| i < msg_len)
            .collect()
    };

    let items: Vec<ListItem> = indices
        .iter()
        .map(|&i| {
            let m = &app.messages[i];
            let marker = if !m.is_read { "● " } else { "  " };
            let star = if m.is_starred { "★ " } else { "" };
            // Thread indentation
            let indent = "  ".repeat(m.thread_depth as usize);
            // Collapse indicator for thread roots
            let collapse = if m.thread_depth == 0 {
                if let Some(tid) = m.thread_id {
                    let size = app.thread_sizes.get(&tid).copied().unwrap_or(1);
                    if size > 1 {
                        if app.collapsed_threads.contains(&tid) {
                            format!("[+{size}] ")
                        } else {
                            format!("[-{size}] ")
                        }
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            let line = format!("{marker}{star}{indent}{collapse}{} — {}", m.from, m.subject);
            ListItem::new(line)
        })
        .collect();

    let border_style = if app.focus == Focus::Messages {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Messages ")
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ");

    // Find the position of selected_message in visible_indices
    let selected_pos = indices
        .iter()
        .position(|&i| i == app.selected_message)
        .unwrap_or(0);

    let mut state = ListState::default();
    state.select(Some(selected_pos));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let text = app
        .body_text
        .as_deref()
        .unwrap_or("Press Enter on a message to view its body.");

    let border_style = if app.focus == Focus::Body {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let has_image = !app.image_protos.is_empty();
    let has_attachments = !app.attachment_info.is_empty();

    // Build title with attachment info
    let proto = app.image_protocol_label();
    let title = if has_attachments {
        let total = app.attachment_info.len();
        let img_count = app
            .attachment_info
            .iter()
            .filter(|(_, m, _)| m.starts_with("image/"))
            .count();
        if img_count > 0 {
            format!(" Preview [{total} attachments, {img_count} images] ({proto}) ")
        } else {
            format!(" Preview [{total} attachments] ({proto}) ")
        }
    } else {
        format!(" Preview ({proto}) ")
    };

    if has_image {
        // Split: text top (60%), image bottom (40%)
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        // Text section
        let paragraph = Paragraph::new(text)
            .block(
                Block::default()
                    .title(title.as_str())
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.body_scroll, 0));
        frame.render_widget(paragraph, sections[0]);

        // Image section
        let img_title = if app.image_protos.len() > 1 {
            format!(
                " \u{25c0} {}/{} \u{25b6} ",
                app.image_index + 1,
                app.image_protos.len()
            )
        } else {
            String::new()
        };
        let img_block = Block::default()
            .title(img_title.as_str())
            .borders(Borders::ALL)
            .border_style(border_style);
        let img_inner = img_block.inner(sections[1]);
        frame.render_widget(img_block, sections[1]);

        if let Some(proto) = app.image_protos.get_mut(app.image_index) {
            frame.render_stateful_widget(StatefulImage::default(), img_inner, proto);
        }
    } else {
        // No images — full text preview
        let paragraph = Paragraph::new(text)
            .block(
                Block::default()
                    .title(title.as_str())
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.body_scroll, 0));
        frame.render_widget(paragraph, area);
    }
}

fn render_compose(frame: &mut Frame, app: &App, area: Rect) {
    let state = match &app.compose {
        Some(s) => s,
        None => return,
    };

    // Centered overlay: 80% width, 80% height
    let popup_area = centered_rect(80, 80, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Compose (Ctrl-S send, Esc cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Split inner into: To (1 line) | Subject (1 line) | Body (rest)
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    // To field
    let to_style = if state.active_field == ComposeField::To {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let to_line = format!("To: {}", state.to);
    frame.render_widget(Paragraph::new(to_line).style(to_style), rows[0]);

    // Subject field
    let subj_style = if state.active_field == ComposeField::Subject {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let subj_line = format!("Subject: {}", state.subject);
    frame.render_widget(Paragraph::new(subj_line).style(subj_style), rows[1]);

    // Body (tui-textarea)
    let body_block = Block::default().borders(Borders::TOP).border_style(
        if state.active_field == ComposeField::Body {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        },
    );
    frame.render_widget(&state.body, body_block.inner(rows[2]));
    frame.render_widget(body_block, rows[2]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    if app.search_active {
        let search_line = format!("/{}_", app.search_query);
        let status = Paragraph::new(search_line).style(Style::default().fg(Color::Cyan).bold());
        frame.render_widget(status, area);
    } else {
        let status = Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::Yellow));
        frame.render_widget(status, area);
    }
}
