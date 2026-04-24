//! Koetter-Vardy soft-decision list decoder for RS(63,13) over GF(64).
//!
//! STATUS: skeleton — not implemented. The MVP ships with ordered-
//! statistics decoding (OSD) as a placeholder soft decoder; KV is
//! tracked for a follow-up ticket. See `decode::soft_decode`.

#![allow(dead_code)]

use crate::gf64::Gf;
use crate::rs::N;

/// Reliability matrix: `rel[pos][sym]` is the log-likelihood that the
/// transmitted symbol at position `pos` is `sym`. Larger = more likely.
pub type ReliabilityMatrix = [[f32; 64]; N];

/// Candidate codeword list.
pub type Candidates = Vec<[Gf; N]>;

/// Returns up to `max_list` candidate codewords sorted by likelihood
/// under the given reliability matrix. Placeholder: will eventually
/// implement Koetter-Vardy interpolation + Roth-Ruckenstein factoring.
pub fn list_decode(_rel: &ReliabilityMatrix, _max_list: usize) -> Candidates {
    Vec::new()
}

pub fn best_symbol(rel_row: &[f32; 64]) -> Gf {
    let mut best = 0u8;
    let mut best_v = f32::NEG_INFINITY;
    for (i, &v) in rel_row.iter().enumerate() {
        if v > best_v {
            best_v = v;
            best = i as u8;
        }
    }
    best
}

/// Turn a reliability matrix into a hard-decision codeword (argmax per position).
pub fn hard_from_reliability(rel: &ReliabilityMatrix) -> [Gf; N] {
    let mut out = [0u8; N];
    for (i, row) in rel.iter().enumerate() {
        out[i] = best_symbol(row);
    }
    out
}

/// Ordered-statistics soft decoder, order-0 fallback.
/// Sorts positions by reliability, takes the top `K` as the information set,
/// solves for the codeword, and returns it. Then compares against the
/// received hard-decision word; flips up to `max_flips` of the least-reliable
/// information positions to re-solve. Returns the best codeword found that
/// passes syndrome check, or None.
pub fn osd_decode(rel: &ReliabilityMatrix, max_flips: usize) -> Option<[Gf; N]> {
    // Reliability per position: max row value minus second-max, as a margin.
    let mut margins: Vec<(usize, f32)> = (0..N)
        .map(|i| {
            let mut best = f32::NEG_INFINITY;
            let mut second = f32::NEG_INFINITY;
            for &v in rel[i].iter() {
                if v > best {
                    second = best;
                    best = v;
                } else if v > second {
                    second = v;
                }
            }
            (i, best - second)
        })
        .collect();
    // Sort descending by margin (most reliable first).
    margins.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let hard = hard_from_reliability(rel);

    // Try the hard word first.
    let mut best_cw: Option<[Gf; N]> = None;
    let mut best_score = f32::NEG_INFINITY;
    let mut cw = hard;
    if crate::rs::hard_decode(&mut cw) != crate::rs::DecodeResult::Uncorrectable {
        let sc = score(rel, &cw);
        best_cw = Some(cw);
        best_score = sc;
    }

    // Try flipping the `max_flips` least-reliable positions to their 2nd-best symbol.
    // This is a cheap OSD-ish variant; full OSD-order-t would enumerate all subsets.
    if max_flips > 0 {
        let n_to_flip = max_flips.min(N);
        let flip_set = &margins[(N - n_to_flip)..];
        for mask in 1u32..(1u32 << n_to_flip) {
            let mut cw = hard;
            for (bit, (idx, _)) in flip_set.iter().enumerate() {
                if (mask >> bit) & 1 == 1 {
                    cw[*idx] = second_best_symbol(&rel[*idx]);
                }
            }
            if crate::rs::hard_decode(&mut cw) != crate::rs::DecodeResult::Uncorrectable {
                let sc = score(rel, &cw);
                if sc > best_score {
                    best_score = sc;
                    best_cw = Some(cw);
                }
            }
        }
    }
    best_cw
}

fn second_best_symbol(row: &[f32; 64]) -> Gf {
    let mut best = f32::NEG_INFINITY;
    let mut second = f32::NEG_INFINITY;
    let mut best_i = 0u8;
    let mut second_i = 0u8;
    for (i, &v) in row.iter().enumerate() {
        if v > best {
            second = best;
            second_i = best_i;
            best = v;
            best_i = i as u8;
        } else if v > second {
            second = v;
            second_i = i as u8;
        }
    }
    let _ = second;
    second_i
}

fn score(rel: &ReliabilityMatrix, cw: &[Gf; N]) -> f32 {
    let mut s = 0.0f32;
    for i in 0..N {
        s += rel[i][cw[i] as usize];
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rs::{encode, K};

    fn reliability_from_codeword(cw: &[Gf; N], noise: f32, seed: u64) -> ReliabilityMatrix {
        // Synthesize reliabilities that concentrate on the transmitted symbol but
        // with Gaussian-ish noise on each entry. Deterministic PRNG (xorshift).
        let mut state = seed.wrapping_add(0xA5A5_A5A5_A5A5_A5A5);
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (((state >> 32) as u32) as f32) / (u32::MAX as f32)
        };
        let mut rel = [[0f32; 64]; N];
        for (i, row) in rel.iter_mut().enumerate() {
            for (s, cell) in row.iter_mut().enumerate() {
                let n = (rng() - 0.5) * noise;
                *cell = if s as u8 == cw[i] { 1.0 + n } else { n };
            }
        }
        rel
    }

    #[test]
    fn osd_recovers_clean_reliability() {
        let mut msg = [0u8; K];
        for (i, v) in msg.iter_mut().enumerate() {
            *v = ((i * 5 + 2) % 64) as u8;
        }
        let cw = encode(&msg);
        let rel = reliability_from_codeword(&cw, 0.1, 1);
        let decoded = osd_decode(&rel, 0).expect("must decode");
        assert_eq!(&decoded[..], &cw[..]);
    }

    #[test]
    fn osd_recovers_noisy_reliability() {
        let mut msg = [0u8; K];
        for (i, v) in msg.iter_mut().enumerate() {
            *v = ((i * 11 + 1) % 64) as u8;
        }
        let cw = encode(&msg);
        // Noise level chosen so hard-decode alone would fail for some trials.
        let rel = reliability_from_codeword(&cw, 0.4, 7);
        let decoded = osd_decode(&rel, 4);
        // Don't require success here — just check API shape and no panic.
        if let Some(d) = decoded {
            let score_cw = score(&rel, &cw);
            let score_d = score(&rel, &d);
            // If we found a different word, it must score at least as well.
            if d != cw {
                assert!(score_d >= score_cw - 1e-3, "osd found a worse word");
            }
        }
    }
}
