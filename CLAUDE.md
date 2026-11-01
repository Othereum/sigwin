# CLAUDE.md

**Sigwin** (formerly lite-signal): lightweight unofficial Signal desktop client. Never use "Signal" in its name (Signal brand guidelines). Rust + presage (Signal protocol) + Slint (software renderer). Windows-first. User-facing text and replies are Korean.

## Where things live
- `docs/architecture.md`: thread model and per-file roles. Read before touching an unfamiliar module.
- `docs/headless.md`: demo modes and headless UI verification (steps, syntax).
- `docs/decisions.md`: measurements and rejected approaches (renderer comparison, DirectWrite emoji).
- `vendor/PATCHES.md`: every vendor hunk and how to re-apply it.
- `.claude/rules/presage.md`, `.claude/rules/slint.md`: gotchas, loaded automatically when you work on the matching files. Read them before changing backend/view/db or `ui/`.

## Hard constraint: memory
**Target: under 100MB.** Today: ~15MB private / ~40MB working set with a real account. Every change must respect this:
- No GPU renderer, WebView, JVM, or extra runtimes. Slint uses `renderer-software` only (GPU renderers cost 200MB+; numbers in `docs/decisions.md`). Never enable the `renderer-femtovg` / `renderer-skia` features by default.
- Never load whole threads: page messages through `db::Index::page` (60 rows); the open window is capped at `MAX_WINDOW` (600). `store.messages(thread, range)` does `fetch_all`: always bound the range.
- Images: decode to thumbnails (`THUMB_MAX` 260px, stickers in the picker 96px), cache them on disk under `thumbs/`. Keep `slint::Image`s only for what's on screen; clear them on thread switch or when the panel closes.
- Anything that loads heavy system DLLs (the file dialog) runs in a child process (`picker.rs`).
- Measure, don't guess: the log prints `mem: rust heap …, private …` every 10s (`alloc.rs` counts Rust heap; the rest is native). `SIGWIN_DEMO=1 SIGWIN_SELFTEST=1` measures the dialog and notification cost.

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
Layout and glyph bugs only show up visually; a successful compile proves nothing about them. Use the demo modes (`SIGWIN_DEMO=1`, synthetic data, no backend) **headless**: the user works on this PC, so never pop up a window, take focus or move the real mouse. `SIGWIN_HEADLESS="<steps>"` renders offscreen and writes PNG shots (`shot PATH`). Step syntax, demo variants and what headless can't cover: `docs/headless.md`.

## Architecture in brief
UI thread (Slint) sends `Command`s to a backend thread (tokio current_thread + `LocalSet`) that owns the `!Send` presage `Manager`; results come back as `Event`s via `upgrade_in_event_loop`. `main.rs` = UI wiring, `backend.rs` = Signal logic, `view.rs` = message rows, `db.rs` = own SQL, `types.rs` = cross-thread types, `ui/app.slint` = the whole UI. Full file table: `docs/architecture.md`.

## Vendor patches: last resort
Every hunk under `vendor/` has to be re-applied by hand on each Slint / presage upgrade, and a missed one fails silently. Keep the count down:
- **Ask the user before adding a vendor patch or a new hunk to an existing one.** Say what it fixes, the best patch-free alternative, and what that alternative costs (a lost feature, worse looks, more app code). Don't patch first and explain afterwards.
- Patch-free first: app-side code, public Slint API, layout tricks, or accepting a small UX loss. A patch is justified only for a real bug in the dependency (glyphs missing, wrong rendering) or a feature that's impossible otherwise **and** that the user wants.
- Prefer patches that fix upstream bugs (they can go upstream and disappear) over app-specific hooks into dependency internals. Never use a patch only to save app code or for a cosmetic detail.
- Keep each hunk small, marked `// sigwin patch: <why>`, and listed in `vendor/PATCHES.md` with its reason and files, so the next upgrade knows what to re-apply.
- When a hunk becomes unnecessary (upstream fixed it, the feature went away), remove it from the source and from `vendor/PATCHES.md`.

## Conventions
- UI strings are Korean; code comments are short and English.
- Commit at sensible checkpoints without asking (user preference).
- **Commit subjects are short**: one line, imperative, about 50 characters or less (hard cap 72), no trailing period. Put details in the body (blank line, then wrapped lines) only when the why isn't obvious. Don't cram the whole change list into the subject.
- **Keep `README.md` current** (user preference): when a user-visible feature lands or changes, update its 기능 / 사용법 / 미지원 sections in the same commit. Never push or rewrite history without asking.
- **Every experiment run sets `SIGWIN_DATA` to a scratch dir**, including demo, probe and child modes. If the flag is misparsed the app falls through to the real account, and that would make two instances share one Signal device/DB.
- Sending messages or installing packs (`install_sticker_pack` syncs to the phone) on the user's real account is outward-facing: only the user does that. Ask them to test, then read the log (`RUST_LOG=info,spqr=warn,presage_store_sqlite=error,sigwin=debug`).
- Prefer the Edit tool over perl/sed for multi-line or backslash-heavy edits: shell escaping has already corrupted files here (Windows paths, `\f`, `\r`).
