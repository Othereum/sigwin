//! Self-update from GitHub Releases. Only the installed exe updates itself.
//!
//! (The "automatic updates" setting gates all of this; the settings button checks once on demand.)
//! A background thread checks `releases/latest` once the first sync is done (or at once on the
//! login screen, where there is nothing to sync), then once an hour, and also
//! when the window gains focus and the last check is over an hour old. "An hour" is wall-clock
//! time checked every few minutes, so a laptop waking from sleep checks soon. A check that fails
//! on the network (none yet right after login) is retried after 5 minutes, backing off to an
//! hour. A release that is itself broken (bad signature, missing file) is skipped until a newer
//! tag appears, so the 40MB exe isn't fetched again every few minutes. All of it stays far
//! below GitHub's unauthenticated limit (60/hour per IP). A newer release's `sigwin.exe` is
//! streamed to disk next to us while its minisign signature is checked (the trusted comment must name that exact version, so an old signed build can't be
//! re-served as a new one), then swapped in with `self_replace`. The UI offers a restart; a
//! normal quit picks the new exe up on the next start anyway. Between checks nothing is held:
//! the tokio runtime and HTTP client live only for one check.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context};
use minisign_verify::{PublicKey, Signature};

const REPO: &str = "Othereum/sigwin";
/// minisign key of release builds; the secret half signs in the release workflow
/// key id 6712F8FDD121E7B4
const PUBLIC_KEY: &str = "RWS05yHR/fgSZw6B0SKSgC8emM5LkgUm5jCMhBw4svpeXtLXS26KBBGI";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const EXE_ASSET: &str = "sigwin.exe";
const SIG_ASSET: &str = "sigwin.exe.minisig";
const MAX_SIZE: u64 = 200 << 20;
const INTERVAL: Duration = Duration::from_secs(3600);
/// after a failed check
const RETRY: Duration = Duration::from_secs(5 * 60);
/// how often the thread looks at the clock (sleep doesn't count suspended time reliably)
const TICK: Duration = RETRY;

enum Msg {
    /// the first check may run (see `start`)
    Start,
    /// the window gained focus
    Poke,
    /// the settings button
    Manual,
}

static POKE: OnceLock<mpsc::Sender<Msg>> = OnceLock::new();
/// The "automatic updates" setting; manual checks ignore it.
static AUTO: AtomicBool = AtomicBool::new(true);

pub fn set_auto(on: bool) {
    AUTO.store(on, Ordering::Relaxed);
}

/// Lets the first check run: called once the first sync is done (the network works and the
/// startup load is over) or when there is no account to sync (login screen). Later calls do nothing.
pub fn start() {
    if let Some(tx) = POKE.get() {
        let _ = tx.send(Msg::Start);
    }
}

/// The window gained focus: check now if the last check is old enough.
pub fn poke() {
    if let Some(tx) = POKE.get() {
        let _ = tx.send(Msg::Poke);
    }
}

/// The "check now" button. False when nothing can update (not the installed copy).
pub fn check_now() -> bool {
    POKE.get().is_some_and(|tx| tx.send(Msg::Manual).is_ok())
}

/// What a check found, for the UI. Automatic checks only report `Ready`.
pub enum Status {
    UpToDate,
    /// a newer version replaced the exe
    Ready(String),
    Failed,
}

/// Whether `wait` has passed since `last` (wall clock; never = passed).
fn elapsed(last: Option<SystemTime>, now: SystemTime, wait: Duration) -> bool {
    match last {
        None => true,
        // a clock set backwards counts as passed
        Some(t) => now.duration_since(t).map_or(true, |d| d >= wait),
    }
}

/// Wait after `failures` failed attempts in a row: 5, 10, 20, 40 min, then an hour.
fn backoff(failures: u32) -> Duration {
    RETRY.saturating_mul(1 << failures.saturating_sub(1).min(8)).min(INTERVAL)
}

