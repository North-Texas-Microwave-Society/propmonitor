//! Self-update channel: watches the release channel for new builds,
//! verifies them, installs them, and re-executes the daemon in place.
//!
//! The channel is a single `latest` GitHub Release that
//! `.github/workflows/latest.yml` republishes on every push to `main`. It
//! carries the raw per-architecture binaries plus
//! `propmonitor-manifest.json` — version, build commit, timestamp, and the
//! SHA-256 of every asset. Because the release tag never moves off
//! `latest`, the manifest URL is a constant: the daemon does not have to
//! discover anything, hold a token, or hit a rate-limited API. GET the
//! manifest, compare commits, install if they differ.
//!
//! Identity is the **commit**, not the version string: `main` moves
//! without a version bump, so `0.2.0` is not a monotonic answer to "am I
//! current?" while the commit always is.
//!
//! Activation is in-place. The new binary is downloaded next to the
//! running one (same directory, therefore same filesystem, therefore an
//! atomic `rename(2)`), verified, preflighted, and renamed over the path
//! after the current binary is kept aside as `propmonitor.prev`. Then the
//! daemon `execve`s itself. The PID does not change, so systemd sees no
//! stop/start: the unit stays `active (running)`, `Restart=`/start-limit
//! accounting is untouched, and the only visible effect is what any
//! restart does — the in-memory ring buffer resets and the SDR is
//! reopened.
//!
//! Every failure here is non-fatal and retried on the next tick. Two gates
//! are hard, and both leave the running binary in place: the SHA-256 check,
//! and the preflight run — the candidate has to load *this node's* config
//! before it is allowed near the install path, because a build that rejects
//! it would crash-loop the daemon with nothing left to roll back to.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::MIN_CHECK_INTERVAL;
use crate::error;
use crate::server::{AppState, WsEvent};
use crate::timefmt;

/// Where the channel manifest lives: the `latest` release of the public
/// repository, republished on every push to `main`. Fixed, because the
/// release tag never moves off `latest` — and `install.sh` installs from
/// the same release, so a fresh install and a self-updated node always
/// agree on what "latest" means.
///
/// Binaries are resolved *relative to this URL* (see [`asset_url`]), so
/// pointing a node at another channel is one setting, not three.
pub const MANIFEST_URL: &str = "https://github.com/North-Texas-Microwave-Society/propmonitor/releases/download/latest/propmonitor-manifest.json";

/// Crate version of the running build.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Commit the running build was made from; "dev" when `build.rs` could not
/// find one.
pub const CURRENT_COMMIT: &str = env!("PROP_BUILD_COMMIT");

/// Whether this binary came out of the release workflows. Unattended
/// updates are gated on it: a laptop build must never be silently replaced
/// by a release binary.
const IS_DIST_BUILD: bool = matches!(env!("PROP_DIST_BUILD").as_bytes(), b"1");

/// In-place activation is an `execve` of the swapped path, which is how
/// the Linux deployment works. Development builds on other platforms
/// update by rebuilding.
const CAN_SELF_UPDATE: bool = cfg!(target_os = "linux");

/// Connect timeout for both the manifest and the asset.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Overall timeout for one asset download. A few MB over a slow
/// residential uplink, with room to spare.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// How soon a *failed* check is retried, before doubling.
///
/// The daemon checks the channel once at startup, and `Restart=always`
/// plus a router that is still coming up after a power cut means that
/// first check can land before the node has working DNS. At the
/// configured interval alone — an hour by default — one such miss leaves
/// a red "could not reach the release channel" in the UI for the whole
/// hour and delays a build the node could have had in seconds.
const FIRST_RETRY: Duration = Duration::from_secs(30);

/// How long the new binary gets to answer its config-load preflight before
/// we call it hung, kill it, and refuse the install.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the preflight thread looks in on the probe. Short enough that
/// a normal probe (milliseconds) is not delayed by polling, long enough to
/// cost nothing over the full deadline.
const PROBE_POLL: Duration = Duration::from_millis(25);

/// Identity of the running build, baked in by `build.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct CurrentBuild {
    pub version: &'static str,
    /// The update identity — see the module docs on why this is the commit
    /// and not the version.
    pub commit: &'static str,
    /// Built by the release workflows. Gate for unattended updates.
    pub dist_build: bool,
    /// Whether in-place self-update is available on this platform at all.
    /// The UI disables its install button when false.
    pub can_self_update: bool,
}

impl CurrentBuild {
    fn running() -> Self {
        Self {
            version: CURRENT_VERSION,
            commit: CURRENT_COMMIT,
            dist_build: IS_DIST_BUILD,
            can_self_update: CAN_SELF_UPDATE,
        }
    }
}

/// One build as published on the channel. Shape of
/// `propmonitor-manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub commit: String,
    /// RFC 3339 build time, from the workflow run.
    pub built_at: String,
    /// Asset name → expected SHA-256, lowercase hex.
    pub assets: BTreeMap<String, String>,
}

/// What the channel task is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Checking,
    Downloading,
    Installing,
}

impl Phase {
    /// A check or install is in flight, so a new one must not start.
    pub fn is_busy(self) -> bool {
        !matches!(self, Phase::Idle)
    }
}

