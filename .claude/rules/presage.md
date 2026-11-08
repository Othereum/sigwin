---
paths:
  - src/backend.rs
  - src/backend/**
  - src/view.rs
  - src/view/**
  - src/db.rs
  - src/types.rs
  - src/avatar.rs
  - vendor/libsignal-service/**
---

# presage gotchas (learned the hard way)

What presage and the store do that bites, and what our code must keep true because of it. How our code does it lives in its comments (names given here to find them).

- `Manager` futures are `!Send`: everything runs on the backend thread's `LocalSet` (`spawn_local`). Box large futures (`Box::pin`).
- **presage stores a sent message only after every recipient was sent to** (groups: one request per member, no sender keys). Until then it exists only in memory (`App.pending`). If one member fails, presage returns the error and stores nothing, though others may have it: a failed row may be partly delivered, so a retry must keep the timestamp (it's the message id; recipients drop the copy by sender + sent timestamp).
- **Receipts are filed under the sender's 1:1 thread**, never the group. Statuses come from our `ls_receipts`, looked up by the content's sent timestamp (`ContentExt::timestamp`).
- **Display order is `ls_order.seq`, not `ts`.** `ts` is the sender's clock (and the message id); `seq` is the stored order, kept by triggers on presage's `thread_messages` (`db::ensure_order`). A `ts` range doesn't describe the window. A sent message is stored when its send ends, so our own sends per thread must stay serialized (`send_turn`). A presage migration that rebuilds `thread_messages` drops the triggers: `Index::open` restores them.
- `store.messages(thread, range)` does `fetch_all`: always bound the range (`Index::page`, then `fetch_range`).
- **presage sends neither delivery nor read receipts; we do.** Read receipts go out only for what the user actually saw (rows on screen in the open thread), never for a whole thread on open or for old history. The phone's read-receipts setting off suppresses the receipts, not the read sync to our devices. See `backend/threads.rs` (`read_through`, `queue_reads`).
- DB key is passed as a raw key `"x'<hex>'"`: a passphrase would trigger sqlcipher's PBKDF2 (seconds per connection). Keep `disable_statement_logging()`, otherwise sqlx logs the key; `sqlx` log level is `error`.
- **Session state is read-modify-write across awaits.** libsignal has no lock of its own, so anything loading then saving a session must hold `cipher::SESSION_LOCK` (vendor patch) and must not hold it across the network. Overlapping tasks don't fail where they overlap: the peer fails to read a later message.
- **Disappearing messages:** presage fills `expire_timer` on every outgoing `DataMessage` from the stored thread timer, so leave it `None` when sending. It never deletes expired messages: we do (`App::sweep`), and `view::build` must hide them right away. The timer runs from the send time.
- **Deletes and edits:** presage applies a received delete for everyone by replacing the target in place with a `NullMessage` (nil sender); it doesn't move the thread's last row, so anything cached from the target (thread list preview) must be dropped (`forget_snippet`). Which deleted rows were ours comes from `ls_deleted_mine` or a missing `server_guid`. Edits are stored as `EditMessage`s and folded in `view::build` (author must match).
- **presage keeps no pin state**: it stores `PinMessage`/`UnpinMessage` only. Ours is `ls_pins`. A pin's target may be outside the window, so its edited text can't come from `view::build`'s folding: `ls_edits` says where the newest edit is.
- **The window can be anywhere in the thread.** `Open.end_seq` is `Some` when it stops short of the newest message: new messages don't join it, and code assuming it ends at the newest (stub rows, reloads, typing) must check. Bound every read by it, never `rows_from` alone.
- **Rows aren't messages.** A 1:1 thread is mostly receipts: hundreds of its newest rows can hold no message at all, so counting rows in `ls_order` says nothing about messages, and a seq range can be far larger than the window (`Open.window` holds only rows that show).
- **What changes a message is stored after it, outside the window.** Reactions come from `ls_reactions`, edits through `ls_edits` (`view::Indexed`); an edit's `ContentExt::timestamp` is its target's and it has no row of its own. Replies are the other way round: only the reply holds the quote, so `ls_replies` (ids only) gives reply counts and must forget replies deleted for everyone or expired.
- **presage fetches link previews by itself:** `send_message` / `send_message_to_group` contact the first link of any outgoing `DataMessage` whose `preview` is empty, directly, with no switch. So every body with a link must carry `preview` (`backend::linkpreview::for_message`: ours or `no_preview`, whose `url` must be present even if empty, because Signal iOS drops a whole message whose preview has no url field). Any new way of sending a body needs the same.
- Typing indicators: presage can't send "online only", so offline devices get them late. Group typing messages carry the group *identifier* (`group_identifier`), not the master key.
- **Our `ls_*` tables outlive builds** (installed apps auto-update and keep their DB; `CREATE TABLE IF NOT EXISTS` doesn't touch an existing table). New table: just add its `CREATE`. Changing an existing one: leave its `CREATE` alone, add a `db::step` and bump `db::SCHEMA`; for rebuildable tables (receipts, pins, edits, packs, stickers) prefer drop + recreate + re-run the backfill. `ls_deleted_mine` can't be rebuilt: carry its data over.
