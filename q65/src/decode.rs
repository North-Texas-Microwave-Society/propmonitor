//! Top-level Q65 decode: audio -> list of Decodes.

use num_complex::Complex32;

use crate::demod::{demodulate, FineSync};
use crate::kv::{osd_decode, ReliabilityMatrix};
use crate::message::{format_message, rs_symbols_to_payload, unpack, Payload77};
use crate::params::Q65Params;
use crate::rs::{syndromes, N as RS_N};
use crate::sync::{frame_duration_s, SYNC_POSITIONS};

#[derive(Debug, Clone)]
pub struct Decode {
    pub snr_db: f32,
    pub dt_s: f32,
    pub freq_hz: f32,
    pub message: String,
    pub raw_payload: Payload77,
}

#[derive(Debug, Clone, Copy)]
pub struct DecodeSearch {
    pub dt_range_s: f32,
    pub dt_step_s: f32,
    pub freq_center_hz: f32,
    pub freq_range_hz: f32,
    pub freq_step_hz: f32,
    pub max_decodes: usize,
}

impl Default for DecodeSearch {
    fn default() -> Self {
        Self {
            dt_range_s: 2.0,
            dt_step_s: 0.1,
            freq_center_hz: 1500.0,
            freq_range_hz: 200.0,
            freq_step_hz: 1.0,
            max_decodes: 5,
        }
    }
}

#[derive(Debug)]
pub enum DecodeError {
    AudioTooShort,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::AudioTooShort => write!(f, "audio buffer shorter than one T/R period"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub fn decode(
    params: &Q65Params,
    audio: &[Complex32],
    audio_sr_hz: f64,
    search: &DecodeSearch,
) -> Result<Vec<Decode>, DecodeError> {
    let frame_s = frame_duration_s(params);
    if (audio.len() as f64) < frame_s * audio_sr_hz {
        return Err(DecodeError::AudioTooShort);
    }

    // Grid search over (dt, freq) using the sync correlator.
    let peaks = sync_grid_search(params, audio, audio_sr_hz, search);

    let mut out = Vec::new();
    for (_score, fine) in peaks.into_iter().take(search.max_decodes) {
        let rel = demodulate(params, audio, audio_sr_hz, fine);
        if let Some(decode) = try_decode_from_reliability(&rel, &fine) {
            out.push(decode);
        }
    }
    Ok(out)
}

fn try_decode_from_reliability(rel: &ReliabilityMatrix, fine: &FineSync) -> Option<Decode> {
    let cw = osd_decode(rel, 6)?;
    let s = syndromes(&cw);
    if !s.iter().all(|&v| v == 0) {
        return None;
    }
    let mut info = [0u8; 13];
    info[..].copy_from_slice(&cw[..13]);
    let payload = rs_symbols_to_payload(&info);
    let msg = unpack(&payload);
    Some(Decode {
        snr_db: estimate_snr(rel, &cw),
        dt_s: fine.dt_s,
        freq_hz: fine.freq_hz,
        message: format_message(&msg),
        raw_payload: payload,
    })
}

fn estimate_snr(rel: &ReliabilityMatrix, cw: &[u8; RS_N]) -> f32 {
    // Average log-power of selected symbol minus average of rest.
    let mut sig = 0.0f32;
    let mut bkg = 0.0f32;
    let mut n_sig = 0;
    let mut n_bkg = 0;
    for (i, row) in rel.iter().enumerate().take(RS_N) {
        let pick = cw[i] as usize;
        for (s, &val) in row.iter().enumerate() {
            if s == pick {
                sig += val;
                n_sig += 1;
            } else {
                bkg += val;
                n_bkg += 1;
            }
        }
    }
    let sig = sig / n_sig.max(1) as f32;
    let bkg = bkg / n_bkg.max(1) as f32;
    // Convert from nats (log power) to dB.
    (sig - bkg) * (10.0 / std::f32::consts::LN_10)
}

fn sync_grid_search(
    params: &Q65Params,
    audio: &[Complex32],
    audio_sr_hz: f64,
    search: &DecodeSearch,
) -> Vec<(f32, FineSync)> {
    let mut peaks: Vec<(f32, FineSync)> = Vec::new();
    let mut dt = -search.dt_range_s;
    while dt <= search.dt_range_s {
        let mut df = -search.freq_range_hz;
        while df <= search.freq_range_hz {
            let fine = FineSync {
                dt_s: dt,
                freq_hz: search.freq_center_hz + df,
            };
            let s = sync_correlate(params, audio, audio_sr_hz, fine);
            peaks.push((s, fine));
            df += search.freq_step_hz;
        }
        dt += search.dt_step_s;
    }
    peaks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    peaks
}

/// Matched filter for the 22 sync symbols (all transmitted at tone 0, which
/// is the LOWEST audio frequency of the occupied band): for each sync
/// position, compute the complex amplitude at `fine.freq_hz` over the
/// symbol window and sum magnitudes.
fn sync_correlate(
    params: &Q65Params,
    audio: &[Complex32],
    audio_sr_hz: f64,
    fine: FineSync,
) -> f32 {
    let samples_per_sym = (params.tsym_s * audio_sr_hz).round() as usize;
    if samples_per_sym == 0 {
        return 0.0;
    }
    let omega = 2.0 * std::f64::consts::PI * fine.freq_hz as f64 / audio_sr_hz;
    let mut total = 0.0f32;
    for &pos in SYNC_POSITIONS.iter() {
        let start_s = fine.dt_s as f64 + pos as f64 * params.tsym_s;
        let start_sample = (start_s * audio_sr_hz).round() as isize;
        if start_sample < 0 || start_sample as usize + samples_per_sym > audio.len() {
            continue;
        }
        let s0 = start_sample as usize;
        let mut acc = Complex32::new(0.0, 0.0);
        for k in 0..samples_per_sym {
            let phase = -omega * k as f64;
            let rot = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            acc += audio[s0 + k] * rot;
        }
        total += (acc.re * acc.re + acc.im * acc.im).sqrt();
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_returns_audiotooshort_for_empty() {
        let audio = vec![Complex32::new(0.0, 0.0); 10];
        let r = decode(
            &crate::params::Q65_60C,
            &audio,
            12_000.0,
            &DecodeSearch::default(),
        );
        assert!(matches!(r, Err(DecodeError::AudioTooShort)));
    }
}