/// Shared state of the update channel. `GET /api/update` returns a
/// snapshot; every transition also broadcasts `WsEvent::Update`, so an
/// open UI follows a running install without polling.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateState {
    pub current: CurrentBuild,
    /// The build the channel offered on the last check, when it differs
    /// from the running one. `None` means "up to date" or "never checked"
    /// — `last_check_at` distinguishes those.
    pub latest: Option<Manifest>,
    pub phase: Phase,
    /// RFC 3339 time of the last completed check, successful or not.
    pub last_check_at: Option<String>,
    /// Why the last check or install failed. Cleared when the next
    /// attempt starts.
    pub last_error: Option<String>,
}

impl UpdateState {
    pub fn new() -> Self {
        Self {
            current: CurrentBuild::running(),
            latest: None,
            phase: Phase::Idle,
            last_check_at: None,
            last_error: None,
        }
    }
}

impl Default for UpdateState {
    fn default() -> Self {
        Self::new()
    }
}

/// What woke the channel task. The `watch` channel coalesces, so a burst
/// of UI clicks collapses into the most recent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateRequest {
    /// Periodic tick — no operator involved.
    Idle,
    /// "Check for updates" from the UI.
    Check,
    /// "Install update" from the UI.
    Install,
}

/// Asset name the channel publishes for the running target, if any.
fn asset_for_arch() -> Option<&'static str> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH)
}

/// The published asset for a target triple's OS and architecture.
///
/// Split out from [`asset_for_arch`] so every arm is reachable in a test,
/// not just the one this binary happens to be compiled for. The names must
/// match what `.github/workflows/latest.yml` uploads.
fn asset_name(os: &str, arch: &str) -> Option<&'static str> {
    if os != "linux" {
        return None;
    }
    match arch {
        "x86_64" => Some("propmonitor-x86_64-linux"),
        "aarch64" => Some("propmonitor-aarch64-linux"),
        // Rust calls the 32-bit ARM target "arm"; the release calls it armv7.
        "arm" => Some("propmonitor-armv7-linux"),
        _ => None,
    }
}

/// Where a named asset lives, given the manifest URL.
///
/// Both are assets of the same release, i.e. siblings in the same
/// directory — so the asset URL is the manifest URL with its last path
/// segment swapped. That keeps the channel a single setting: point a node
/// at a fork's manifest and its binaries come from the fork too, never
/// half from one place and half from another.
fn asset_url(manifest_url: &str, asset: &str) -> String {
    match manifest_url.rfind('/') {
        Some(i) => format!("{}{asset}", &manifest_url[..=i]),
        // No slash at all is not a URL we could have fetched a manifest
        // from; leave it to the download to report the failure.
        None => asset.to_string(),
    }
}

/// Unattended installs are for release binaries on the deployment
/// platform. Everything else waits for an explicit click.
fn auto_allowed() -> bool {
    IS_DIST_BUILD && CAN_SELF_UPDATE
}

/// Absolute path of the running binary — where a new one has to land for
/// the swap to be a rename within one directory.
///
/// `/proc/self/exe` is the kernel's own answer, and it stays correct
/// across a rename of the file. It goes stale in exactly one case: the
/// inode was unlinked outright, which the kernel marks with a
/// " (deleted)" suffix. Then the pre-deletion path is the best guess, and
/// only if something exists there now. `argv[0]` (what the service
/// manager exec'd) is the fallback, and the only route on non-Linux.
pub fn resolve_install_path() -> PathBuf {
    if let Ok(link) = std::fs::read_link("/proc/self/exe") {
        if let Some(path) = usable_exe_path(&link.to_string_lossy()) {
            return path;
        }
    }
    std::env::args_os()
        .next()
        .and_then(|a| std::fs::canonicalize(a).ok())
        .unwrap_or_else(|| PathBuf::from("propmonitor"))
}

/// Turn a `/proc/self/exe` readlink value into a path we can install
/// over, or `None` if it does not name a file that exists.
fn usable_exe_path(link: &str) -> Option<PathBuf> {
    let clean = link.strip_suffix(" (deleted)").unwrap_or(link);
    if clean.is_empty() || !Path::new(clean).is_file() {
        return None;
    }
    Some(std::fs::canonicalize(clean).unwrap_or_else(|_| PathBuf::from(clean)))
}

/// Long-running channel task; spawn once from `main`. Never returns.
///
/// Wakes on `update.check_interval` (re-read from the live config every
/// pass, so a UI edit takes effect on the next tick) or immediately on an
/// explicit UI request. `update.enabled: false` silences the timer but
/// never an explicit request: an operator who clicks "check now" gets a
/// check.
pub async fn run(state: Arc<AppState>) {
    let client = match reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "propmonitor: self-update disabled: HTTP client setup failed: {}",
                error::chain(&e)
            );
            return;
        }
    };

    let mut requests = state.update_notify.subscribe();
    // The first check runs immediately: a UI opened right after boot
    // should show a real answer, not "never checked".
    let mut wait = Duration::ZERO;
    // Set while the last check failed, so the next one comes sooner than
    // the configured interval.
    let mut retry: Option<Duration> = None;

    loop {
        let request = tokio::select! {
            _ = tokio::time::sleep(wait) => UpdateRequest::Idle,
            changed = requests.changed() => match changed {
                Ok(()) => *requests.borrow_and_update(),
                // The sender lives in AppState, which outlives this task;
                // if it ever does drop, fall back to timer-only behaviour.
                Err(_) => UpdateRequest::Idle,
            },
        };

        let (enabled, auto, interval) = {
            let cfg = state.config.read().await;
            (
                cfg.update.enabled,
                cfg.update.auto,
                cfg.update.check_interval.max(MIN_CHECK_INTERVAL),
            )
        };
        wait = Duration::from_secs(u64::from(interval));

        if !enabled && request == UpdateRequest::Idle {
            continue;
        }

        match check(&state, &client).await {
            Checked::Failed => {
                // Sooner than the configured interval, and the error the
                // check recorded stays in the UI until it clears. An
                // explicit install request has nothing to install here.
                let delay = retry_delay(retry, wait);
                retry = Some(delay);
                wait = delay;
            }
            outcome => {
                retry = None;
                if request == UpdateRequest::Install
                    || (outcome == Checked::Available && auto && auto_allowed())
                {
                    install(&state, &client).await;
                }
            }
        }
    }
}

