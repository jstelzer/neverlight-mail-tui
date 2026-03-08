# Claude Context: neverlight-mail-tui

**Last Updated:** 2026-03-08

## What This Is

Terminal email client built on [neverlight-mail-core](../neverlight-mail-core) using [ratatui](https://ratatui.rs/) + crossterm. Feature-complete for daily email use — read, write, search, multi-account.

Shares the same JMAP email engine as [neverlight-mail](../neverlight-mail) (COSMIC desktop client). Same config files, same credential resolution, same JMAP client.

**This is a JMAP-only client.** No IMAP, no SMTP, no melib. Sending uses JMAP `EmailSubmission/set`, not SMTP. Push uses JMAP EventSource SSE, not IMAP IDLE.

## Read First

- `docs/code-conventions.md` — Code style, state modeling, error handling. **You must follow this.** Shared with neverlight-mail-core: enums over boolean flags, `let-else` over nested `if let`, no `#[allow(...)]` without discussion, warnings are errors.
- `neverlight-mail-core/CLAUDE.md` — Engine architecture, JMAP design rationale.

## Structure

```
src/
├── main.rs          — Terminal setup/restore, picker init, async event loop (tokio::select!)
├── compose.rs       — ComposeState, quote/forward helpers
├── threading.rs     — Thread collapse/expand logic, visible index computation
├── setup.rs         — JMAP account setup TUI (label, URL, username, token, email)
├── ui.rs            — Three-pane ratatui layout, compose overlay, thread indentation, inline images
└── app/
    ├── mod.rs       — App state, BgResult enum, AccountState, apply() dispatcher
    ├── actions.rs   — Flag/move handlers (toggle read/star, trash, archive)
    ├── compose.rs   — Compose key handling + spawn_send (JMAP EmailSubmission)
    ├── images.rs    — Image picker/protocol detection
    ├── lanes.rs     — Lane epoch management, stale apply protection, refresh watchdog
    ├── navigation.rs — Keyboard/mouse handlers, thread collapse, focus cycling
    ├── search.rs    — FTS5 cache search
    ├── sync.rs      — Folder/message/body loading (JMAP query_and_get + cache)
    └── watch.rs     — JMAP EventSource push + reconnect scheduling
```

## How It Works

### Startup
`App::with_accounts()` resolves all accounts via `config::resolve_all_accounts()`, connects each via `JmapSession::connect()` (failures are non-fatal), opens the SQLite cache, then spawns folder loading + EventSource push watchers for each account.

### Event Loop
`main.rs` runs a `tokio::select!` loop over three sources: crossterm `EventStream` for key/mouse events, an `mpsc` channel for background task results (JMAP fetches, flag ops, sends, push events), and an image resize channel for `ThreadProtocol` resize requests. UI redraws every iteration.

### Multi-Account Model
`Vec<AccountState>` holds per-account config, `JmapClient`, and folders. `active_account: usize` selects which account is displayed. Keys `1`-`9` switch accounts.

### Data Flow
```
JmapSession::connect → JmapClient
  → mailbox::fetch_all(&client) → cache.save_folders → select folder
  → cache.load_messages (instant) + email::query_and_get(&client) (authoritative)
  → sort by timestamp desc → select message
  → cache.load_body || email::get_body(&client) → cache.save_body → render
```

### ID Types
All identifiers are strings. `email_id: String` (JMAP Email ID), `mailbox_id: String` (JMAP Mailbox ID), `thread_id: Option<String>` (JMAP Thread ID), `account_id: String` (local UUID). No u64 hashes anywhere.

### Background Tasks
All JMAP and cache calls run via `tokio::spawn`, sending results through `BgResult` enum on an `mpsc::UnboundedSender`. The main loop applies results to app state.

### BgResult Variants
- `Folders` — folder list loaded
- `Messages` / `CachedMessages` — message list from JMAP / cache
- `Body` — rendered body text + attachments
- `FlagOp` — flag toggle confirmation/rollback
- `MoveOp` — trash/archive confirmation/rollback (JMAP moves are atomic — no postcondition check)
- `SearchResults` — FTS5 search results
- `SendResult` — JMAP EmailSubmission confirmation
- `PushStateChanged` — EventSource state change (triggers refresh)
- `PushEnded` — EventSource stream ended
- `Reconnected` — reconnect attempt result

### Optimistic Updates
Flag toggles and moves update the UI immediately, then sync with JMAP in background. On failure, the UI reverts. Cache tracks pending operations for crash recovery.

### Lane Epochs (Stale Apply Protection)
Async completions carry lane epochs so stale results are dropped instead of mutating current state. Per-account lanes for Flag and Mutation operations.

### Core API Surface (what the TUI calls)
```rust
// Folders
mailbox::fetch_all(&client) -> Result<Vec<Folder>>
mailbox::find_by_role(&folders, "trash") -> Option<String>

// Messages
email::query_and_get(&client, &mailbox_id, limit, offset) -> Result<(Vec<MessageSummary>, QueryResult)>
email::get_body(&client, &email_id) -> Result<(String, String, Vec<AttachmentData>)>
email::set_flag(&client, &email_id, &FlagOp) -> Result<()>
email::move_to(&client, &email_id, &from_mailbox, &to_mailbox) -> Result<()>

// Sending (replaces SMTP)
submit::get_identities(&client) -> Result<Vec<Identity>>
submit::send(&client, &SendRequest) -> Result<String>

// Push (replaces IMAP IDLE)
push::listen(&client, &EventSourceConfig, on_change_callback) -> Result<()>

// Session
session::JmapSession::connect(&config) -> Result<(JmapSession, JmapClient)>
```

### Connection Health & Reconnect
EventSource (SSE) replaces IMAP IDLE. When the SSE stream ends or errors, `PushEnded` triggers reconnect with exponential backoff (5s → 15s → 30s → 60s cap). Sync failures also drop the client and schedule reconnect.

## Known Limitations

- No pagination (loads up to 200 messages per folder)
- Compose doesn't support attachments
- No address book / autocomplete
- No offline compose — requires active JmapClient

## Dependencies

| Crate | Purpose |
|-------|---------|
| neverlight-mail-core | JMAP email engine (query, send, push, cache, config) |
| ratatui | TUI framework |
| crossterm | Terminal backend (raw mode, alternate screen, mouse, events) |
| ratatui-textarea | Multiline text editor for compose body |
| ratatui-image | Inline image rendering (Sixel, Kitty, iTerm2, halfblocks) |
| image | Image decoding (PNG, JPEG, GIF, etc.) |
| tokio | Async runtime |
| futures | Stream utilities |
| anyhow | Error handling |
| log / env_logger | `RUST_LOG` logging |

## Credentials

Same as neverlight-mail — env vars or config file:
```bash
export NEVERLIGHT_MAIL_JMAP_TOKEN=your-app-password
export NEVERLIGHT_MAIL_USER=you@example.com
```

Or `~/.config/neverlight-mail/config.json` with keyring backend. Multiple accounts supported — all resolved accounts connect on startup.
