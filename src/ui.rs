use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus};

pub fn render(frame: &mut Frame, app: &App) {
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

    render_folders(frame, app, columns[0]);
    render_messages(frame, app, columns[1]);
    render_body(frame, app, columns[2]);
    render_status(frame, app, status_area);
}

fn render_folders(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .folders
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
    let items: Vec<ListItem> = app
        .messages
        .iter()
        .map(|m| {
            let marker = if !m.is_read { "● " } else { "  " };
            let star = if m.is_starred { "★ " } else { "" };
            let line = format!("{marker}{star}{} — {}", m.from, m.subject);
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

    let mut state = ListState::default();
    state.select(Some(app.selected_message));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let text = app
        .body_text
        .as_deref()
        .unwrap_or("Press Enter on a message to view its body.");

    let border_style = if app.focus == Focus::Body {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .title(" Preview ")
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let status = Paragraph::new(app.status.as_str())
        .style(Style::default().fg(Color::Yellow));
    frame.render_widget(status, area);
}
