# CLAUDE.md

**Sigwin** (formerly lite-signal): lightweight unofficial Signal desktop client. Never use "Signal" in its name (Signal brand guidelines). Rust + presage (Signal protocol) + Slint (software renderer). Windows-first. User-facing text and replies are Korean.

## Hard constraint: memory
**Target: under 100MB.** Today: ~15MB private / ~40MB working set with a real account. Every change must respect this:
- No GPU renderer, WebView, JVM, or extra runtimes. Slint uses `renderer-software` only.
- Never load whole threads: page messages through `db::Index::page` (60 rows); the open window is capped at `MAX_WINDOW` (600).
- Images: decode to thumbnails (`THUMB_MAX` 260px, stickers in the picker 96px), cache them on disk under `thumbs/`. Keep `slint::Image`s only for what's on screen; clear them on thread switch or when the panel closes.
- Anything that loads heavy system DLLs (the file dialog) runs in a child process (`picker.rs`).
- Measure, don't guess: the log prints `mem: rust heap …, private …` every 10s (`alloc.rs` counts Rust heap; the rest is native). `SIGWIN_DEMO=1 SIGWIN_SELFTEST=1` measures the dialog and notification cost.

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
- **A running `sigwin.exe` locks its exe and the build fails with "액세스가 거부되었습니다".** The user runs `target\fast\sigwin.exe` with their real account. To ship a change: stop the process, run `cargo build --profile fast`, relaunch it. Use the **debug** build for experiments so the user's app keeps running.
- `.cargo/config.toml` pins `PROTOC`, `LIBCLANG_PATH`, and dummy `OPENSSL_*` dirs. libsqlite3-sys uses `bundled-sqlcipher-custom-crypto` (pure-Rust crypto); its Windows build script still demands OpenSSL paths, hence `build-support/no-openssl`.
- presage and presage-store-sqlite are git deps pinned to one `rev`. Bump both together, and keep the `[patch.crates-io] libsqlite3-sys` entry in sync with presage's workspace.

## Verifying UI without an account
Use the demo modes (synthetic data, no backend): `SIGWIN_DEMO=1`, plus `SIGWIN_DEMO_STICKERS=1|add`, `SIGWIN_DEMO_INFO=1`, `SIGWIN_DEMO_RELOAD=1` (uneven rows + the list re-sent every 250ms, like a live account), `SIGWIN_DEMO_OLDER=<ms>` (60 rows, older pages of 60 loaded at the top after <ms>), `SIGWIN_DEMO_MENU=1|edit|pin` or `SIGWIN_DEMO_SETTINGS=1`. Layout and glyph bugs only show up visually; a successful compile proves nothing about them.

