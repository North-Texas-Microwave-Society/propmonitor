# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`propmonitor` is a Rust CLI that captures IQ samples from an SDRplay RSP1A over SoapySDR, runs a streaming FFT-based spectrum analyzer on them, and once per minute prints noise floor, in-band signal level (peak and average), and SNR for a configured frequency and radio mode. Only validated on macOS with an RSP1A.

## Commands

```bash
cargo build                       # debug build
cargo build --release             # release build (use this for real captures)
cargo run                         # runs with ./config.yaml
cargo run -- path/to/config.yaml  # runs with an explicit config
cargo test                        # runs all unit tests
cargo test tone_in_passband_has_high_snr   # run a single test by name
cargo clippy
cargo fmt
```

Runtime dependencies (not installed by cargo): `SoapySDR` and the `SoapySDRPlay` driver must be present for the binary to find an SDRplay device. If `soapysdr::enumerate("driver=sdrplay")` returns empty, that's the cause.

## Architecture

Three files, one binary. The interesting split is between *streaming* and *windowed* processing:

- **`src/main.rs`** — owns the SDR device and the outer one-minute capture loop. Does **not** buffer a full minute of IQ. Instead it reads from the SoapySDR stream into a small `scratch` buffer, appends into `frame_buf` until it holds exactly `FFT_SIZE` (16384) samples, hands that one frame to the analyzer, and clears it. At the end of each 60-second window it calls `finalize()` and prints the `Measurement`.

- **`src/measure.rs`** — `SpectrumAnalyzer` is a streaming accumulator, not a batch processor. The lifecycle is:
  1. `start_window(mode)` — zeros the PSD sum, resets peak/avg signal power and frame count, and pre-computes the in-band bin range (`sig_lo`..`sig_hi`) for the mode's passband.
  2. `add_frame(&[Complex32])` — applies a Hann window, does one FFT, does `fftshift` on the fly while accumulating into `psd_sum`, and updates the per-frame in-band peak and running sum.
  3. `finalize()` — time-averages `psd_sum`, estimates the noise floor as the **median of out-of-passband bins** (with a guard region of `half_width_bins` on each side of the passband to keep spectral leakage from a strong in-band signal out of the noise estimate), and returns a `Measurement`.

  `Measurement` reports **both** `signal_peak_dbfs` and `signal_avg_dbfs` on purpose: peak is for keyed/bursty signals (CW, SSB voice) where averaging across silent frames understates the carrier; avg is for continuous carriers (FM/AM broadcast) and captures duty cycle. The tests in this file pin that contract — `keyed_cw_signal_is_gated_to_active_frames` specifically asserts that a half-keyed CW signal has the same peak as always-on but ~3 dB lower average. Don't "fix" that by collapsing the two fields.

- **`src/config.rs`** — YAML-backed config plus the `Mode` enum. `Mode::passband()` is the single source of truth for per-mode `(offset_hz, bandwidth_hz)`; changing those numbers changes both the signal-bin range used during accumulation and the guard region used during noise estimation.

## Things to know before editing

- `FFT_SIZE` is a `pub const` in `measure.rs` and is baked into buffer sizes in `main.rs`. If you change it, the frame-buffer fill loop in `main.rs` still works, but expect the noise/signal numbers to shift slightly (bin width changes, so passband rounding changes).
- `start_window` / `add_frame` / `finalize` is an ordered protocol. `add_frame` has a `debug_assert!` that `start_window` was called. `finalize` clears `accum_mode` so a window can't be silently reused.
- The `analyze()` batch helper in `measure.rs` is `#[cfg(test)]` only — it exists so tests can feed a whole buffer at once. Production code goes through the streaming path in `main.rs`.
- Gain handling: if `config.gain` is `Some`, AGC is explicitly disabled and the gain is set; if `None`, AGC is enabled (best-effort — the `.ok()` swallows unsupported-driver errors intentionally).
