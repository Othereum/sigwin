---
name: real-db-probe
description: Read the real account's encrypted Sigwin DB read-only and print only counts, ages and shapes, never values. Use this whenever a Sigwin bug or question depends on what's actually stored and the demo modes can't show it, for example "this only happens in my 1:1 chats", how many rows or messages a thread has, what kinds of rows dominate, whether a pin, edit or reaction was indexed, or memory that varies by thread. Also for "실제 DB 확인", "내 계정 데이터 확인", "행 개수 세어 봐". Don't use it for logs, or to read message contents, which isn't allowed at all.
---

# Real DB probe

Answers questions about the real `%LOCALAPPDATA%\Sigwin\data\signal.db3` with a throwaway cargo example. The app's own rule applies: real account data stays out of the work, so only shapes and counts ever leave the program.

Try the demo modes first (`docs/headless.md`). The real DB is for what depends on real data: how threads are actually made up, what got indexed.

## What may be printed

Printed output ends up in the conversation, so treat it as leaving the machine.

- **Allowed:** counts, sizes, durations, ages, sent timestamps, field numbers, row kinds, schema (column and table names), and a thread's position in a list.
- **Never:** message text, names, phone numbers, UUIDs, group or thread keys, the DB key, raw `content_body` or any other blob, and URLs or filenames from messages.

When in doubt whether a value identifies someone, print its length instead. A `{:?}` of a row or a struct can carry any of these, so print the fields you chose, one by one. Say which thread you mean by its `pos`, not its DB id, and let the user tell you which room that is rather than trying to work it out from contents.

## Steps

1. Copy `assets/db_probe.rs` (next to this file) to `examples/db_probe.rs`. It handles the key, the read-only connection and the protobuf field walking.
2. Edit only `report()`, plus helpers it needs, to ask your question. `threads()` and `kinds_newest_first()` cover most of what comes up. Open question? Run it unedited first: the default overview often answers it.
3. Stop the app and run it, following the build rule in `CLAUDE.local.md`. The app holds the DB in WAL mode, so reading alongside it works, but a build rule is a build rule.
   ```
   cargo run --profile fast --example db_probe
   ```
4. Delete it in a separate command, before anything else: `rm -rf examples target/fast/examples/db_probe*` from Bash. Then check `git status --short` shows no `examples/`.
5. If the app was running, start it again: the user may be relying on it.
6. Report numbers with what they mean, and say that the contents weren't read.

## Things to know

- **Row kinds are approximate.** `kind()` goes by protobuf field numbers, without the app's types. It's close to `view::classify` but not the same: a synced edit counts as `sent-sync`, for instance, so don't quote its counts as exact. Say so if the difference matters.
- **Rows aren't messages.** A 1:1 thread is mostly receipts (they're filed under the sender's thread, even for group messages), so its newest 600 rows can hold no message at all. Display order is `ls_order.seq`, not `ts`.
- **Schema:** `thread_messages` is presage's (`ts`, `thread_id`, `content_body`, ...); `ls_*` tables are ours, created in `Index::open` (`src/db.rs`). `ls_pins`, `ls_edits` and `ls_reactions` have `thread` as the thread key string, which is a secret for groups: filter by it if you must, but don't print it.
- **Read-only isn't optional.** Keep `.read_only(true)`. Fixing data on a real account isn't what this is for; if data looks wrong, report it and fix the code that wrote it.
- **The exit.** `main` ends in `process::exit(0)` because dropping the connection crashes (`0xc0000005`) after a good run. A crash at the end of a run of yours that removed that line says nothing about the results.
