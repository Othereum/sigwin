# pack.json

The sticker pack page ("Create a sticker pack", in the sticker panel's Add pack view) can fill itself from a `pack.json` instead of picking pictures by hand: load it with its button or drop it on the page. It is only taken while nothing has been added.

```json
{"title": "My cats", "author": "Me", "stickers": [{"file": "001.webp", "emoji": "😏"}, {"file": "sub/002.png"}]}
```

- `file` is relative to the folder the pack.json is in, and stays inside it (no `..` or absolute paths: those entries are skipped). Order is the pack's order.
- `emoji` is one emoji. Missing, empty or not an emoji: it is left for the "Pick emoji" step.
- `title` and `author` fill the info step. Keys it doesn't know are ignored.
- Files that can't be stickers are reported and left out, as when picked by hand.
