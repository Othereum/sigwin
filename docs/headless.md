# Verifying UI without an account

Layout and glyph bugs only show up visually; a successful compile proves nothing about them.

## Demo modes (synthetic data, no backend)
`SIGWIN_DEMO=1`, plus:
- `SIGWIN_DEMO_REAL=1` (a believable chat list and conversation instead of test rows, for README screenshots)
- `SIGWIN_DEMO_STICKERS=1|add`
- `SIGWIN_DEMO_UNREAD=<n>` (the thread opens with its last n rows unread: the line above the first, the list scrolled to it, the count on the jump button)
- `SIGWIN_DEMO_INFO=1`
- `SIGWIN_DEMO_RELOAD=1` (uneven rows + the list re-sent every 250ms, like a live account)
- `SIGWIN_DEMO_OLDER=<ms>` (60 rows, older pages of 60 loaded at the top after <ms>); with `SIGWIN_DEMO_CAP=<rows>` the window is bounded like the real one and moves through the rows
- `SIGWIN_DEMO_TALL=<row>:<chars>[,...]` (very long messages at those rows, a line per 20 characters)
- `SIGWIN_DEMO_MENU=1|edit|pin|picker[<tab 0-7>]` (`picker`: the emoji picker on that tab, with a few recent emoji)
- `SIGWIN_DEMO_SETTINGS=1`
- `SIGWIN_DEMO_REQUEST=1` (the open chat is a message request: the request bar instead of the composer)
- `SIGWIN_DEMO_VIDEO=1|<path>` (the newest rows show videos: blurhash and size label replaced by a frame and length after 2s, downloading, nothing, in a grid; with a path the frame is that file's first, through the encrypted file and the decoding child, and the blurhash is the one that file would be sent with)
- `SIGWIN_DEMO_VIEWER=1|<width>x<height>` (a photo on the newest row; clicking it opens the image viewer with three fake images, the first of the given size; `SIGWIN_DEMO_VIEWER_COUNT=<n>` for a longer album)
- `SIGWIN_DEMO_GRID=1` (the newest rows show 2, 3, 4, 5 and 8 images as grids, with a caption and a file on some; one cell stays blank, as until its image arrives)
- `SIGWIN_DEMO_LINK=1` (the newest rows show link previews of every shape, one image never arrives; a link typed in the composer gets a fake preview shortly, none on a host starting with "fail")
- `SIGWIN_DEMO_UPDATE=<version>` (the sidebar "new version ready" bar), `SIGWIN_DEMO_UPDATE_AVAILABLE=<version>` ("new version available" bar, automatic download off), `SIGWIN_DEMO_WHATSNEW=1` (the "updated, what's new" bar)
- `SIGWIN_NOTES_FILE=<path>` (dev builds only, any demo: the "what's new" notes to offer, since a dev build embeds none; the offer shows once per `SIGWIN_DATA`, so use a fresh scratch dir to see it again)
- `SIGWIN_DEMO_NOTICE=<text>` (a notice over the conversation pane), `SIGWIN_DEMO_CONNECTION=<text>` (the connection banner, after its delay like a real drop), `SIGWIN_DEMO_STATUS=1` (loops through notices, progress, short and long connection drops)
- `SIGWIN_DEMO_BADGE=<ms>` (the real tray and taskbar badges step through none and 1 to 12 every <ms>; needs a real window, so it's for the user; run it with a scratch `SIGWIN_DATA`)
- `SIGWIN_DEMO_TICK=1` (periodic updates, for the minimize/restore repro)
- `SIGWIN_DEMO_NOTIFY=<secs>` (a fake incoming message every <secs>, through the real `Notifier` and real toasts: 1:1 with photo, group, reaction, disappearing; needs a real window and an unfocused app, so it's for the user; run it with a scratch `SIGWIN_DATA`)
- `SIGWIN_SELFTEST=1` (with `SIGWIN_DEMO=1`: measures the file dialog and notification memory cost)

## Headless runs (`src/headless.rs`)
The user works on this PC: never pop up a window, take focus or move the real mouse. `SIGWIN_HEADLESS="<steps>"` (or `@file`) renders offscreen with the same software renderer and fonts, feeds scripted input, and exits after the last step:
```
$env:SIGWIN_DATA="<scratch>"; $env:SIGWIN_DEMO="1"
$env:SIGWIN_HEADLESS="wait 500; wheel 650 350 6; click 960 590; type 안녕; key shift+Enter; wait 300; shot <scratch>\a.png"
.\target\fast\sigwin.exe
```
Steps: `size W H` (logical, default 1000x680), `scale S`, `wait MS`, `move/click/rclick/dblclick X Y` (a click rests the pointer and holds the button a moment, as a hand does, so a row drawn anew in between loses it as it would for a user), `drag X1 Y1 X2 Y2`, `wheel X Y N` (+ = up), `type TEXT`, `key [ctrl+][shift+]NAME`, `shot PATH`, `record DIR` (save every frame drawn from now on as `DIR\NNNN.png`, for one-frame glitches; `record` alone stops; saving slows frames down, so give `wait`s more time), `window N` (later steps act on window 1, the main one, or 2, the image viewer).

Theme follows the system (`SIGWIN_HEADLESS_THEME=dark|light`). Coordinates are window-client logical px; read them off a previous shot. Headless ignores theme and zoom settings.

## Scenarios (real backend, no account)
Demo data can't show behavior: reading, unread counts, focus, the list moving as news arrives. `SIGWIN_FIXTURE=1` (with `SIGWIN_DATA`, and `SIGWIN_HEADLESS` for the window) runs the **real backend with no account and no network** (`src/backend/fixture.rs`): messages are seeded or received through the real receive path, and the receipts and toasts that would go out are kept for checks instead. A data dir that holds an account is refused. Headless also pins "away" to "present" (the PC's real idle and lock state must not decide a run); `away on|off` sets it.

Scenario steps, besides the ones above (`@file`: `#` lines are comments, one step per line):
- `fx seed who=1 name=철수 n=100 unread=30` (older messages from contact 1, stored and synced, the newest 30 unread), `fx recv who=1 n=2` (new ones through the receive path), `fx gifs who=1 n=2 same=1` (one message with GIFs whose files are already cached, so their pictures arrive after the rows; `same=1`: one GIF repeated, sharing a key), `fx phone-read who=1`, `fx receipts who=2 n=3` (read receipts alone, as a group member sends them; `old=1`: only stored, as before they were dropped as they came; `mine=1`: ours to that contact, stored as presage stores what it sends), `fx phone-storage who=1 accepted=1 blocked=0` (the phone's storage has been read, so message requests apply; it says only this of contact 1: in the address book, blocked, or neither)
- `attach A|B` (files into the composer, as if picked), `open N`, `jump` (the jump button), `focus on|off`, `away on|off`, `shown on|off` (the window counts as seen without the focus, like on a second monitor; headless starts as not seen), plus the input steps (`wheel`, `click`, ...)
- `settle`: waits until the UI and the backend have stopped talking. Use it, never a `wait`, between a cause and its check. Start a run with it too: `fx` steps sent before the backend is up fail with "no fixture backend is running".
- `expect backend WHAT [who=N] OP VALUE`: `unread`, `read_upto`, `sent` (delivery, read receipts and read-sync entries), `answers` (message request answers and profile keys sent), `request` (whether the chat is a message request), `toasts`, `window` (rows, where the line is), `rows` (stored rows of the thread)
- `expect ui NAME OP VALUE`: `open-unread`, `inactive`, `reading-hold`, `near-bottom`, `rows`, `cells` and `blank-cells` (photo grid cells, and those still showing nothing), `current-key` (an empty value after the operator: nothing open), `current-request` (the request bar shows), `bottom-gap` (0 = the newest row is whole at the bottom), `divider-top` (the "unread" line's row, from the list's top; only while that row is instantiated)
- OP is `=` `!=` `<` `<=` `>` `>=`. A failed `expect` is printed and the run exits with 1.

Files in `tests/scenarios/*.sc` run under `cargo test --profile fast` (`tests/scenarios.rs`; `SIGWIN_SCENARIO=<name>` runs one, `SIGWIN_SCENARIO_SHOTS=<dir>` saves a shot at the end of each). Scenarios on the backend alone, without a window, are `src/backend/scenarios.rs` (the `Harness` drives the real `App` step by step; faster, no rendering). Prefer those when the window isn't part of the question. When a scenario protects a fix, break the fix once and see it fail.

Not covered (needs a real window, ask the user first): minimize/restore repaint (`winhook.rs`), file dialog, notifications (headless stays "focused", so `SIGWIN_DEMO_NOTIFY` only logs), clipboard, IME composition.
