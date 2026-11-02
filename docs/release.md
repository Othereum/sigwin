# Releases and updates

## Publishing
1. Bump `version` in `Cargo.toml` (and `Cargo.lock` via `cargo build`), commit.
2. Push `main`, then run the `release` workflow on `main` (Actions > release > Run workflow, or
   `gh workflow run release.yml`). It builds `--release`, signs `sigwin.exe` with minisign
   (trusted comment `sigwin <version>`) and creates tag `v<version>` plus the GitHub release
   with `sigwin.exe` + `sigwin.exe.minisig`. It fails if that release already exists.
3. Installed apps check `releases/latest` 10s after start, then hourly (and when the window
   gains focus after an hour without a check), download and verify the exe, swap it in, and
   show "새 버전 … 준비됨 · 다시 시작" in the sidebar. Network failures retry after 5 min,
   backing off to an hour.

The workflow runs on `main`, so it restores and saves the same cache as `.github/workflows/ci.yml`
(shared key `release`): no need to wait for CI before releasing.

`GITHUB_TOKEN` can't create a tag on a commit that changes `.github/workflows/` (403 "Resource not
accessible by integration" at Publish). After editing a workflow, push a commit that doesn't touch
it before running `release`.

Only versions greater than the running one are installed, so a bad release is fixed by releasing
a higher version, never by re-tagging. A release the app rejects (signature mismatch, missing
`sigwin.exe` or `.minisig`) is skipped until a newer tag appears; it is remembered only until the
app restarts, so each running app downloads a broken release at most once per launch.

## License notices
The workflow generates third-party notices with `cargo about` (`build-support/about.toml`:
accepted licenses in order of preference, Slint taken under GPL-3.0;
`build-support/about.hbs`: the HTML, with the bundled SQLCipher license added by hand),
embeds them through `SIGWIN_LICENSES` (`build.rs`; without it a placeholder pointing at the
release is embedded) and publishes `licenses.html` with the release. Settings > 정보 >
오픈소스 라이선스 opens the embedded copy. A new dependency under a license not in `accepted`
fails the release: check it, then add it. Locally:
`cargo about generate -c build-support/about.toml build-support/about.hbs -o target/licenses.html`
(`cargo install cargo-about --features cli`; CI downloads the pinned 0.9.3 binary).

## Signing key
- Public key: `PUBLIC_KEY` in `src/update.rs`. Secret key: the `MINISIGN_KEY` repository
  secret (the full contents of the minisign secret key file, generated **without a password**
  (`minisign -G -W`) so CI can sign non-interactively).
- Keep an offline backup of the secret key. If it is lost, installed apps can no longer
  verify updates and every user has to download a new build by hand.
- Rotating: ship a release (signed with the old key) whose `PUBLIC_KEY` is the new one, then
  switch the secret.

## What only the installed exe does
Self-update and the Installed apps entry apply only to `%LOCALAPPDATA%\Sigwin\sigwin.exe`.
Builds run from this repo's `target\` never install or update themselves (`install.rs`).
