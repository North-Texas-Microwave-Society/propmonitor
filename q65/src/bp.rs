//! Belief-propagation (sum-product) decoder for the qra15_65_64 IRA code.
//!
//! Ports `qra_extrinsic` + `qra_mapdecode` from WSJT-X `lib/qra/q65/qracodes.c`
//! (GPL-3.0, Nico Palermo IV3NWV) into Rust. Also ports the supporting
//! Walsh-Hadamard transform from `npfwht.c` and the probability-distribution
//! utilities from `pdmath.c`.
//!
//! Code-graph shape (hard-coded for qra15_65_64_irr_e23):
//!   M = 64 (alphabet cardinality), m = 6 (bits per symbol)
//!   V = 65 (variable nodes; one per codeword symbol)
//!   C = 116 (check nodes; the first V are intrinsic-carrying degree-1 checks,
//!            the remaining C-V are real parity checks)
//!   NMSG = 216 (total directed edges in the bipartite graph)
//!   MAXVDEG = 5 (max outgoing edges per variable)
//!   MAXCDEG = 3 (max edges per check; all real checks have degree 2 or 3)
//!
//! Probabilities are stored as flat `Vec<f32>` with `M` floats per row
//! (matches WSJT-X's PD_ROWADDR layout). Intrinsics `ix` and extrinsics `ex`
//! are shape [V][M] = 65*64 = 4160 floats. Edge messages are [NMSG][M] =
//! 216*64 = 13824 floats each.

use crate::qra_tables::{C2VMIDX, CDEG, MSGW, PMAT, V2CMIDX, VDEG};

pub const M: usize = 64;
pub const M_LOG2: u32 = 6;
pub const V: usize = 65;
pub const C: usize = 116;
pub const NMSG: usize = 216;
pub const MAXVDEG: usize = 5;
pub const MAXCDEG: usize = 3;

/// Walsh-Hadamard transform of a size-64 probability distribution, in place.
/// Self-inverse up to a scalar factor of 64. The BP loop below relies on
/// proportionality, not exact inversion, so the scalar never matters.
pub fn fwht64(x: &mut [f32; M]) {
    let mut h = 1usize;
    while h < M {
        let mut i = 0usize;
        while i < M {
            for j in i..(i + h) {
                let a = x[j];
                let b = x[j + h];
                x[j] = a + b;
                x[j + h] = a - b;
            }
            i += h * 2;
        }
        h *= 2;
    }
}

/// Uniform distribution over M elements: 1/M per position.
pub const fn uniform_m() -> f32 {
    1.0 / (M as f32)
}

/// Compute dst *= src elementwise over the M-sized distribution.
pub fn pd_imul(dst: &mut [f32; M], src: &[f32; M]) {
    for i in 0..M {
        dst[i] *= src[i];
    }
}

/// Normalize in place. Returns the sum prior to normalization. If the sum is
/// non-positive (underflow or pathological product) the distribution is
/// replaced with uniform, matching WSJT-X's pd_norm64 behavior.
pub fn pd_norm(pd: &mut [f32; M]) -> f32 {
    let sum: f32 = pd.iter().sum();
    if sum <= 0.0 {
        let u = uniform_m();
        for v in pd.iter_mut() {
            *v = u;
        }
        return sum;
    }
    let inv = 1.0 / sum;
    for v in pd.iter_mut() {
        *v *= inv;
    }
    sum
}

/// dst[k] = src[perm[k]]. Argument signature matches WSJT-X's pd_fwdperm.
pub fn pd_fwdperm(dst: &mut [f32; M], src: &[f32; M], perm: &[u8]) {
    for k in 0..M {
        dst[k] = src[perm[k] as usize];
    }
}

/// Argmax of a distribution. Returns the index of the maximum.
pub fn pd_argmax(src: &[f32; M]) -> u8 {
    let mut best = 0u8;
    let mut best_v = f32::NEG_INFINITY;
    for (i, &v) in src.iter().enumerate() {
        if v >= best_v {
            best_v = v;
            best = i as u8;
        }
    }
    best
}

/// Max value of a distribution.
pub fn pd_max(src: &[f32; M]) -> f32 {
    let mut best = 0.0f32;
    for &v in src.iter() {
        if v >= best {
            best = v;
        }
    }
    best
}

