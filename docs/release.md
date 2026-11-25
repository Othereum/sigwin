# Releases and updates

## Versioning
`MAJOR.MINOR.PATCH`, compared numerically by the updater (the parts mean nothing to it):
- **PATCH** by default: fixes and features.
- **MINOR** when going back or staying behind stops working: a Signal protocol/server change that
  makes older versions unusable, or a local data format change older versions can't read
  (`db.rs` `SCHEMA` migration, incompatible settings).
- **MAJOR** stays 0 until the core features feel complete.

## Publishing
1. Run `/security-review` on the changes since the last release tag.
2. Write the release notes to `target\release-notes.md` (see below) and have the user check them
   along with the version. This comes before the push: writing them can turn up a missed fix.
3. Bump `version` in `Cargo.toml` (and `Cargo.lock` via `cargo build`), commit as `Release <version>`, push `main`.
4. The user runs `scripts\release.ps1` (in a terminal: it asks for the key password and the publish
   confirmation). An agent only opens it in a new window for the user
   (`Start-Process pwsh -WorkingDirectory <repo> -ArgumentList '-NoExit','-File','scripts\release.ps1'`);
   the password and the confirmation stay with the user. It refuses missing or
   empty notes, checks the tree is
   clean and equals `origin/main` and the release is new, builds `--release`, signs `sigwin.exe`
   with minisign (trusted comment `sigwin <version>`), verifies the signature against
   `PUBLIC_KEY`, shows the notes, asks for confirmation and creates tag `v<version>` plus the GitHub release with
   `sigwin.exe`, `sigwin.exe.minisig` and `licenses.html`, removes the notes file, then fetches the new tag. Needs `cargo-about`, `minisign` and
   `gh` (logged in) in `PATH`; the key defaults to `~\.minisign\sigwin.key` (`-Key` to change).
5. Installed apps check `releases/latest` after the first sync (at once on the login screen), then hourly (and when the window
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

## After a release: updating dependencies
Right after a release, run `scripts\update-deps.ps1` to move `Cargo.lock` to the newest compatible
versions. Then a full development cycle runs on them before the next release, and the release
just made is unaffected. Not before a release: that ships them untested.
- It refuses a dirty tree, runs `cargo update`, and keeps every `[patch]` in use: a newer registry
  release of a patched crate (e.g. `swash`) would otherwise replace the vendor copy and the build
  would still succeed, without the fix. If it can't, the vendor copy needs rebasing.
- Then it builds, runs all tests and lists what changed. Newer releases it can't
  take need a `Cargo.toml` change and maybe code changes, so they are left alone unless a fix or
  feature is needed (security ones come from the alerts below). It points out the one other case
  worth a hand bump: only our crates hold an old version that the build also has in another
  version, so moving to that one drops a copy.
- After it: run the real app (receiving, images, emoji), then commit `Cargo.lock` alone as
  `Update dependencies`, so it can be reverted alone.
- Git deps (presage, libsignal-service, rusqlite) stay on their pinned revs; Slint stays on its
  exact version. Those are upgraded on their own.
- Security fixes don't wait for this: Dependabot alerts (on in the GitHub repository) report
  advisories against `Cargo.lock`; update just that crate with `cargo update -p`. Its automatic
  PRs stay off: they don't check the patches.

## Release notes
Written per release from `git log <last tag>..HEAD` and the actual changes, not kept in a file along
the way: several commits often make one change for users, and a bug made and fixed between two
releases was never seen by anyone. GitHub releases are the record; the repository has no changelog
file.
- From the user's side: what they can now do or what no longer goes wrong. No function names or
  internals.
- English first, as `## New` and `## Fixes`, leaving out an empty one. Only what differs from the
  last release.
- Several lines about one feature become one line naming it, with the changes indented under it.
- Then Korean, the maintainer's language, folded in `<details><summary>한국어</summary>`
  (a blank line after the summary, so the Markdown inside renders),
  with the same content and setting and screen names as its catalog spells them.
- A MINOR release (no going back to older versions) says so first.
- No account data, and nothing about a conversation it was found in.
- Security fixes: say that there are some and to update, plus the behavior users will notice.
  Not what an attacker could do or how: users still on the old version are exposed.
- No `@word` outside backticks: GitHub takes it for a user (`@erwähnen` mentions `erw`), notifies
  them and lists them as a contributor. The script refuses such notes.
- Notes of past releases stay as they are.

### In the app
"What's new" sits under the version on the empty conversation pane, seen at every start, and in
Settings > About (`gui/whatsnew.rs`). Nothing announces an update itself: the app needn't be
known by its version. Both open the GitHub releases page, which lists every version newest first,
so an update that skipped versions loses nothing. The app carries no notes: embedding them took a
history asset chained across releases, which one release made without the script would break
(releases up to 0.2.11 still carry a `changelog.md` asset and embed it; nothing reads it now).

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
