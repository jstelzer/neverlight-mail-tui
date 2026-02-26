# nevermail-tui

Terminal email client powered by [nevermail-core](https://github.com/neverlight/nevermail-core). Built with [ratatui](https://ratatui.rs/) and [crossterm](https://crates.io/crates/crossterm).

## Status

Early scaffold. Connects to IMAP, lists folders and messages, and renders message bodies in the terminal. Expect rough edges.

## Usage

```bash
# Configure credentials (same env vars as nevermail)
export NEVERMAIL_SERVER=mail.example.com
export NEVERMAIL_USER=you@example.com
export NEVERMAIL_PASSWORD=yourpassword

cargo run
```

Or use a `~/.config/nevermail/config.json` file — see [nevermail-core](https://github.com/neverlight/nevermail-core) for config resolution details.

## Keybindings

| Key | Action |
|-----|--------|
| `Tab` / `Shift-Tab` | Cycle focus: Folders → Messages → Body |
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Enter` | Open (load messages / view body) |
| `q` | Quit |

## Layout

Three-pane layout: folder sidebar, message list, and body preview.

```
┌──────────┬───────────────┬────────────────────────┐
│ Folders  │ Messages      │ Preview                │
│          │               │                        │
│ INBOX(3) │ ● From — Subj │ Message body text...   │
│ Sent     │   From — Subj │                        │
│ Drafts   │   From — Subj │                        │
│ Trash    │               │                        │
└──────────┴───────────────┴────────────────────────┘
 Status bar
```

## Related

- [nevermail-core](https://github.com/neverlight/nevermail-core) — Headless email engine
- [nevermail](https://github.com/neverlight/nevermail) — COSMIC desktop email client

## License

GPL-3.0-or-later
