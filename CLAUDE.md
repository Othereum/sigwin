# CLAUDE.md

**Sigwin**: lightweight unofficial Signal desktop client. Never use "Signal" in its name (Signal brand guidelines). Rust + presage (Signal protocol) + Slint (software renderer). Windows-first. User-facing text is Korean.

## Where things live
- `docs/architecture.md`: thread model and per-file roles. Read before touching an unfamiliar module.
- `docs/headless.md`: demo modes and headless UI verification (steps, syntax).
- `docs/release.md`: publishing a release, the update signing key.
- `docs/decisions.md`: measurements and rejected approaches (renderer comparison, DirectWrite emoji).
- `vendor/PATCHES.md`: every vendor hunk and how to re-apply it.
- `.claude/rules/presage.md`, `.claude/rules/slint.md`: gotchas, loaded automatically when you work on the matching files. Read them before changing backend/view/db or `ui/`.

## Hard constraint: memory
**Target: under 100MB.** Today: ~15MB private / ~40MB working set with a real account. Every change must respect this:
- No GPU renderer, WebView, JVM, or extra runtimes. Slint uses `renderer-software` only (GPU renderers cost 200MB+; numbers in `docs/decisions.md`). Never enable the `renderer-femtovg` / `renderer-skia` features by default.
- Never load whole threads: page messages through `db::Index::page` (60 rows); the open window is capped at `MAX_WINDOW` (600). `store.messages(thread, range)` does `fetch_all`: always bound the range.
- Images: decode to thumbnails (`THUMB_MAX` 260px, stickers in the picker 96px), cache them on disk under `thumbs/`. Keep `slint::Image`s only for what's on screen; clear them on thread switch or when the panel closes.
- Animated sticker frames are decoded RGBA, so they're the costliest images: only for stickers shown on screen, all together under `ANIM_BUDGET` (24MB), and only the hovered sticker in the picker (`gui/anim.rs`).
- Anything that loads heavy system DLLs (the file dialog) runs in a child process (`picker.rs`).
- Measure, don't guess: the log prints `mem: rust heap …, private …` every 10s at debug level (`sigwin=debug`; info only every 10 min, for users' log files) (`alloc.rs` counts Rust heap; the rest is native). `SIGWIN_DEMO=1 SIGWIN_SELFTEST=1` measures the dialog and notification cost.

## Build & run
```
cargo build --profile fast     # day-to-day, compile checks too: optimized, no LTO, incremental -> target\fast\
cargo build                    # debug (dev profile): only for a debugger; rebuilds slower than fast here -> target\debug\
cargo build --release          # distribution only (~2.5 min, thin LTO)
cargo test
```
- **A running `sigwin.exe` locks its exe and the build fails with "액세스가 거부되었습니다".** Stop it first, or build a different profile.
- `protoc` comes from `PATH`. `.cargo/config.toml` defaults `LIBCLANG_PATH` (overridable by env) and pins dummy `OPENSSL_*` dirs. libsqlite3-sys uses `bundled-sqlcipher-custom-crypto` (pure-Rust crypto); its Windows build script still demands OpenSSL paths, hence `build-support/no-openssl`.
- presage and presage-store-sqlite are git deps pinned to one `rev`. Bump both together, and keep the `[patch.crates-io] libsqlite3-sys` entry in sync with presage's workspace.

## Verifying UI without an account
Layout and glyph bugs only show up visually; a successful compile proves nothing about them. The demo modes (`SIGWIN_DEMO=1`, synthetic data, no backend) can run **headless**, without popping up a window, taking focus or moving the real mouse: `SIGWIN_HEADLESS="<steps>"` renders offscreen and writes PNG shots (`shot PATH`). Step syntax, demo variants and what headless can't cover: `docs/headless.md`.

## Architecture in brief
UI thread (Slint) sends `Command`s to a backend thread (tokio current_thread + `LocalSet`) that owns the `!Send` presage `Manager`; results come back as `Event`s via `upgrade_in_event_loop`. `gui/` = UI wiring and rows, `backend/` = Signal logic, `view.rs` = message rows, `db.rs` = own SQL, `types.rs` = cross-thread types, `ui/` = Slint UI (`app.slint` is the window). Full file table: `docs/architecture.md`.

## Vendor patches: last resort
Every hunk under `vendor/` has to be re-applied by hand on each Slint / presage upgrade, and a missed one fails silently. Keep the count down:
- **Ask the user before adding a vendor patch or a new hunk to an existing one.** Say what it fixes, the best patch-free alternative, and what that alternative costs (a lost feature, worse looks, more app code). Don't patch first and explain afterwards.
- Patch-free first: app-side code, public Slint API, layout tricks, or accepting a small UX loss. A patch is justified only for a real bug in the dependency (glyphs missing, wrong rendering) or a feature that's impossible otherwise **and** that the user wants.
- Prefer patches that fix upstream bugs (they can go upstream and disappear) over app-specific hooks into dependency internals. Never use a patch only to save app code or for a cosmetic detail.
- Keep each hunk small, marked `// sigwin patch: <why>`, and listed in `vendor/PATCHES.md` with its reason and files, so the next upgrade knows what to re-apply.
- When a hunk becomes unnecessary (upstream fixed it, the feature went away), remove it from the source and from `vendor/PATCHES.md`.

## Conventions
- UI strings are Korean; code comments are short and English.
- **Commit subjects are short**: one line, imperative, about 50 characters or less (hard cap 72), no trailing period. Put details in the body (blank line, then wrapped lines) only when the why isn't obvious. Don't cram the whole change list into the subject.
- **Keep `README.md` current**: when a user-visible feature lands or changes, update its 기능 / 사용법 / 미지원 sections in the same commit.
- **Keep `README.md` concise**: always short. Don't describe detailed behavior there (that belongs in `docs/`).
- **Demo, probe and child runs set `SIGWIN_DATA` to a scratch dir.** The instance lock is per data dir: without it a demo either brings the real window to the front and exits, or runs on the real data dir (writes its settings and drafts, rewrites the autostart entry to this exe, mixes logs). Headless enforces it in code.
- Sending messages or installing packs (`install_sticker_pack` syncs to the phone) on a real account is outward-facing: never do it from tests or automation. Useful log filter: `RUST_LOG=info,spqr=warn,presage_store_sqlite=error,sigwin=debug`.
- Prefer the Edit tool over perl/sed for multi-line or backslash-heavy edits: shell escaping has already corrupted files here (Windows paths, `\f`, `\r`).
