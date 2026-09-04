# AGENTS.md

> **Do not create or update `CLAUDE.md`.** `AGENTS.md` is this repository's single source of agent guidance. Never create or modify `CLAUDE.md` — all agent guidance belongs here.

## What this is

`propmonitor` is a headless Rust daemon that captures IQ from a SoapySDR
device, measures noise/signal/SNR over a configurable integration
window, exposes a live web UI + REST/WebSocket API on the LAN, and
periodically uploads measurements to **microwaveprop** so beacon
strength can be correlated with weather over time. The point is
24/7 beacon monitoring, not interactive use.

## Commands

```bash
cargo build --release
./target/release/propmonitor                  # uses ./config.yaml
./target/release/propmonitor path/to.yaml
cargo test
cargo test tone_in_passband_has_high_snr      # single test
cargo test --lib sync::                       # one module's tests
cargo clippy --all-targets -- -D warnings     # the gate; plain `cargo clippy` misses tests
cargo fmt
```

Build prerequisites: Rust 1.85+ (floor comes from `reqwest` 0.13; there is
no `rust-version` key in `Cargo.toml` enforcing it), `pkg-config`, and
SoapySDR headers. Runtime additionally needs the driver module for the
dongle being used.

There is no `tests/` directory. Every test lives in an inline
`#[cfg(test)] mod tests` at the bottom of its own module, including the
axum handler tests in `server.rs` (via `tower::ServiceExt::oneshot`) and
the sync-protocol tests in `sync.rs` (which stand up a loopback
WebSocket server). New tests go in the module under test, not a new file.

## Repo layout and forge

`origin` is a private Forgejo instance (`git.mcintire.me`) that
push-mirrors to the public GitHub repo. Consequences:

- Pull requests go to Forgejo through the `gitea` MCP server. `gh` does
  not work against it.
- CI only exists on the GitHub side. Nothing runs on push to Forgejo, so
  `cargo clippy --all-targets -- -D warnings && cargo test` locally is
  the only pre-merge check there is.

## Releasing

Two channels, and the first one is a deployment:

**Push to `main` → the rolling channel.** `.github/workflows/latest.yml`
builds all three Linux targets and republishes the `latest` GitHub Release
(raw binaries + tarballs + `propmonitor-manifest.json`). Deployed nodes
poll that release and install it themselves, by default within the hour, so
**a push to `main` ships to every node**. `install.sh` resolves "latest
release" to the same build, so a fresh install and a self-updated node run
identical binaries. Run the checks before pushing, not after.

**Push a tag → an archival release.**

```bash
cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release
git tag vX.Y.Z && git push origin vX.Y.Z
```

The tag reaches GitHub via the mirror and triggers
`.github/workflows/release.yml`, which cross-compiles x86_64, aarch64 and
armv7 Linux binaries (aarch64/armv7 through `cross` using the images in
`docker/`) and attaches them to a GitHub Release. The tagged commit
should be the tip of `main`.

Both workflows set `PROP_BUILD_COMMIT`; `build.rs` bakes it in as the
update identity and as the "official build" flag that gates unattended
updates. `Cross.toml` passes it into the cross containers — drop that and
cross-built binaries silently become `dev` builds that never auto-update.

