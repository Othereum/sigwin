# Verifying UI without an account

Layout and glyph bugs only show up visually; a successful compile proves nothing about them.

## Demo modes (synthetic data, no backend)
`SIGWIN_DEMO=1`, plus:
- `SIGWIN_DEMO_STICKERS=1|add`
- `SIGWIN_DEMO_INFO=1`
- `SIGWIN_DEMO_RELOAD=1` (uneven rows + the list re-sent every 250ms, like a live account)
- `SIGWIN_DEMO_OLDER=<ms>` (60 rows, older pages of 60 loaded at the top after <ms>)
- `SIGWIN_DEMO_MENU=1|edit|pin`
- `SIGWIN_DEMO_SETTINGS=1`
- `SIGWIN_DEMO_TICK=1` (periodic updates, for the minimize/restore repro)
- `SIGWIN_DEMO_NOTIFY=<secs>` (a fake incoming message every <secs>, through the real `Notifier` and real toasts: 1:1 with photo, group, reaction, disappearing; needs a real window and an unfocused app, so it's for the user: `scripts\demo-notify.ps1`)
- `SIGWIN_SELFTEST=1` (with `SIGWIN_DEMO=1`: measures the file dialog and notification memory cost)

## Headless runs (`src/headless.rs`)
The user works on this PC: never pop up a window, take focus or move the real mouse. `SIGWIN_HEADLESS="<steps>"` (or `@file`) renders offscreen with the same software renderer and fonts, feeds scripted input, and exits after the last step:
```
$env:SIGWIN_DATA="<scratch>"; $env:SIGWIN_DEMO="1"
$env:SIGWIN_HEADLESS="wait 500; wheel 650 350 6; click 960 590; type 안녕; key shift+Enter; wait 300; shot <scratch>\a.png"
.\target\debug\sigwin.exe
```
Steps: `size W H` (logical, default 1000x680), `scale S`, `wait MS`, `move/click/rclick/dblclick X Y`, `drag X1 Y1 X2 Y2`, `wheel X Y N` (+ = up), `type TEXT`, `key [ctrl+][shift+]NAME`, `shot PATH`.

Theme follows the system (`SIGWIN_HEADLESS_THEME=dark|light`). Coordinates are window-client logical px; read them off a previous shot. Headless ignores theme and zoom settings.

Not covered (needs a real window, ask the user first): minimize/restore repaint (`winhook.rs`), file dialog, notifications (headless stays "focused", so `SIGWIN_DEMO_NOTIFY` only logs), clipboard, IME composition.