/// View helpers for the flat rows layout. `ix` has shape [V][M]; likewise ex
/// and the v2c / c2v message buffers which are [NMSG][M].
#[inline]
fn row_mut(a: &mut [f32], idx: usize) -> &mut [f32; M] {
    (&mut a[idx * M..(idx + 1) * M]).try_into().unwrap()
}
#[inline]
fn row(a: &[f32], idx: usize) -> &[f32; M] {
    (&a[idx * M..(idx + 1) * M]).try_into().unwrap()
}

/// Row-major permutation-index slice for `pd_fwdperm` given a weight-log.
#[inline]
fn pmat_row(wlog: usize) -> &'static [u8] {
    &PMAT[wlog * M..(wlog + 1) * M]
}

/// Iterative belief-propagation decoder.
///
/// `ix` is the intrinsic probability matrix [V][M] (already depunctured —
/// see `q65_decode`). `ex` receives the extrinsic probabilities. The two
/// message buffers `v2c` and `c2v` are scratch space of size [NMSG][M];
/// the caller reuses them across calls to amortize allocation.
///
/// Returns `Some(iters)` if converged (each symbol's extrinsic max is ~1.0),
/// `None` if `max_iters` was exhausted without convergence.
pub fn qra_extrinsic(
    ix: &[f32],
    ex: &mut [f32],
    max_iters: usize,
    v2c: &mut [f32],
    c2v: &mut [f32],
) -> Option<usize> {
    debug_assert_eq!(ix.len(), V * M);
    debug_assert_eq!(ex.len(), V * M);
    debug_assert_eq!(v2c.len(), NMSG * M);
    debug_assert_eq!(c2v.len(), NMSG * M);

    // Message initialization. The first V check-nodes carry intrinsic
    // information (they have cdeg=1, one outgoing msg). WSJT-X initializes
    // the entire c2v array with intrinsics at offset 0, which relies on
    // those V check-message indices being 0..V. See qracodes.c:269 —
    // "pd_init(C2VMSG(0), pix, qra_M*qra_V)" copies V*M floats from ix.
    c2v[..V * M].copy_from_slice(&ix[..V * M]);

    // Initialize v->c messages directed to real code factors (k=1..deg) with
    // intrinsic information. k=0 is the edge toward the intrinsic check and
    // isn't needed in v2c (it never changes).
    for (nv, &ndeg_u8) in VDEG.iter().enumerate() {
        let ndeg = ndeg_u8 as usize;
        let msgbase = nv * MAXVDEG;
        for k in 1..ndeg {
            let imsg = V2CMIDX[msgbase + k];
            if imsg < 0 {
                continue;
            }
            row_mut(v2c, imsg as usize).copy_from_slice(row(ix, nv));
        }
    }

    let mut scratch = [0f32; M];

    for nit in 0..max_iters {
        // --- c -> v step ---------------------------------------------------
        // Skip the first V intrinsic-carrying degree-1 checks (nc < V).
        for (nc, &ndeg_u8) in CDEG.iter().enumerate().skip(V) {
            let ndeg = ndeg_u8 as usize;
            if ndeg < 2 {
                // Real checks must have degree >= 2; bail if the tables are
                // malformed.
                return None;
            }
            let msgbase = nc * MAXCDEG;

            // Forward WHT on every incoming v->c message for this check.
            for k in 0..ndeg {
                let imsg = C2VMIDX[msgbase + k] as usize;
                fwht64(row_mut(v2c, imsg));
            }

            // For each outgoing c->v edge, compute output by multiplying all
            // the other WHT'd distributions, then inverse-WHT, then apply
            // the symbol-weight permutation.
            for k in 0..ndeg {
                // Seed with uniform (product identity).
                scratch.fill(uniform_m());
                for kk in 0..ndeg {
                    if kk == k {
                        continue;
                    }
                    let imsg_in = C2VMIDX[msgbase + kk] as usize;
                    pd_imul(&mut scratch, row(v2c, imsg_in));
                }
                // Bias WHT[0] to avoid underflow side effects (per WSJT-X).
                scratch[0] += 1e-7;
                fwht64(&mut scratch);

                let imsg_out = C2VMIDX[msgbase + k] as usize;
                let wmsg = MSGW[imsg_out];
                if wmsg == 0 {
                    row_mut(c2v, imsg_out).copy_from_slice(&scratch);
                } else {
                    // Apply alpha^(-wmsg) * x permutation via PMAT. That is
                    // pd_bwdperm: dst[perm[k]] = src[k]. Rendered in terms
                    // of pd_fwdperm: dst[k] = src[perm_inv[k]]. PMAT row wmsg
                    // is alpha^(-wmsg)*k, and its inverse permutation (for
                    // bwdperm) is the row for -wmsg mod 63.
                    let inv_wmsg = (63 - (wmsg as usize)) % 63;
                    pd_fwdperm(row_mut(c2v, imsg_out), &scratch, pmat_row(inv_wmsg));
                }
            }
        }

        // --- v -> c step ---------------------------------------------------
        for (nv, &ndeg_u8) in VDEG.iter().enumerate() {
            let ndeg = ndeg_u8 as usize;
            let msgbase = nv * MAXVDEG;

            for k in 0..ndeg {
                scratch.fill(uniform_m());
                for kk in 0..ndeg {
                    if kk == k {
                        continue;
                    }
                    let imsg_in = V2CMIDX[msgbase + kk];
                    if imsg_in < 0 {
                        continue;
                    }
                    pd_imul(&mut scratch, row(c2v, imsg_in as usize));
                }
                pd_norm(&mut scratch);

                let imsg_out = V2CMIDX[msgbase + k];
                if imsg_out < 0 {
                    continue;
                }
                let wmsg = MSGW[imsg_out as usize];
                if wmsg == 0 {
                    row_mut(v2c, imsg_out as usize).copy_from_slice(&scratch);
                } else {
                    // Forward: apply alpha^(+wmsg)*x permutation. That's
                    // pd_fwdperm with PMAT row wmsg directly.
                    pd_fwdperm(
                        row_mut(v2c, imsg_out as usize),
                        &scratch,
                        pmat_row(wmsg as usize),
                    );
                }
            }
        }

        // --- convergence check --------------------------------------------
        // Edge v2c[0..V] are the edges TO the intrinsic checks, which equal
        // the variable's current best estimate (via the WSJT-X code-table
        // construction). Sum of per-variable maxes approaching V means
        // every symbol has locked to a specific value.
        let mut totex = 0.0f32;
        for nv in 0..V {
            totex += pd_max(row(v2c, nv));
        }
        if totex > (V as f32) - 0.01 {
            // Copy v2c[0..V] (the extrinsic for each variable) to ex.
            ex[..V * M].copy_from_slice(&v2c[..V * M]);
            return Some(nit);
        }
    }

    // No convergence — still copy whatever we ended up with so the caller
    // can attempt a MAP decode.
    ex[..V * M].copy_from_slice(&v2c[..V * M]);
    None
}

