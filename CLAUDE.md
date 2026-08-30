# CLAUDE.md

Guidance for Claude Code when working in this repo.

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

```bash
cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release
git tag vX.Y.Z && git push origin vX.Y.Z
```

The tag reaches GitHub via the mirror and triggers
`.github/workflows/release.yml`, which cross-compiles x86_64, aarch64 and
armv7 Linux binaries (aarch64/armv7 through `cross` using the images in
`docker/`) and attaches them to a GitHub Release. The tagged commit
should be the tip of `main`.

Don't force-move a published tag; cut the next version instead —
`install.sh` resolves the latest release, and re-pointing a tag makes
already-installed boxes disagree about what they're running. Keep the tag
in step with `version` in `Cargo.toml`.

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
  WS clients and the uploader subscribe to.
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
  measurement readouts, upload-status line.
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

## Deployed layout

`install.sh` puts the binary at `/usr/local/bin/propmonitor` (plus
`sdr_diag`), config at `/etc/propmonitor/config.yaml` owned by a system
user with mode 2770, and a hardened `propmonitor.service` systemd unit
whose only `ReadWritePaths` is the config dir. Anything the daemon needs
to write outside `/etc/propmonitor` has to be added to the unit.

## Dependencies

Keep TLS on rustls end to end — `reqwest` with `default-features = false`,
`tokio-tungstenite` with `rustls-tls-webpki-roots`. No openssl anywhere;
it would break the `cross` builds for aarch64/armv7. `tokio-tungstenite`
is pinned to 0.29 so the sync client shares one tungstenite copy with
axum's `ws` feature.

## Persistent vs ephemeral

- **Persistent:** `config.yaml`. Written atomically by `PUT /api/config`
  (tmp + fsync + rename).
- **Ephemeral:** measurement history, retry queue, raw dBFS, device
  info. All in-memory. Restarts reset them.

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
  parse/validate → rebind if needed → write_atomic → swap → worker
  restart). `PUT /api/config` and `sync.rs` both call it; don't grow a
  second one. `put_config` bumps `sync_notify` afterwards so the edit is
  reported upstream — a config that arrived *from* microwaveprop must not
  bump it, or the node and website mint versions at each other forever.
  Both writers (and `persist_config_version`) hold `AppState::config_write`
  for the whole read-modify-write; a new config writer must take it too.
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