/// Outcome of one channel check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checked {
    /// The channel could not be read; `last_error` says why, and the next
    /// check comes after [`retry_delay`] rather than a full interval.
    Failed,
    /// The channel offers the build that is already running.
    UpToDate,
    /// The channel offers a different build.
    Available,
}

/// Delay before retrying a failed check: [`FIRST_RETRY`], then doubling,
/// capped at the configured interval — a channel that is genuinely down
/// ends up polled no harder than a healthy one, while a node that came up
/// before its network did recovers in seconds.
fn retry_delay(previous: Option<Duration>, interval: Duration) -> Duration {
    match previous {
        None => FIRST_RETRY,
        Some(d) => d.saturating_mul(2),
    }
    .min(interval)
}

/// Fetch the channel manifest and record what it says.
async fn check(state: &Arc<AppState>, client: &reqwest::Client) -> Checked {
    let url = state.manifest_url.clone();
    set_state(state, |s| {
        s.phase = Phase::Checking;
        s.last_error = None;
    })
    .await;

    let manifest = match client.get(&url).send().await {
        Err(e) => {
            fail(
                state,
                format!("could not reach the release channel: {}", error::chain(&e)),
            )
            .await;
            return Checked::Failed;
        }
        Ok(resp) if !resp.status().is_success() => {
            let status = resp.status();
            fail(state, format!("release channel answered HTTP {status}")).await;
            return Checked::Failed;
        }
        Ok(resp) => match resp.json::<Manifest>().await {
            Ok(m) => m,
            Err(e) => {
                fail(
                    state,
                    format!(
                        "release channel manifest is unreadable: {}",
                        error::chain(&e)
                    ),
                )
                .await;
                return Checked::Failed;
            }
        },
    };

    let available = manifest.commit != CURRENT_COMMIT;
    set_state(state, |s| {
        s.latest = available.then_some(manifest);
        s.phase = Phase::Idle;
        s.last_check_at = Some(timefmt::format_utc_iso8601(timefmt::unix_now_secs()));
    })
    .await;
    if available {
        Checked::Available
    } else {
        Checked::UpToDate
    }
}

/// Download, verify and activate the build recorded by the last check.
async fn install(state: &Arc<AppState>, client: &reqwest::Client) {
    let (manifest, path) = {
        let s = state.update_state.read().await;
        (s.latest.clone(), state.install_path.clone())
    };

    let Some(manifest) = manifest else {
        fail(
            state,
            "no update available to install — check for updates first".to_string(),
        )
        .await;
        return;
    };
    if !CAN_SELF_UPDATE {
        fail(
            state,
            "in-place self-update requires a Linux install".to_string(),
        )
        .await;
        return;
    }
    let Some(asset) = asset_for_arch() else {
        fail(
            state,
            "the release channel has no asset for this architecture".to_string(),
        )
        .await;
        return;
    };
    let Some(expected_sha) = manifest.assets.get(asset).cloned() else {
        fail(
            state,
            format!("release manifest lists no SHA-256 for {asset}"),
        )
        .await;
        return;
    };
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        fail(
            state,
            format!(
                "cannot locate a directory to install into (binary path: {})",
                path.display()
            ),
        )
        .await;
        return;
    };

    // A previous cycle may have already put this build at the install path
    // and only failed to activate it (a refused `execve`). Installing again
    // would rename that same build to `propmonitor.prev` — destroying the
    // last known-good binary the rollback in the README depends on — and
    // would re-download the asset every `check_interval` for as long as
    // activation keeps failing. If the build is already on disk, activating
    // it is all that is left to do.
    let installed = {
        let candidate = path.clone();
        let want = expected_sha.clone();
        tokio::task::spawn_blocking(move || already_installed(&candidate, &want))
            .await
            .unwrap_or(false)
    };
    if installed {
        eprintln!(
            "propmonitor: self-update: {} is already the published build; activating it",
            path.display()
        );
        set_state(state, |s| s.phase = Phase::Installing).await;
        activate(state, &path).await;
        return;
    }

    set_state(state, |s| s.phase = Phase::Downloading).await;

    let url = asset_url(&state.manifest_url, asset);
    // Same directory as the binary: the final step has to be a rename
    // within one filesystem, and PID-suffixed so two attempts can never
    // fight over one temp file.
    let tmp = dir.join(format!("propmonitor.new.{}", std::process::id()));

    match download_and_verify(
        client,
        &url,
        &expected_sha,
        &path,
        dir,
        &tmp,
        &state.config_path,
    )
    .await
    {
        Ok(()) => {
            set_state(state, |s| s.phase = Phase::Installing).await;
            activate(state, &path).await;
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            fail(state, e).await;
        }
    }
}