/// Map decode: for each of the K variables, compute `ex * ix` and take argmax.
/// `out` receives the K decoded symbol values.
pub fn qra_mapdecode(ix: &[f32], ex: &mut [f32], out: &mut [u8], k: usize) {
    debug_assert!(out.len() >= k);
    for (nv, slot) in out.iter_mut().enumerate().take(k) {
        let ex_row = row_mut(ex, nv);
        let ix_row = row(ix, nv);
        pd_imul(ex_row, ix_row);
        *slot = pd_argmax(ex_row);
    }
}

/// Q65 decoder result codes.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeOutcome {
    /// Decoded successfully, CRC check passed. Carries (info13, iters_used).
    Ok([u8; 13], usize),
    /// BP did not converge within max_iters.
    NoConvergence,
    /// BP converged but the CRC-12 check failed (corrupt message).
    CrcMismatch,
}

/// Scratch buffers reused across q65_decode calls. Pre-allocating these
/// once amortizes ~31 KB of float allocations per decode.
pub struct DecodeScratch {
    pub ix: Vec<f32>,  // [V * M]
    pub ex: Vec<f32>,  // [V * M]
    pub v2c: Vec<f32>, // [NMSG * M]
    pub c2v: Vec<f32>, // [NMSG * M]
}

impl Default for DecodeScratch {
    fn default() -> Self {
        Self {
            ix: vec![0.0; V * M],
            ex: vec![0.0; V * M],
            v2c: vec![0.0; NMSG * M],
            c2v: vec![0.0; NMSG * M],
        }
    }
}

