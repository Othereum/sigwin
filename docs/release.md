# Releases and updates

## Versioning
`MAJOR.MINOR.PATCH`, compared numerically by the updater (the parts mean nothing to it):
- **PATCH** by default: fixes and features.
- **MINOR** when going back or staying behind stops working: a Signal protocol/server change that
  makes older versions unusable, or a local data format change older versions can't read
  (`db.rs` `SCHEMA` migration, incompatible settings).
- **MAJOR** stays 0 until the core features feel complete.

## Publishing
1. Write the release notes to `target\release-notes.md` (see below) and have the user check them
   along with the version. This comes before the push: writing them can turn up a missed fix.
2. Bump `version` in `Cargo.toml` (and `Cargo.lock` via `cargo build`), commit, push `main`.
3. Run `scripts\release.ps1` (in a terminal: it asks for the key password). It refuses missing or
   empty notes, checks the tree is
   clean and equals `origin/main` and the release is new, builds `--release`, signs `sigwin.exe`
   with minisign (trusted comment `sigwin <version>`), verifies the signature against
   `PUBLIC_KEY`, shows the notes, asks for confirmation and creates tag `v<version>` plus the GitHub release with
   `sigwin.exe`, `sigwin.exe.minisig` and `licenses.html`, removes the notes file, then fetches the new tag. Needs `cargo-about`, `minisign` and
   `gh` (logged in) in `PATH`; the key defaults to `~\.minisign\sigwin.key` (`-Key` to change).
4. Installed apps check `releases/latest` after the first sync (at once on the login screen), then hourly (and when the window
   gains focus after an hour without a check), download and verify the exe, swap it in, and
   show "새 버전 … 준비됨 · 다시 시작" in the sidebar. Network failures retry after 5 min,
   backing off to an hour.

Releases are built locally on purpose: the CI workflow took ~17 min (the release build alone
~10 min even with a warm cache) and needed the key in a GitHub secret; there is no CI now. Should
CI releasing ever come back (see git history for `release.yml`), `GITHUB_TOKEN` can't create a tag
on a commit that changes `.github/workflows/` (403 "Resource not accessible by integration"),
and its key would have to be a password-less copy in a repository secret.

Only versions greater than the running one are installed, so a bad release is fixed by releasing
a higher version, never by re-tagging. A release the app rejects (signature mismatch, missing
`sigwin.exe` or `.minisig`) is skipped until a newer tag appears; it is remembered only until the
app restarts, so each running app downloads a broken release at most once per launch.

## Release notes
Written per release from `git log <last tag>..HEAD` and the actual changes, not kept in a file along
the way: several commits often make one change for users, and a bug made and fixed between two
releases was never seen by anyone. GitHub releases are the record; there is no changelog file.
- In the app's languages, from the user's side: what they can now do or what no longer goes wrong.
  No function names or internals.
- `## 새 기능` and `## 수정`, leaving out an empty one. Only what differs from the last release.
- A MINOR release (no going back to older versions) says so first.
- No account data, and nothing about a conversation it was found in.
- With more than one language, the app would need to pick its own (the current parser takes every
  `##` as a section). Notes of past releases stay as they are.

### In the app
`scripts\release.ps1` embeds the notes in the exe (`SIGWIN_NOTES` → `build.rs`), so the signed exe
carries its own version's notes and nothing is fetched. After an update the sidebar offers them
once as a "what's new" panel (`src/whatsnew.rs`, `gui/whatsnew.rs`); Settings > 정보 shows them
again. The script refuses notes without a `## ` heading and `- ` bullets.
- Format: `##` headings (any text) are sections, `- ` lines are bullets, anything else is plain
  text; links, images and HTML are never rendered. The parser is lenient, so a new heading needs
  no app change. Notes and parser are built together, so an older app never reads a newer format.
- Shown when `last_seen_version` in `settings.json` is older than the running version. An app
  from before the feature recorded nothing: no value but an existing account means it was updated
  (show), no account means a fresh install (record, show nothing). It records when the panel is
  opened or dismissed, never for a dev build without notes.
- Only the running version's notes are in the exe: a user who skipped releases sees the last one's.
  Write the notes for someone who didn't read the earlier ones if that matters for the change.
- If notes from outside the exe are ever shown (GitHub's body, older releases), they are not
  signed: plain text only, and give the format a version marker first (no marker = version 1).

## License notices
`scripts\release.ps1` generates third-party notices with `cargo about`
(`build-support/about.toml`: accepted licenses in order of preference, Slint taken under
GPL-3.0; `build-support/about.hbs`: the HTML, with the bundled SQLCipher license added by hand),
embeds them through `SIGWIN_LICENSES` (`build.rs`; without it a placeholder pointing at the
release is embedded) and publishes `licenses.html` with the release. Settings > 정보 >
오픈소스 라이선스 opens the embedded copy. A new dependency under a license not in `accepted`
fails the release: check it, then add it. Locally:
`cargo about generate -c build-support/about.toml build-support/about.hbs -o target/licenses.html`
(`cargo install cargo-about --features cli`).

## Signing key
- Public key: `PUBLIC_KEY` in `src/update.rs`. Secret key: a password-protected minisign key
  file on the release machine (`~\.minisign\sigwin.key`); the password is typed on every release.
- Keep an offline backup of the secret key. If it is lost, installed apps can no longer
  verify updates and every user has to download a new build by hand.
- Rotating: ship a release (signed with the old key) whose `PUBLIC_KEY` is the new one, then
  switch the key file.

## What only the installed exe does
Self-update and the Installed apps entry apply only to `%LOCALAPPDATA%\Sigwin\sigwin.exe`.
Dev builds never install or update themselves (`install.rs`, see below).

## Dev builds
Any build without `SIGWIN_RELEASE` is a dev build (`sigwin_dev` cfg, set by `build.rs`), whatever
the profile and wherever its exe runs, so a local `cargo build --release` is one too. It shows the
commit hash in the title (`Sigwin [34f43ce]`) and the version line, uses `assets/icon-dev.ico`
(window, tray), shows an error on a second launch instead of bringing the running app to the
front, and never installs, updates or registers autostart. Releases carry none of it.

`scripts\release.ps1` sets `SIGWIN_RELEASE`, remaps the home and repo dirs out of panic locations
(`--remap-path-prefix`: they held the user name), and refuses an exe that is still a dev build
(`SpecialBuild` in its version info, set by `build.rs`) or contains either path. To try the install or update flow locally, build with `SIGWIN_RELEASE=1` and run
the exe from outside `%LOCALAPPDATA%\Sigwin\`.
