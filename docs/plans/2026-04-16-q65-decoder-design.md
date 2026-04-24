# Q65-60C decoder: design

**Date:** 2026-04-16
**Status:** Design — not yet implemented

## 1. Purpose

Add Q65-60C decoding to `propmonitor` so the user can see what weak-signal traffic the RSP1A pulls in on 6 m (50.211 MHz). The decoder itself lives in a separate Rust crate (`q65`) inside a Cargo workspace so it can be extracted to its own project later without disturbing `propmonitor`.

## 2. Goals / non-goals

### Goals
- Decode Q65-60C frames captured from IQ, with sensitivity approximately matching WSJT-X's `jt9 -q` on the same audio (within ~1–2 dB).
- Soft-decision Reed-Solomon decoder — this is the only way to reach that sensitivity.
- Clean decoder API: input complex audio at 12 kHz, output a list of `Decode { snr_db, dt_s, freq_hz, message }`.
- Decoder crate has zero dependency on SDRs, TUIs, filesystem, or time. Pure DSP + codec.

### Non-goals (for MVP)
- Submodes other than 60C. The crate is internally parameterized on a `Q65Params` struct so adding 30A/60A/etc. is adding test vectors and a few constants, not a rewrite.
- Message types beyond `i3=1` (standard QSO), `i3=0 n3=0` (free-text), `i3=4` (nonstandard hashed callsigns).
- Running alongside analog modes (Q65 replaces the analog per-minute view).
- Automatic NTP sync. We assume the host clock is within ~±2 s of UTC.
- Reply/TX — decode-only.
- Multi-signal decoding at different audio frequencies per period (subsequent tickets; MVP decodes the single strongest candidate).

## 3. Q65-60C signal facts

| Parameter | Q65-60C |
|---|---|
| T/R period | 60 s |
| Total channel symbols | 85 |
| Data symbols | 63 (carries an RS(63,13) codeword over GF(2^6)) |
| Sync symbols | 22, interleaved at fixed positions (see **§8 open questions**) |
| Symbol rate (baud) | ~1.6553 Hz |
| Symbol duration Tsym | ~0.604 s |
| Tone spacing (C-variant = 4× baud) | ~6.6212 Hz |
| Tones per symbol | 65 (64 data + 1 sync tone) |
| Total occupied BW | ~430 Hz |
| Payload | 77 bits (same 77-bit format FT8 uses) |

All DSP-layer constants will live in a single `Q65Params` struct (§4.2). The exact sync symbol positions, the sync tone index, and the RS generator polynomial over GF(64) must be cross-referenced against WSJT-X's reference code before implementation (see §8).

## 4. Decoder crate (`q65`)

### 4.1 Workspace layout

```
propmonitor/                      (Cargo workspace root)
├── Cargo.toml                    (workspace manifest)
├── Cargo.lock
├── src/                          (unchanged propmonitor binary sources)
├── q65/
│   ├── Cargo.toml                (crate manifest — publishable standalone)
│   ├── README.md
│   ├── src/
│   │   ├── lib.rs                (public API, re-exports)
│   │   ├── params.rs             (Q65Params, Submode, Variant)
│   │   ├── gf64.rs               (GF(2^6) arithmetic: add/mul/inv/log/exp)
│   │   ├── rs.rs                 (RS(63,13) over GF(64): encoder + hard decoder)
│   │   ├── kv.rs                 (Koetter-Vardy soft-decision list decoder)
│   │   ├── sync.rs               (sync pattern definition + sync search)
│   │   ├── demod.rs              (per-symbol 64-bin FFT → soft reliability matrix)
│   │   ├── message.rs            (77-bit payload pack/unpack, i3/n3 dispatch)
│   │   ├── callhash.rs           (22-bit hashed-callsign table for i3=4)
│   │   ├── encode.rs             (symbolic Q65 transmitter: message → 85 tones)
│   │   └── decode.rs             (top-level decode: audio → Vec<Decode>)
│   └── tests/
│       ├── roundtrip.rs          (encode → synthesize → decode at various SNRs)
│       ├── vectors_q65_60c.rs    (reference vectors from WSJT-X)
│       └── fixtures/             (wav files, binary symbol streams)
└── docs/plans/                   (this doc)
```

