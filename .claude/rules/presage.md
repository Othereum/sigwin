---
paths:
  - src/backend.rs
  - src/backend/**
  - src/view.rs
  - src/db.rs
  - src/types.rs
  - src/avatar.rs
  - vendor/libsignal-service/**
---

# presage gotchas (learned the hard way)

- `Manager` futures are `!Send`: everything runs on the backend thread's `LocalSet` (`spawn_local`). Box large futures (`Box::pin`).
- Received and sent messages are saved by presage itself, but a sent one only after every recipient was sent to (groups: one encrypted copy per member device, one request per member; no sender keys / multi-recipient endpoint). The vendored `libsignal-service` patch (see `vendor/PATCHES.md`) sends to up to `GROUP_SEND_CONCURRENCY` (16) members at once. The log prints `group send done ms=…`. Until the save, `App.pending` shows a stub row (`pending_row`, status "전송 중"/"실패"); `Internal::Sent(ts, …)` drops it and reloads the window. The send timestamp is fixed up front and is the message id.
- **Receipts are filed under the sender's 1:1 thread**, never the group. Status therefore comes from our `ls_receipts(target_ts, who, level)` table: filled live in `content()`, backfilled once (`Persist.receipts_indexed`). Look statuses up by the content's sent timestamp (`ContentExt::timestamp`).
- **Display order is `ls_order.seq`, not `ts`.** `ts` is the sender's clock (and still the message id); `seq` is the stored order, assigned by triggers on presage's `thread_messages` (`db::ensure_order`), so the window isn't sorted by `ts` and a `ts` range doesn't describe it. A sent message is stored when its send ends, so it lands after what arrived meanwhile; sends per thread are queued (`send_turn`) to keep our own in order. A presage migration rebuilding `thread_messages` drops the triggers: `Index::open` restores them and fills the gaps.
- `store.messages(thread, range)` does `fetch_all`: always bound the range (use `Index::page` first, then `fetch_range`, which falls back to single reads when one wrong clock stretches the range).
- presage doesn't send delivery receipts. We batch them per sender in `pending_receipts`. Read receipts go out (plus one `SyncMessage.read` to our devices) for incoming messages in the open window while focused (`queue_reads`, flushed with the delivery batch); `Persist.read_upto` is the per-thread watermark, advanced by read syncs from the phone too, and a thread without one only counts its `unread` messages (no flood of receipts for old history). The phone's `Configuration.read_receipts` off suppresses the receipts but not the sync.
- DB key is passed as a raw key `"x'<hex>'"`: a passphrase would trigger sqlcipher's PBKDF2 (seconds per connection). Keep `disable_statement_logging()`, otherwise sqlx logs the key; `sqlx` log level is `error`.
- `install_sticker_pack` also syncs the pack to the user's phone. Don't call it in tests.
- **Disappearing messages:** presage fills `expire_timer` on every outgoing `DataMessage` from the stored thread timer (`restore_thread_timer`), so leave it `None` when sending. It never deletes expired messages: `App::sweep` does (per-thread progress in `Persist.sweep`), and `view::build` hides expired rows right away. The timer runs from the send time (we don't track reads).
- Received deletes for everyone are applied by presage (target replaced by a `NullMessage` with a nil sender). Which deleted rows were ours comes from `ls_deleted_mine` (filled when our delete is sent or synced from our phone) or a missing `server_guid` (presage-sent rows have none). Edits are stored as `EditMessage`s and folded in `view::build` (author must match). Our own reactions, edits and deletes are `App.ops`, shown optimistically until `Internal::Sent`.
- **Pinned messages:** presage stores `PinMessage`/`UnpinMessage` data messages but keeps no pin state. Ours is `ls_pins` (thread key, target sent ts), filled live in `content()` via `record_pin`, backfilled once (`Persist.pins_indexed`); our own pins are written when sending and undone if the send fails. `load_pins` (every `emit_messages`) drops expired pins, pins of deleted messages and everything past `MAX_PINS`. Jumping to a pin loads older pages (`extend_older`) up to `MAX_WINDOW`; the UI then homes in on the row (`jump_to_row`): a ListView only instantiates rows on screen and rescales all positions after every layout, so it alternates measure / move frames and seeks with the ListView's own formula; `stick_to_bottom` stays off meanwhile.
- Typing indicators: presage can't send "online only", so offline devices get them late; we drop stale incoming ones and skip sending in groups over `TYPING_MAX_GROUP` (one send per member). Group typing messages carry the group *identifier* (`group_identifier`), not the master key.
- **Our `ls_*` tables outlive builds** (installed apps auto-update and keep their DB; `CREATE TABLE IF NOT EXISTS` doesn't touch an existing table). New table: just add its `CREATE`. Changing an existing one: leave its `CREATE` alone, add a `db::step` and bump `db::SCHEMA`; for rebuildable tables (receipts, pins, packs, stickers) prefer drop + recreate + re-run the backfill. `ls_deleted_mine` can't be rebuilt: carry its data over.