/// `ok` = last successful check, `tried` = last attempt of any outcome. The attempt limit also
/// keeps focus changes from hammering the API while offline.
fn due(ok: Option<SystemTime>, tried: Option<SystemTime>, failures: u32, now: SystemTime) -> bool {
    elapsed(ok, now, INTERVAL) && elapsed(tried, now, backoff(failures))
}

/// Why a check failed decides when to try again.
#[derive(Debug)]
enum Failure {
    /// network, GitHub, local disk: back off and retry
    Transient(anyhow::Error),
    /// the release itself is unusable (bad signature, missing file): skip this tag until a
    /// newer one appears, so a broken release isn't downloaded again and again
    BadRelease { tag: String, err: anyhow::Error },
}

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(e: E) -> Self {
        Failure::Transient(e.into())
    }
}

/// Starts checking for updates of `exe` (the installed copy we run from). `report` gets the new
/// version once it has replaced `exe` (checking stops then), and the outcome of manual checks.
pub fn spawn(exe: PathBuf, report: impl Fn(Status) + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    if POKE.set(tx).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        // wait for the first check; pokes meanwhile are dropped, a manual check isn't
        let mut manual = false;
        loop {
            match rx.recv() {
                Ok(Msg::Start) => break,
                Ok(Msg::Manual) => {
                    manual = true;
                    break;
                }
                Ok(Msg::Poke) => {}
                Err(_) => return,
            }
        }
        let (mut ok, mut tried, mut failures) = (None, None, 0u32);
        let mut bad_tag: Option<String> = None;
        loop {
            if manual || (AUTO.load(Ordering::Relaxed) && due(ok, tried, failures, SystemTime::now())) {
                // a manual check looks at a skipped release again
                let skip = if manual { None } else { bad_tag.as_deref() };
                match check_once(&exe, skip) {
                    Ok(Some(v)) => {
                        tracing::info!("update: {v} installed, restart to use it");
                        report(Status::Ready(v));
                        return;
                    }
                    Ok(None) => {
                        tracing::debug!("update: up to date ({VERSION})");
                        ok = Some(SystemTime::now());
                        failures = 0;
                        if manual {
                            report(Status::UpToDate);
                        }
                    }
                    Err(Failure::BadRelease { tag, err }) => {
                        tracing::warn!("update: skipping {tag} until a newer release: {err:#}");
                        bad_tag = Some(tag);
                        ok = Some(SystemTime::now());
                        failures = 0;
                        if manual {
                            report(Status::Failed);
                        }
                    }
                    Err(Failure::Transient(e)) => {
                        tracing::warn!("update: {e:#}");
                        if manual {
                            report(Status::Failed);
                        } else {
                            failures += 1;
                            tracing::debug!("update: retrying in {} min", backoff(failures).as_secs() / 60);
                        }
                    }
                }
                tried = Some(SystemTime::now());
            }
            manual = match rx.recv_timeout(TICK) {
                Ok(Msg::Manual) => true,
                Ok(Msg::Poke | Msg::Start) | Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            };
        }
    });
}

/// `Some(version)` once a newer version has replaced `exe`. `bad_tag` counts as up to date.
fn check_once(exe: &Path, bad_tag: Option<&str>) -> Result<Option<String>, Failure> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(check(exe, bad_tag))
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

async fn check(exe: &Path, bad_tag: Option<&str>) -> Result<Option<String>, Failure> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("sigwin/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()?;
    let rel: Release = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let latest = rel.tag_name.trim_start_matches('v').to_string();
    if !newer(&latest, VERSION) || bad_tag == Some(rel.tag_name.as_str()) {
        return Ok(None);
    }
    let bad = |err: anyhow::Error| Failure::BadRelease { tag: rel.tag_name.clone(), err };
    let url = |name: &str| {
        rel.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| bad(anyhow::anyhow!("{} has no {name}", rel.tag_name)))
    };
    let (sig_url, exe_url) = (url(SIG_ASSET)?, url(EXE_ASSET)?);
    let sig_text = client.get(sig_url).send().await?.error_for_status()?.text().await?;
    let (pk, sig) = signature(PUBLIC_KEY, &sig_text, &latest).map_err(bad)?;

    let new = exe.with_file_name("sigwin.exe.new");
    let result = download(&client, &exe_url, &pk, &sig, &new).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    match result {
        Ok(()) => {}
        Err(Downloaded::Transient(e)) => return Err(Failure::Transient(e)),
        Err(Downloaded::Rejected(e)) => return Err(bad(e)),
    }
    self_replace::self_replace(&new).context("replacing the exe")?;
    let _ = std::fs::remove_file(&new);
    Ok(Some(latest))
}

