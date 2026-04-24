//! Q65 transmit-side encoder: 77-bit message -> 85 channel-symbol tone indices.

use crate::message;
use crate::params::Q65Params;
use crate::qra;
use crate::sync::{data_positions, DATA_TONE_OFFSET, SYNC_POSITIONS, SYNC_TONE};

/// Convert a 77-bit payload (MSB-first, packed into 10 bytes with the top 3
/// bits of the last byte zero) into 85 channel-symbol tone indices (each in
/// 0..64). Sync positions get sync tones from `SYNC_TONES`; data positions
/// carry the 63 RS(63,13) codeword symbols.
pub fn encode_message(_params: &Q65Params, payload77: &[u8; 10]) -> [u8; 85] {
    // 1. Payload 77 bits -> 13 six-bit info symbols. WSJT-X's genq65.f90
    //    reads bits 0..71 as 12 six-bit groups and bits 72..76 as a five-bit
    //    value, then multiplies the 13th element by 2 (left-shift by 1) to
    //    zero the LSB. Equivalent to: info[12] carries bits 72..76 in its
    //    high-5 bits with the LSB forced to 0.
    let mut info = message::payload_to_rs_symbols(payload77);
    info[12] &= 0x3E; // clear LSB — the 78th bit is a fixed pad, not carried.
    // 2. QRA encode (with CRC-12 and puncture) -> 63 codeword symbols.
    let cw = qra::encode(&info);
    // 3. Lay out 85-symbol frame: sync positions get tone 0, data positions
    //    get `codeword_value + DATA_TONE_OFFSET` (tones 1..=64).
    let mut frame = [0u8; 85];
    for &pos in SYNC_POSITIONS.iter() {
        frame[pos] = SYNC_TONE;
    }
    for (cw_idx, &pos) in data_positions().iter().enumerate() {
        frame[pos] = cw[cw_idx] + DATA_TONE_OFFSET;
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_frame_has_correct_shape() {
        let payload = [0u8; 10];
        let frame = encode_message(&crate::params::Q65_60C, &payload);
        assert_eq!(frame.len(), 85);
        for &pos in SYNC_POSITIONS.iter() {
            assert_eq!(frame[pos], SYNC_TONE, "sync position {} wrong", pos);
        }
        // Tones 0..=64 are valid (0 = sync, 1..=64 = data).
        for &v in frame.iter() {
            assert!(v <= 64);
        }
    }

    #[test]
    fn different_payloads_give_different_frames() {
        let a = [0u8; 10];
        let mut b = [0u8; 10];
        b[0] = 0xAA;
        let fa = encode_message(&crate::params::Q65_60C, &a);
        let fb = encode_message(&crate::params::Q65_60C, &b);
        assert_ne!(fa, fb);
    }
}
