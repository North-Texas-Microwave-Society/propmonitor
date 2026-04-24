//! Reference-vector cross-check against a WSJT-X `q65sim` capture.
//!
//! Fixture: `tests/fixtures/q65_60C_K1ABC_W9XYZ_EN37.wav` — a clean
//! (SNR +30 dB) Q65-60C synthesis of "K1ABC W9XYZ EN37" at audio
//! center 1500 Hz. q65sim also printed the expected 13-symbol info,
//! 63-symbol codeword, and 85-symbol channel-symbol sequence, which
//! we assert the crate reproduces.

use q65::qra;
use q65::sync::{data_positions, DATA_TONE_OFFSET, SYNC_POSITIONS, SYNC_TONE};

const FIXTURE_INFO: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];

const FIXTURE_CODEWORD: [u8; 63] = [
    2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18, 42, 63, 28, 8, 23, 17, 17, 8, 38, 37, 22, 31,
    17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56, 57, 43, 21, 7, 54, 45, 59, 12, 12, 3, 6, 3, 40, 8,
    10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7, 50, 19,
];

const FIXTURE_TONES: [u8; 85] = [
    0, 3, 28, 56, 36, 21, 7, 6, 0, 10, 56, 0, 0, 1, 0, 34, 23, 19, 43, 64, 29, 0, 0, 9, 24, 0, 0,
    18, 18, 9, 39, 38, 0, 23, 0, 32, 18, 0, 24, 46, 46, 60, 32, 10, 41, 0, 64, 58, 57, 0, 58, 44,
    22, 8, 0, 55, 46, 60, 13, 0, 13, 0, 4, 7, 4, 0, 41, 9, 0, 11, 47, 25, 25, 0, 27, 0, 7, 45, 19,
    5, 52, 8, 51, 20, 0,
];

#[test]
fn qra_encoder_matches_q65sim_codeword() {
    assert_eq!(qra::encode(&FIXTURE_INFO), FIXTURE_CODEWORD);
}

#[test]
fn full_frame_layout_matches_q65sim() {
    let cw = qra::encode(&FIXTURE_INFO);
    let mut frame = [0u8; 85];
    for &pos in SYNC_POSITIONS.iter() {
        frame[pos] = SYNC_TONE;
    }
    for (cw_idx, &pos) in data_positions().iter().enumerate() {
        frame[pos] = cw[cw_idx] + DATA_TONE_OFFSET;
    }
    assert_eq!(frame, FIXTURE_TONES);
}
