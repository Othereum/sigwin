---
name: headless-scroll
description: Check Sigwin's message list scrolling headlessly and see a whole scroll in one image, with no window, focus or real account. Use this whenever a change touches scrolling, paging, jumps or row layout in the message list (`gui/scroll.rs`, `gui/rows.rs`, `ui/app.slint`), when reproducing a scroll bug with long messages or a bounded window, or whenever several headless shots need comparing at once. Also for "스크롤 확인", "headless로 스크롤 돌려 봐". It can't judge feel, and it doesn't run the backend.
---

# Headless scroll check

`scripts/run.ps1` runs a demo offscreen, scrolls it, takes a shot per step and joins them into one labelled image, which costs far less to read than the shots one by one. Full step syntax and demo variants: `docs/headless.md`.

## Run it

Build with `--profile fast` first (the build rule in `CLAUDE.local.md` applies), then from the repo root:

```powershell
.claude\skills\headless-scroll\scripts\run.ps1 -Out <scratchpad>\up -Up 30 -Down 45 -Notches 12 -Env @{ SIGWIN_DEMO_OLDER = "20"; SIGWIN_DEMO_CAP = "120" }
```

It prints the image's path; open that with Read. `-Out` is emptied first, so give it a directory of its own in the scratchpad. Each `-Up`/`-Down` step is `-Notches` wheel notches (60px each) and one shot. A step takes about 0.6s and a run stops after 3 minutes.

Other cases:

- **Long messages:** `SIGWIN_DEMO_TALL = "285:8000"` puts one at row 285. For a whole stretch of them, build the list: `(150..299 | ForEach-Object { "${_}:800" }) -join ","`.
- **Your own steps:** `-Steps @("click 600 80", "wait 1500", "shot <dir>\pin.png")` run before any scrolling. The demo's pin bar is at about (600, 80) and the jump-to-bottom button at about (960, 598).
- **Other parts of the window:** `-Full` keeps whole shots; the default crops to the conversation pane.
- **Joining shots from elsewhere:** `scripts\sheet.ps1 -Dir <dir> -Output <dir>\sheet.png`. Include no earlier sheet in `<dir>`, and name files so they sort in order.

## Reading the result

The demo numbers its rows ("메시지 250"), so a scroll should read as steadily changing numbers. A number that leaps, a shot that repeats while the wheel is moving, or an empty pane is what to look into. Use smaller `-Notches` and a shorter range around the spot to see more of it, and `record DIR` (`docs/headless.md`) for one-frame glitches.

Some rows are known to look empty: the demo sends sticker thumbnails once, so rows 297 and 298 can show blank, and always do once the window has dropped them and brought them back.

## What it can't tell you

- **The backend doesn't run.** Demo rows come straight from `demo.rs`, which only imitates paging. A bug in `App` (`backend/window.rs`), or one that depends on what a real thread holds, can't show up here. If a real-only symptom doesn't reproduce after a couple of tries, stop and look at the real data's shape with `real-db-probe`.
- **Feel.** Shakes between frames and wheel response need the real app and the user.
