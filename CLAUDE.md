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
Use the demo modes (synthetic data, no backend): `LITE_SIGNAL_DEMO=1`, plus `LITE_SIGNAL_DEMO_STICKERS=1|add`, `LITE_SIGNAL_DEMO_INFO=1` or `LITE_SIGNAL_DEMO_MENU=1|edit`. Layout and glyph bugs only show up visually; a successful compile proves nothing about them.

**Run them headless** (`src/headless.rs`): the user works on this PC, so never pop up a window, take focus or move the real mouse. `LITE_SIGNAL_HEADLESS="<steps>"` (or `@file`) renders offscreen with the same software renderer and fonts, feeds scripted input, and exits after the last step:
```
$env:LITE_SIGNAL_DATA="<scratch>"; $env:LITE_SIGNAL_DEMO="1"
$env:LITE_SIGNAL_HEADLESS="wait 500; wheel 650 350 6; click 960 590; type 안녕; key shift+Enter; wait 300; shot <scratch>\a.png"
.\target\debug\lite-signal.exe
```
Steps: `size W H` (logical, default 1000x680), `scale S`, `wait MS`, `move/click/rclick/dblclick X Y`, `drag X1 Y1 X2 Y2`, `wheel X Y N` (+ = up), `type TEXT`, `key [ctrl+][shift+]NAME`, `shot PATH`. Theme follows the system (`LITE_SIGNAL_HEADLESS_THEME=dark|light`). Coordinates are window-client logical px; read them off a previous shot. Not covered (needs a real window, ask the user first): minimize/restore repaint (`winhook.rs`), file dialog, notifications, clipboard, IME composition.

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
| `src/clip.rs` | Ctrl+V: files (CF_HDROP) or images (PNG / DIB streamed to PNG under `paste/`) from the clipboard |
| `src/alloc.rs` | counting global allocator + `PrivateUsage` |
| `src/headless.rs` | offscreen test platform: scripted input, PNG shots |
| `src/winhook.rs` | window-proc subclass: full repaint on restore/activate, focus events |
| `src/emoji.rs` | emoji-only message detection (grapheme clusters) |
| `ui/app.slint` | the whole UI |

