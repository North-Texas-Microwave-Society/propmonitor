//! End-to-end: read a Q65-60C WAV, sync, demodulate to per-symbol tone
//! energies, normalize to probabilities, feed to the BP decoder, report the
//! 13-symbol decoded info. No AP info, no fading model — the simplest
//! possible energy-to-probability map.
//!
//! Usage: cargo run --release -p q65 --example decode_wav_bp -- path/to.wav

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use q65::bp::{q65_decode, DecodeOutcome, DecodeScratch, M};
use q65::sync::{data_positions, SYNC_POSITIONS};
use q65::Q65_60C;
use rustfft::FftPlanner;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).ok_or("usage: decode_wav_bp <path.wav>")?;
    let (samples, sr) = load_wav_mono(path)?;
    let audio: Vec<Complex32> = samples.iter().map(|&s| Complex32::new(s, 0.0)).collect();

    // 1. Coarse then fine sync search (or use override if args supplied).
    let (dt, freq, score) = if args.len() >= 4 {
        let dt = args[2].parse::<f32>()?;
        let freq = args[3].parse::<f32>()?;
        (dt, freq, 0.0f32)
    } else {
        println!("searching for sync...");
        find_sync(&audio, sr)
    };
    println!("  sync: dt={:.3} s, tone0_freq={:.2} Hz, score={:.1}", dt, freq, score);

    // 2. Per-symbol FFT to get a 63 x 64 energy matrix.
    let energies = demodulate_energies(&audio, sr, dt, freq);

    // 3. Normalize to probabilities (row-wise, so each symbol row sums to 1).
    let mut intrinsics = vec![0f32; 63 * M];
    for row in 0..63 {
        let sum: f32 = energies[row].iter().sum::<f32>().max(1e-30);
        for k in 0..M {
            intrinsics[row * M + k] = energies[row][k] / sum;
        }
    }

    // Diagnostic: compare demod argmax against the known q65sim codeword.
    let expected_cw: [u8; 63] = [
        2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18, 42, 63, 28, 8, 23, 17, 17, 8, 38, 37, 22,
        31, 17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56, 57, 43, 21, 7, 54, 45, 59, 12, 12, 3, 6, 3,
        40, 8, 10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7, 50, 19,
    ];
    let mut n_correct = 0;
    let mut n_in_top5 = 0;
    for (i, &want) in expected_cw.iter().enumerate() {
        let mut order: Vec<(usize, f32)> =
            (0..M).map(|k| (k, intrinsics[i * M + k])).collect();
        order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if order[0].0 == want as usize {
            n_correct += 1;
        }
        if order[..5].iter().any(|(k, _)| *k == want as usize) {
            n_in_top5 += 1;
        }
    }
    println!(
        "demod argmax correct: {}/63, top-5 hit: {}/63",
        n_correct, n_in_top5
    );

    // 4. BP decode.
    let mut scratch = DecodeScratch::default();
    let outcome = q65_decode(&intrinsics, 100, &mut scratch);
    match outcome {
        DecodeOutcome::Ok(info, iters) => {
            println!("DECODED in {} iters: {:?}", iters, info);
            let expected = [2u8, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
            if info == expected {
                println!("-> matches K1ABC W9XYZ EN37 ground-truth info symbols.");
            } else {
                println!("-> but does NOT match expected {:?}", expected);
            }
        }
        DecodeOutcome::NoConvergence => println!("BP did not converge"),
        DecodeOutcome::CrcMismatch => println!("BP converged but CRC mismatch"),
    }
    Ok(())
}