**Run them headless** (`src/headless.rs`): the user works on this PC, so never pop up a window, take focus or move the real mouse. `SIGWIN_HEADLESS="<steps>"` (or `@file`) renders offscreen with the same software renderer and fonts, feeds scripted input, and exits after the last step:
```
$env:SIGWIN_DATA="<scratch>"; $env:SIGWIN_DEMO="1"
$env:SIGWIN_HEADLESS="wait 500; wheel 650 350 6; click 960 590; type 안녕; key shift+Enter; wait 300; shot <scratch>\a.png"
.\target\debug\sigwin.exe
```
Steps: `size W H` (logical, default 1000x680), `scale S`, `wait MS`, `move/click/rclick/dblclick X Y`, `drag X1 Y1 X2 Y2`, `wheel X Y N` (+ = up), `type TEXT`, `key [ctrl+][shift+]NAME`, `shot PATH`. Theme follows the system (`SIGWIN_HEADLESS_THEME=dark|light`). Coordinates are window-client logical px; read them off a previous shot. Not covered (needs a real window, ask the user first): minimize/restore repaint (`winhook.rs`), file dialog, notifications, clipboard, IME composition.

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
| `src/winhook.rs` | window-proc subclass: full repaint on restore/activate, focus events, re-applies the taskbar badge on `TaskbarButtonCreated` |
| `src/badge.rs` | unread count as a taskbar overlay icon (`ITaskbarList3::SetOverlayIcon`, hand-rolled vtable; GDI-drawn red circle at the DPI's small-icon size) |
| `src/link.rs` | URL detection in message text, opening links (clicks come from StyledText `link-clicked`) |
| `src/avatar.rs` | profile/group photos (thread list + header 40px, next to group messages 28px as `m:<uuid>`): fetch (presage caches them forever, so profiles are refetched every 3 days and group photos when the path changes), crop round, cache `thumbs/av-*` |
| `src/format.rs` | composing text styles with markers (`**` `_` `~~` `` ` `` `\|\|`): `parse` on send (via `mention::encode`), `to_markup` when editing, `toggle` for the Ctrl+B/I/E, Ctrl+Shift+X/B shortcuts |
| `src/emoji.rs` | emoji-only message detection (grapheme clusters) |
| `src/settings.rs` | user settings (`settings.json`, UI thread); notification mode for the backend via an atomic. Zoom = monitor DPI scale x setting, applied by dispatching `ScaleFactorChanged` (`main::apply_zoom`, re-applied by `winhook` on `WM_DPICHANGED`). Headless ignores theme and zoom |
| `src/toast.rs` | notifications: own AppUserModelID `Sigwin` registered under HKCU (name + icon), otherwise toasts show as "Windows PowerShell" |
| `ui/app.slint` | the whole UI |

## presage gotchas (learned the hard way)
- `Manager` futures are `!Send`: everything runs on the backend thread's `LocalSet` (`spawn_local`). Box large futures (`Box::pin`).
- Received and sent messages are saved by presage itself, but a sent one only after every recipient was sent to (groups: one encrypted copy per member device, one request per member; no sender keys / multi-recipient endpoint). **Vendored `vendor/libsignal-service`** (same rev presage pins, via `[patch."https://github.com/whisperfish/libsignal-service-rs"]`) has one "sigwin patch": `send_message_to_group` sends to up to `GROUP_SEND_CONCURRENCY` (16) members at once; upstream awaited them one by one (members x RTT). Re-vendor at the new rev when bumping presage. The log prints `group send done ms=…`. Until then `App.pending` shows a stub row (`pending_row`, status "전송 중"/"실패"); `Internal::Sent(ts, …)` drops it and reloads the window. The send timestamp is fixed up front and is the message id.
- **Receipts are filed under the sender's 1:1 thread**, never the group. Status therefore comes from our `ls_receipts(target_ts, who, level)` table: filled live in `content()`, backfilled once (`Persist.receipts_indexed`). Look statuses up by the content's sent timestamp (`ContentExt::timestamp`).
- `store.messages(thread, range)` does `fetch_all`: always bound the range (use `Index::page` first).
- presage doesn't send delivery receipts. We batch them per sender in `pending_receipts`. Read receipts go out (plus one `SyncMessage.read` to our devices) for incoming messages in the open window while focused (`queue_reads`, flushed with the delivery batch); `Persist.read_upto` is the per-thread watermark, advanced by read syncs from the phone too, and a thread without one only counts its `unread` messages (no flood of receipts for old history). The phone's `Configuration.read_receipts` off suppresses the receipts but not the sync.
- DB key is passed as a raw key `"x'<hex>'"`: a passphrase would trigger sqlcipher's PBKDF2 (seconds per connection). Keep `disable_statement_logging()`, otherwise sqlx logs the key; `sqlx` log level is `error`.
- `install_sticker_pack` also syncs the pack to the user's phone. Don't call it in tests.
- **Disappearing messages:** presage fills `expire_timer` on every outgoing `DataMessage` from the stored thread timer (`restore_thread_timer`), so leave it `None` when sending. It never deletes expired messages: `App::sweep` does (per-thread progress in `Persist.sweep`), and `view::build` hides expired rows right away. The timer runs from the send time (we don't track reads).
- Received deletes for everyone are applied by presage (target replaced by a `NullMessage` with a nil sender). Which deleted rows were ours comes from `ls_deleted_mine` (filled when our delete is sent or synced from our phone) or a missing `server_guid` (presage-sent rows have none). Edits are stored as `EditMessage`s and folded in `view::build` (author must match). Our own reactions, edits and deletes are `App.ops`, shown optimistically until `Internal::Sent`.
- **Pinned messages:** presage stores `PinMessage`/`UnpinMessage` data messages but keeps no pin state. Ours is `ls_pins` (thread key, target sent ts), filled live in `content()` via `record_pin`, backfilled once (`Persist.pins_indexed`); our own pins are written when sending and undone if the send fails. `load_pins` (every `emit_messages`) drops expired pins, pins of deleted messages and everything past `MAX_PINS`. Jumping to a pin loads older pages (`extend_older`) up to `MAX_WINDOW`; the UI then homes in on the row (`jump_to_row`): a ListView only instantiates rows on screen and rescales all positions after every layout, so it alternates measure / move frames and seeks with the ListView's own formula; `stick_to_bottom` stays off meanwhile.
- Typing indicators: presage can't send "online only", so offline devices get them late; we drop stale incoming ones and skip sending in groups over `TYPING_MAX_GROUP` (one send per member). Group typing messages carry the group *identifier* (`group_identifier`), not the master key.

## Slint gotchas
- **Missing glyphs render as blank. There is no per-glyph font fallback.** Malgun Gothic (UI font) lacks `✓ ✕ ➤`; Segoe UI Symbol lacks Hangul. Icons use `font-family: "Segoe UI Symbol"`; the status text switches font based on its content. Check every new symbol in a screenshot.
- **Vendored renderer patch:** `vendor/i-slint-renderer-software` is Slint's software renderer 1.18.1 with a small patch, wired in via `[patch.crates-io]`. Search for "sigwin patch": glyphs are rasterized with swash `Source::ColorOutline(0)` first and drawn as premultiplied RGBA images. That gives inline color emoji in all text from the COLR v0 layers that Segoe UI Emoji still ships (flat style). The patch also turns on TrueType **hinting** and applies a DirectWrite-style **enhanced contrast** curve (k=0.5) to glyph coverage. Without them, small Malgun Gothic text looked blurry and thin on dark backgrounds compared with native apps (compared at 3x zoom; k=0.75 looked too bold). `SIGWIN_HINT=0` / `SIGWIN_CONTRAST=<k>` override at runtime. It also keeps sub-pixel `fill_rectangle`s (text underlines) at least 1px: rounding both edges dropped the underline on every other line, and applies fontique's synthetic italic skew (`with_skew`; upstream ignored `Synthesis`, and Malgun Gothic has no italic). **When upgrading Slint**, re-vendor the matching version and re-apply the hunks (fonts.rs field, vectorfont.rs render/hint/contrast/skew + cache weight/key + `text_tuning`, lib.rs draw_glyph_run + draw_text_vector + fill_rectangle), or the patch silently stops applying (cargo warns "patch was not used").
- **Vendored core patch:** `vendor/i-slint-core` is 1.18.1 with these hunks ("sigwin patch"). Text styles (Signal's bold/italic/strike/mono/spoiler `BodyRange`s): `styled_text.rs` adds `sigwin_styled` (StyledText from byte ranges, no markdown) and `sigwin_set_plain_style`, a registry by exact text that `shaping.rs` `create_text_paragraphs` applies to plain Text / read-only TextInput, so the selectable TextInput under the StyledText overlay gets the same font styles and both lay out identically (`main::to_item` registers, cleared on thread switch). `Style::Code` is plain monospace (no capsule, same size). `draw.rs`: a color with alpha 1 draws an opaque spoiler box of its RGB instead of the glyphs; spoilers are clickable via a links entry `sigwin-spoiler:<id>`. `items/text.rs`: `StyledText` takes only mouse events over a link and ignores the rest, so a StyledText drawn over a transparent read-only TextInput gives clickable links (pointer cursor, `link-clicked`) while selection still works. Links are `SigwinStyle::Link` spans built in `main::rich_text`. `textlayout/sharedparley/shaping.rs`: the font stack is the requested family + `SIGWIN_CJK_FAMILIES` (Yu Gothic UI, Microsoft YaHei UI, Microsoft JhengHei UI) + generics. parley's script fallback picks one font per script, and for Han on Korean Windows that is Malgun Gothic again, which lacks Japanese/Chinese-only ideographs (駅 鉄 国 rendered blank). The fonts are memory-mapped and loaded only when a glyph needs them (no measurable memory cost). Re-vendor together with the renderer when upgrading Slint.
- **Color emoji are plain text everywhere** (reactions, emoji-only messages at 40px, inline): the patched renderer draws them in one consistent flat style. History: a DirectWrite path (3D COLRv1 style) was tried and removed. In-process it kept ~20MB of font caches (+18MB for 16 emoji, only 6MB returned on drop), it needed a child process plus a PNG cache, and it mixed two emoji styles on screen. Windows has no flag emoji (🇰🇷 renders as "KR").
- Don't use `EmbedResourcesKind::EmbedForSoftwareRenderer`: it bakes only glyphs that appear in `.slint` files, so runtime Korean text loses characters.
- A `Flickable`'s max size defaults to its content size and propagates up through layouts, which can shrink the window. Set `max-width: 100000px` on Flickables inside layouts.
- Elements inside `if` can't be referenced by id from outside. Use `visible` and min/max sizes instead (see `composer-bar`).
- Use `content-y` / `content-height` (the `viewport-*` names are deprecated).
- `FocusScope` and a `Rectangle` without a layout don't take their child's size, and an explicit `min-height` overrides what a layout child would give. The composer gets its height from `min-height: min(composer.preferred-height, 140px)` on its FocusScope.
- **Hover over text: make the TouchArea the parent.** A TouchArea sets `has-hover` before forwarding to its children, so wrapping the bubble content in it (forwarding the layout's min/max/preferred sizes) sees hover over the TextInput; a TouchArea as a sibling below does not (the TextInput accepts the move). See `Bubble` / `MsgTools`.
- A read-only `TextInput` ignores right clicks, so they reach the bubble's `TouchArea` (`pointer-event`) even over text. Nested TouchAreas (image, attachment) forward right clicks themselves.
- **Don't replace the message model wholesale on reloads.** `set_vec` makes the ListView drop every row and re-seek, snapping the scroll position to a row boundary. Same-thread updates go through `update_rows` (prefix/suffix diff, `set_row_data` for changed rows); only thread switches use `set_vec`.
- **Older pages keep the view in place (`hold_older`).** Loading starts two screens before the top (`check-older`), so usually the rows are inserted above the first instantiated row, which keeps the position by itself and doesn't fight a wheel scroll in progress. At the very top they don't, and shifting by the `content-height` difference is wrong (it is average x rows, and the average changes with the rows on screen). So the old first row is measured, the model replaced, the ListView seeked straight to that row's index with its own average (`set-content-y-raw`, unclamped), and the row settled at its old bottom edge via `seek_row`.
- **Wheel scrolling is a 180ms physics animation**, and `scrolled` fires when it starts, before `content-y` arrives. Anything that reacts to a scroll position (loading older history) must watch `changed content-y` too. Setting `content-y` during the animation doesn't cancel it: it keeps adding its remaining distance.
- **Images are sampled nearest-neighbor** by the software renderer (RGBA textures, any scale). A downscaled sticker gets jagged outlines. Thumbnails shown smaller than their pixels are pre-resampled in the backend to the exact physical size (`downscale`: Lanczos3 in premultiplied alpha; `types::physical_px` uses the window scale factor). Give new image views a matching `fit` size.
- **Software renderer and minimize:** it redraws only damaged regions, and UI updates arriving while minimized make it lose track of them. On restore the window was transparent except the last-changed bits. `winhook.rs` subclasses the window proc: on `WM_SIZE` restore and `WM_ACTIVATE` it flips `repaint-flip` (window background toggles between two identical-looking colors), which forces a full repaint. The same hook reports focus to the backend; polling is only a fallback. To reproduce, run `SIGWIN_DEMO=1 SIGWIN_DEMO_TICK=1`, minimize/restore with `ShowWindow`, and capture with `CopyFromScreen`. Don't use `PrintWindow` here: it forces a repaint and hides the bug.

## Vendor patches: last resort
Every hunk under `vendor/` has to be re-applied by hand on each Slint / presage upgrade, and a missed one fails silently. Keep the count down:
- **Ask the user before adding a vendor patch or a new hunk to an existing one.** Say what it fixes, the best patch-free alternative, and what that alternative costs (a lost feature, worse looks, more app code). Don't patch first and explain afterwards.
- Patch-free first: app-side code, public Slint API, layout tricks, or accepting a small UX loss. A patch is justified only for a real bug in the dependency (glyphs missing, wrong rendering) or a feature that's impossible otherwise **and** that the user wants.
- Prefer patches that fix upstream bugs (they can go upstream and disappear) over app-specific hooks into dependency internals. Never use a patch only to save app code or for a cosmetic detail.
- Keep each hunk small, marked `// sigwin patch: <why>`, and listed in this file with its reason and files, so the next upgrade knows what to re-apply.
- When a hunk becomes unnecessary (upstream fixed it, the feature went away), remove it.

## Conventions
- UI strings are Korean; code comments are short and English.
- Commit at sensible checkpoints without asking (user preference).
- **Commit subjects are short**: one line, imperative, about 50 characters or less (hard cap 72), no trailing period. Put details in the body (blank line, then wrapped lines) only when the why isn't obvious. Don't cram the whole change list into the subject.
- **Keep `README.md` current** (user preference): when a user-visible feature lands or changes, update its 기능 / 사용법 / 미지원 sections in the same commit. Never push or rewrite history without asking.
- **Every experiment run sets `SIGWIN_DATA` to a scratch dir**, including demo, probe and child modes. If the flag is misparsed the app falls through to the real account, and that would make two instances share one Signal device/DB.
- Sending messages or installing packs on the user's real account is outward-facing: only the user does that. Ask them to test, then read the log (`RUST_LOG=info,spqr=warn,presage_store_sqlite=error,sigwin=debug`).
- Prefer the Edit tool over perl/sed for multi-line or backslash-heavy edits: shell escaping has already corrupted files here (Windows paths, `\f`, `\r`).
