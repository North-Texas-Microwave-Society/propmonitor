//! Per-symbol demodulation: given a complex-audio buffer covering a single
//! Q65 T/R period, compute a reliability matrix for the 63 data symbols.

use num_complex::Complex32;
use rustfft::FftPlanner;

use crate::kv::ReliabilityMatrix;
use crate::params::Q65Params;
use crate::rs::N as RS_N;
use crate::sync::{data_positions, DATA_TONE_OFFSET};

/// Demodulation parameters resolved from a sync search.
#[derive(Debug, Clone, Copy)]
pub struct FineSync {
    /// Time offset in seconds from the period's UTC boundary.
    pub dt_s: f32,
    /// Audio frequency of tone 0 (the sync tone) in Hz. Data tones 1..=64
    /// sit at `freq_hz + k * tone_spacing_hz` for k=1..=64.
    pub freq_hz: f32,
}

/// Demodulate the 63 data symbols and produce a reliability matrix.
/// `audio` is complex baseband at `audio_sr_hz` sample rate. Window for
/// symbol `i` starts at `dt_s + i * tsym` (for i in 0..85), but we skip
/// sync positions and only fill the 63 data entries in the matrix.
pub fn demodulate(
    params: &Q65Params,
    audio: &[Complex32],
    audio_sr_hz: f64,
    fine: FineSync,
) -> ReliabilityMatrix {
    let mut rel = [[0f32; 64]; RS_N];
    let data_pos = data_positions();

    let samples_per_sym = (params.tsym_s * audio_sr_hz).round() as usize;
    if samples_per_sym == 0 {
        return rel;
    }

    // FFT size: power of 2 covering at least samples_per_sym so bin spacing
    // is audio_sr/fft_size. For Q65-60C we want bin spacing well below
    // tone_spacing (~6.62 Hz); 1 Hz per bin is fine.
    let fft_size = samples_per_sym.next_power_of_two().max(8192);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);

    // Bin index for tone k (k in 0..num_tones): freq_hz + k * tone_spacing.
    let bin_for_freq = |f: f64| -> isize {
        let b = f * fft_size as f64 / audio_sr_hz;
        b.round() as isize
    };
    let center_bin = bin_for_freq(fine.freq_hz as f64);
    let bin_step = (params.tone_spacing_hz * fft_size as f64 / audio_sr_hz).max(1.0);

    let mut scratch = vec![Complex32::new(0.0, 0.0); fft_size];

    for (rs_idx, &slot) in data_pos.iter().enumerate() {
        let start_s = fine.dt_s as f64 + slot as f64 * params.tsym_s;
        let start_sample = (start_s * audio_sr_hz).round() as isize;
        if start_sample < 0 || start_sample as usize + samples_per_sym > audio.len() {
            // Out of range; leave row zeroed.
            continue;
        }
        let s0 = start_sample as usize;
        for (i, slot_buf) in scratch.iter_mut().enumerate() {
            *slot_buf = if i < samples_per_sym {
                audio[s0 + i]
            } else {
                Complex32::new(0.0, 0.0)
            };
        }
        fft.process(&mut scratch);
        // For data-symbol value v (0..=63), transmitted tone index = v + DATA_TONE_OFFSET.
        // Tones run UPWARD in frequency, so audio_freq = freq_hz + tone_index * spacing.
        let mut row = [0f32; 64];
        for (v, row_slot) in row.iter_mut().enumerate() {
            let tone_idx = v as isize + DATA_TONE_OFFSET as isize;
            let b_signed = center_bin + (tone_idx as f64 * bin_step).round() as isize;
            if b_signed < 0 || b_signed >= fft_size as isize {
                *row_slot = f32::NEG_INFINITY;
                continue;
            }
            let x = scratch[b_signed as usize];
            let p = (x.re * x.re + x.im * x.im).max(1e-20);
            *row_slot = p.ln();
        }
        // Normalize row by subtracting the median so values are
        // comparable across symbols.
        let mut sorted: [f32; 64] = row;
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[32];
        for v in row.iter_mut() {
            if v.is_finite() {
                *v -= median;
            }
        }
        rel[rs_idx] = row;
        // `slot` used only to locate the audio window.
        let _ = slot;
    }
    rel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demodulate_empty_audio_returns_zero_matrix() {
        let audio = vec![Complex32::new(0.0, 0.0); 10];
        let rel = demodulate(
            &crate::params::Q65_60C,
            &audio,
            12_000.0,
            FineSync {
                dt_s: 0.0,
                freq_hz: 1500.0,
            },
        );
        // All rows zero because no data window fits.
        for row in rel.iter() {
            for &v in row.iter() {
                assert_eq!(v, 0.0);
            }
        }
    }
}