The root `Cargo.toml` becomes a workspace manifest. The existing `[package]` section moves under the new `[workspace]` section. `propmonitor` depends on `q65` via a path dep.

### 4.2 Public API (sketch)

```rust
// q65/src/lib.rs
pub use decode::{decode, Decode, DecodeError};
pub use encode::encode_message;
pub use params::{Q65Params, Submode, Variant, Q65_60C};

// q65/src/params.rs
#[derive(Debug, Clone, Copy)]
pub struct Q65Params {
    pub submode: Submode,        // TR15, TR30, TR60, TR120, TR300
    pub variant: Variant,        // A, B, C, D, E (tone-spacing multiplier 1/2/4/8/16)
    pub tsym_s: f64,
    pub baud_hz: f64,
    pub tone_spacing_hz: f64,    // baud * 2^variant_exp
    pub total_bw_hz: f64,
    pub num_symbols: usize,      // 85
    pub num_data_symbols: usize, // 63
    pub num_sync_symbols: usize, // 22
}
pub const Q65_60C: Q65Params = ...;

// q65/src/decode.rs
pub struct Decode {
    pub snr_db: f32,
    pub dt_s: f32,
    pub freq_hz: f32,   // audio frequency offset of signal center
    pub message: String,
    pub raw_bits: [u8; 77],
}

/// Decode a single Q65 T/R period of complex baseband audio at 12 kHz.
/// `audio` must be long enough to cover the full period plus DT search
/// margin (~64 s for Q65-60). The decoder internally searches over DT
/// (time offset) and frequency offset.
pub fn decode(
    params: &Q65Params,
    audio: &[num_complex::Complex32],
    audio_sr_hz: f64,
    search: &DecodeSearch,   // { dt_range_s, freq_range_hz, max_decodes }
) -> Result<Vec<Decode>, DecodeError>;

// q65/src/encode.rs
/// Encode a 77-bit payload into 85 channel-symbol tone indices.
pub fn encode_message(
    params: &Q65Params,
    payload: &[u8; 77],
) -> [u8; 85];   // each byte is 0..64 (sync tone index or data tone 0..63)
```

### 4.3 Decode pipeline

1. **Coarse sync search.** Slide a matched filter for the known 22-symbol sync pattern across the audio in (time offset, frequency offset) space. Time step ~Tsym/4, frequency step ~tone_spacing/2. Keep the top N peaks.
2. **Fine sync refinement.** For each candidate, fit a parabola around the peak in both axes to get sub-bin DT/freq. This gives ±Tsym/20 and ±tone_spacing/20 accuracy, which is sufficient.
3. **Symbol demodulation.** With known (DT, freq), compute the 64-bin FFT over each data symbol's window. Yields a `[63 × 64]` reliability matrix (log-likelihoods).
4. **Soft Reed-Solomon decode.** Koetter-Vardy algebraic soft-decision decoder with a small list size (WSJT-X uses L ≈ 5–16). Output: up to L candidate 13-symbol GF(64) information sequences.
5. **CRC check.** For each candidate, unpack to 77 bits + CRC, verify CRC (polynomial TBD — verify against WSJT-X; FT8 uses a 14-bit CRC, Q65 may differ). Keep the first that passes.
6. **Message unpack.** Dispatch on `i3/n3` to render the 77-bit payload as a human-readable string.
7. **Return.** Populate `Decode { snr_db, dt_s, freq_hz, message, raw_bits }`. SNR is computed from tone-energy contrast across the demod; DT is the fine-sync time offset; freq_hz is the signal center.

### 4.4 Koetter-Vardy notes

