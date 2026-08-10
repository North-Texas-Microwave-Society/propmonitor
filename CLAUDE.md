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
cargo clippy
cargo fmt
```

Runtime dependency: a working `SoapySDR` install plus the driver module
for the dongle being used.

## Releasing

When pushing a release, bump the version tag:

```bash
cargo test && cargo build --release   # verify everything is green
git tag -d v0.0.N  2>/dev/null; git tag v0.0.N && git push origin v0.0.N --force
```

The tag triggers the GitHub release workflow (`.github/workflows/release.yml`)
which builds binaries for x86_64, aarch64, and armv7 and attaches them to
a GitHub Release. The tagged commit should always be the tip of `main`
and should pass `cargo test`.

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
  `gmtime_r` so it works on Windows MSVC.
- **`src/web/`** — Embedded HTML/CSS/JS (single page, no build step).
  Canvas waterfall, settings form bound to `/api/config`, live dBFS +
  measurement readouts, upload-status line.
- **`src/bin/sdr_diag.rs`** — Standalone diagnostic CLI. Useful when
  the waterfall in the web UI looks wrong and you want to rule out the
  DSP/server path.

`Measurement` reports **both** `signal_peak_dbfs` and `signal_avg_dbfs`
on purpose: peak is for keyed/bursty signals (CW beacons), avg is for
continuous carriers (FM/AM). Tests in `measure.rs` pin that contract.

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
- The HTTP `bind` field can be `0.0.0.0:port` for LAN access, but the
  startup message prints the loopback URL (`http://127.0.0.1:port`) that
  actually works in a browser — `main.rs` does this substitution.
- The microwaveprop ingest endpoint **doesn't yet exist on the server
  side** — see `api.md` §4 for the wire contract this client targets.
- Windows builds aren't in CI. See `windows_build.md`.
