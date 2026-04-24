//! Reed-Solomon (63, 13) code over GF(2^6).
//!
//! Generator polynomial g(x) = prod_{i=1}^{50} (x - alpha^i).
//! Systematic encoding: codeword c = [message || parity] where parity =
//! (m(x) * x^50) mod g(x). A codeword satisfies c(alpha^i) = 0 for
//! i = 1..=50.
//!
//! NOTE: the choice of first root (alpha^1 vs alpha^0) and the specific
//! primitive polynomial for GF(64) is an internal convention for this
//! crate — consistent between encoder and hard decoder, but may not
//! match WSJT-X's. Aligning with WSJT-X happens at the Stage 6
//! reference-vector check, where we verify by cross-checking actual
//! on-air captures.

use crate::gf64::{self, Gf};

pub const N: usize = 63;
pub const K: usize = 13;
pub const PARITY: usize = N - K;
pub const MAX_ERRORS: usize = PARITY / 2;

/// Evaluate polynomial `p` (coefficient-ascending, p[0] is constant term) at x.
pub fn poly_eval(p: &[Gf], x: Gf) -> Gf {
    let mut acc: Gf = 0;
    for &c in p.iter().rev() {
        acc = gf64::add(gf64::mul(acc, x), c);
    }
    acc
}

fn generator() -> Vec<Gf> {
    // g(x) = prod_{i=1}^{PARITY} (x - alpha^i). Coefficients ascending.
    let mut g: Vec<Gf> = vec![1];
    for i in 1..=PARITY {
        let root = gf64::alpha_pow(i as i32);
        // multiply g(x) by (x - root) = (x + root) in GF(2^m).
        let mut next = vec![0u8; g.len() + 1];
        for (j, &c) in g.iter().enumerate() {
            next[j] = gf64::add(next[j], gf64::mul(c, root));
            next[j + 1] = gf64::add(next[j + 1], c);
        }
        g = next;
    }
    g
}

/// Systematic RS(63,13) encode: input `message[0..13]` (each in 0..64),
/// returns 63-symbol codeword with message in positions 0..13 and parity
/// in positions 13..63.
pub fn encode(message: &[Gf; K]) -> [Gf; N] {
    let g = generator();
    // Compute parity = (m(x) * x^PARITY) mod g(x), using synthetic division.
    // Shift message up: coefficient array of length N with message in high positions.
    let mut out = [0u8; N];
    out[PARITY..(PARITY + K)].copy_from_slice(&message[..K]);
    // Now treat `out` as polynomial of degree <N, and reduce mod g(x).
    // We divide out by g(x) (high-to-low) and the remainder goes into 0..PARITY.
    let mut work = out;
    for i in (PARITY..N).rev() {
        let coef = work[i];
        if coef != 0 {
            // subtract coef * g(x) * x^(i - PARITY)
            for (j, &gc) in g.iter().enumerate() {
                let idx = i - PARITY + j;
                work[idx] = gf64::add(work[idx], gf64::mul(coef, gc));
            }
        }
    }
    // work[0..PARITY] is now the parity; message is still in work[PARITY..N].
    // But caller wants message in positions 0..K and parity in positions K..N
    // — OR the other way? Standard systematic-RS uses high-order positions for
    // message, low-order for parity. Pick a convention and stick to it.
    //
    // We use: codeword[0..K] = message, codeword[K..N] = parity.
    let mut cw = [0u8; N];
    cw[..K].copy_from_slice(&message[..K]);
    cw[K..(K + PARITY)].copy_from_slice(&work[..PARITY]);
    cw
}