KV is the technical centerpiece and the single biggest implementation risk. The algorithm:

1. Build a multiplicity matrix M from the reliability matrix (costed by a total-multiplicity budget parameter).
2. Bivariate polynomial interpolation over GF(64): find Q(x,y) of minimal (1, k-1)-weighted degree passing through each (αⁱ, β_j) with multiplicity M[i,j].
3. Factor Q(x,y) to enumerate y-roots that are polynomials of degree < k. Each root is a candidate message polynomial.

Step 3 is typically done with the Roth-Ruckenstein algorithm. Total code for gf64 + rs + kv + rr is probably 1500–2500 lines of careful Rust.

Reference: Koetter & Vardy, "Algebraic soft-decision decoding of Reed-Solomon codes," IEEE Trans. Inf. Theory, 2003. And Franke/Somerville/Taylor, "The FT4 and FT8 Communication Protocols" (QEX, 2020) — contains pointers to the Q65 decoder.

### 4.5 Message unpack scope

Three dispatch paths off the leading `i3` (3-bit) field:

- **i3 = 1 (standard QSO):** `CALL1 CALL2 {GRID4 | R±dB | RRR | RR73 | 73}`. 28-bit compressed callsign × 2, 15-bit grid/report, 1-bit flag. Shared with FT8.
- **i3 = 0, n3 = 0 (free-text):** up to 13 characters from a 42-character alphabet, 71 bits.
- **i3 = 4 (nonstandard callsigns):** two 22-bit hashed callsign fields plus a standard callsign. Requires a rolling hash table of recently-heard callsigns so that `<…>` hashed entries can be rendered as the real callsign if we've seen it. The hash table lives inside `callhash.rs` with capacity ~128 entries, LRU-evicted.

All three use the same 28-bit callsign compression (alphabet-ordered enumeration of valid call shapes) — that compressor lives once in `message.rs`.

## 5. Propmonitor integration

### 5.1 Config

Extend the existing `Mode` enum with a `Q65` variant:

```rust
pub enum Mode { Usb, Lsb, Am, Nfm, Wfm, Cw, Q65 }
```

Q65 uses a different chunk of config than the analog modes, so add a nested optional block:

```yaml
mode: q65
frequency: 50211000       # Hz — dial frequency; Q65 signals appear as audio offsets
gain: 40

q65:
  submode: "60C"          # MVP accepts only "60C"; parser rejects others
  audio_center_hz: 1500   # where in the 12 kHz audio band to search (WSJT-X default)
  audio_search_hz: 200    # ± search range around audio_center_hz
  max_decodes_per_period: 5
```

`Config::load` validates that `q65:` is present iff `mode == Q65`.

### 5.2 IQ → audio DSP

In `worker.rs`, when `mode == Q65`, replace the spectrum-analyzer branch with:

1. **Tune-and-decimate filter.** From IQ at 2 MS/s, produce complex audio at 12 kHz centered on the configured `audio_center_hz` offset from the dial frequency. Implementation: CIC or halfband decimation chain from 2 MS/s → 12 kHz (factor 166.67 — use a fractional resampler, or retune the SDR to a rate that divides evenly to 12 kHz, e.g. 1.92 MS/s → ÷160 → 12 kHz).
2. **UTC-aligned period buffer.** Buffer 64 s of 12 kHz complex audio (4 s of margin on a 60 s period for DT search and symbol tails), gated so that buffers are aligned to UTC minute boundaries. Use `chrono::Utc::now()` to compute `(next_minute_boundary - now)` at startup and skip the leading partial minute.
3. **Period trigger.** Once every 60 s, when the buffer is full, ship the last 64 s of audio to a decode worker (another thread) so the capture loop isn't blocked on decode latency.

### 5.3 UI

`App` gets a new enum variant `AppMode::Q65 { decodes: VecDeque<UiDecode>, current_period_start: Option<Instant> }`. When `mode == Q65`:

