# Claude Context: nevermail-tui

**Last Updated:** 2026-02-26

## What This Is

Terminal email client built on [nevermail-core](https://github.com/neverlight/nevermail-core) using [ratatui](https://ratatui.rs/) + crossterm. Feature-complete for daily email use — read, write, search, multi-account.

Shares the same email engine as [nevermail](https://github.com/neverlight/nevermail) (COSMIC desktop client). Same config files, same credential resolution, same IMAP session logic.

## Structure

```
src/
├── main.rs      — Terminal setup/restore, picker init, async event loop (tokio::select!)
├── app.rs       — App state, multi-account IMAP/cache/SMTP, key/mouse handling, threading, images
├── compose.rs   — ComposeState, quote/forward helpers
└── ui.rs        — Three-pane ratatui layout, compose overlay, thread indentation, inline images
```

## How It Works

### Startup
`App::new()` resolves all accounts via `Config::resolve_all_accounts()`, connects each via IMAP (failures are non-fatal), opens the SQLite cache, then spawns folder loading + IDLE watchers for each account.

### Event Loop
`main.rs` runs a `tokio::select!` loop over three sources: crossterm `EventStream` for key/mouse events, an `mpsc` channel for background task results (IMAP fetches, flag ops, SMTP sends, IDLE events), and an image resize channel for `ThreadProtocol` resize requests. UI redraws every iteration.

### Multi-Account Model
`Vec<AccountState>` holds per-account config, IMAP session, folders, and folder_map. `active_account: usize` selects which account is displayed. Keys `1`-`9` switch accounts.

### Focus Model
Three panes: `Focus::Folders`, `Focus::Messages`, `Focus::Body`. Tab/Shift-Tab cycles focus. j/k/arrows navigate within the focused pane. In Body focus, j/k scrolls the body text. Mouse clicks set focus and select items; scroll wheel navigates/scrolls within the hovered pane.

### Data Flow
```
Folders loaded on connect → cache.save_folders → select folder
  → cache.load_messages (instant) + session.fetch_messages (authoritative)
  → sort by timestamp desc → select message
  → cache.load_body || session.fetch_body → cache.save_body → render
```

### Background Tasks
All IMAP, cache, and SMTP calls run via `tokio::spawn`, sending results through `BgResult` enum on an `mpsc::UnboundedSender`. The main loop applies results to app state. This keeps the UI responsive during network operations.

### BgResult Variants
- `Folders` — folder list loaded
- `Messages` / `CachedMessages` — message list from IMAP / cache
- `Body` — rendered body text + attachments (Vec<AttachmentData>)
- `FlagOp` — flag toggle confirmation/rollback
- `MoveOp` — trash/archive confirmation/rollback
- `SearchResults` — FTS5 search results
- `SendResult` — SMTP send confirmation
- `ImapEvent` — IDLE notification (new mail, removal, rescan)
- `WatchEnded` — IDLE stream ended

### Optimistic Updates
Flag toggles and move-to-folder update the UI immediately, then sync with IMAP in background. On failure, the UI reverts to the original state. Cache tracks pending operations for crash recovery.

### Threading
Messages carry `thread_id` and `thread_depth` from nevermail-core. `recompute_visible()` builds `visible_indices` by filtering collapsed thread children. Space key toggles collapse on thread roots. Navigation uses visible_indices for correct up/down movement.

### Search
`/` enters search mode (replaces status bar with text input). Enter submits query to `cache.search()` (FTS5). Escape restores previous folder view.

### Compose
`c`/`r`/`f` open a full-screen compose overlay. Uses `ratatui-textarea` for the body editor. Tab cycles To/Subject/Body fields. Reply quotes the original body and sets In-Reply-To/References headers. Forward includes a forwarded header block. Ctrl-S sends via `smtp::send_email()`.

### Inline Images
On body load, image attachments (`AttachmentData::is_image()`) are decoded via the `image` crate and wrapped in a `ratatui_image::ThreadProtocol` for non-blocking resize/encode. The preview pane splits 60/40 (text/image) when an image is present. Terminal image protocol (Sixel, Kitty, iTerm2, halfblocks) is auto-detected via `Picker::from_query_stdio()` at startup, with halfblocks as fallback.

### Mouse Support
Mouse capture is enabled via crossterm. `handle_mouse()` in app.rs hit-tests click/scroll events against cached `LayoutRects` (saved by ui::render each frame). Click selects folders/messages and sets focus. Scroll wheel navigates lists or scrolls the body pane. Disabled during compose and search input modes.

## Key nevermail-core APIs Used

| API | Purpose |
|-----|---------|
| `ImapSession::connect/fetch_folders/fetch_messages/fetch_body` | IMAP operations |
| `ImapSession::set_flags` | Read/star flag toggles |
| `ImapSession::move_messages` | Trash/archive |
| `ImapSession::watch` | IMAP IDLE stream |
| `CacheHandle::save_*/load_*` | SQLite read-through/write-through cache |
| `CacheHandle::update_flags/clear_pending_op/revert_pending_op` | Optimistic flag sync |
| `CacheHandle::search` | FTS5 full-text search |
| `store::flags_to_u8/flags_from_u8` | Compact flag encoding |
| `smtp::send_email` | SMTP send |
| `mime::render_body` | HTML→plain text rendering |
| `AttachmentData::is_image` | Filter image attachments for inline display |
| `FlagOp::Set/UnSet` with `Flag::SEEN/FLAGGED` | melib flag types |
| `BackendEvent::Refresh` with `RefreshEventKind` | IDLE event types |

## Known Limitations

- Only first image attachment is rendered inline (multiple images not tiled)
- No attachment save-to-disk
- No pagination (loads up to 200 messages per folder)
- Compose doesn't support attachments
- No address book / autocomplete
- Thread view depends on core providing thread_id/thread_depth (may be empty for some IMAP servers)

## Dependencies

| Crate | Purpose |
|-------|---------|
| nevermail-core | Email engine (IMAP, SMTP, MIME, cache, config) |
| ratatui | TUI framework |
| crossterm | Terminal backend (raw mode, alternate screen, mouse, events) |
| ratatui-textarea | Multiline text editor for compose body |
| ratatui-image | Inline image rendering (Sixel, Kitty, iTerm2, halfblocks) |
| image | Image decoding (PNG, JPEG, GIF, etc.) |
| tokio | Async runtime |
| futures | Stream utilities (IMAP IDLE) |
| anyhow | Error handling |
| log / env_logger | `RUST_LOG` logging |

## Version Pinning

This crate depends on nevermail-core which depends on melib. The lockfile must pin `imap-codec` and `imap-types` to `2.0.0-alpha.4`. See [nevermail-core/CLAUDE.md](../nevermail-core/CLAUDE.md) for details and re-pin commands.

## Credentials

Same as nevermail — env vars or config file:
```bash
export NEVERMAIL_SERVER=mail.example.com
export NEVERMAIL_USER=you@example.com
export NEVERMAIL_PASSWORD=yourpassword
```

Or `~/.config/nevermail/config.json` with keyring backend. Multiple accounts supported — all resolved accounts connect on startup.
