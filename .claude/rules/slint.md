---
paths:
  - ui/**
  - src/main.rs
  - src/winhook.rs
  - src/headless.rs
  - vendor/i-slint-*/**
---

# Slint gotchas

- **Missing glyphs render as blank. There is no per-glyph font fallback.** Malgun Gothic (UI font) lacks `✓ ✕ ➤`; Segoe UI Symbol lacks Hangul. Icons use `font-family: "Segoe UI Symbol"`; the status text switches font based on its content. Check every new symbol in a screenshot.
- **Color emoji are plain text everywhere** (reactions, emoji-only messages at 40px, inline): the patched renderer draws them in one consistent flat style. Don't add a separate emoji path (see `docs/decisions.md`). Windows has no flag emoji (🇰🇷 renders as "KR").
- Text styles, links and spoilers depend on the vendored core patch (`main::to_item` registers styled text, `main::rich_text` builds link spans). Details in `vendor/PATCHES.md`.
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
