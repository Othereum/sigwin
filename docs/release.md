# Releases and updates

## Publishing
1. Bump `version` in `Cargo.toml` (and `Cargo.lock` via `cargo build`), commit, push `main`.
2. Run `scripts\release.ps1` (in a terminal: it asks for the key password). It checks the tree is
   clean and equals `origin/main` and the release is new, builds `--release`, signs `sigwin.exe`
   with minisign (trusted comment `sigwin <version>`), verifies the signature against
   `PUBLIC_KEY`, asks for confirmation and creates tag `v<version>` plus the GitHub release with
   `sigwin.exe`, `sigwin.exe.minisig` and `licenses.html`. Needs `cargo-about`, `minisign` and
   `gh` (logged in) in `PATH`; the key defaults to `~\.minisign\sigwin.key` (`-Key` to change).
3. Installed apps check `releases/latest` after the first sync (at once on the login screen), then hourly (and when the window
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
Builds run from this repo's `target\` never install or update themselves (`install.rs`).
