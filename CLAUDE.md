# CLAUDE.md

Lightweight unofficial Signal desktop client: Rust + presage (Signal protocol) + Slint (software renderer). Windows-first. User-facing text and replies are Korean.

## Hard constraint: memory
**Target: under 100MB.** Today: ~15MB private / ~40MB working set with a real account. Every change must respect this:
- No GPU renderer, WebView, JVM, or extra runtimes. Slint uses `renderer-software` only.
- Never load whole threads: page messages through `db::Index::page` (60 rows); the open window is capped at `MAX_WINDOW` (600).
- Images: decode to thumbnails (`THUMB_MAX` 260px, stickers in the picker 96px), cache them on disk under `thumbs/`. Keep `slint::Image`s only for what's on screen; clear them on thread switch or when the panel closes.
- Anything that loads heavy system DLLs (the file dialog) runs in a child process (`picker.rs`).
- Measure, don't guess: the log prints `mem: rust heap …, private …` every 10s (`alloc.rs` counts Rust heap; the rest is native). `LITE_SIGNAL_DEMO=1 LITE_SIGNAL_SELFTEST=1` measures the dialog and notification cost.

### Why the software renderer (measured 2026-11-01, demo data, NVIDIA GPU)
| renderer | private | working set | CPU for 80 scroll notches | color emoji | ✓✕➤ fallback |
|---|---|---|---|---|---|
| software (software-only build) | **8.4MB** | 28MB | **750ms** | no (monochrome) | no (blank) |
| femtovg (OpenGL) | 212MB | 157MB | 2328ms | – | – |
| skia | 388MB | 252MB | 1641ms | yes | no (tofu) |

GPU renderers load the graphics driver into the process (nvoglv64, d3d12): they blow the 100MB budget by themselves, and they are not cheaper on CPU either. Only compiling the GPU renderers in, even when running software, already costs ~54MB private. The `renderer-femtovg` / `renderer-skia` cargo features exist only for re-measuring (`SLINT_BACKEND=winit-skia`); never enable them by default.

## Build & run
```
cargo build --profile fast     # day-to-day: optimized, no LTO, incremental -> target\fast\
cargo build                    # debug: quickest for UI experiments -> target\debug\
cargo build --release          # distribution only (~2.5 min, thin LTO)
cargo test
```
- **A running `lite-signal.exe` locks its exe and the build fails with "액세스가 거부되었습니다".** The user runs `target\fast\lite-signal.exe` with their real account. To ship a change: stop the process, run `cargo build --profile fast`, relaunch it. Use the **debug** build for experiments so the user's app keeps running.
- `.cargo/config.toml` pins `PROTOC`, `LIBCLANG_PATH`, and dummy `OPENSSL_*` dirs. libsqlite3-sys uses `bundled-sqlcipher-custom-crypto` (pure-Rust crypto); its Windows build script still demands OpenSSL paths, hence `build-support/no-openssl`.
- presage and presage-store-sqlite are git deps pinned to one `rev`. Bump both together, and keep the `[patch.crates-io] libsqlite3-sys` entry in sync with presage's workspace.

## Verifying UI without an account
Use the demo modes (synthetic data, no backend): `LITE_SIGNAL_DEMO=1`, plus `LITE_SIGNAL_DEMO_STICKERS=1|add` or `LITE_SIGNAL_DEMO_INFO=1`. Take a screenshot of the window (PrintWindow via PowerShell `System.Drawing`) and look at it. Layout and glyph bugs only show up visually; a successful compile proves nothing about them.

## Architecture
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
| `src/alloc.rs` | counting global allocator + `PrivateUsage` |
| `src/winhook.rs` | window-proc subclass: full repaint on restore/activate, focus events |
| `ui/app.slint` | the whole UI |

## presage gotchas (learned the hard way)
- `Manager` futures are `!Send`: everything runs on the backend thread's `LocalSet` (`spawn_local`). Box large futures (`Box::pin`).
- Received and sent messages are saved by presage itself. After sending, just reload the window (`current_dirty`).
- **Receipts are filed under the sender's 1:1 thread**, never the group. Status therefore comes from our `ls_receipts(target_ts, who, level)` table: filled live in `content()`, backfilled once (`Persist.receipts_indexed`). Look statuses up by the content's sent timestamp (`ContentExt::timestamp`).
- `store.messages(thread, range)` does `fetch_all`: always bound the range (use `Index::page` first).
- presage doesn't send delivery receipts. We batch them per sender in `pending_receipts`. Read receipts are deliberately not sent.
- DB key is passed as a raw key `"x'<hex>'"`: a passphrase would trigger sqlcipher's PBKDF2 (seconds per connection). Keep `disable_statement_logging()`, otherwise sqlx logs the key; `sqlx` log level is `error`.
- `install_sticker_pack` also syncs the pack to the user's phone. Don't call it in tests.

## Slint gotchas
- **Missing glyphs render as blank. There is no per-glyph font fallback.** Malgun Gothic (UI font) lacks `✓ ✕ ➤`; Segoe UI Symbol lacks Hangul. Icons use `font-family: "Segoe UI Symbol"`; the status text switches font based on its content. Check every new symbol in a screenshot.
- Don't use `EmbedResourcesKind::EmbedForSoftwareRenderer`: it bakes only glyphs that appear in `.slint` files, so runtime Korean text loses characters.
- A `Flickable`'s max size defaults to its content size and propagates up through layouts, which can shrink the window. Set `max-width: 100000px` on Flickables inside layouts.
- Elements inside `if` can't be referenced by id from outside. Use `visible` and min/max sizes instead (see `composer-bar`).
- Use `content-y` / `content-height` (the `viewport-*` names are deprecated).
- **Software renderer and minimize:** it redraws only damaged regions, and UI updates arriving while minimized make it lose track of them. On restore the window was transparent except the last-changed bits. `winhook.rs` subclasses the window proc: on `WM_SIZE` restore and `WM_ACTIVATE` it flips `repaint-flip` (window background toggles between two identical-looking colors), which forces a full repaint. The same hook reports focus to the backend; polling is only a fallback. To reproduce, run `LITE_SIGNAL_DEMO=1 LITE_SIGNAL_DEMO_TICK=1`, minimize/restore with `ShowWindow`, and capture with `CopyFromScreen`. Don't use `PrintWindow` here: it forces a repaint and hides the bug.

## Conventions
- UI strings are Korean; code comments are short and English.
- Commit at sensible checkpoints without asking (user preference). Never push or rewrite history without asking.
- Sending messages or installing packs on the user's real account is outward-facing: only the user does that. Ask them to test, then read the log (`RUST_LOG=info,spqr=warn,presage_store_sqlite=error,lite_signal=debug`).
- Prefer the Edit tool over perl/sed for multi-line or backslash-heavy edits: shell escaping has already corrupted files here (Windows paths, `\f`, `\r`).