/// Compute syndromes S_i = c(alpha^i) for i=1..=PARITY.
/// Codeword layout: same as `encode()` output — message in 0..K, parity in K..N.
/// To evaluate as a polynomial over GF, we treat position p as coefficient of x^p
/// after un-systematic'ing: the transmitted polynomial c(x) has message coefficients
/// at x^PARITY..x^(N-1) and parity at x^0..x^(PARITY-1).
pub fn syndromes(codeword: &[Gf; N]) -> [Gf; PARITY] {
    // Rebuild the polynomial-form codeword: poly[0..PARITY] = codeword[K..N] (parity),
    // poly[PARITY..N] = codeword[0..K] (message).
    let mut poly = [0u8; N];
    poly[..PARITY].copy_from_slice(&codeword[K..(K + PARITY)]);
    poly[PARITY..(PARITY + K)].copy_from_slice(&codeword[..K]);
    let mut s = [0u8; PARITY];
    for i in 1..=PARITY {
        let x = gf64::alpha_pow(i as i32);
        s[i - 1] = poly_eval(&poly, x);
    }
    s
}

/// Berlekamp-Massey: find the error locator polynomial from syndromes.
/// Returns (locator, locator_degree).
fn berlekamp_massey(syns: &[Gf]) -> Vec<Gf> {
    let n = syns.len();
    let mut c: Vec<Gf> = vec![0; n + 1];
    let mut b: Vec<Gf> = vec![0; n + 1];
    c[0] = 1;
    b[0] = 1;
    let mut l: usize = 0;
    let mut m: usize = 1;
    let mut big_b: Gf = 1;
    for i in 0..n {
        let mut d: Gf = syns[i];
        for j in 1..=l {
            d = gf64::add(d, gf64::mul(c[j], syns[i - j]));
        }
        if d == 0 {
            m += 1;
        } else if 2 * l <= i {
            let t = c.clone();
            let coef = gf64::div(d, big_b);
            for j in 0..=n {
                if j + m <= n {
                    c[j + m] = gf64::add(c[j + m], gf64::mul(coef, b[j]));
                }
            }
            l = i + 1 - l;
            b = t;
            big_b = d;
            m = 1;
        } else {
            let coef = gf64::div(d, big_b);
            for j in 0..=n {
                if j + m <= n {
                    c[j + m] = gf64::add(c[j + m], gf64::mul(coef, b[j]));
                }
            }
            m += 1;
        }
    }
    // Trim to actual degree + 1.
    let mut deg = l;
    while deg > 0 && c[deg] == 0 {
        deg -= 1;
    }
    c.truncate(deg + 1);
    c
}

/// Chien search: find roots of locator polynomial. Returns the powers of alpha
/// whose inverse is a root (= error locations in 0..N).
fn chien_search(locator: &[Gf]) -> Vec<usize> {
    let mut locs = Vec::new();
    for i in 0..N {
        let x = gf64::alpha_pow(-(i as i32));
        if poly_eval(locator, x) == 0 {
            locs.push(i);
        }
    }
    locs
}

