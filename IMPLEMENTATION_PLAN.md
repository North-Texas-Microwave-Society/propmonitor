# Q65-60C decoder — implementation status

Paired with `docs/plans/2026-04-16-q65-decoder-design.md`.

## Stage 1: workspace skeleton — **Complete**
- Cargo workspace with `q65/` as a sibling crate path-dep by `propmonitor`.
- Modules stubbed: `params`, `gf64`, `rs`, `kv`, `sync`, `demod`, `encode`, `decode`, `message`, `callhash`.
- `cargo build`, `cargo test`, `cargo clippy` all green for the workspace.

## Stage 2: GF(64) + RS(63,13) hard decoder — **Complete**
- GF(2^6) arithmetic with primitive polynomial `x^6 + x + 1`, log/exp tables, static-init.
- Systematic RS(63,13) encoder; Berlekamp-Massey + Chien search + Forney hard decoder.
- Tests: exhaustive GF ops, encode→zero-syndrome, decode 0..=25 errors, uncorrectable >25.
- **Verified:** all 34 unit tests pass.
- **Not verified:** constants match WSJT-X's RS convention (primitive poly, first root). See Stage 6.

## Stage 3: soft-decision decoder — **Partial (OSD placeholder)**
- `kv::osd_decode` is a small ordered-statistics decoder: pick the hard word, try 2^max_flips flips of the least-reliable positions.
- Koetter-Vardy interpolation + Roth-Ruckenstein factoring is **not implemented** (still a stubbed `kv::list_decode`).
- **Sensitivity impact:** OSD with max_flips=6 is probably 3-6 dB worse than KV at Q65-60C's operating range. The thing will decode strong signals but miss weak ones WSJT-X catches.

## Stage 4: synthesize→decode round-trip — **Plumbing Complete, Sensitivity TBD**
- `q65/tests/roundtrip.rs` synthesizes an 85-tone Q65-60C frame at 12 kHz, feeds it through the decoder, and asserts no panic.
- **Does not currently assert message content recovery** — the sync pattern and tone layout in `sync.rs` are provisional; until they match WSJT-X or are empirically tuned to the decoder's own encoder, the decoder won't lock onto even its own clean output reliably.

## Stage 5: 77-bit message pack/unpack — **Complete for i3=1 and i3=0 n3=0**
- i3=1 standard message: pack/unpack CALL1 CALL2 {GRID|REPORT|RRR|RR73|73}.
- i3=0 n3=0 free-text (13 chars from the 42-char alphabet).
- i3=4 hashed callsigns: placeholder string rendering; 22-bit hash (FNV-1a) in `callhash` for table storage.
- **Not verified:** the 28-bit callsign packing matches WSJT-X exactly (the alphabet ordering and reserved-range offsets are best-effort; CQ + 3-digit and CQ + tag branches are implemented but not cross-checked).
- **Not implemented:** CRC-14 check on the 77-bit payload. We accept any codeword that passes RS syndromes. False-positive rate will be noticeably higher than WSJT-X's.

## Stage 6: reference-vector cross-check against WSJT-X — **Encoder Complete**

Worked through the first q65sim capture (Q65-60C, "K1ABC W9XYZ EN37", SNR +30 dB) on
2026-04-16, then consulted the WSJT-X source on 2026-04-17 to port the QRA code constants.

**Encoder: byte-for-byte match with q65sim, verified by `tests/vectors_q65_60c.rs`.**
- `qra::encode(info13)` produces the exact 63-symbol codeword q65sim prints.
- Full `encode_message` -> 85 channel symbols matches q65sim's "Channel symbols" dump.

**Pinned down empirically:**