/// `download`'s failures: the connection (retry) or the file itself (don't fetch it again).
enum Downloaded {
    Transient(anyhow::Error),
    Rejected(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Downloaded {
    fn from(e: E) -> Self {
        Downloaded::Transient(e.into())
    }
}

fn newer(latest: &str, current: &str) -> bool {
    crate::install::version(latest) > crate::install::version(current)
}

/// Decodes the key and signature and checks that the signed comment names `version`.
fn signature(public_key: &str, sig_text: &str, version: &str) -> anyhow::Result<(PublicKey, Signature)> {
    let pk = PublicKey::from_base64(public_key).context("update key")?;
    let sig = Signature::decode(sig_text).context("signature")?;
    let want = format!("sigwin {version}");
    if sig.trusted_comment().trim() != want {
        bail!("signature is for {:?}, not {want:?}", sig.trusted_comment());
    }
    Ok((pk, sig))
}

/// Streams `url` into `dest`, hashing as it goes; fails unless the signature matches.
async fn download(
    client: &reqwest::Client,
    url: &str,
    pk: &PublicKey,
    sig: &Signature,
    dest: &Path,
) -> Result<(), Downloaded> {
    let mut verifier = pk
        .verify_stream(sig)
        .map_err(|e| Downloaded::Rejected(anyhow::Error::new(e).context("signature")))?;
    let mut file = std::fs::File::create(dest).with_context(|| format!("{}", dest.display()))?;
    let mut resp = client.get(url).send().await?.error_for_status()?;
    let mut total = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        total += chunk.len() as u64;
        if total > MAX_SIZE {
            return Err(Downloaded::Rejected(anyhow::anyhow!("download is larger than {MAX_SIZE} bytes")));
        }
        verifier.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    drop(file);
    verifier
        .finalize()
        .map_err(|e| Downloaded::Rejected(anyhow::Error::new(e).context("signature does not match the download")))
}

/// The restart button: start the (already replaced) exe, which waits for us to exit
/// (`--after <pid>`), then leave through the normal exit path.
pub fn restart() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).arg("--after").arg(std::process::id().to_string()).spawn() {
                tracing::warn!("update: restart: {e}");
                return;
            }
            let _ = slint::quit_event_loop();
        }
        Err(e) => tracing::warn!("update: restart: {e}"),
    }
}