/// Top-level Q65 decode given a 63-row symbol intrinsic matrix.
///
/// `intrinsics_63` is a [63 * 64] slice: one probability distribution per
/// transmitted codeword symbol, in transmission order (no sync symbols).
/// Each row should be a non-negative probability distribution (not
/// necessarily normalized — BP will normalize internally).
///
/// The 2 punctured CRC positions are depunctured with uniform priors,
/// BP runs for up to `max_iters` iterations, the resulting 15 symbols are
/// checked against the CRC-12, and the 13 info symbols are returned on
/// success.
pub fn q65_decode(
    intrinsics_63: &[f32],
    max_iters: usize,
    scratch: &mut DecodeScratch,
) -> DecodeOutcome {
    debug_assert_eq!(intrinsics_63.len(), 63 * M);

    // 1. Depuncture to 65 rows: first 13 info, then 2 CRC (uniform), then 50 check.
    let ix = &mut scratch.ix;
    ix.fill(0.0);
    // Copy info rows 0..13.
    ix[..13 * M].copy_from_slice(&intrinsics_63[..13 * M]);
    // Uniform for the 2 CRC rows (positions 13, 14).
    let u = uniform_m();
    for r in 13..15 {
        for k in 0..M {
            ix[r * M + k] = u;
        }
    }
    // Copy the 50 check rows 15..65 from intrinsics_63 rows 13..63.
    ix[15 * M..65 * M].copy_from_slice(&intrinsics_63[13 * M..63 * M]);

    let ex = &mut scratch.ex;
    let v2c = &mut scratch.v2c;
    let c2v = &mut scratch.c2v;

    let iters = match qra_extrinsic(ix, ex, max_iters, v2c, c2v) {
        Some(i) => i,
        None => return DecodeOutcome::NoConvergence,
    };

    let mut decoded = [0u8; 15];
    qra_mapdecode(ix, ex, &mut decoded, 15);

    // CRC-12 check: recompute CRC of decoded[0..13] and compare against decoded[13..15].
    let info13: [u8; 13] = decoded[..13].try_into().unwrap();
    let (c0, c1) = crate::qra::crc12(&info13);
    if c0 != decoded[13] || c1 != decoded[14] {
        return DecodeOutcome::CrcMismatch;
    }

    DecodeOutcome::Ok(info13, iters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fwht64_is_self_inverse_up_to_scale() {
        let mut x = [0f32; M];
        for (i, v) in x.iter_mut().enumerate() {
            *v = (i as f32 * 0.1).sin();
        }
        let original = x;
        fwht64(&mut x);
        fwht64(&mut x);
        // After two applications, every value scales by M.
        for i in 0..M {
            let expected = original[i] * M as f32;
            assert!(
                (x[i] - expected).abs() < 1e-3,
                "i={} got {} expected {}",
                i,
                x[i],
                expected
            );
        }
    }

    #[test]
    fn pd_norm_handles_positive_distribution() {
        let mut x = [0.25f32; M];
        let sum = pd_norm(&mut x);
        // Sum was 64 * 0.25 = 16, after norm each entry = 1/64.
        assert!((sum - 16.0).abs() < 1e-5);
        for &v in x.iter() {
            assert!((v - 1.0 / 64.0).abs() < 1e-6);
        }
    }

    #[test]
    fn pd_norm_replaces_nonpositive_with_uniform() {
        let mut x = [0f32; M];
        let sum = pd_norm(&mut x);
        assert_eq!(sum, 0.0);
        for &v in x.iter() {
            assert!((v - 1.0 / 64.0).abs() < 1e-6);
        }
    }

    #[test]
    fn pd_argmax_returns_first_peak() {
        let mut x = [0.1f32; M];
        x[42] = 0.9;
        assert_eq!(pd_argmax(&x), 42);
    }

    /// End-to-end: encode the known-good info symbols, build a perfect
    /// intrinsic (delta at each transmitted symbol) after depuncturing, and
    /// check BP + map-decode recovers the same info symbols.
    #[test]
    fn decode_clean_intrinsic_recovers_info() {
        let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];

        // Build the internal 15-symbol input (info + CRC-12).
        let mut px = [0u8; 15];
        px[..13].copy_from_slice(&info);
        let (c0, c1) = crate::qra::crc12(&info);
        px[13] = c0;
        px[14] = c1;

        // QRA-encode to get 65 codeword symbols (pre-puncturing).
        let cw = internal_encode(&px);

        // Build a "clean" intrinsic: shape [65][64], 1.0 at cw[i], tiny epsilon
        // elsewhere so pd_norm doesn't collapse to uniform. Puncture positions
        // 13, 14 get uniform (CRC symbols are not transmitted).
        let mut ix = vec![0f32; V * M];
        let eps = 1e-8f32;
        for i in 0..V {
            let row_start = i * M;
            if i == 13 || i == 14 {
                // Uniform at CRC positions (depuncture with no info).
                for k in 0..M {
                    ix[row_start + k] = 1.0 / (M as f32);
                }
            } else {
                for k in 0..M {
                    ix[row_start + k] = eps;
                }
                ix[row_start + cw[i] as usize] = 1.0;
            }
        }

        let mut ex = vec![0f32; V * M];
        let mut v2c = vec![0f32; NMSG * M];
        let mut c2v = vec![0f32; NMSG * M];
        let rc = qra_extrinsic(&ix, &mut ex, 100, &mut v2c, &mut c2v);
        assert!(rc.is_some(), "BP must converge on a clean intrinsic");

        let mut decoded = [0u8; 15];
        qra_mapdecode(&ix, &mut ex, &mut decoded, 15);

        for (i, want) in px.iter().enumerate() {
            assert_eq!(decoded[i], *want, "decoded[{}] = {}, want {}", i, decoded[i], want);
        }
    }

    /// End-to-end via the top-level `q65_decode`: feed a 63-row clean
    /// intrinsic built from the real 63-symbol codeword, assert the 13 info
    /// symbols come back with a valid CRC.
    #[test]
    fn q65_decode_clean_recovers_info() {
        let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
        let cw63 = crate::qra::encode(&info);
        let eps = 1e-8f32;
        let mut ix = vec![eps; 63 * M];
        for (i, &v) in cw63.iter().enumerate() {
            ix[i * M + v as usize] = 1.0;
        }
        let mut scratch = DecodeScratch::default();
        match q65_decode(&ix, 100, &mut scratch) {
            DecodeOutcome::Ok(got, _iters) => assert_eq!(got, info),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    /// Decode with noisy intrinsics: delta peaks at wrong symbols on a few
    /// positions (simulates hard-decision errors). BP + CRC should still
    /// recover the original message as long as errors are below the code's
    /// correction threshold.
    #[test]
    fn q65_decode_with_symbol_errors() {
        let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
        let cw63 = crate::qra::encode(&info);
        let eps = 1e-3f32;
        let mut ix = vec![eps; 63 * M];
        for (i, &v) in cw63.iter().enumerate() {
            ix[i * M + v as usize] = 1.0;
        }
        // Flip a handful of symbols to wrong values: bump the "wrong" value
        // well above the "correct" one.
        let error_positions = [3, 17, 29, 41, 52];
        for &pos in &error_positions {
            let correct = cw63[pos] as usize;
            let wrong = (correct + 7) % 64;
            ix[pos * M + correct] = 0.01;
            ix[pos * M + wrong] = 1.0;
        }
        let mut scratch = DecodeScratch::default();
        match q65_decode(&ix, 100, &mut scratch) {
            DecodeOutcome::Ok(got, _iters) => assert_eq!(got, info),
            other => panic!(
                "expected Ok with {} symbol errors, got {:?}",
                error_positions.len(),
                other
            ),
        }
    }

    /// Internal helper: run the QRA accumulator without puncturing, returning
    /// the full 65-symbol codeword. Useful for tests that need to inspect the
    /// CRC symbols too.
    fn internal_encode(px: &[u8; 15]) -> [u8; 65] {
        use crate::qra::{ACC_INPUT_IDX, ACC_INPUT_WLOG, NC};
        let mut py = [0u8; 65];
        py[..15].copy_from_slice(px);
        let mut chk: u8 = 0;
        for k in 0..NC {
            let t = px[ACC_INPUT_IDX[k]];
            if t != 0 {
                let exponent = (crate::gf64::log(t) as u32 + ACC_INPUT_WLOG[k]) % 63;
                let weighted = crate::gf64::alpha_pow(exponent as i32);
                chk ^= weighted;
            }
            py[15 + k] = chk;
        }
        py
    }
}
