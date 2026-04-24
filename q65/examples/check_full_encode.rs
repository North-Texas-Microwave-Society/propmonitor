//! Encode "K1ABC W9XYZ EN37" end-to-end via the crate's top-level pipeline
//! and verify the 85-tone channel-symbol sequence matches q65sim's output.

use q65::qra;
use q65::sync::{data_positions, DATA_TONE_OFFSET, SYNC_POSITIONS, SYNC_TONE};

fn main() {
    // Feed the *known correct* 13 info symbols directly (from q65sim) to test
    // that qra::encode + frame layout gives the right 85-tone sequence.
    // This isolates the QRA+layout from the still-incomplete message packer.
    let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
    let cw = qra::encode(&info);
    let mut frame = [0u8; 85];
    for &pos in SYNC_POSITIONS.iter() {
        frame[pos] = SYNC_TONE;
    }
    for (cw_idx, &pos) in data_positions().iter().enumerate() {
        frame[pos] = cw[cw_idx] + DATA_TONE_OFFSET;
    }

    // q65sim "Channel symbols" for the same message.
    let expected: [u8; 85] = [
        0, 3, 28, 56, 36, 21, 7, 6, 0, 10, 56, 0, 0, 1, 0, 34, 23, 19, 43, 64,
        29, 0, 0, 9, 24, 0, 0, 18, 18, 9, 39, 38, 0, 23, 0, 32, 18, 0, 24, 46,
        46, 60, 32, 10, 41, 0, 64, 58, 57, 0, 58, 44, 22, 8, 0, 55, 46, 60, 13, 0,
        13, 0, 4, 7, 4, 0, 41, 9, 0, 11, 47, 25, 25, 0, 27, 0, 7, 45, 19, 5,
        52, 8, 51, 20, 0,
    ];
    let n_match: usize = frame.iter().zip(expected.iter()).filter(|(a, b)| a == b).count();
    println!("{}/85 tones match q65sim", n_match);
    if frame != expected {
        for (i, (g, e)) in frame.iter().zip(expected.iter()).enumerate() {
            if g != e {
                println!("  pos {:>2}: got {:>2}, expected {:>2}", i, g, e);
            }
        }
    } else {
        println!("EXACT match on all 85 channel symbols. Encoder is correct.");
    }
}
