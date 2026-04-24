//! Q-ary Repeat-Accumulate code used by Q65: the (N=65, K=15) IRA code
//! over GF(64) named `qra15_65_64_irr_e23` in WSJT-X.
//!
//! The K, N, tables, and CRC-12 polynomial below are transcribed from the
//! WSJT-X source at `lib/qra/q65/qra15_65_64_irr_e23.c` and
//! `lib/qra/q65/q65.c` (Nico Palermo IV3NWV, GPL-3.0-or-later). The
//! algorithm itself is implemented independently in Rust.
//!
//! The Q65 message pipeline is:
//!   1. 13 six-bit info symbols (dgen[0..13])
//!   2. append 2 CRC-12 symbols -> 15 input symbols (px[0..15])
//!   3. QRA encode -> 65 codeword symbols (py[0..65])
//!   4. drop py[13..15] (the 2 CRC symbols) -> 63 transmitted symbols
//!   5. interleave with 22 sync symbols (tone 0) into 85 channel symbols,
//!      with `tone = codeword_value + 1` for data.

use crate::gf64;

/// Info-symbol count seen by callers of `encode` (excludes CRC).
pub const K_EXTERNAL: usize = 13;
/// Codeword length seen by callers (excludes 2 punctured CRC symbols).
pub const N_EXTERNAL: usize = 63;

/// Internal QRA code dimensions.
pub const K: usize = 15;
pub const NC: usize = 50;

/// Accumulator input-index table. Entry k gives which of the 15 input
/// symbols contributes to check symbol k. The 51st entry is the
/// accumulator-termination index used only by the encoder debug check;
/// for encoding we iterate k = 0..NC.
pub const ACC_INPUT_IDX: [usize; NC + 1] = [
    13, 1, 3, 4, 8, 12, 9, 14, 10, 5, 0, 7, 1, 11, 8, 9, 12, 6, 3, 10, 7, 5, 2, 13, 12, 4, 8, 0, 1,
    11, 2, 9, 14, 5, 6, 13, 7, 12, 11, 2, 9, 0, 10, 4, 7, 14, 8, 11, 3, 6, 10,
];

/// Accumulator weight-logarithm table: each entry is the alpha-power
/// exponent applied to the input symbol selected by ACC_INPUT_IDX.
pub const ACC_INPUT_WLOG: [u32; NC + 1] = [
    0, 14, 0, 0, 13, 37, 0, 27, 56, 62, 29, 0, 52, 34, 62, 4, 3, 22, 25, 0, 22, 0, 20, 10, 0, 43,
    53, 60, 0, 0, 0, 62, 0, 5, 0, 61, 36, 31, 61, 59, 10, 0, 29, 39, 25, 18, 0, 14, 11, 50, 17,
];

/// CRC-12 generator polynomial (reciprocal, LSB-first bit ordering):
/// g(x) = x^12 + x^11 + x^3 + x^2 + x + 1.
const CRC12_GEN_POLY: u16 = 0xF01;

/// Compute the two CRC-12 six-bit symbols from the first `sz` info symbols.
/// `out` receives `(low_6_bits, high_6_bits)`.
pub fn crc12(info: &[u8]) -> (u8, u8) {
    let mut sr: u16 = 0;
    for &x in info.iter() {
        let mut t = x as u16;
        for _ in 0..6 {
            if (t ^ sr) & 1 != 0 {
                sr = (sr >> 1) ^ CRC12_GEN_POLY;
            } else {
                sr >>= 1;
            }
            t >>= 1;
        }
    }
    ((sr & 0x3F) as u8, ((sr >> 6) & 0x3F) as u8)
}

/// Encode 13 info symbols to 63 channel-symbol values (0..=63). This is the
/// full Q65 encoding pipeline: CRC-12 append, QRA accumulator, CRC puncture.
pub fn encode(info: &[u8; K_EXTERNAL]) -> [u8; N_EXTERNAL] {
    // 1. Copy info into a 15-symbol working buffer, then append 2 CRC symbols.
    let mut px = [0u8; K];
    px[..K_EXTERNAL].copy_from_slice(info);
    let (c0, c1) = crc12(info);
    px[K_EXTERNAL] = c0;
    px[K_EXTERNAL + 1] = c1;

    // 2. QRA accumulator encode: 15 input -> 65 codeword = 15 systematic + 50 check.
    let mut py = [0u8; 65];
    py[..K].copy_from_slice(&px);
    let mut chk: u8 = 0;
    for k in 0..NC {
        let t = px[ACC_INPUT_IDX[k]];
        if t != 0 {
            let exponent = (gf64::log(t) as u32 + ACC_INPUT_WLOG[k]) % 63;
            let weighted = gf64::alpha_pow(exponent as i32);
            chk ^= weighted;
        }
        py[K + k] = chk;
    }

    // 3. Puncture: drop the 2 CRC symbols at py[13..15]. Output = py[0..13] || py[15..65].
    let mut out = [0u8; N_EXTERNAL];
    out[..K_EXTERNAL].copy_from_slice(&py[..K_EXTERNAL]);
    out[K_EXTERNAL..].copy_from_slice(&py[K..65]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qra_tables::PMAT;

    /// Ground-truth codeword for message "K1ABC W9XYZ EN37", captured from
    /// WSJT-X's `q65sim` (Q65-60C, 2026-04-16).
    #[test]
    fn encodes_q65sim_reference_codeword() {
        let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
        let expected: [u8; 63] = [
            2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18, 42, 63, 28, 8, 23, 17, 17, 8, 38, 37,
            22, 31, 17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56, 57, 43, 21, 7, 54, 45, 59, 12, 12,
            3, 6, 3, 40, 8, 10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7, 50, 19,
        ];
        let got = encode(&info);
        assert_eq!(got, expected, "QRA encoder output must match q65sim");
    }

    /// PMAT[wlog*64 + k] must equal `alpha^(-wlog) * k` in GF(64). This is
    /// the `perm` index used by WSJT-X's `pd_fwdperm`, computing
    /// `dst[k] = src[perm[k]]` — substituting `x -> alpha^wlog * x` in a
    /// distribution. Cross-checks the transcribed table against our GF.
    #[test]
    fn pmat_is_alpha_neg_wlog_times_k() {
        for wlog in 0i32..63 {
            let alpha_neg_w = gf64::alpha_pow(-wlog);
            for k in 0..64u8 {
                let expected = gf64::mul(alpha_neg_w, k);
                let got = PMAT[(wlog as usize) * 64 + k as usize];
                assert_eq!(got, expected, "PMAT row {} col {}", wlog, k);
            }
        }
    }
}
