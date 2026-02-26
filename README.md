# nevermail-tui

Terminal email client powered by [nevermail-core](https://github.com/neverlight/nevermail-core). Built with [ratatui](https://ratatui.rs/) and [crossterm](https://crates.io/crates/crossterm).

Shares the same email engine, config files, and credential resolution as [nevermail](https://github.com/neverlight/nevermail) (COSMIC desktop client).

## Features

- Three-pane layout (folders, messages, body preview)
- Multi-account support with instant switching
- SQLite cache for offline reading and instant startup
- IMAP IDLE for live mailbox updates
- Message threading with collapsible threads
- Full-text search (FTS5 via cache)
- Flag operations (read/unread, star)
- Trash and archive
- Compose, reply, and forward with SMTP send
- Body scrolling
- Messages sorted newest-first

## Usage

```bash
# Configure credentials (same env vars as nevermail)
export NEVERMAIL_SERVER=mail.example.com
export NEVERMAIL_USER=you@example.com
export NEVERMAIL_PASSWORD=yourpassword

cargo run
```

Or use a `~/.config/nevermail/config.json` file — see [nevermail-core](https://github.com/neverlight/nevermail-core) for config resolution details.

Multiple accounts are supported. All accounts from config resolution are connected on startup. Press `1`-`9` to switch between them.

## Keybindings

### Navigation

| Key                 | Action                                 |
|---------------------|----------------------------------------|
| `Tab` / `Shift-Tab` | Cycle focus: Folders → Messages → Body |
| `j` / `↓`           | Move down (scroll body when focused)   |
| `k` / `↑`           | Move up (scroll body when focused)     |
| `Enter`             | Open (load messages / view body)       |
| `q`                 | Quit                                   |

### Message Actions

| Key     | Action                        |
|---------|-------------------------------|
| `s`     | Toggle star                   |
| `R`     | Toggle read/unread            |
| `d`     | Move to Trash                 |
| `a`     | Move to Archive               |
| `Space` | Toggle thread collapse/expand |

### Search

| Key     | Action                        |
|---------|-------------------------------|
| `/`     | Enter search mode             |
| `Enter` | Submit search query           |
| `Esc`   | Cancel search, restore folder |

### Compose

| Key     | Action                          |
|---------|---------------------------------|
| `c`     | Compose new message             |
| `r`     | Reply to selected message       |
| `f`     | Forward selected message        |
| `Ctrl-S` | Send (in compose mode)         |
| `Esc`   | Cancel compose                  |
| `Tab`   | Next field (To → Subject → Body) |

### Multi-Account

| Key     | Action              |
|---------|---------------------|
| `1`-`9` | Switch to account N |


## Layout

```
┌──────────┬───────────────────┬────────────────────────┐
│ Folders  │ Messages          │ Preview                │
│          │                   │                        │
│ INBOX(3) │ ● ★ From — Subj  │ Message body text...   │
│ Sent     │   [-3] From — Re: │                        │
│ Drafts   │     From — Re:   │                        │
│ Trash    │     From — Re:   │                        │
│ Archive  │   From — Subj    │                        │
└──────────┴───────────────────┴────────────────────────┘
 Status bar / Search: query_
```

## Architecture

```
src/
├── main.rs      — Terminal setup/restore, async event loop (tokio::select!)
├── app.rs       — App state, IMAP/cache/SMTP integration, key handling
├── compose.rs   — Compose state, quote/forward helpers
└── ui.rs        — Three-pane ratatui layout + compose overlay
```

All IMAP and SMTP operations run as background tasks via `tokio::spawn`, communicating results through an `mpsc` channel. The UI never blocks on network I/O.

Cache provides instant display of previously-seen folders, messages, and bodies while IMAP fetches authoritative data in the background.

## Dependencies

| Crate            | Purpose                                               |
|------------------|-------------------------------------------------------|
| nevermail-core   | Email engine (IMAP, SMTP, MIME, cache, config)        |
| ratatui          | TUI framework                                         |
| crossterm        | Terminal backend (raw mode, alternate screen, events) |
| tui-textarea     | Multiline text editor for compose                     |
| tokio            | Async runtime                                         |
| futures          | Stream utilities (IMAP IDLE)                          |
| anyhow           | Error handling                                        |
| log / env_logger | `RUST_LOG` logging                                    |

## Related

- [nevermail-core](https://github.com/neverlight/nevermail-core) — Headless email engine
- [nevermail](https://github.com/neverlight/nevermail) — COSMIC desktop email client

## License

GPL-3.0-or-later
