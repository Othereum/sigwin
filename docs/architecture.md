# Architecture

```
UI thread (Slint)  --Command-->  backend thread (tokio current_thread + LocalSet)
        ^                               owns Manager<SqliteStore, Registered>  (!Send)
        +-------Event (upgrade_in_event_loop)----+
```
| File | Role |
|---|---|
| `src/main.rs` | startup: logging, data dir, settings, backend or demo, event loop |
| `src/gui.rs` | UI-only state in `thread_local! UI`, `apply(Event)` |
| `src/gui/rows.rs` | `MsgDto` / `ThreadDto` → Slint rows: rich text (styles, mentions, links, spoilers), `update_rows` diffing |
| `src/gui/scroll.rs` | message list scrolling: `stick_to_bottom`, `jump_to_row` / `seek_row`, `hold_older` |
| `src/gui/anim.rs` | animated stickers: rows report themselves on screen (`Anim.shown`), frames are asked once (`Command::StickerFrames`), kept under `ANIM_BUDGET` (least recently shown goes back to its still); picker plays only the hovered sticker (`PickerFrames`, from the `sp_*-anim.enc` bytes `open_pack` keeps). Exact frame times (µs, by the wall clock): one single-shot timer wakes at the earliest frame change and sets that sticker's shared one-element `frame` model (rows untouched); it stops while minimized |
| `src/gui/drafts.rs` | per-thread composer drafts (text, mentions, reply, files) in `drafts.json`, saved on thread switch and exit; merged into the backend's thread list ("작성 중:", sorted by draft time when newer than the last message, like Signal Desktop's `shouldShowDraft`) |
| `src/gui/wire.rs` | Slint callbacks → `Command`s, one `wire_*` per area (threads, composer, messages, stickers, settings) |
| `src/gui/window.rs` | theme, zoom, native window handle, focus hook (`hook_window`) |
| `src/demo.rs` | demo modes: fake backend with synthetic data, `SIGWIN_DEMO_*` extras, self-test |
| `src/backend.rs` | `App` state, `run` (startup, linking, receive reconnect with backoff), command dispatch, internal events, sweep of expired messages, unlink |
| `src/backend/receive.rs` | incoming content, receipts (batched delivery/read), notifications, pin/receipt index backfills |
| `src/backend/threads.rs` | thread list: names, snippets, unread, read receipts queue, avatars |
| `src/backend/window.rs` | the open thread's window: paging, jumps, `emit_messages`, header |
| `src/backend/send.rs` | sending text and attachments, pending rows, `deliver` |
| `src/backend/ops.rs` | reactions, edits, deletes, pins (`App.ops`, `load_pins`) |
| `src/backend/typing.rs` | typing indicators in and out |
| `src/backend/stickers.rs` | sticker packs: picker thumbs, grids, sending, installing |
| `src/backend/files.rs` | attachment / thumbnail caches (encrypted), opening attachments, `seal_legacy_files` |
| `src/view.rs` | `Content` → `MsgDto` rows: folds reactions and edits, receipt status, day separators, snippets |
| `src/db.rs` | own SQL on presage's DB: thread list by recency, timestamp paging, `ls_receipts` index. Our `ls_*` tables are versioned in `ls_meta` (`SCHEMA`); their `CREATE` statements stay at version 1 and changes go through `migrate` steps, since auto-updates carry old DBs forward |
| `src/types.rs` | `Command` / `Event` / DTOs crossing threads (all `Send`), thread keys `c:<uuid>` / `g:<hex>` |
| `src/secret.rs` | random 256-bit DB key, stored DPAPI-protected (`db.key`) |
| `src/picker.rs` | file dialog in a child process (`--pick-files <hwnd>`) |
| `src/instance.rs` | one process per data dir: a named event (`Local\Sigwin-<FNV hash of the dir>`); a second launch signals it (first comes to the front) and exits. A second `-quit` event lets the installer/uninstaller close a running app (normal exit path). Headless runs skip it |
| `src/install.rs` | self-install: an exe run from outside `%LOCALAPPDATA%\Sigwin\` copies itself there (running copy renamed to `.old`), writes the HKCU Uninstall entry and a Start menu shortcut, starts the installed copy and exits; never overwrites a newer installed version (`DisplayVersion`). `--uninstall` asks (data kept by default), quits the app, removes registry/shortcut/files and self-deletes. Skipped for exes under this repo's `target\`, demo and headless |
| `src/logfile.rs` | log file `<data>\logs\sigwin.log` (rotated to `.1` past 1MB) next to stderr, plus a panic hook; skipped for `--uninstall` (it may delete the data folder). Settings > 문제 해결 opens the folder |
| `src/reg.rs` | HKCU registry helpers (set/get/delete) |
| `src/update.rs` | self-update of the installed exe from GitHub Releases: own thread, a throwaway tokio runtime + reqwest per check (10s after start, then hourly by wall clock looked at every 5 min so sleep doesn't stall it, and on window focus once an hour has passed: `update::poke`; network failures retry after 5 min backing off to 1h; a release rejected by signature/missing asset is skipped until a newer tag); streams the exe to `sigwin.exe.new` while hashing, minisign check (trusted comment must be `sigwin <version>`), `self_replace`, then the sidebar restart bar. Restart spawns `--after <pid>` so the new process waits out the old one's instance lock. Release process: `docs/release.md` |
| `src/clip.rs` | Ctrl+V: files (CF_HDROP) or images (PNG / DIB streamed to PNG under `paste/`) from the clipboard |
| `src/alloc.rs` | counting global allocator + `PrivateUsage` |
| `src/headless.rs` | offscreen test platform: scripted input, PNG shots |
| `src/winhook.rs` | window-proc subclass: full repaint on restore/activate, focus events, re-applies the taskbar badge on `TaskbarButtonCreated` |
| `src/badge.rs` | unread count as a taskbar overlay icon (`ITaskbarList3::SetOverlayIcon`, hand-rolled vtable; GDI-drawn red circle at the DPI's small-icon size) |
| `src/link.rs` | URL detection in message text, opening links (clicks come from StyledText `link-clicked`) |
| `src/avatar.rs` | profile/group photos (thread list + header 40px, next to group messages 28px as `m:<uuid>`): fetch (presage caches them forever, so profiles are refetched every 3 days and group photos when the path changes), crop round, cache `thumbs/av-*` |
| `src/format.rs` | composing text styles with markers (`**` `_` `~~` `` ` `` `\|\|`): `parse` on send (via `mention::encode`), `to_markup` when editing, `toggle` for the Ctrl+B/I/E, Ctrl+Shift+X/B shortcuts |
| `src/emoji.rs` | emoji-only message detection (grapheme clusters) |
| `src/settings.rs` | user settings (`settings.json`, UI thread); notification mode for the backend via an atomic. Zoom = monitor DPI scale x setting, applied by dispatching `ScaleFactorChanged` (`gui::window::apply_zoom`, re-applied by `winhook` on `WM_DPICHANGED`). Headless ignores theme and zoom |
| `src/backend/notify.rs` | notification logic after Signal Desktop's `NotificationService`: one slot, 1s debounce (`Notifier::tick` in `flush`), nothing while focused, removed on focus / read / read sync / delete / changed reaction; reactions to our messages notify; group title "{group}의 {sender} 님"; click opens the chat and jumps to the message |
| `src/toast.rs` | WinRT toasts with the `windows` crate (notify-rust only off Windows), Signal's XML (`ToastImageAndText02`, round photo or GDI-drawn initial written as plain PNG under `open/`, IM sound). One worker thread runs show/clear in order; a launch token maps clicks back to the current toast. No COM activator: toasts are cleared at exit (`clear_now`) and at startup. Own AppUserModelID `Sigwin` registered under HKCU (name + icon), otherwise toasts show as "Windows PowerShell" |
| `crates/thumb` | decode / resample (`downscale`, `square`), animation frames (`animation`). Own crate because the `fast` profile builds `sigwin` at opt-level 0 and generic pixel code compiles in the calling crate: a 200-sticker pack resampled in 7.4s there, 0.34s here. Picker thumbs are also cached at their final size (`thumbs/sp_*-<px>px.enc`) |
| `crates/vault` | encrypted cache files: everything under `attachments/` and `thumbs/` (all named `*.enc`) goes through `sigwin_vault::read`/`write` (per-file HKDF key from the DB key, chunked AES-256-GCM). Own crate so the cipher is optimized in the `fast` profile. Opening an attachment decrypts it to `open/<random>/<name>`, emptied on exit (`main`) and at startup. `seal_legacy_files` converts older caches once per `CACHE_FORMAT` bump (`Persist.cache_format`) |
| `ui/app.slint` | `AppWindow`: properties and callbacks the Rust side uses, main layout, overlays (menu, mention popup, info). Re-exports the structs and globals for the generated code |
| `ui/types.slint`, `ui/theme.slint` | rows filled by Rust (`MsgItem`, `ThreadItem`, ...); `Theme` colors and the `Links` hook |
| `ui/bubble.slint` | message bubble (`Bubble`, `MsgMeta`, `MsgImage`, `MsgTools`) |
| `ui/widgets.slint` | small shared widgets (`Avatar`, `IconButton`, `MenuRow`, `TextButton`, `Segmented`) |
| `ui/thread_row.slint`, `ui/stickers.slint`, `ui/settings.slint`, `ui/link.slint` | thread list row, sticker panel, settings page, linking page |