/// Stream the asset to a temp file beside the running binary, hashing as
/// it lands; verify the digest, preflight the result, then swap it in.
///
/// Nothing observable happens until the digest matches: a truncated,
/// corrupted or tampered download leaves the running binary alone.
async fn download_and_verify(
    client: &reqwest::Client,
    url: &str,
    expected_sha: &str,
    path: &Path,
    dir: &Path,
    tmp: &Path,
    config_path: &str,
) -> Result<(), String> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not download the new binary: {}", error::chain(&e)))?;
    if !resp.status().is_success() {
        return Err(format!(
            "new binary download failed: HTTP {}",
            resp.status()
        ));
    }

    let mut file = tokio::fs::File::create(tmp)
        .await
        .map_err(|e| format!("cannot write into {}: {e}", dir.display()))?;
    let mut hasher = Sha256::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                file.write_all(&chunk)
                    .await
                    .map_err(|e| format!("cannot write the new binary: {e}"))?;
                hasher.update(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(format!(
                    "new binary download interrupted: {}",
                    error::chain(&e)
                ))
            }
        }
    }
    // Both flushes matter: the bytes have to be on the device before the
    // rename publishes them, or a power cut leaves a valid directory
    // entry pointing at a partial file.
    file.flush()
        .await
        .map_err(|e| format!("cannot write the new binary: {e}"))?;
    file.sync_all()
        .await
        .map_err(|e| format!("cannot flush the new binary to disk: {e}"))?;
    drop(file);

    let actual_sha = hex_of(hasher.finalize());
    if !expected_sha.eq_ignore_ascii_case(&actual_sha) {
        return Err(format!(
            "new binary failed its SHA-256 check (manifest {expected_sha}, download {actual_sha}); the running binary was left untouched"
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot make the new binary executable: {e}"))?;
    }

    let probe = tmp.to_path_buf();
    let config_path = config_path.to_string();
    // `preflight` owns its own deadline and always reaps the probe, so
    // there is no outer `tokio::time::timeout` here: one around
    // `spawn_blocking` would abandon the future while `wait(2)` kept
    // running, leaking a blocking thread and an orphan child per attempt.
    tokio::task::spawn_blocking(move || preflight(&probe, &config_path, PREFLIGHT_TIMEOUT))
        .await
        .map_err(|e| format!("preflight run could not be started: {e}"))??;

    let target = path.to_path_buf();
    let install_dir = dir.to_path_buf();
    let staged = tmp.to_path_buf();
    // Two renames plus a directory `fsync`, all blocking — and the fsync
    // is the one that can stall on an SD card. Off the runtime it goes.
    tokio::task::spawn_blocking(move || swap(&target, &install_dir, &staged))
        .await
        .map_err(|e| format!("install step could not be started: {e}"))?
}

/// Prove the downloaded binary both runs on this system and accepts the
/// config this node is actually running.
///
/// Two failures make an install fatal. A build that cannot start at all
/// (bad glibc, missing symbol) is caught by anything that executes it. A
/// build that starts but *rejects the live config* — tightened validation,
/// a key that changed shape — is the dangerous one: the swap succeeds, the
/// `execve` succeeds, and the new image then dies during config load, which
/// `Restart=always` flaps forever while `propmonitor.prev` is never
/// restored. Because every node polls the same manifest, that would take
/// the fleet off the air within one `check_interval`.
///
/// `--check-config <path>` covers both: the candidate loads and validates
/// the real config file, binds no port, and opens no SDR.
fn preflight(new_binary: &Path, config_path: &str, timeout: Duration) -> Result<(), String> {
    let probe = run_probe(new_binary, &["--check-config", config_path], timeout)?;
    if probe.ok {
        return Ok(());
    }
    // A build older than `--check-config` reads it as the config path and
    // fails its load with that argument in the message. Refusing those
    // would strand a fleet whenever the channel republishes an earlier
    // commit, so they fall back to the probe they do understand.
    if probe
        .stderr
        .contains("failed to load config from --check-config")
    {
        return legacy_preflight(new_binary, timeout);
    }
    // Covers both shapes of failure: a build that cannot start at all, and
    // one that starts and then refuses this node's config.
    Err(format!(
        "the new binary failed its preflight run against {config_path}: {}",
        excerpt(&probe.stderr)
    ))
}

/// The pre-`--check-config` probe: run the candidate against a config path
/// that cannot exist. A working build fails during config load — before it
/// opens the SDR, binds a port, or writes anything — and says so on
/// stderr. This is the probe `install.sh` also runs after downloading a
/// release.
fn legacy_preflight(new_binary: &Path, timeout: Duration) -> Result<(), String> {
    let probe = run_probe(
        new_binary,
        &["/nonexistent/propmonitor-preflight.yaml"],
        timeout,
    )?;
    if probe.ok {
        return Err("the new binary accepted a config path that does not exist".to_string());
    }
    if !probe.stderr.contains("failed to load config from") {
        return Err(format!(
            "the new binary failed its preflight run: {}",
            excerpt(&probe.stderr)
        ));
    }
    Ok(())
}

/// Outcome of one probe run: whether it exited successfully, and what it
/// said on stderr.
struct Probe {
    ok: bool,
    stderr: String,
}

/// Run the candidate binary with a hard deadline, and *always* reap it.
///
/// A probe that never exits is killed and waited for, so no orphan child
/// and no stuck blocking thread survives the attempt — this runs once per
/// `check_interval` forever, so a leak here is unbounded, and an orphan
/// that did manage to start up would be a second daemon competing for the
/// SDR.
fn run_probe(binary: &Path, args: &[&str], timeout: Duration) -> Result<Probe, String> {
    let mut child = std::process::Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("the new binary will not start: {e}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut raw = Vec::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_end(&mut raw);
                }
                return Ok(Probe {
                    ok: status.success(),
                    stderr: String::from_utf8_lossy(&raw).into_owned(),
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the new binary did not finish its preflight run in time".to_string());
            }
            Ok(None) => std::thread::sleep(PROBE_POLL),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait for the preflight run: {e}"));
            }
        }
    }
}

