# Architecture

How the code is organized and why. Exact values and behavior live in the code and its comments, starting with each file's module doc.

## Threads
```
UI thread (Slint)  --Command-->  backend thread (tokio current_thread + LocalSet)
        ^                               owns Manager<SqliteStore, Registered>  (!Send)
        +-------Event (upgrade_in_event_loop)----+
```
The UI thread never touches Signal or the database. Everything crossing between the threads is a `Send` type from `src/types.rs`. Slow work with no state of its own (updates, toasts, resampling, decoding) gets its own thread or a blocking task.

## Design
- **Memory follows what's on screen.** Messages are read in pages and the open window has a cap, but it moves: scrolling or jumping past its end lets go of the rows at the other end, so all of history stays reachable while memory doesn't grow with a thread. What was loaded for rows that left (thumbnails, long texts) goes with them, and once the user has stayed back at the newest messages for a while the window shrinks to a page again. The UI holds images for the open thread only, and a window hidden for a while drops those too. The image viewer is its own window, so all of it goes when it closes. Why this is the shape of the budget: `docs/decisions.md`.
- **Caches are encrypted on disk.** Attachments and thumbnails only reach the disk through `crates/vault`. Plain copies exist only in `open/`, for outside apps, and are removed.
- **Heavy system DLLs stay out of the process.** File dialogs run in a child process of the same exe.
- **Own SQL beside presage.** Our `ls_*` tables add what presage's store doesn't offer. They are versioned and migrated, since auto-updates carry old databases forward.
- **One installed exe.** It installs and updates itself in place, without admin rights, so the shortcut, autostart entry and updater all point at the same file.
- **Separate crates keep the `fast` profile usable.** It builds the main crate at opt-level 0 because of Slint's generated code. Pixel work and the cipher have their own crates so they stay optimized, and the generated UI has its own so Rust-only edits don't reparse it.

## Files
| File | Role |
|---|---|
| `src/main.rs` | startup: logging, data dir, settings, backend or demo, event loop |
| `src/types.rs` | `Command` / `Event` / DTOs crossing threads, thread keys |
| `src/demo.rs` | demo modes: fake backend with synthetic data, self-test |
| `src/headless.rs` | offscreen test platform: scripted input, PNG shots, scenario steps |
| `src/alloc.rs` | counting global allocator, private memory |

### UI
| File | Role |
|---|---|
| `src/gui.rs` | UI-only state, `apply(Event)` |
| `src/gui/wire.rs` | Slint callbacks → `Command`s, per area |
| `src/gui/notice.rs` | short notices and the connection banner |
| `src/gui/rows.rs` | DTOs → Slint rows: rich text, row diffing |
| `src/gui/media.rs` | thumbnails, their labels and photos arriving for rows already shown |
| `src/gui/stickers.rs` | sticker panel: pack tabs, the open pack's grid |
| `src/gui/scroll.rs` | message list scrolling and jumps |
| `src/gui/anim.rs` | animated stickers: frames under a memory budget, the timer that flips them |
| `src/gui/drafts.rs` | per-thread composer drafts |
| `src/gui/linkcard.rs` | the link preview above the composer |
| `src/gui/viewer.rs` | image viewer window |
| `src/gui/window.rs` | theme, zoom, native window handle, releasing memory while hidden |
| `src/away.rs` | whether the user is away (no input for a while, screen locked or off) and whether the window can be seen (not covered) |
| `src/winhook.rs` | window-proc subclass: repaint on restore, focus events, close to tray |
| `src/format.rs` | composing text styles with markers |
| `src/mention.rs` | composing @mentions: finding the query, filtering members, Signal's wire form |
| `src/link.rs` | URL detection in message text, opening links |
| `src/emoji.rs` | emoji-only message detection, the reaction picker's emoji list and recent row |
| `src/clip.rs` | pasting files and images from the clipboard |
| `src/picker.rs` | file dialogs in a child process |
| `src/settings.rs` | user settings, owned by the UI thread |
| `src/badge.rs` | unread count on the taskbar and tray icons |
| `src/tray.rs` | notification-area icon |
| `src/toast.rs` | Windows toasts on their own worker thread |

### Backend
| File | Role |
|---|---|
| `src/backend.rs` | `App` state, connection and receive loop, command dispatch, sweep of expired messages, unlink |
| `src/backend/receive.rs` | incoming content, receipts (`Outgoing`: what goes out is decided apart from sending it), index backfills |
| `src/backend/fixture.rs` | backend without an account or network, for scenarios: seeded and received messages, what would go out kept for checks; the in-process `Harness` |
| `src/backend/scenarios.rs` | backend scenarios (`cargo test`) on the `Harness` |
| `src/gui/scenario.rs` | scenario steps of the headless mode: drive the window, check its values |
| `tests/scenarios/*.sc`, `tests/scenarios.rs` | UI + backend scenarios: headless window on the offline backend, one run per file |
| `src/backend/threads.rs` | thread list: names, snippets, unread, read receipts queue |
| `src/backend/window.rs` | the open thread's window: paging, jumps, header |
| `src/backend/send.rs` | sending text and attachments, pending rows |
| `src/backend/linkpreview.rs` | link previews on outgoing messages: fetching, attaching, stopping presage's own fetch |
| `src/backend/ops.rs` | reactions, edits, deletes, pins |
| `src/backend/typing.rs` | typing indicators |
| `src/backend/stickers.rs` | sticker packs: picker, sending, installing |
| `src/backend/files.rs` | attachment and thumbnail caches, opening and viewing attachments |
| `src/videoframe.rs` | a video's first frame, decoded by Media Foundation in a child process |
| `src/backend/mute.rs` | per-thread notification settings, the phone's values from the Storage Service |
| `src/backend/notify.rs` | when notifications show and go away, after Signal Desktop |
| `src/view.rs` | stored content → message rows: reactions, edits, receipts, day separators, snippets |
| `src/db.rs` | own SQL on presage's DB: thread list, paging in display order, `ls_*` tables and migrations |
| `src/avatar.rs` | profile and group photos: fetch, crop round, cache |
| `src/secret.rs` | the DB key, DPAPI-protected |

### App lifecycle
| File | Role |
|---|---|
| `src/instance.rs` | one process per data dir |
| `src/install.rs` | self-install and uninstall |
| `src/update.rs` | self-update from GitHub Releases (process: `docs/release.md`) |
| `src/whatsnew.rs` | the release notes embedded in the exe, their parser, when to offer them (`gui/whatsnew.rs` shows) |
| `src/autostart.rs` | start with Windows |
| `src/logfile.rs` | log file and panic hook |
| `src/notices.rs` | embedded third-party license notices |
| `src/reg.rs` | HKCU registry helpers |

### Crates
| Crate | Role |
|---|---|
| `crates/thumb` | image decoding, resampling, animation frames |
| `crates/vault` | encrypted cache files |
| `crates/ui` | compiles `ui/app.slint` and holds the generated code |

### Slint UI
| File | Role |
|---|---|
| `ui/app.slint` | `AppWindow`: what Rust uses, main layout, overlays |
| `ui/types.slint`, `ui/theme.slint` | rows filled by Rust, colors and the `Links` hook |
| `ui/bubble.slint` | message bubble |
| `ui/reactions.slint` | reaction buttons and the emoji picker of the message menu |
| `ui/viewer.slint` | image viewer window |
| `ui/widgets.slint` | small shared widgets |
| `ui/thread_row.slint`, `ui/stickers.slint`, `ui/settings.slint`, `ui/link.slint` | thread list row, sticker panel, settings page, linking page |