/// Two-stage sync search over (dt, tone0_freq). Coarse pass scans a wide
/// grid, fine pass refines around the peak. Sync = matched filter at the
/// 22 fixed sync slots against a tone at tone0_freq.
fn find_sync(audio: &[Complex32], sr: f64) -> (f32, f32, f32) {
    let p = &Q65_60C;
    let samples_per_sym = (p.tsym_s * sr).round() as usize;

    let correlate = |dt_s: f64, freq: f64| -> f32 {
        let mut total = 0.0f32;
        let omega = 2.0 * std::f64::consts::PI * freq / sr;
        for &pos in SYNC_POSITIONS.iter() {
            let start = (dt_s * sr).round() as isize
                + (pos as isize) * (samples_per_sym as isize);
            if start < 0 || start as usize + samples_per_sym > audio.len() {
                continue;
            }
            let mut acc = Complex32::new(0.0, 0.0);
            for k in 0..samples_per_sym {
                let phase = -omega * k as f64;
                let rot = Complex32::new(phase.cos() as f32, phase.sin() as f32);
                acc += audio[start as usize + k] * rot;
            }
            total += (acc.re * acc.re + acc.im * acc.im).sqrt();
        }
        total
    };

    // Coarse: dt step 0.1 s, freq step 5 Hz. Tone 0 is the low end of the
    // occupied band; with default f0=1500 it sits right at 1500 Hz.
    let mut best = (0f64, 0f64, 0f32);
    let mut dt = 0.0f64;
    while dt <= 3.0 {
        let mut f = 1400.0f64;
        while f <= 1700.0 {
            let s = correlate(dt, f);
            if s > best.2 {
                best = (dt, f, s);
            }
            f += 5.0;
        }
        dt += 0.1;
    }

    // Fine: dt step 0.005 s, freq step 0.1 Hz, +-0.15 s / +-5 Hz around coarse peak.
    let (cdt, cf, _) = best;
    let mut dt = (cdt - 0.15).max(0.0);
    let end = (cdt + 0.15).min(3.0);
    while dt <= end {
        let mut f = (cf - 5.0).max(1400.0);
        while f <= cf + 5.0 {
            let s = correlate(dt, f);
            if s > best.2 {
                best = (dt, f, s);
            }
            f += 0.1;
        }
        dt += 0.005;
    }

    (best.0 as f32, best.1 as f32, best.2)
}

/// Demodulate all 63 data-slot symbols: return energy at each of the 64 tones
/// for each codeword position. Tones run DOWNWARD in freq from tone 0.
fn demodulate_energies(audio: &[Complex32], sr: f64, dt: f32, freq_tone0: f32) -> Vec<[f32; 64]> {
    let p = &Q65_60C;
    let samples_per_sym = (p.tsym_s * sr).round() as usize;
    let fft_size = samples_per_sym.next_power_of_two().max(8192);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut scratch = vec![Complex32::new(0.0, 0.0); fft_size];

    let center_bin = (freq_tone0 as f64 * fft_size as f64 / sr).round() as isize;
    let bin_step = p.tone_spacing_hz * fft_size as f64 / sr;

    let data_pos = data_positions();
    let mut out = vec![[0f32; 64]; 63];

    for (rs_idx, &slot) in data_pos.iter().enumerate() {
        let start = (dt as f64 * sr).round() as isize
            + (slot as isize) * (samples_per_sym as isize);
        if start < 0 || start as usize + samples_per_sym > audio.len() {
            continue;
        }
        for s in scratch.iter_mut() {
            *s = Complex32::new(0.0, 0.0);
        }
        for k in 0..samples_per_sym {
            scratch[k] = audio[start as usize + k];
        }
        fft.process(&mut scratch);

        // data symbol value v in 0..64 corresponds to tone (v+1), which is
        // audio_freq = freq_tone0 + (v+1) * tone_spacing.
        let mut row = [0f32; 64];
        for (v, cell) in row.iter_mut().enumerate() {
            let tone_idx = (v + 1) as f64;
            let b_signed = center_bin + (tone_idx * bin_step).round() as isize;
            if b_signed < 0 || b_signed >= fft_size as isize {
                continue;
            }
            let z = scratch[b_signed as usize];
            *cell = (z.re * z.re + z.im * z.im).max(1e-20);
        }
        out[rs_idx] = row;
    }
    out
}

fn load_wav_mono(path: &str) -> Result<(Vec<f32>, f64), Box<dyn std::error::Error>> {
    let mut f = File::open(path)?;
    let mut hdr = [0u8; 44];
    f.read_exact(&mut hdr)?;
    let channels = u16::from_le_bytes([hdr[22], hdr[23]]) as usize;
    let sr = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]) as f64;
    f.seek(SeekFrom::Start(36))?;
    let mut tag = [0u8; 4];
    let mut size_buf = [0u8; 4];
    let data_size;
    loop {
        f.read_exact(&mut tag)?;
        f.read_exact(&mut size_buf)?;
        let size = u32::from_le_bytes(size_buf) as u64;
        if &tag == b"data" {
            data_size = size as usize;
            break;
        }
        f.seek(SeekFrom::Current(size as i64))?;
    }
    let mut raw = vec![0u8; data_size];
    f.read_exact(&mut raw)?;
    let n_samples = raw.len() / 2 / channels;
    let mut out = Vec::with_capacity(n_samples);
    for i in 0..n_samples {
        let lo = raw[i * 2 * channels] as i16;
        let hi = raw[i * 2 * channels + 1] as i16;
        let s = ((hi << 8) | (lo & 0xFF)) as i16;
        out.push(s as f32 / 32768.0);
    }
    Ok((out, sr))
}