The `latest` tag is load-bearing, and the Forgejo→GitHub push mirror is out
to delete it. GitHub converts a release into a **draft** the moment its tag
ref disappears, and a draft serves no assets — so a mirror sync that prunes
`refs/tags/latest` (it prunes every ref the upstream doesn't have) takes the
whole channel offline: `releases/download/latest/...` 404s on every node and
`install.sh` silently falls back to the newest *version* tag, which predates
self-update. That happened between 2026-08-30 and 2026-09-01.

The tag is therefore pinned on **Forgejo** (`git tag latest <sha> && git
push origin latest`), so a mirror sync force-updates it instead of deleting
it, and `latest.yml` only creates it when it is missing — never moves it,
since the mirror would undo the move and the release notes carry the commit
the assets were built from. Keep that tag on origin; deleting it re-arms the
failure.

Two consequences for the publish job: it looks the release up by *listing*
releases (`releases/tags/latest` does not return drafts, so a by-tag lookup
misses a drafted channel and mints a duplicate release for the same tag),
and it sets `draft=false` on every run. Its last step curls the manifest URL
nodes actually poll and fails if it isn't serving the new commit, so a
broken channel is a red build rather than a field problem.

Don't force-move a published *version* tag; cut the next version instead.
Keep version tags in step with `version` in `Cargo.toml` — the manifest
reports that value, so a stale `Cargo.toml` mislabels every node's build in
the UI (the update decision itself uses the commit, so it stays correct).

The release runners are Ubuntu 24.04, so published binaries need glibc
2.38. That floor is what README's support table promises; lowering it
means changing the base images in `docker/`.

## Architecture

One process owns the SDR (single-instance). Inside the process:

- **`src/main.rs`** — Boots the tokio runtime on the main thread, sets
  up shared state via `boot`, spawns the HTTP/WS server task, and then
  blocks on Ctrl+C until shutdown. Fully headless — no tray, no
  auto-browser.
- **`src/worker.rs`** — Owns the SDR device. Sync `std::thread`, not
  async — SoapySDR is sync and blocking reads are simplest. Emits
  `WorkerEvent`s (`PeriodStarted`, `PeriodMeasurement`, `WaterfallRow`,
  `RawLevel`, `DeviceInfo`, `Error`) over an mpsc channel.
- **`src/server.rs`** — axum HTTP/WS server + a sync→async bridge
  thread. The bridge reads `WorkerEvent`s from the mpsc, converts each
  to a `WsEvent`, updates derived state (`device_info`, `last_raw_dbfs`,
  `MeasurementStore`), and broadcasts on a `tokio::sync::broadcast` that
  WS clients and the uploader subscribe to. `handle_ws` opens each client
  with `snapshot_events` (device, raw level, last measurement, uploader
  status, config) and then heartbeats every 10 s, so the LAN UI is
  push-only: nothing it displays needs a page reload or a poll.
- **`src/uploader.rs`** — Long-running tokio task. Subscribes to the
  broadcast, builds a JSON payload from each `Measurement` + the current
  config (callsign, frequency, passband), POSTs to microwaveprop with
  Bearer auth. Failures go into a bounded retry queue (cap 1440 = 24 h)
  with exponential backoff (1 s → 5 min).
- **`src/sync.rs`** — Two-way config sync with microwaveprop for
  **managed** monitors (`api.md` §5). Long-running tokio task: WebSocket
  to `MICROWAVEPROP_SYNC_ENDPOINT` with 1 s → 5 min reconnect backoff,
  `hello` on connect, config pushed down / local edits reported up, 60 s
  status heartbeat, 30 s protocol pings, and a 60 s HTTP polling cycle
  while the socket is down. Idles until `microwaveprop.monitor_token`
  exists, woken by `AppState::sync_notify`, so a self-service install
  never talks to the endpoint and pasting a token in the LAN UI starts
  sync without a restart.
- **`src/update.rs`** — Self-update channel (`api.md` §6). Long-running
  tokio task: GETs the fixed `latest`-release manifest, compares the
  manifest commit against `PROP_BUILD_COMMIT` baked in by `build.rs`, and
  on a difference downloads the arch asset next to the running binary,
  verifies its SHA-256, preflights it (`propmonitor --check-config <the
  node's own config>`, killed and reaped if it overruns 20 s), keeps the old
  one as `propmonitor.prev`, renames the new one into place, and `execve`s
  it. Same PID, so systemd sees no restart. Woken early by
  `AppState::update_notify` for the UI's check/install buttons. Guards worth
  keeping: SHA mismatch aborts; a build that rejects the live config is
  refused (it would otherwise crash-loop the whole fleet under
  `Restart=always`); a build already sitting at the install path is
  activated rather than re-installed, so one generation of `propmonitor.prev`
  survives a failed `execve`; unattended installs require an official build;
  non-Linux refuses outright.
- **`src/measure.rs`** — `SpectrumAnalyzer`: Hann-windowed FFT,
  fftshift on the fly, in-band peak + average tracking, median-of-out-
  of-passband noise floor with a guard region. `start_window(offset_hz,
  width_hz)` → `add_frame(…)` → `finalize()`. Mode-agnostic.
- **`src/config.rs`** — `Mode` enum + per-mode `passband_for(mode,
  beacon)`. `Mode::Beacon` reads its passband from `beacon: { offset_hz,
  bandwidth_hz }` so the operator can dial in a narrow window for a
  specific beacon carrier.
- **`src/store.rs`** — 24 h in-memory ring buffer (1440 cap) of recent
  `Measurement`s. Drives `/api/measurements`. Not persisted — restarts
  reset history.
- **`src/yaml.rs`** — Minimal in-tree YAML parser AND writer for the
  small set of fields the config uses. No serde/yaml dep.
- **`src/timefmt.rs`** — Portable Unix-seconds → ISO-8601 UTC
  formatting via Howard Hinnant's `civil_from_days` algorithm. No
  `gmtime_r`, so there's no libc dependency to vary across targets.
- **`src/error.rs`** — One stringy `Error(String)` for the whole crate,
  built with `Error::msg(..)`. There is no error enum and callers don't
  match on variants; keep new failures as formatted context strings.
- **`src/web/`** — Embedded HTML/CSS/JS (single page, no build step).
  Canvas waterfall, settings form bound to `/api/config`, live dBFS +
  measurement readouts, upload-status line, update status + check/install
  buttons bound to `/api/update`. The page fetches at boot and then lives
  off `/ws`: it redials after 25 s without a frame (the heartbeat makes
  that mean "dead socket", not "quiet SDR"), re-reads `/api/update` on
  every connect because a self-update changes the running build under it,
  and re-renders the settings form from `config` frames — unless the
  operator has unsaved edits, which are announced rather than clobbered.
- **`src/bin/sdr_diag.rs`** — Standalone diagnostic CLI. Useful when
  the waterfall in the web UI looks wrong and you want to rule out the
  DSP/server path.

`Measurement` reports **both** `signal_peak_dbfs` and `signal_avg_dbfs`
on purpose: peak is for keyed/bursty signals (CW beacons), avg is for
continuous carriers (FM/AM). Tests in `measure.rs` pin that contract.

## Docs that are contracts, not commentary

- **`api.md`** is the spec both halves of the system target. §1–§3 are the
  LAN REST/WS/config surface, §4 is the microwaveprop ingest POST, §5 is
  the config-sync protocol. Changing a payload shape in `uploader.rs` or
  `sync.rs` without updating the matching section leaves microwaveprop
  building against a stale contract.
- **`output.md`** restates §4 for the microwaveprop implementers, down to
  a sketch Elixir schema. It duplicates api.md by design and drifts by
  accident — when the upload payload changes, both files need the edit.
- **`README.md`** is operator-facing (supported OSes, supported dongles,
  install, troubleshoot). Keep dev detail out of it; it belongs here.

The three microwaveprop URLs are compile-time constants, not config:
`MICROWAVEPROP_ENDPOINT` (`uploader.rs`) and `MICROWAVEPROP_SYNC_ENDPOINT`
/ `_CONFIG_` / `_STATUS_ENDPOINT` (`sync.rs`). Every install reports to the
same server; only the per-station credentials live in `config.yaml`.

## Adding or renaming a config field

The config schema is hand-maintained in more places than a serde struct
would be, and one of them is bash. A new field usually means all of:

1. `src/config.rs` — the struct field, its default, and validation.
2. `src/yaml.rs` — both the parse side and the write side.
3. `src/server.rs` — `ConfigView` (the LAN API shape) and `apply_config`.
4. `src/sync.rs` — `SyncConfig`, *only* if microwaveprop should own the
   field. Secrets and anything that could strand the LAN UI stay out.
5. `src/web/` — the settings form, if an operator should be able to set it.
6. `install.sh` — it reads and rewrites `config.yaml` with its own
   line-oriented `yaml_get` / `yaml_put` / `yaml_set` bash helpers, which
   know the field list by hand. A field the installer doesn't know is a
   field first-run setup silently can't configure.
7. `api.md` §3 (and §5 if synced), plus README's config sample.
8. `api.md` §1's `GET /api/config` example, if the field is in `ConfigView`.

## Deployed layout

`install.sh` puts the daemon at `/opt/propmonitor/bin/propmonitor`, **owned
by the `propmonitor` service user** — self-update is a rename inside that
directory, which needs write on the directory, so it cannot live in root's
`/usr/local/bin` (that's where `sdr_diag` stays). Config is
`/etc/propmonitor/config.yaml`, owned by a system user with mode 2770. The
hardened `propmonitor.service` unit lists exactly two `ReadWritePaths`: the
config dir and the binary dir. Anything else the daemon needs to write has
to be added there.

A re-run of the installer removes a stale `/usr/local/bin/propmonitor` from
a pre-self-update install, so a box doesn't end up with two binaries and a
unit pointing at one of them.

## Dependencies

Keep TLS on rustls end to end — `reqwest` with `default-features = false`,
`tokio-tungstenite` with `rustls-tls-webpki-roots`. No openssl anywhere;
it would break the `cross` builds for aarch64/armv7. `tokio-tungstenite`
is pinned to 0.29 so the sync client shares one tungstenite copy with
axum's `ws` feature.

`sha2` is pure Rust on purpose (self-update asset verification): a C
hashing library would reintroduce the cross-compilation problem rustls
exists to avoid. Hex formatting is a six-line local helper rather than the
`hex` crate.

## Persistent vs ephemeral

- **Persistent:** `config.yaml`. Written atomically by `PUT /api/config`
  (tmp + fsync + rename). The installed binary itself, plus one generation
  of backup (`propmonitor.prev`), courtesy of self-update.
- **Ephemeral:** measurement history, retry queue, raw dBFS, device
  info, and update-channel state (`latest`, `last_check_at`). All
  in-memory; a restart re-checks the channel immediately.

## When editing

- `FFT_SIZE` in `measure.rs` (16 384) is baked into buffer logic in
  `worker.rs`. Changing it shifts the noise/signal numbers slightly
  because bin width changes.
- `start_window` / `add_frame` / `finalize` is an ordered protocol.
  `add_frame` has a `debug_assert!` that `start_window` was called.
- The `analyze()` batch helper in `measure.rs` is `#[cfg(test)]` only.
  Production goes through the streaming path in `worker.rs`.
- Gain handling: if `cfg.gain` is `Some`, AGC is explicitly disabled and
  the gain is set. RTL-SDR's `TUNER` element gets the value; other
  drivers fall back to `set_gain`. If `cfg.gain` is `None`, AGC is on.
- The HTTP `bind` field defaults to `0.0.0.0:port` so the UI is reachable
  over the LAN, which is the only way to reach it on a headless install.
  `spawn_server` logs the actual bound address; don't add a second
  hardcoded-loopback log line next to it.
- Config writes go through one path: `server::apply_config` (yaml build →
  parse/validate → rebind if needed → write_config_file → swap →
  broadcast `WsEvent::Config` → worker restart). `PUT /api/config` and
  `sync.rs` both call it; don't grow a second one. That single funnel is
  what makes every open LAN page follow a config change it didn't make,
  so a new write path has to broadcast too or it goes out silently.
  `put_config` bumps `sync_notify` afterwards so the edit is
  reported upstream — a config that arrived *from* microwaveprop must not
  bump it, or the node and website mint versions at each other forever.
  Both writers (and `persist_config_version`) hold `AppState::config_write`
  for the whole read-modify-write; a new config writer must take it too.
- Nothing blocking runs on the runtime. The config write (`write` +
  `fsync` + `rename`, tens of ms on a Pi's SD card), the worker join in
  `restart_worker` (waits out an SDR stream read), `enumerate_devices`
  (scans USB), the update preflight and the binary swap all go through
  `spawn_blocking`. A blocking call left inline stalls every other
  request that runtime thread was serving — including the WS pushes the
  UI depends on.
- `microwaveprop.config_version` is bookkeeping owned by microwaveprop.
  The node persists and echoes it, never invents it. `persist_config_version`
  records a version *without* restarting the worker; that's the whole point
  of the content-compare no-op skip on a pushed config.
- The sync wire shape (`sync::SyncConfig`) is a distinct struct with no
  `monitor_token` field and no `http` block. Don't "simplify" it into
  `ConfigView`: the token would leak and a bad pushed `bind` would strand
  the LAN UI.
- The microwaveprop ingest endpoint (`api.md` §4) and the sync socket +
  polling endpoints (`api.md` §5) are being built on the microwaveprop
  side in parallel; those sections are the contract both halves target.
- Linux-only by design: the deployment target is a headless Debian /
  Raspberry Pi OS box running the systemd unit from `install.sh`. CI
  builds x86_64, aarch64 and armv7 Linux only.
- Self-update invariants worth not breaking: the SHA-256 check is the one
  hard gate (mismatch aborts, running binary untouched); `execve` keeps the
  PID so systemd never sees a restart; the previous binary stays as
  `propmonitor.prev` for rollback; assets resolve relative to the manifest
  URL so one setting moves a node to a fork's channel; and unattended
  installs require `PROP_BUILD_COMMIT` to have been set at build time, which
  is what stops a laptop build from replacing itself with a release binary.
- The `update` block never enters `sync::SyncConfig`. When a node is
  managed, the website still must not decide when it reboots itself;
  `to_config_update` sends `update: None`, which means "keep local values".
