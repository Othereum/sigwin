# UI design

How Sigwin's UI looks and why. The values (colors, sizes) live in `ui/theme.slint` and `ui/icons.slint`; this file holds the rules that pick them.

## Icons
- **Only from the icon set** (`Icons` in `ui/icons.slint`, drawn with `Icon`). Never a font glyph: the software renderer has no per-glyph fallback, so a symbol the font lacks renders blank, and the same symbol looks different in every font.
- Paths sit on a 24-unit grid and are outlines with round caps and joins. `Icon` keeps the stroke at 1.5px at any size, so every icon has the same line weight. Filled shapes are the exception for things that are dots or solid marks (more, play).
- Color: `Theme.subtle` by default, the accent when the button's panel is open or the icon marks what's selected, white on photos and on the accent.

## Color
- **Colors by `Theme` name.** A color used in more than one place gets a name there, with its light and dark value side by side.
- **The accent is for one action and for selection**: send, the main button of a choice, the selected row, links, unread counts. Not for decoration.
- Destructive actions use `Theme.danger`, never the accent.
- **Over photos and videos it is dark whatever the theme**: marks and buttons on media are white on translucent black. The image viewer is dark always.

## Type
- Malgun Gothic has Semilight, Regular and Bold only: **weights are 400 and 700**. A 500 or 600 renders as one of those and says something the screen doesn't do.
- Sizes come from the steps already in use; no half sizes. Names, titles and the controls a user reads first are bold; hints and times are smaller and `Theme.subtle`.

## Shapes and surfaces
- Things are rounded: rows of the thread list are inset cards, menus and settings sections are cards, the composer is a pill.
- **Input fields have no border**: a grey ground (`Theme.field`), and the accent ring only while they have the focus.
- **What goes with a message being written** (reply, edit, link preview, files) is a card inside the composer pill, above the text, with its own close button.
- **A run of bubbles from one sender reads as one block**: rows of a run sit close, and the corners where two bubbles meet are tighter (4px) than the outer ones. Only the last row of a run shows the time.
- Hover must show on whatever the button sits on: inside the grey pill it uses `Theme.field-off`, not the usual hover color.
- Menus are a card with a border and a shadow. Rows that are actions carry their icon; choices of a value (durations, options) don't.
