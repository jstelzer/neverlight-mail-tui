use tui_textarea::TextArea;

// ---------------------------------------------------------------------------
// Compose state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeMode {
    New,
    Reply,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Subject,
    Body,
}

#[allow(dead_code)]
pub struct ComposeState {
    pub mode: ComposeMode,
    pub to: String,
    pub subject: String,
    pub body: TextArea<'static>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub active_field: ComposeField,
}

impl ComposeState {
    pub fn new(mode: ComposeMode) -> Self {
        let mut body = TextArea::default();
        body.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            mode,
            to: String::new(),
            subject: String::new(),
            body,
            in_reply_to: None,
            references: None,
            active_field: ComposeField::To,
        }
    }
}

// ---------------------------------------------------------------------------
// Compose helpers — lifted from nevermail GUI
// ---------------------------------------------------------------------------

pub fn quote_body(body: &str, from: &str, date: &str) -> String {
    let quoted: String = body
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("On {date}, {from} wrote:\n{quoted}\n")
}

pub fn forward_body(body: &str, from: &str, date: &str, subject: &str) -> String {
    format!(
        "---------- Forwarded message ----------\n\
         From: {from}\n\
         Date: {date}\n\
         Subject: {subject}\n\n\
         {body}\n"
    )
}

pub fn build_references(in_reply_to: Option<&str>, message_id: &str) -> String {
    match in_reply_to {
        Some(irt) => format!("{irt} {message_id}"),
        None => message_id.to_string(),
    }
}
