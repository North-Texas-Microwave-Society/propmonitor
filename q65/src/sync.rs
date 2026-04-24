//! Q65 sync pattern: which of the 85 channel-symbol slots carry sync
//! symbols, and what tone those sync symbols transmit.
//!
//! Q65 uses a SINGLE sync tone (tone index 0) transmitted at 22 fixed
//! positions across the 85-symbol frame. Data symbols at the other 63
//! positions carry RS(63,13) codeword symbols mapped as
//! `tone = codeword_value + 1` (so data tones occupy indices 1..64).
//!
//! The sync positions below were extracted empirically from a clean
//! q65sim Q65-60C capture (message "K1ABC W9XYZ EN37", SNR +30 dB) on
//! 2026-04-16, and cross-checked against the q65sim.f90 signal synthesis
//! logic on 2026-04-17.
//!
//! Tone indices run UPWARD in audio frequency: tone 0 sits at the LOWEST
//! audio frequency of the occupied band, and tone 64 at the highest.
//! The mapping used throughout the crate is
//! `audio_freq(t) = f_tone0 + t * tone_spacing_hz`.
//! This matches q65sim.f90 line 176: `freq = f0 + itone * baud * mode65`.

use crate::params::Q65Params;

pub const NUM_SYNC: usize = 22;
pub const NUM_TOTAL: usize = 85;

/// Tone index 0 is the sync tone. Data tones are 1..=64.
pub const SYNC_TONE: u8 = 0;

/// Offset added to each RS codeword symbol value (0..=63) to get the
/// transmitted tone index (1..=64).
pub const DATA_TONE_OFFSET: u8 = 1;

/// The 22 sync-symbol slot indices within the 85-symbol frame.
/// Empirically extracted from q65sim output; applies to all Q65 submodes.
pub const SYNC_POSITIONS: [usize; NUM_SYNC] = [
    0, 8, 11, 12, 14, 21, 22, 25, 26, 32, 34, 37, 45, 49, 54, 59, 61, 65, 68, 73, 75, 84,
];

/// All 22 sync symbols transmit the same tone (tone 0).
pub const SYNC_TONES: [u8; NUM_SYNC] = [SYNC_TONE; NUM_SYNC];

/// Yield `(slot_index, tone_index)` for each sync symbol.
pub fn sync_schedule() -> impl Iterator<Item = (usize, u8)> {
    SYNC_POSITIONS.iter().copied().zip(SYNC_TONES.iter().copied())
}

/// Return the 63 slot indices (within the 85-symbol frame) that carry
/// data codeword symbols, in codeword order.
pub fn data_positions() -> Vec<usize> {
    let mut sync_mask = [false; NUM_TOTAL];
    for &p in &SYNC_POSITIONS {
        sync_mask[p] = true;
    }
    (0..NUM_TOTAL).filter(|&i| !sync_mask[i]).collect()
}

/// Time-domain width of the frame in seconds, including trailing symbol tail.
pub fn frame_duration_s(p: &Q65Params) -> f64 {
    p.tsym_s * p.num_symbols as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_positions_are_distinct_and_in_range() {
        let mut seen = [false; NUM_TOTAL];
        for &p in &SYNC_POSITIONS {
            assert!(p < NUM_TOTAL);
            assert!(!seen[p], "duplicate sync position {}", p);
            seen[p] = true;
        }
    }

    #[test]
    fn data_positions_count_is_63() {
        assert_eq!(data_positions().len(), 63);
    }

    #[test]
    fn sync_tones_in_range() {
        for &t in &SYNC_TONES {
            assert!(t < 64);
        }
    }
}
