# Claude Context: nevermail-tui

**Last Updated:** 2026-02-26

## What This Is

Terminal email client built on [nevermail-core](https://github.com/neverlight/nevermail-core) using [ratatui](https://ratatui.rs/) + crossterm. Early scaffold — functional but rough.

Shares the same email engine as [nevermail](https://github.com/neverlight/nevermail) (COSMIC desktop client). Same config files, same credential resolution, same IMAP session logic.

## Structure

```
src/
├── main.rs    — Terminal setup/restore, async event loop (100ms poll)
├── app.rs     — App state, IMAP connection, key handling, data loading
└── ui.rs      — Three-pane ratatui layout (folders, messages, body)
```

Three files. That's it.

## How It Works

### Startup
`App::new()` resolves accounts via `Config::resolve_all_accounts()`, connects IMAP, opens the SQLite cache, then loads folders and auto-selects the first one.

### Event Loop
`main.rs` runs a synchronous `event::poll(100ms)` loop. Key events go to `app.handle_key()`, then `app.tick()` runs for background work (placeholder for now). The UI redraws every iteration.

### Focus Model
Three panes: `Focus::Folders`, `Focus::Messages`, `Focus::Body`. Tab/Shift-Tab cycles focus. j/k/arrows navigate within the focused pane. Enter triggers action (load messages, view body).

### Data Flow
```
Folders loaded on connect → select folder → fetch_messages(mailbox_hash)
  → select message → fetch_body(envelope_hash) → render_body(plain, html)
```

All IMAP calls go through `nevermail_core::imap::ImapSession`. Body rendering uses `nevermail_core::mime::render_body()` which returns plain text (prefers text/plain, falls back to sanitized HTML).

## Known Issues

- Messages not sorted by date (comes back in whatever order IMAP returns)
- No body scroll (Body focus exists but up/down is a no-op there)
- No message flag operations (read, star, trash, archive)
- No search
- No compose/reply
- No IDLE watch for live updates
- Single account only (picks first from `resolve_all_accounts`)
- `account` and `cache` fields on App are unused (wired up for future use)

## Dependencies

| Crate | Purpose |
|-------|---------|
| nevermail-core | Email engine (IMAP, SMTP, MIME, cache, config) |
| ratatui | TUI framework |
| crossterm | Terminal backend (raw mode, alternate screen, events) |
| tokio | Async runtime |
| anyhow | Error handling |
| env_logger | `RUST_LOG` logging |

## Version Pinning

This crate depends on nevermail-core which depends on melib. The lockfile must pin `imap-codec` and `imap-types` to `2.0.0-alpha.4`. See [nevermail-core/CLAUDE.md](../nevermail-core/CLAUDE.md) for details and re-pin commands.

## Credentials

Same as nevermail — env vars or config file:
```bash
export NEVERMAIL_SERVER=mail.example.com
export NEVERMAIL_USER=you@example.com
export NEVERMAIL_PASSWORD=yourpassword
```

Or `~/.config/nevermail/config.json` with keyring backend.
