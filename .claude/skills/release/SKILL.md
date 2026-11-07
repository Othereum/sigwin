---
name: release
description: Prepare a Sigwin release up to the point where the user runs the release script. Use this whenever the user says "릴리스 하자" or otherwise asks to publish a new version.
---

# Release

`docs/release.md` holds the version rules, the notes format and what the script does. This is the order to work in:

1. Choose the version and write the notes to `target\release-notes.md`. Have the user approve both.
2. Once approved, bump the version, commit and push `main`. Push only after the notes are approved: writing them can turn up a missed fix, which should go in before the push.
3. Tell the user to run `scripts\release.ps1`. Never run it yourself: it asks for the signing key password and the publish confirmation in the terminal.