/// Forney algorithm: given syndromes, locator, and error locations, compute
/// error magnitudes.
fn forney(syns: &[Gf], locator: &[Gf], locs: &[usize]) -> Vec<Gf> {
    // Error evaluator: Omega(x) = S(x) * Lambda(x) mod x^PARITY, where
    // S(x) = sum s_i x^(i-1) (but our syndromes are s_1..s_PARITY; treat s_i
    // at index i-1).
    let mut s_poly = vec![0u8; syns.len()];
    s_poly[..].copy_from_slice(syns);
    let mut omega = vec![0u8; syns.len()];
    for i in 0..s_poly.len() {
        for j in 0..locator.len() {
            if i + j < omega.len() {
                omega[i + j] = gf64::add(omega[i + j], gf64::mul(s_poly[i], locator[j]));
            }
        }
    }

    // Formal derivative of locator.
    let mut deriv = vec![0u8; locator.len().saturating_sub(1)];
    for i in 1..locator.len() {
        if i % 2 == 1 {
            deriv[i - 1] = locator[i];
        }
    }

    let mut mags = Vec::with_capacity(locs.len());
    for &loc in locs {
        let x_inv = gf64::alpha_pow(-(loc as i32));
        let num = poly_eval(&omega, x_inv);
        let den = poly_eval(&deriv, x_inv);
        // Magnitude = -Omega(X^-1) / Lambda'(X^-1). In GF(2), negation is identity.
        let mag = gf64::div(num, den);
        mags.push(mag);
    }
    mags
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeResult {
    Clean,
    Corrected(usize),
    Uncorrectable,
}

/// Hard-decision decode in place. Returns Clean / Corrected(n) / Uncorrectable.
pub fn hard_decode(codeword: &mut [Gf; N]) -> DecodeResult {
    let s = syndromes(codeword);
    if s.iter().all(|&v| v == 0) {
        return DecodeResult::Clean;
    }
    let locator = berlekamp_massey(&s);
    let err_count = locator.len() - 1;
    if err_count == 0 || err_count > MAX_ERRORS {
        return DecodeResult::Uncorrectable;
    }
    let locs = chien_search(&locator);
    if locs.len() != err_count {
        return DecodeResult::Uncorrectable;
    }
    let mags = forney(&s, &locator, &locs);
    // Convert polynomial positions back to codeword positions.
    // Polynomial form: poly[0..PARITY] = parity (codeword[K..N]),
    // poly[PARITY..N] = message (codeword[0..K]). Position p in polynomial
    // corresponds to codeword position: p < PARITY -> K + p; else -> p - PARITY.
    for (loc, mag) in locs.iter().zip(mags.iter()) {
        let cw_idx = if *loc < PARITY {
            K + *loc
        } else {
            *loc - PARITY
        };
        codeword[cw_idx] = gf64::add(codeword[cw_idx], *mag);
    }
    // Verify.
    let s2 = syndromes(codeword);
    if s2.iter().all(|&v| v == 0) {
        DecodeResult::Corrected(err_count)
    } else {
        DecodeResult::Uncorrectable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message() -> [Gf; K] {
        let mut m = [0u8; K];
        for (i, v) in m.iter_mut().enumerate() {
            *v = ((i * 7 + 3) % 64) as u8;
        }
        m
    }

    #[test]
    fn encoded_codeword_has_zero_syndromes() {
        let m = sample_message();
        let cw = encode(&m);
        let s = syndromes(&cw);
        assert!(s.iter().all(|&v| v == 0), "syndromes = {:?}", s);
    }

    #[test]
    fn decode_clean_codeword() {
        let m = sample_message();
        let mut cw = encode(&m);
        assert_eq!(hard_decode(&mut cw), DecodeResult::Clean);
        assert_eq!(&cw[..K], &m[..]);
    }

    #[test]
    fn decode_single_error() {
        let m = sample_message();
        let mut cw = encode(&m);
        let orig = cw[5];
        cw[5] ^= 0b101010;
        assert_eq!(hard_decode(&mut cw), DecodeResult::Corrected(1));
        assert_eq!(cw[5], orig);
    }

    #[test]
    fn decode_many_errors() {
        // MAX_ERRORS = 25 for RS(63,13). Should still correct.
        let m = sample_message();
        let cw = encode(&m);
        for n_err in 1..=MAX_ERRORS {
            let mut corrupted = cw;
            for i in 0..n_err {
                corrupted[i * 2] ^= ((i * 13 + 1) % 63 + 1) as u8;
            }
            let r = hard_decode(&mut corrupted);
            assert!(
                matches!(r, DecodeResult::Corrected(_) | DecodeResult::Clean),
                "n_err={} result={:?}",
                n_err,
                r
            );
            assert_eq!(&corrupted[..], &cw[..], "n_err={}", n_err);
        }
    }

    #[test]
    fn too_many_errors_uncorrectable() {
        let m = sample_message();
        let cw = encode(&m);
        let mut corrupted = cw;
        // 26 distinct errors > MAX_ERRORS=25.
        for (i, slot) in corrupted.iter_mut().enumerate().take(26) {
            *slot ^= (((i * 17 + 7) % 63) + 1) as u8;
        }
        let r = hard_decode(&mut corrupted);
        // Either Uncorrectable or miscorrected — but it must NOT silently claim
        // the original message was recovered.
        if let DecodeResult::Corrected(_) = r {
            assert_ne!(&corrupted[..], &cw[..]);
        }
    }
}