## presage gotchas (learned the hard way)
- `Manager` futures are `!Send`: everything runs on the backend thread's `LocalSet` (`spawn_local`). Box large futures (`Box::pin`).
- Received and sent messages are saved by presage itself, but a sent one only after every recipient was sent to (seconds in groups: one encrypted copy per member device, sequentially). Until then `App.pending` shows a stub row (`pending_row`, status "전송 중"/"실패"); `Internal::Sent(ts, …)` drops it and reloads the window. The send timestamp is fixed up front and is the message id.
- **Receipts are filed under the sender's 1:1 thread**, never the group. Status therefore comes from our `ls_receipts(target_ts, who, level)` table: filled live in `content()`, backfilled once (`Persist.receipts_indexed`). Look statuses up by the content's sent timestamp (`ContentExt::timestamp`).
- `store.messages(thread, range)` does `fetch_all`: always bound the range (use `Index::page` first).
- presage doesn't send delivery receipts. We batch them per sender in `pending_receipts`. Read receipts are deliberately not sent.
- DB key is passed as a raw key `"x'<hex>'"`: a passphrase would trigger sqlcipher's PBKDF2 (seconds per connection). Keep `disable_statement_logging()`, otherwise sqlx logs the key; `sqlx` log level is `error`.
- `install_sticker_pack` also syncs the pack to the user's phone. Don't call it in tests.
- **Disappearing messages:** presage fills `expire_timer` on every outgoing `DataMessage` from the stored thread timer (`restore_thread_timer`), so leave it `None` when sending. It never deletes expired messages: `App::sweep` does (per-thread progress in `Persist.sweep`), and `view::build` hides expired rows right away. The timer runs from the send time (we don't track reads).
- Received deletes for everyone are applied by presage (target replaced by a `NullMessage` with a nil sender). Which deleted rows were ours comes from `ls_deleted_mine` (filled when our delete is sent or synced from our phone) or a missing `server_guid` (presage-sent rows have none). Edits are stored as `EditMessage`s and folded in `view::build` (author must match). Our own reactions, edits and deletes are `App.ops`, shown optimistically until `Internal::Sent`.
- Typing indicators: presage can't send "online only", so offline devices get them late; we drop stale incoming ones and skip sending in groups over `TYPING_MAX_GROUP` (one send per member). Group typing messages carry the group *identifier* (`group_identifier`), not the master key.

## Slint gotchas
- **Missing glyphs render as blank. There is no per-glyph font fallback.** Malgun Gothic (UI font) lacks `✓ ✕ ➤`; Segoe UI Symbol lacks Hangul. Icons use `font-family: "Segoe UI Symbol"`; the status text switches font based on its content. Check every new symbol in a screenshot.
- **Vendored renderer patch:** `vendor/i-slint-renderer-software` is Slint's software renderer 1.18.1 with a small patch, wired in via `[patch.crates-io]`. Search for "lite-signal patch": glyphs are rasterized with swash `Source::ColorOutline(0)` first and drawn as premultiplied RGBA images. That gives inline color emoji in all text from the COLR v0 layers that Segoe UI Emoji still ships (flat style). The patch also turns on TrueType **hinting** and applies a DirectWrite-style **enhanced contrast** curve (k=0.5) to glyph coverage. Without them, small Malgun Gothic text looked blurry and thin on dark backgrounds compared with native apps (compared at 3x zoom; k=0.75 looked too bold). `LITE_SIGNAL_HINT=0` / `LITE_SIGNAL_CONTRAST=<k>` override at runtime. **When upgrading Slint**, re-vendor the matching version and re-apply the hunks (fonts.rs field, vectorfont.rs render/hint/contrast + cache weight + `text_tuning`, lib.rs draw_text_vector), or the patch silently stops applying (cargo warns "patch was not used").
- **Color emoji are plain text everywhere** (reactions, emoji-only messages at 40px, inline): the patched renderer draws them in one consistent flat style. History: a DirectWrite path (3D COLRv1 style) was tried and removed. In-process it kept ~20MB of font caches (+18MB for 16 emoji, only 6MB returned on drop), it needed a child process plus a PNG cache, and it mixed two emoji styles on screen. Windows has no flag emoji (🇰🇷 renders as "KR").
- Don't use `EmbedResourcesKind::EmbedForSoftwareRenderer`: it bakes only glyphs that appear in `.slint` files, so runtime Korean text loses characters.
- A `Flickable`'s max size defaults to its content size and propagates up through layouts, which can shrink the window. Set `max-width: 100000px` on Flickables inside layouts.
- Elements inside `if` can't be referenced by id from outside. Use `visible` and min/max sizes instead (see `composer-bar`).
- Use `content-y` / `content-height` (the `viewport-*` names are deprecated).
- `FocusScope` and a `Rectangle` without a layout don't take their child's size, and an explicit `min-height` overrides what a layout child would give. The composer gets its height from `min-height: min(composer.preferred-height, 140px)` on its FocusScope.
- A read-only `TextInput` ignores right clicks, so they reach the bubble's `TouchArea` (`pointer-event`) even over text. Nested TouchAreas (image, attachment) forward right clicks themselves.
- **Images are sampled nearest-neighbor** by the software renderer (RGBA textures, any scale). A downscaled sticker gets jagged outlines. Thumbnails shown smaller than their pixels are pre-resampled in the backend to the exact physical size (`downscale`: Lanczos3 in premultiplied alpha; `types::physical_px` uses the window scale factor). Give new image views a matching `fit` size.
- **Software renderer and minimize:** it redraws only damaged regions, and UI updates arriving while minimized make it lose track of them. On restore the window was transparent except the last-changed bits. `winhook.rs` subclasses the window proc: on `WM_SIZE` restore and `WM_ACTIVATE` it flips `repaint-flip` (window background toggles between two identical-looking colors), which forces a full repaint. The same hook reports focus to the backend; polling is only a fallback. To reproduce, run `LITE_SIGNAL_DEMO=1 LITE_SIGNAL_DEMO_TICK=1`, minimize/restore with `ShowWindow`, and capture with `CopyFromScreen`. Don't use `PrintWindow` here: it forces a repaint and hides the bug.

## Conventions
- UI strings are Korean; code comments are short and English.
- Commit at sensible checkpoints without asking (user preference).
- **Keep `README.md` current** (user preference): when a user-visible feature lands or changes, update its 기능 / 사용법 / 미지원 sections in the same commit. Never push or rewrite history without asking.
- **Every experiment run sets `LITE_SIGNAL_DATA` to a scratch dir**, including demo, probe and child modes. If the flag is misparsed the app falls through to the real account, and that would make two instances share one Signal device/DB.
- Sending messages or installing packs on the user's real account is outward-facing: only the user does that. Ask them to test, then read the log (`RUST_LOG=info,spqr=warn,presage_store_sqlite=error,lite_signal=debug`).
- Prefer the Edit tool over perl/sed for multi-line or backslash-heavy edits: shell escaping has already corrupted files here (Windows paths, `\f`, `\r`).