- **Sync positions** (22 of them): `[0, 8, 11, 12, 14, 21, 22, 25, 26, 32, 34, 37, 45, 49, 54, 59, 61, 65, 68, 73, 75, 84]` — committed to `q65/src/sync.rs`.
- **Sync-tone layout**: all 22 sync symbols transmit **tone 0** (a single reference tone, not a Costas-like permutation).
- **Data-to-tone mapping**: `transmitted_tone = codeword_value + 1`. So tone 0 is reserved for sync and data tones occupy 1..=64 (65 tones total).
- **Tone direction**: tone indices run **downward** in audio frequency. `audio_freq(tone_index) = f_tone0 - tone_index * tone_spacing_hz`. (q65sim's `freq 1500` is the *center* of the occupied band; tone 0 lands at `freq + 32 * tone_spacing`.)
- **77-bit payload layout**: verified — the 13 six-bit info symbols in `q65sim`'s output match my `message::payload_to_rs_symbols` output bit-for-bit.

**Code turned out to be a custom QRA-IRA code, not Reed-Solomon.** The qra15_65_64_irr_e23 code used by Q65 is a (N=65, K=15) irregular-repeat-accumulate LDPC-like code over GF(64), with a 2-symbol CRC-12 that is appended before encoding and punctured after. That's why 27,216 RS variants missed — it's not an RS code at all. See `q65/src/qra.rs` for the implementation; `q65/src/rs.rs` is now vestigial and unused by the encode path.

**Licence note:** the QRA tables (acc_input_idx, acc_input_wlog) and the CRC-12 polynomial were transcribed from WSJT-X (GPL-3.0). The q65 crate is accordingly licensed `GPL-3.0-or-later`. The Rust implementation is our own.

## Decoder complete end-to-end (2026-04-17)

The `q65` crate now decodes the q65sim reference WAV
(`tests/fixtures/q65_60C_K1ABC_W9XYZ_EN37.wav`) autonomously — no hardcoded
DT/freq, no pre-known info symbols. Run via
`cargo run --release -p q65 --example decode_wav_bp -- <wav>`:

```
searching for sync...
  sync: dt=0.810 s, tone0_freq=1500.00 Hz, score=6139.4
demod argmax correct: 53/63, top-5 hit: 62/63
DECODED in 4 iters: [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18]
-> matches K1ABC W9XYZ EN37 ground-truth info symbols.
```

### What got built

**`q65/src/bp.rs` — belief-propagation decoder (~350 lines).** Ports
`qra_extrinsic` + `qra_mapdecode` from WSJT-X `qracodes.c` plus the
supporting Walsh-Hadamard transform (`npfwht.c`) and probability-distribution
utilities (`pdmath.c`). Top-level `q65_decode()` handles depuncturing,
running BP, and CRC-12 verification. Unit-tested on clean synthetic
intrinsic (converges, 4 iters), and on intrinsic with 5 forced symbol
errors (converges + CRC passes).

**`q65/src/qra_tables.rs` — decoder tables transcribed from WSJT-X.**
MSGW[216], VDEG[65], CDEG[116], V2CMIDX[325], C2VMIDX[348], PMAT[4032].
PMAT cross-checked independently via `gf64::mul`.

**Tone-direction fix.** Tones run UPWARD in audio frequency from tone 0
(the LOWEST tone), matching q65sim.f90 line 176:
`freq = f0 + itone * baud * mode65`. Sync tone is at tone 0 = audio
frequency `f0`. Data tones span `f0 + spacing..f0 + 64*spacing`.

### Remaining work (in descending priority)

1. **Demod improvements.** Current demod recovers 53/63 symbols on the
   clean q65sim WAV. BP+CRC compensates easily at high SNR (decodes in 4
   iters), but for weak-signal performance we should port WSJT-X's
   `q65_intrinsics_fastfading` — a proper energy-to-probability
   conversion that accounts for the fading channel model (Gaussian or
   Lorentzian) and the B90 spread-bandwidth parameter. Lives in `q65.c`
   lines 287-500 in the WSJT-X source.

2. **Message packer.** `pack_standard` / `unpack_c28` / `pack_g15` in
   `message.rs` do NOT match WSJT-X's `pack77.f90`. 3455 lines of Fortran
   covering all message variants (standard, CQ, DXpedition, contest,
   telemetry, hashed callsigns). For `propmonitor` to *render* decodes
   correctly this needs to be ported. Decoder → info-symbols works today;
   info-symbols → human-readable string is the gap.

3. **Live-RF integration.** `propmonitor/src/worker.rs` already has the
   capture + decimate + period-align scaffolding for Q65 mode. Needs to
   invoke the new `q65::bp::q65_decode` with a 63-row intrinsic built from
   sync + demod. Coupling this end-to-end on live SDR audio is the final
   step for "see what Q65 can decode from the air."

4. **More submodes.** Adding Q65-30, Q65-60A, etc. is mostly changing
   `tsym_s`, `tone_spacing_hz`, and the `mode65` factor; the QRA code,
   sync positions, and decoder are shared across submodes.

## Stage 7: propmonitor integration — **Plumbed, Untested on Live RF**
- `Mode::Q65` added to the mode enum. `q65:` config block.
- Worker branches on `mode == Q65`: opens SDR, mixes to audio center, decimates to ~12 kHz, buffers 64 s aligned to the next UTC minute, feeds `q65::decode` per period, emits `WorkerEvent::Q65Decodes`.
- UI switches to a scrolling decode-list view (TIME / SNR / DT / HZ / MESSAGE).
- **Not tested on live RF.** Expected behavior on 50.211 MHz Q65-60C activity: decodes will be sparse or zero until Stage 6 is done. The TUI itself should render and the capture pipeline should not crash.
- **Known limitations in the capture path:**
  - Decimator is a naive boxcar average, not a proper anti-aliasing polyphase filter. Aliases of out-of-band signals will fold into the 12 kHz audio. Fine for demo, bad for sensitivity.
  - No soft-live-reload: a config change requires restart.
  - Sample-rate/decim ratio can miss `audio_sr = 12_000.0` exactly; the decoder uses whatever `audio_sr` it's handed, so this is not a bug but the nominal Q65 timing will be slightly off (fractional PPM).

## How to try it

```yaml
# config.yaml
mode: q65
frequency: 50211000
sample_rate: 2000000
gain: 40

q65:
  submode: "60C"
  audio_center_hz: 1500
  audio_search_hz: 200
  max_decodes_per_period: 5
```

```sh
cargo run --release
```

The TUI will show a `Q65-60C decodes` list. Decodes (if any) will scroll into it on UTC minute boundaries. Until Stage 6 is done, expect zero decodes on weak signals and possibly false-positives on strong ones.

## What to do next

1. **Capture a reference WAV** from WSJT-X while it's decoding Q65-60C on 6 m. Save the decode log too.
2. **Reconcile constants** against WSJT-X source (`q65_decode.f90`, `q65_enc.f90`). Primary suspects: sync positions/tones, RS generator, CRC poly, callsign-hash function.
3. **Implement full Koetter-Vardy** (replace `kv::osd_decode` call site in `decode::try_decode_from_reliability` with `kv::list_decode`). This is the multi-week item.
4. **Improve the decimator** (polyphase FIR, 160:1 or fractional). Pair with retuning the SDR to 1.92 MS/s for integer decimation once confirmed RSP1A-supported.
