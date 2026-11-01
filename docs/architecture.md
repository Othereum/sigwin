# Architecture

```
UI thread (Slint)  --Command-->  backend thread (tokio current_thread + LocalSet)
        ^                               owns Manager<SqliteStore, Registered>  (!Send)
        +-------Event (upgrade_in_event_loop)----+
```
| File | Role |
|---|---|
| `src/main.rs` | Slint wiring, `apply(Event)`, UI-only state in `thread_local! UI`, demo modes |
| `src/backend.rs` | command handling, receive loop (reconnect with backoff), sending, attachments and thumbnails, stickers, receipts, linking |
| `src/view.rs` | `Content` → `MsgDto` rows: folds reactions and edits, receipt status, day separators, snippets |
| `src/db.rs` | own SQL on presage's DB: thread list by recency, timestamp paging, `ls_receipts` index |
| `src/types.rs` | `Command` / `Event` / DTOs crossing threads (all `Send`), thread keys `c:<uuid>` / `g:<hex>` |
| `src/secret.rs` | random 256-bit DB key, stored DPAPI-protected (`db.key`) |
| `src/picker.rs` | file dialog in a child process (`--pick-files <hwnd>`) |
| `src/clip.rs` | Ctrl+V: files (CF_HDROP) or images (PNG / DIB streamed to PNG under `paste/`) from the clipboard |
| `src/alloc.rs` | counting global allocator + `PrivateUsage` |
| `src/headless.rs` | offscreen test platform: scripted input, PNG shots |
| `src/winhook.rs` | window-proc subclass: full repaint on restore/activate, focus events, re-applies the taskbar badge on `TaskbarButtonCreated` |
| `src/badge.rs` | unread count as a taskbar overlay icon (`ITaskbarList3::SetOverlayIcon`, hand-rolled vtable; GDI-drawn red circle at the DPI's small-icon size) |
| `src/link.rs` | URL detection in message text, opening links (clicks come from StyledText `link-clicked`) |
| `src/avatar.rs` | profile/group photos (thread list + header 40px, next to group messages 28px as `m:<uuid>`): fetch (presage caches them forever, so profiles are refetched every 3 days and group photos when the path changes), crop round, cache `thumbs/av-*` |
| `src/format.rs` | composing text styles with markers (`**` `_` `~~` `` ` `` `\|\|`): `parse` on send (via `mention::encode`), `to_markup` when editing, `toggle` for the Ctrl+B/I/E, Ctrl+Shift+X/B shortcuts |
| `src/emoji.rs` | emoji-only message detection (grapheme clusters) |
| `src/settings.rs` | user settings (`settings.json`, UI thread); notification mode for the backend via an atomic. Zoom = monitor DPI scale x setting, applied by dispatching `ScaleFactorChanged` (`main::apply_zoom`, re-applied by `winhook` on `WM_DPICHANGED`). Headless ignores theme and zoom |
| `src/toast.rs` | notifications: own AppUserModelID `Sigwin` registered under HKCU (name + icon), otherwise toasts show as "Windows PowerShell" |
| `crates/thumb` | decode / resample (`downscale`, `square`). Own crate because the `fast` profile builds `sigwin` at opt-level 0 and generic pixel code compiles in the calling crate: a 200-sticker pack resampled in 7.4s there, 0.34s here. Picker thumbs are also cached at their final size (`thumbs/sp_*-<px>px.png`) |
| `crates/vault` | encrypted cache files: everything under `attachments/` (`<key>.enc`) and `thumbs/` goes through `sigwin_vault::read`/`write` (per-file HKDF key from the DB key, chunked AES-256-GCM). Own crate so the cipher is optimized in the `fast` profile. Opening an attachment decrypts it to `open/<random>/<name>`, emptied on exit (`main`) and at startup. `seal_legacy_files` converts pre-encryption caches once (`Persist.files_sealed`) |
| `ui/app.slint` | the whole UI |
