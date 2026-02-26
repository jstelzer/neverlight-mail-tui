use ratatui_textarea::TextArea;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_body_prefixes_lines() {
        let result = quote_body("line 1\nline 2", "Alice", "2026-01-15");
        assert_eq!(
            result,
            "On 2026-01-15, Alice wrote:\n> line 1\n> line 2\n"
        );
    }

    #[test]
    fn quote_body_empty() {
        let result = quote_body("", "Bob", "2026-02-01");
        // Empty string has zero lines, so no "> " prefix — just the header
        assert_eq!(result, "On 2026-02-01, Bob wrote:\n\n");
    }

    #[test]
    fn forward_body_includes_header_block() {
        let result = forward_body("Hello world", "Alice", "2026-01-15", "Test Subject");
        assert!(result.starts_with("---------- Forwarded message ----------\n"));
        assert!(result.contains("From: Alice\n"));
        assert!(result.contains("Date: 2026-01-15\n"));
        assert!(result.contains("Subject: Test Subject\n"));
        assert!(result.contains("Hello world\n"));
    }

    #[test]
    fn build_references_with_in_reply_to() {
        let result = build_references(Some("<abc@example.com>"), "<def@example.com>");
        assert_eq!(result, "<abc@example.com> <def@example.com>");
    }

    #[test]
    fn build_references_without_in_reply_to() {
        let result = build_references(None, "<def@example.com>");
        assert_eq!(result, "<def@example.com>");
    }
}