- Top bar: dial frequency, submode, current UTC time, next-period countdown.
- Main area: scrolling decode list, newest on top, capped at ~200 entries. Columns: `HH:MM:SS`, `SNR`, `DT`, `Hz`, `message`.
- Bottom status: last-period decode count, "decoding…" indicator while the decoder runs.

No per-minute noise-floor line — that's the analog view.

### 5.4 Threading

Current: two threads (main UI + worker). Add a third for decode:
- **Worker thread**: captures IQ, does DSP, fills the 64-s audio buffer, hands it to the decoder thread via `mpsc::Sender<Vec<Complex32>>`.
- **Decoder thread**: owns a `Q65Params`, runs `q65::decode()`, sends `WorkerEvent::Q65Decodes(Vec<Decode>)` to the UI.
- **UI thread**: unchanged shape, new event variants.

Decode latency for Q65-60 on modern hardware is well under 60 s, so one decoder thread keeps up with the capture cadence.

## 6. Testing strategy

### 6.1 Unit tests

- `gf64`: exhaustive — every pair of elements for add/mul, every nonzero for inv. GF(64) has 64 elements; 64² = 4096 pairs is trivial.
- `rs`: encode → hard-decode round-trip for all single-symbol and double-symbol errors (≤ floor((63-13)/2)=25 correctable). Spot check up to 25 errors.
- `kv`: encode → add Gaussian symbol noise at known SNR → soft-decode; assert successful decode rate ≥ threshold curve taken from published Q65 performance data.
- `sync`: synthesize a known sync pattern at a known DT/freq in white noise; assert coarse sync finds it within 1 bin.
- `message`: round-trip every supported i3/n3 path with a canned message table (e.g. `"K1ABC W2DEF FN42" → 77 bits → "K1ABC W2DEF FN42"`).
- `callhash`: seed table, query, evict, re-seed. Deterministic 22-bit hash output for known inputs.

### 6.2 Round-trip integration test

`q65::encode_message(&Q65_60C, payload) → synthesize 85-tone audio at 12 kHz → add AWGN at a chosen SNR → q65::decode(...)`.

Sweep SNR from -30 dB to -10 dB in 2 dB steps; at each step, 100 trials. Assert the decode-success curve bounds the published Q65-60C sensitivity (-24 dB reference).

### 6.3 Reference vectors

