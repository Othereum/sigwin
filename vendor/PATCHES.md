# Vendor patches (re-apply on every upgrade)

Every hunk is marked `// sigwin patch: <why>` in the source; search for "sigwin patch". A missed hunk fails silently (cargo warns "patch was not used"). Rules for adding or removing hunks are in `CLAUDE.md` ("Vendor patches: last resort").

## `vendor/libsignal-service`
Same rev presage pins, wired via `[patch."https://github.com/whisperfish/libsignal-service-rs"]`. Re-vendor at the new rev when bumping presage.
- `send_message_to_group` sends to up to `GROUP_SEND_CONCURRENCY` (16) members at once; upstream awaited them one by one (members x RTT).

## `vendor/i-slint-renderer-software` (Slint 1.18.1, via `[patch.crates-io]`)
Re-vendor the matching version together with `i-slint-core` when upgrading Slint, then re-apply:
- `fonts.rs` (field), `vectorfont.rs` (render / hint / contrast / skew + cache weight/key + `text_tuning`), `lib.rs` (`draw_glyph_run`, `draw_text_vector`, `fill_rectangle`).
- Glyphs are rasterized with swash `Source::ColorOutline(0)` first and drawn as premultiplied RGBA images. That gives inline color emoji in all text from the COLR v0 layers that Segoe UI Emoji still ships (flat style).
- Turns on TrueType **hinting** and a DirectWrite-style **enhanced contrast** curve (k=0.5) on glyph coverage. Without them, small Malgun Gothic text looked blurry and thin on dark backgrounds compared with native apps (compared at 3x zoom; k=0.75 looked too bold). `SIGWIN_HINT=0` / `SIGWIN_CONTRAST=<k>` override at runtime.
- Keeps sub-pixel `fill_rectangle`s (text underlines) at least 1px: rounding both edges dropped the underline on every other line.
- `fill_rectangle` cuts a fill to the clip before its i16 cast when it would overflow: a selection, cursor or underline past 32767px down a very tall text (a composer with thousands of lines, an expanded long message) panicked in euclid's `cast`. Upstream bug, still in 1.18.1.
- Applies fontique's synthetic italic skew (`with_skew`; upstream ignored `Synthesis`, and Malgun Gothic has no italic).

## `vendor/i-slint-core` (Slint 1.18.1)
- **Text styles** (Signal's bold/italic/strike/mono/spoiler `BodyRange`s): `styled_text.rs` adds `sigwin_styled` (StyledText from byte ranges, no markdown) and `sigwin_set_plain_style`, a registry by exact text that `shaping.rs` `create_text_paragraphs` applies to plain Text / read-only TextInput, so the selectable TextInput under the StyledText overlay gets the same font styles and both lay out identically (`gui::rows::to_item` registers, cleared on thread switch). `Style::Code` is plain monospace (no capsule, same size).
- **Spoilers**: `draw.rs`: a color with alpha 1 draws an opaque spoiler box of its RGB instead of the glyphs; spoilers are clickable via a links entry `sigwin-spoiler:<id>`.
- **Clickable links over selectable text**: `items/text.rs`: `StyledText` takes only mouse events over a link and ignores the rest, so a StyledText drawn over a transparent read-only TextInput gives clickable links (pointer cursor, `link-clicked`) while selection still works. Links are `SigwinStyle::Link` spans built in `gui::rows::rich_text`.
- **ListView end of model**: `model/repeater.rs` `update_visible_instances`: when the rows run out before the view is full ("Reached end of model with room to spare. Scroll up."), upstream raised `content_y_value` but not `new_offset_y`, which is what the next pass lays out from, so the view never moved up. Fixed, a content-y past the end lays out the last row at the bottom with real row heights above it in one pass. `gui::scroll::show_rows` relies on it to show a switched-to thread at the bottom from its first frame (`seek-end`). Upstream bug, still in 1.18.1.
- **CJK fallback**: `textlayout/sharedparley/shaping.rs`: the font stack is the requested family + `SIGWIN_CJK_FAMILIES` (Yu Gothic UI, Microsoft YaHei UI, Microsoft JhengHei UI) + generics. parley's script fallback picks one font per script, and for Han on Korean Windows that is Malgun Gothic again, which lacks Japanese/Chinese-only ideographs (駅 鉄 国 rendered blank). The fonts are memory-mapped and loaded only when a glyph needs them (no measurable memory cost).