/// Enough of a probe's stderr to diagnose it from the UI or the journal,
/// without pasting a whole backtrace into the update state.
fn excerpt(stderr: &str) -> String {
    let text: String = stderr.trim().chars().take(300).collect();
    if text.is_empty() {
        // A build that dies without a word still has to produce a usable
        // line in `last_error`.
        return "no output on stderr".to_string();
    }
    text
}

/// Publish the verified binary: keep the current one aside, rename the new
/// one over the path, persist the directory entry.
///
/// `rename(2)` over a running executable is allowed — the kernel holds the
/// old inode open for the live process (which is why the old binary keeps
/// working until it execs). Writing *into* the running file is what would
/// fail with `ETXTBSY`, and this never does that.
///
/// Whatever sits at `path` becomes `propmonitor.prev`, so the caller must
/// not call this for a build that is already installed — [`install`] hashes
/// the file first for exactly that reason, or the one generation of backup
/// would be overwritten with the build being installed.
fn swap(path: &Path, dir: &Path, tmp: &Path) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("propmonitor");
    let backup = dir.join(format!("{name}.prev"));

    let backed_up = if path.exists() {
        std::fs::rename(path, &backup)
            .map_err(|e| format!("could not set the current binary aside: {e}"))?;
        true
    } else {
        false
    };

    if let Err(e) = std::fs::rename(tmp, path) {
        // Never leave the install without a binary at its known path.
        if backed_up {
            let _ = std::fs::rename(&backup, path);
        }
        return Err(format!("could not install the new binary: {e}"));
    }

    // Persist the directory entry, so a power cut cannot resurrect the
    // old name pointing at nothing.
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// SHA-256 of a file, or `None` if it cannot be read.
///
/// Used to recognise a build that is already sitting at the install path.
/// Blocking, so callers on the runtime hand it to `spawn_blocking`.
fn file_sha256(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Some(hex_of(hasher.finalize())),
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Whether the build the manifest describes is already the file at the
/// install path — the state a successful swap plus a failed `execve`
/// leaves behind.
///
/// Case-insensitive, like the download's own digest check: the manifest is
/// generated by `sha256sum` today, but nothing about the format promises
/// lowercase.
fn already_installed(path: &Path, expected_sha: &str) -> bool {
    file_sha256(path).is_some_and(|sha| sha.eq_ignore_ascii_case(expected_sha))
}

/// Replace this process image with the freshly installed binary.
///
/// On success this call never returns: same PID, same systemd unit state,
/// argv and environment carried over verbatim. If `execve` fails, the old
/// image is still running and perfectly usable, so we ask the service
/// manager for a conventional restart and report what happened.
async fn activate(state: &Arc<AppState>, path: &Path) {
    #[cfg(unix)]
    {
        eprintln!(
            "propmonitor: self-update: re-executing {} in place",
            path.display()
        );
        match exec_in_place(path) {
            Ok(()) => unreachable!("execve does not return on success"),
            Err(e) => {
                fail(
                    state,
                    format!(
                        "the new binary is installed, but restarting in place failed ({e}); \
                         run `systemctl restart propmonitor` to activate it"
                    ),
                )
                .await;
                // Best effort: on a systemd install this activates the new
                // binary anyway, a second or two later.
                std::thread::spawn(|| {
                    let _ = std::process::Command::new("systemctl")
                        .args(["restart", "propmonitor"])
                        .status();
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Unreachable: `install` refuses off-Linux. Kept total so the
        // function compiles everywhere the daemon does.
        let _ = path;
        set_state(state, |s| s.phase = Phase::Idle).await;
    }
}

/// `execve(2)` on the installed path, carrying argv and the environment
/// across unchanged.
///
/// Declared here rather than pulling in `libc`: one extern for one call on
/// one platform is smaller than a dependency, and the signature is
/// POSIX-fixed.
#[cfg(unix)]
fn exec_in_place(path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::io::{Error, ErrorKind};
    use std::os::raw::c_char;
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn execve(
            path: *const c_char,
            argv: *const *const c_char,
            envp: *const *const c_char,
        ) -> i32;
    }

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "install path contains a NUL byte"))?;

    // argv[0] is the installed path — which is also what it was before the
    // swap, since the file was renamed over the same name. Everything
    // after it (the config path) carries over untouched.
    //
    // `into_raw` leaks each string deliberately: it has to outlive the
    // call, and on success there is no "after" in this image.
    let mut argv: Vec<*const c_char> = vec![c_path.as_ptr()];
    for arg in std::env::args_os().skip(1) {
        let c = CString::new(arg.as_os_str().as_bytes())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "argument contains a NUL byte"))?;
        argv.push(c.into_raw());
    }
    argv.push(std::ptr::null());

    let mut envp: Vec<*const c_char> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let mut entry = key.as_os_str().as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_os_str().as_bytes());
        let c = CString::new(entry).map_err(|_| {
            Error::new(
                ErrorKind::InvalidData,
                "environment entry contains a NUL byte",
            )
        })?;
        envp.push(c.into_raw());
    }
    envp.push(std::ptr::null());

    // SAFETY: `c_path` is a live NUL-terminated string; `argv` and `envp`
    // are NULL-terminated arrays of live NUL-terminated strings (leaked
    // above, so they outlive the call). On success the image is replaced
    // and nothing here is reachable; on failure nothing was modified.
    let rc = unsafe { execve(c_path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    debug_assert_eq!(rc, -1, "execve returns only on failure");
    let _ = rc;
    Err(Error::last_os_error())
}

/// Mutate the channel state and push the result to every open UI.
async fn set_state(state: &Arc<AppState>, f: impl FnOnce(&mut UpdateState)) {
    let mut s = state.update_state.write().await;
    f(&mut s);
    let event = WsEvent::Update {
        phase: s.phase,
        latest: s.latest.clone(),
        error: s.last_error.clone(),
    };
    // Errors here mean "no UI is open", which is not a problem.
    let _ = state.broadcaster.send(event);
}

/// Record a failure: journal it for the operator running `journalctl`, and
/// surface it in the UI.
async fn fail(state: &Arc<AppState>, message: String) {
    eprintln!("propmonitor: self-update: {message}");
    set_state(state, |s| {
        s.phase = Phase::Idle;
        s.last_error = Some(message);
    })
    .await;
}

/// Lowercase hex, without pulling in a crate for sixteen characters.
fn hex_of(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch directory that cleans itself up, so a failed assertion
    /// cannot leave junk in the temp dir for the next run to trip over.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "propmonitor-update-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_first_failure_retries_within_the_minute() {
        let hour = Duration::from_secs(3600);
        assert_eq!(retry_delay(None, hour), FIRST_RETRY);
    }

    #[test]
    fn repeated_failures_back_off_up_to_the_configured_interval() {
        let interval = Duration::from_secs(600);
        let mut delay = retry_delay(None, interval);
        let mut seen = vec![delay];
        for _ in 0..8 {
            delay = retry_delay(Some(delay), interval);
            seen.push(delay);
        }
        assert_eq!(
            seen,
            [30, 60, 120, 240, 480, 600, 600, 600, 600]
                .map(Duration::from_secs)
                .to_vec()
        );
    }

    #[test]
    fn a_retry_never_polls_faster_than_the_interval_floor() {
        // `check_interval` is floored at 60 s, but an interval shorter than
        // the first retry must still cap it rather than the other way
        // round: the retry path may not out-poll the healthy path.
        let interval = Duration::from_secs(10);
        assert_eq!(retry_delay(None, interval), interval);
        assert_eq!(retry_delay(Some(interval), interval), interval);
    }

    /// Serves one asset body at `/asset` and one manifest at `/manifest`.
    /// Returns the base URL.
    async fn fake_channel(asset: &'static [u8], manifest: String) -> String {
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/asset", get(move || async move { asset }))
            .route(
                "/manifest",
                get(move || {
                    let manifest = manifest.clone();
                    async move {
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            manifest,
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[test]
    fn manifest_parses_the_shape_the_workflow_publishes() {
        let json = r#"{
            "version": "0.2.0",
            "commit": "0f1e2d3c4b5a69788796a5b4c3d2e1f001234567",
            "built_at": "2026-08-30T12:34:56Z",
            "assets": {
              "propmonitor-x86_64-linux": "ABCDEF",
              "propmonitor-aarch64-linux": "123456"
            }
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.commit, "0f1e2d3c4b5a69788796a5b4c3d2e1f001234567");
        assert_eq!(m.built_at, "2026-08-30T12:34:56Z");
        assert_eq!(m.assets["propmonitor-aarch64-linux"], "123456");
        // Round-trips, because the same struct is echoed to the UI.
        let back: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn phase_serializes_as_the_ui_expects() {
        let json = serde_json::to_string(&Phase::Downloading).unwrap();
        assert_eq!(json, "\"downloading\"");
        assert!(Phase::Checking.is_busy());
        assert!(Phase::Downloading.is_busy());
        assert!(Phase::Installing.is_busy());
        assert!(!Phase::Idle.is_busy(), "idle is the only free state");
    }

    #[test]
    fn hex_of_matches_a_known_digest() {
        // SHA-256 of the empty input, from the FIPS test vectors.
        assert_eq!(
            hex_of(Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn usable_exe_path_handles_renamed_and_deleted_binaries() {
        let dir = TempDir::new("exepath");
        let bin = dir.join("propmonitor");
        std::fs::write(&bin, b"#!/bin/sh\nexit 1\n").unwrap();
        let canonical = std::fs::canonicalize(&bin).unwrap();

        // The ordinary case: the path the kernel reports exists.
        assert_eq!(
            usable_exe_path(bin.to_str().unwrap()),
            Some(canonical.clone())
        );

        // The kernel appends " (deleted)" once the inode is unlinked. If
        // something has since been installed at that path, that is the
        // binary to replace.
        assert_eq!(
            usable_exe_path(&format!("{} (deleted)", bin.display())),
            Some(canonical)
        );

        // Nothing there at all → caller falls back to argv[0].
        std::fs::remove_file(&bin).unwrap();
        assert_eq!(usable_exe_path(bin.to_str().unwrap()), None);
        assert_eq!(
            usable_exe_path(&format!("{} (deleted)", bin.display())),
            None
        );
        assert_eq!(usable_exe_path(""), None);
    }

    #[test]
    fn resolve_install_path_returns_an_absolute_path() {
        let path = resolve_install_path();
        assert!(
            path.is_absolute(),
            "install path must be absolute, got {}",
            path.display()
        );
    }

    /// Writes an executable stand-in for a downloaded build.
    fn write_probe(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A stand-in build that behaves like a current one: it validates the
    /// config path it is handed and says nothing.
    const CHECKS_CONFIG: &[u8] = b"#!/bin/sh\n[ \"$1\" = \"--check-config\" ] && [ -f \"$2\" ] && exit 0\necho \"the config path was not checked: $*\" >&2\nexit 1\n";

    /// The whole file-level chain: download, hash, verify, preflight,
    /// back up, swap. Activation (`execve`) is deliberately not exercised
    /// — it would replace the test process.
    #[tokio::test]
    async fn download_and_verify_swaps_the_binary_and_keeps_a_backup() {
        let base = fake_channel(CHECKS_CONFIG, String::new()).await;

        let dir = TempDir::new("swap");
        let path = dir.join("propmonitor");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let tmp = dir.join("propmonitor.new.test");
        let sha = hex_of(Sha256::digest(CHECKS_CONFIG));
        // The preflight has to be handed this, not a path that cannot
        // exist: the probe above only passes if the file is really there.
        let config = dir.join("config.yaml");
        std::fs::write(&config, b"frequency: 28330000\n").unwrap();

        let client = reqwest::Client::new();
        download_and_verify(
            &client,
            &format!("{base}/asset"),
            &sha,
            &path,
            &dir.0,
            &tmp,
            config.to_str().unwrap(),
        )
        .await
        .expect("install chain");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            CHECKS_CONFIG,
            "the new binary is at the install path"
        );
        assert_eq!(
            std::fs::read(dir.join("propmonitor.prev")).unwrap(),
            b"#!/bin/sh\nexit 0\n",
            "the previous binary is kept for rollback"
        );
        assert!(!tmp.exists(), "the temp file is consumed by the rename");
    }

    #[tokio::test]
    async fn download_and_verify_refuses_a_bad_digest_and_keeps_the_running_binary() {
        const TAMPERED: &[u8] = b"#!/bin/sh\nexit 1\n";
        let base = fake_channel(TAMPERED, String::new()).await;

        let dir = TempDir::new("sha");
        let path = dir.join("propmonitor");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let tmp = dir.join("propmonitor.new.test");

        let client = reqwest::Client::new();
        let err = download_and_verify(
            &client,
            &format!("{base}/asset"),
            "00000000000000000000000000000000000000000000000000000000deadbeef",
            &path,
            &dir.0,
            &tmp,
            "/nonexistent/config.yaml",
        )
        .await
        .expect_err("a digest mismatch must abort the install");

        assert!(err.contains("SHA-256"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"#!/bin/sh\nexit 0\n",
            "the running binary is untouched"
        );
        assert!(
            !dir.join("propmonitor.prev").exists(),
            "nothing was swapped, so nothing was backed up"
        );
    }

    #[tokio::test]
    async fn download_and_verify_rejects_a_binary_that_fails_preflight() {
        // Right digest, but the build is broken: it exits non-zero without
        // ever reaching config load, exactly like a glibc mismatch.
        const BROKEN: &[u8] = b"#!/bin/sh\necho 'symbol lookup error' >&2\nexit 127\n";
        let base = fake_channel(BROKEN, String::new()).await;

        let dir = TempDir::new("preflight");
        let path = dir.join("propmonitor");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let tmp = dir.join("propmonitor.new.test");
        let sha = hex_of(Sha256::digest(BROKEN));

        let client = reqwest::Client::new();
        let err = download_and_verify(
            &client,
            &format!("{base}/asset"),
            &sha,
            &path,
            &dir.0,
            &tmp,
            "/nonexistent/config.yaml",
        )
        .await
        .expect_err("a binary that cannot start must not be swapped in");

        assert!(err.contains("preflight"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"#!/bin/sh\nexit 0\n",
            "the running binary is untouched"
        );
    }

    /// The fleet-killer this preflight exists for: a build that starts
    /// fine and then refuses the config the node is running. Installing it
    /// would crash-loop the daemon under `Restart=always` with nothing
    /// restoring `propmonitor.prev`.
    #[tokio::test]
    async fn download_and_verify_rejects_a_build_that_refuses_the_live_config() {
        const PICKY: &[u8] = b"#!/bin/sh\necho 'Error: config: update.check_interval must be at least 60 seconds' >&2\nexit 1\n";
        let base = fake_channel(PICKY, String::new()).await;

        let dir = TempDir::new("liveconfig");
        let path = dir.join("propmonitor");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let tmp = dir.join("propmonitor.new.test");
        let sha = hex_of(Sha256::digest(PICKY));
        let config = dir.join("config.yaml");
        std::fs::write(&config, b"frequency: 28330000\n").unwrap();

        let client = reqwest::Client::new();
        let err = download_and_verify(
            &client,
            &format!("{base}/asset"),
            &sha,
            &path,
            &dir.0,
            &tmp,
            config.to_str().unwrap(),
        )
        .await
        .expect_err("a build that rejects the live config must not be installed");

        assert!(
            err.contains("check_interval") && err.contains(config.to_str().unwrap()),
            "the error must name the config and quote the build's complaint: {err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"#!/bin/sh\nexit 0\n",
            "the running binary is untouched"
        );
        assert!(
            !dir.join("propmonitor.prev").exists(),
            "nothing was swapped, so nothing was backed up"
        );
    }

    /// A build that predates `--check-config` reads it as a config path.
    /// Refusing those would strand a fleet whenever the channel
    /// republishes an earlier commit, so they get the older probe.
    #[test]
    fn preflight_accepts_builds_that_predate_check_config() {
        let dir = TempDir::new("legacy");
        let bin = dir.join("propmonitor.new");
        write_probe(
            &bin,
            "#!/bin/sh\necho \"failed to load config from $1\" >&2\nexit 1\n",
        );

        preflight(&bin, "/nonexistent/config.yaml", Duration::from_secs(10))
            .expect("an older build is still installable");
    }

    /// A probe that never exits is killed, not abandoned: the marker its
    /// script would write afterwards must never appear, or the child (and
    /// the blocking thread waiting on it) outlived the attempt.
    #[test]
    fn preflight_kills_a_probe_that_hangs() {
        let dir = TempDir::new("hang");
        let bin = dir.join("propmonitor.new");
        let marker = dir.join("probe-finished");
        write_probe(
            &bin,
            &format!(
                "#!/bin/sh\nsleep 3\n: > {}\n",
                marker.to_str().expect("temp path is utf-8")
            ),
        );

        let started = Instant::now();
        let err = preflight(&bin, "/nonexistent/config.yaml", Duration::from_millis(200))
            .expect_err("a hung probe must fail the install");

        assert!(err.contains("in time"), "unexpected error: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "preflight waited past its deadline"
        );
        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "the probe kept running after preflight gave up"
        );
    }

    #[test]
    fn already_installed_recognises_the_build_on_disk() {
        let dir = TempDir::new("installed");
        let path = dir.join("propmonitor");
        std::fs::write(&path, CHECKS_CONFIG).unwrap();
        let sha = hex_of(Sha256::digest(CHECKS_CONFIG));

        assert!(
            already_installed(&path, &sha),
            "a swapped-in build that failed to activate must be recognised"
        );
        assert!(
            already_installed(&path, &sha.to_uppercase()),
            "the manifest's digest case must not matter"
        );
        assert!(
            !already_installed(&path, &hex_of(Sha256::digest(b"something else"))),
            "a different build must still be downloaded"
        );
        assert!(
            !already_installed(&dir.join("absent"), &sha),
            "a missing file is not an installed build"
        );
    }

    #[test]
    fn swap_rolls_back_when_the_new_binary_cannot_be_moved_into_place() {
        let dir = TempDir::new("rollback");
        let path = dir.join("propmonitor");
        std::fs::write(&path, b"current\n").unwrap();
        // Nothing at the temp path at all, so the second rename fails with
        // ENOENT after the current binary has already been moved aside.
        // That is the window the rollback exists for.
        let tmp = dir.join("propmonitor.new.missing");

        let err = swap(&path, &dir.0, &tmp).expect_err("renaming a missing file must fail");
        assert!(err.contains("could not install the new binary"), "{err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"current\n",
            "the previous binary is restored to its path"
        );
    }

    #[test]
    fn asset_url_is_a_sibling_of_the_manifest() {
        // The real channel: both are assets of the same release.
        assert_eq!(
            asset_url(MANIFEST_URL, "propmonitor-aarch64-linux"),
            "https://github.com/North-Texas-Microwave-Society/propmonitor/releases/download/latest/propmonitor-aarch64-linux"
        );
        // A node pointed at another channel downloads binaries from that
        // channel too — never half from one host and half from another.
        assert_eq!(
            asset_url(
                "http://127.0.0.1:8099/propmonitor-manifest.json",
                "propmonitor-x86_64-linux"
            ),
            "http://127.0.0.1:8099/propmonitor-x86_64-linux"
        );
        assert_eq!(
            asset_url("nonsense", "propmonitor-x86_64-linux"),
            "propmonitor-x86_64-linux"
        );
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        // Every target the channel builds for, and the exact asset name
        // latest.yml uploads for it.
        assert_eq!(
            asset_name("linux", "x86_64"),
            Some("propmonitor-x86_64-linux")
        );
        assert_eq!(
            asset_name("linux", "aarch64"),
            Some("propmonitor-aarch64-linux")
        );
        assert_eq!(asset_name("linux", "arm"), Some("propmonitor-armv7-linux"));

        // Nothing is published for these, so a node on one must not go
        // looking for a binary that cannot exist.
        assert_eq!(asset_name("linux", "riscv64"), None);
        assert_eq!(asset_name("macos", "aarch64"), None);
        assert_eq!(asset_name("windows", "x86_64"), None);
        assert_eq!(asset_name("freebsd", "x86_64"), None);
    }

    #[test]
    fn auto_updates_require_a_release_build() {
        // Belt and braces on the one invariant that keeps a developer's
        // laptop from swapping itself for a release binary.
        assert_eq!(auto_allowed(), IS_DIST_BUILD && CAN_SELF_UPDATE);
        if !IS_DIST_BUILD {
            assert!(!auto_allowed(), "local builds never auto-install");
        }
    }
}
