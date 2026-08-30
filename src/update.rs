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
//! Every failure here is non-fatal and retried on the next tick. The one
//! hard gate is the SHA-256 check: a mismatch aborts the install and
//! leaves the running binary in place.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::MIN_CHECK_INTERVAL;
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

/// How long the new binary gets to fail its config-load preflight before
/// we call it hung.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);

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
            eprintln!("propmonitor: self-update disabled: HTTP client setup failed: {e}");
            return;
        }
    };

    let mut requests = state.update_notify.subscribe();
    // The first check runs immediately: a UI opened right after boot
    // should show a real answer, not "never checked".
    let mut wait = Duration::ZERO;

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

        let available = check(&state, &client).await;
        if request == UpdateRequest::Install || (available && auto && auto_allowed()) {
            install(&state, &client).await;
        }
    }
}

/// Fetch the channel manifest and record what it says. Returns whether the
/// channel offers a build other than the running one.
async fn check(state: &Arc<AppState>, client: &reqwest::Client) -> bool {
    let url = state.manifest_url.clone();
    set_state(state, |s| {
        s.phase = Phase::Checking;
        s.last_error = None;
    })
    .await;

    let manifest = match client.get(&url).send().await {
        Err(e) => {
            fail(state, format!("could not reach the release channel: {e}")).await;
            return false;
        }
        Ok(resp) if !resp.status().is_success() => {
            let status = resp.status();
            fail(state, format!("release channel answered HTTP {status}")).await;
            return false;
        }
        Ok(resp) => match resp.json::<Manifest>().await {
            Ok(m) => m,
            Err(e) => {
                fail(
                    state,
                    format!("release channel manifest is unreadable: {e}"),
                )
                .await;
                return false;
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
    available
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

    set_state(state, |s| s.phase = Phase::Downloading).await;

    let url = asset_url(&state.manifest_url, asset);
    // Same directory as the binary: the final step has to be a rename
    // within one filesystem, and PID-suffixed so two attempts can never
    // fight over one temp file.
    let tmp = dir.join(format!("propmonitor.new.{}", std::process::id()));

    match download_and_verify(client, &url, &expected_sha, &path, dir, &tmp).await {
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
) -> Result<(), String> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not download the new binary: {e}"))?;
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
            Err(e) => return Err(format!("new binary download interrupted: {e}")),
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
    tokio::time::timeout(
        PREFLIGHT_TIMEOUT,
        tokio::task::spawn_blocking(move || preflight(&probe)),
    )
    .await
    .map_err(|_| "new binary did not finish its preflight run in time".to_string())?
    .map_err(|e| format!("preflight run could not be started: {e}"))??;

    swap(path, dir, tmp)
}

/// Run the new binary against a config path that cannot exist.
///
/// A working build fails during config load — before it opens the SDR,
/// binds a port, or writes anything — and says so on stderr. Anything
/// else (a linker error, a missing glibc symbol, an immediate crash) is a
/// build we refuse to swap in. This is the same probe `install.sh` runs
/// after downloading a release.
fn preflight(new_binary: &Path) -> Result<(), String> {
    let out = std::process::Command::new(new_binary)
        .arg("/nonexistent/propmonitor-preflight.yaml")
        .output()
        .map_err(|e| format!("the new binary will not start: {e}"))?;

    if out.status.success() {
        return Err("the new binary accepted a config path that does not exist".to_string());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.contains("failed to load config from") {
        let detail: String = stderr.trim().chars().take(300).collect();
        return Err(format!("the new binary failed its preflight run: {detail}"));
    }
    Ok(())
}

/// Publish the verified binary: keep the current one aside, rename the new
/// one over the path, persist the directory entry.
///
/// `rename(2)` over a running executable is allowed — the kernel holds the
/// old inode open for the live process (which is why the old binary keeps
/// working until it execs). Writing *into* the running file is what would
/// fail with `ETXTBSY`, and this never does that.
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

    /// The whole file-level chain: download, hash, verify, preflight,
    /// back up, swap. Activation (`execve`) is deliberately not exercised
    /// — it would replace the test process.
    #[tokio::test]
    async fn download_and_verify_swaps_the_binary_and_keeps_a_backup() {
        // A "binary" that behaves like the real one under preflight:
        // non-zero exit, with the config-load message on stderr.
        const NEW_BINARY: &[u8] =
            b"#!/bin/sh\necho \"failed to load config from $1\" >&2\nexit 1\n";
        let base = fake_channel(NEW_BINARY, String::new()).await;

        let dir = TempDir::new("swap");
        let path = dir.join("propmonitor");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let tmp = dir.join("propmonitor.new.test");
        let sha = hex_of(Sha256::digest(NEW_BINARY));

        let client = reqwest::Client::new();
        download_and_verify(&client, &format!("{base}/asset"), &sha, &path, &dir.0, &tmp)
            .await
            .expect("install chain");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            NEW_BINARY,
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
        let err = download_and_verify(&client, &format!("{base}/asset"), &sha, &path, &dir.0, &tmp)
            .await
            .expect_err("a binary that cannot start must not be swapped in");

        assert!(err.contains("preflight"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"#!/bin/sh\nexit 0\n",
            "the running binary is untouched"
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