/// `--after <pid>`: wait (bounded) until the process that started us has exited, so its
/// single-instance lock is gone.
pub fn wait_for_previous() {
    let mut args = std::env::args().skip_while(|a| a != "--after").skip(1);
    let Some(pid) = args.next().and_then(|p| p.parse::<u32>().ok()) else { return };
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        // SAFETY: a handle we open and close here
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !h.is_null() {
                WaitForSingleObject(h, 20_000);
                CloseHandle(h);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use blake2::Digest;
    use ed25519_dalek::{Signer, SigningKey};

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    /// minisign's formats: key = "Ed" + key id + public key; signature line = "ED" (prehashed)
    /// + key id + sig over BLAKE2b-512(file); global sig over sig + trusted comment.
    fn keypair(seed: u8) -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let mut pk = b"Ed".to_vec();
        pk.extend_from_slice(&[seed; 8]);
        pk.extend_from_slice(sk.verifying_key().as_bytes());
        (sk, B64.encode(pk))
    }

    fn sign(sk: &SigningKey, key_id: u8, data: &[u8], comment: &str) -> String {
        let hash = blake2::Blake2b512::digest(data);
        let sig = sk.sign(&hash).to_bytes();
        let mut line = b"ED".to_vec();
        line.extend_from_slice(&[key_id; 8]);
        line.extend_from_slice(&sig);
        let mut global = sig.to_vec();
        global.extend_from_slice(comment.as_bytes());
        let global = sk.sign(&global).to_bytes();
        format!(
            "untrusted comment: test\n{}\ntrusted comment: {comment}\n{}\n",
            B64.encode(line),
            B64.encode(global)
        )
    }

    fn verify(pk: &str, sig_text: &str, version: &str, chunks: &[&[u8]]) -> anyhow::Result<()> {
        let (pk, sig) = signature(pk, sig_text, version)?;
        let mut v = pk.verify_stream(&sig)?;
        for c in chunks {
            v.update(c);
        }
        v.finalize()?;
        Ok(())
    }

    #[test]
    fn accepts_a_matching_signature_in_chunks() {
        let (sk, pk) = keypair(7);
        let sig = sign(&sk, 7, b"hello world", "sigwin 0.2.0");
        verify(&pk, &sig, "0.2.0", &[b"hello ", b"world"]).unwrap();
    }

    #[test]
    fn rejects_tampering_wrong_version_and_wrong_key() {
        let (sk, pk) = keypair(7);
        let sig = sign(&sk, 7, b"hello world", "sigwin 0.2.0");
        assert!(verify(&pk, &sig, "0.2.0", &[b"hello world!"]).is_err());
        // an old signed build served under a newer tag
        assert!(verify(&pk, &sig, "0.3.0", &[b"hello world"]).is_err());
        let (other, other_pk) = keypair(9);
        assert!(verify(&other_pk, &sig, "0.2.0", &[b"hello world"]).is_err());
        // right key id, wrong key
        let forged = sign(&other, 7, b"hello world", "sigwin 0.2.0");
        assert!(verify(&pk, &forged, "0.2.0", &[b"hello world"]).is_err());
        // edited trusted comment
        let edited = sig.replace("sigwin 0.2.0", "sigwin 0.3.0");
        assert!(verify(&pk, &edited, "0.3.0", &[b"hello world"]).is_err());
    }

    #[test]
    fn only_newer_versions_update() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn checks_are_due_hourly_by_wall_clock() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let min = |m: u64| Duration::from_secs(m * 60);
        assert!(due(None, None, 0, t));
        // succeeded at t
        assert!(!due(Some(t), Some(t), 0, t + min(59)));
        assert!(due(Some(t), Some(t), 0, t + INTERVAL));
        // clock moved backwards
        assert!(due(Some(t), Some(t), 0, t - Duration::from_secs(1)));
    }

    #[test]
    fn failed_checks_back_off_up_to_an_hour() {
        let min = |m: u64| Duration::from_secs(m * 60);
        let waits: Vec<_> = (1..=7).map(backoff).collect();
        assert_eq!(waits, [min(5), min(10), min(20), min(40), INTERVAL, INTERVAL, INTERVAL]);
        assert_eq!(backoff(u32::MAX), INTERVAL);

        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // never succeeded, first failure at t
        assert!(!due(None, Some(t), 1, t + min(1)));
        assert!(due(None, Some(t), 1, t + RETRY));
        // third failure in a row
        assert!(!due(None, Some(t), 3, t + min(19)));
        assert!(due(None, Some(t), 3, t + min(20)));
        // succeeded long ago, then one failed attempt
        assert!(!due(Some(t), Some(t + INTERVAL), 1, t + INTERVAL + min(2)));
        assert!(due(Some(t), Some(t + INTERVAL), 1, t + INTERVAL + RETRY));
    }

    #[test]
    fn release_key_parses() {
        PublicKey::from_base64(PUBLIC_KEY).unwrap();
    }
}