Pull ~5 short WAV files recorded by WSJT-X on 6 m Q65-60C (or synthesize with `jt9`/`wsjt-x`'s encoder as a one-time fixture step). Check each into `q65/tests/fixtures/`. Test that `q65::decode` produces the same messages WSJT-X reports for these captures. **This is the gold-standard cross-check.**

### 6.4 End-to-end propmonitor test

No automated test — this is validated by running on live 6 m signals and comparing the decode list against a concurrent WSJT-X session on the same antenna (split via SDRuno or a T).

## 7. Dependencies

New additions to the `q65` crate:
- `num-complex` (already present in propmonitor)
- `rustfft` (already present)
- No new external deps if we write KV ourselves. Avoid pulling in generic Reed-Solomon crates — they're all byte-oriented and hard-decision, which is the wrong abstraction.

Propmonitor gains a path dep on `q65` and nothing else new. `chrono` is already a dep.

## 8. Risks & open questions

**Algorithmic (high risk):**

1. Exact Q65-60C sync symbol positions (22 positions out of 85). Must be verified against WSJT-X `q65_sync.f90` or the Franke/Somerville/Taylor paper.
2. Exact RS(63,13) generator polynomial over GF(64). WSJT-X's choice may differ from the "textbook" primitive polynomial.
3. CRC polynomial and bit width on the 77-bit payload. FT8 uses CRC-14; Q65 may be the same or different.
4. Koetter-Vardy parameter choices (total multiplicity budget, list size). WSJT-X tunes these empirically; we may need to replicate their values rather than rederive.
5. Soft-metric formula from tone-FFT magnitudes. Joe Taylor's code uses a specific log-likelihood formulation; copying the *concept* is fine (not copyrightable) but the specific constants matter for sensitivity.

**Mitigation:** before starting KV implementation, read the QEX article and the WSJT-X source carefully (for algorithm understanding only — implementation is clean-room, MIT/Apache licensed). Write a short "q65_internals.md" as a companion to this doc recording the verified constants.

**DSP (medium risk):**

6. 2 MS/s → 12 kHz resampling. Non-integer ratio. Easiest fix: retune the SDR to 1.92 MS/s so decimation is exactly ÷160. Need to confirm RSP1A supports 1.92 MS/s.
7. SDRplay frontend noise floor may be high enough that the last few dB of Q65's sensitivity are wasted. Empirical.

**Integration (low risk):**

8. `chrono` drift vs. system clock: we'll call `Utc::now()` each period; if NTP is off we just miss decodes, no crash.
9. TUI redraws during long decode: decoder runs on a separate thread, so this isn't an issue.

## 9. Implementation plan

Stages sized to ship independently; each ends with a working `cargo test` green.

### Stage 1: workspace + empty `q65` crate
**Goal:** Cargo workspace with `q65` as a path-dep of `propmonitor`. No behavior changes.
**Success criteria:** `cargo build`, `cargo test`, `cargo clippy` all green. Running `propmonitor` still shows the existing analog view.
**Tests:** existing tests still pass.

### Stage 2: GF(64) + RS(63,13) hard encoder/decoder
**Goal:** Working algebraic codec, hard-decision only.
**Success criteria:** encode→corrupt ≤25 symbols→hard-decode round-trips exactly. No panic on uncorrectable patterns.
**Tests:** `gf64` exhaustive, `rs` round-trip up to 25 errors, `rs` rejects 26+ errors.

### Stage 3: Koetter-Vardy soft decoder
**Goal:** Soft-decision list decoder producing the correct codeword at Q65 operating SNRs.
**Success criteria:** synthetic reliability-matrix test at several SNRs meets published Q65 performance curve within 1 dB.
**Tests:** `kv` with synthetic AWGN-noised symbol metrics.

### Stage 4: sync + demod + encoder (end-to-end synthetic round-trip)
**Goal:** `encode_message` + `decode` round-trip over synthetic 12 kHz audio.
**Success criteria:** 100-trial sweep from -30 to -10 dB SNR matches Q65-60C published curve ±1 dB.
**Tests:** `roundtrip.rs` integration test.

### Stage 5: 77-bit message pack/unpack (i3=1, i3=0, i3=4)
**Goal:** Real message rendering.
**Success criteria:** canned table of 20 representative messages round-trips exactly.
**Tests:** `message.rs` unit tests.

### Stage 6: Reference-vector cross-check against WSJT-X
**Goal:** decode real captured Q65-60C audio and match WSJT-X's output.
**Success criteria:** ≥80% of decodes WSJT-X finds, we also find. No false-positive decodes (wrong message).
**Tests:** `vectors_q65_60c.rs`.

### Stage 7: propmonitor integration
**Goal:** `mode: q65` works end-to-end on live RF.
**Success criteria:** run on 50.211 MHz, see decodes appear in the TUI. Manual comparison with a concurrent WSJT-X session on the same signal shows comparable decode rates.
**Tests:** manual. No automated test for the live path.

## 10. Extraction path (future)

When `q65` is solid, extract it:
1. `git subtree split --prefix=q65 -b q65-split` → push to `github.com/<user>/q65-rs`.
2. Publish to crates.io as `q65` (check name availability; fall back to `q65-decoder` if taken).
3. In `propmonitor`, replace the path dep with a version dep.
4. Ongoing improvements (new submodes, variants) happen in the standalone repo and flow back via version bumps.

The design constraint that `q65` knows nothing about SoapySDR, ratatui, or `propmonitor::Config` is what makes this extraction cheap.
