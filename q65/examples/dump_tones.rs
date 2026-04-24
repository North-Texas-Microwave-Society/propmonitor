//! Dump the 85-symbol tone grid from a clean Q65-60C WAV.
//!
//! For a noise-free q65sim output, per-symbol argmax over the 64 data-tone
//! bins gives the transmitted tone sequence. This lets us reverse-engineer
//! the sync pattern (positions + tones) by comparing multiple captures, or
//! by recognising the well-known "22 symbols at a constant tone"
//! fingerprint of Q65's sync design.

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use q65::Q65_60C;
use rustfft::FftPlanner;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).ok_or("usage: dump_tones <path.wav>")?;
    let freq_override: Option<f64> = args.get(2).and_then(|s| s.parse().ok());
    let dt_override: Option<f64> = args.get(3).and_then(|s| s.parse().ok());

    let (samples, sr) = load_wav_mono(path)?;
    let audio: Vec<Complex32> = samples.iter().map(|&s| Complex32::new(s, 0.0)).collect();

    let (dt, freq) = match (dt_override, freq_override) {
        (Some(dt), Some(f)) => (dt, f),
        _ => {
            println!("searching for strongest signal-energy peak...");
            let (dt, f) = find_signal_peak(&audio, sr);
            println!("  dt = {:.2} s, freq = {:.1} Hz", dt, f);
            (dt, f)
        }
    };

    // Allow tuning the tone-0 anchor. We try several "tone 0 offset" assumptions
    // and pick the one whose argmax sequence is most consistent (lowest entropy
    // at fixed positions across multiple runs). For a single run we just dump
    // the result using our default (tone index 32 at the given freq).
    println!();
    println!("85-symbol tone grid (argmax over 64 bins around {} Hz):", freq);
    let tones = extract_tone_sequence(&audio, sr, dt, freq);
    for row in 0..5 {
        let start = row * 17;
        let end = (start + 17).min(85);
        let segment: Vec<String> = tones[start..end]
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{:>2}:{:>2}", start + i, t))
            .collect();
        println!("  {}", segment.join(" "));
    }

    // Count tone-value frequency. In Q65 the sync uses a specific (small)
    // set of tone values while data spans all 64. A tone that appears at
    // exactly 22 positions is a strong sync-tone candidate.
    println!();
    println!("tone frequency histogram (tone -> count across 85 slots):");
    let mut counts = [0u32; 64];
    for &t in &tones {
        if (t as usize) < 64 {
            counts[t as usize] += 1;
        }
    }
    let mut with_index: Vec<(usize, u32)> = counts.iter().enumerate().map(|(i, &c)| (i, c)).collect();
    with_index.sort_by(|a, b| b.1.cmp(&a.1));
    for &(tone, count) in with_index.iter().take(6) {
        if count > 0 {
            println!("  tone {:>2}: {} slot(s)", tone, count);
        }
    }

    // If the top tone count == 22, we've very likely found the sync tone.
    // Dump its positions.
    if with_index[0].1 == 22 {
        let sync_tone = with_index[0].0;
        let positions: Vec<usize> = tones
            .iter()
            .enumerate()
            .filter(|(_, &t)| t as usize == sync_tone)
            .map(|(i, _)| i)
            .collect();
        println!();
        println!(
            "FOUND probable sync: tone {} appears at exactly 22 positions:",
            sync_tone
        );
        println!("  SYNC_POSITIONS = {:?}", positions);
        println!("  (all 22 sync symbols = tone {})", sync_tone);
    }

    Ok(())
}

fn find_signal_peak(audio: &[Complex32], sr: f64) -> (f64, f64) {
    let p = &Q65_60C;
    let samples_per_sym = (p.tsym_s * sr).round() as usize;
    let mut best = (0.0f32, 0.0f64, 1500.0f64);
    let mut dt = 0.0f64;
    while dt <= 3.0 {
        let mut freq = 200.0f64;
        while freq <= 2500.0 {
            let mut total = 0.0f32;
            for slot in 0..p.num_symbols {
                let start =
                    (dt * sr).round() as isize + (slot as isize) * (samples_per_sym as isize);
                if start < 0 || start as usize + samples_per_sym > audio.len() {
                    continue;
                }
                let s0 = start as usize;
                let mut row_max = 0.0f32;
                for t in 0..64usize {
                    let f = freq + (t as f64 - 32.0) * p.tone_spacing_hz;
                    let omega = 2.0 * std::f64::consts::PI * f / sr;
                    let mut acc = Complex32::new(0.0, 0.0);
                    for k in (0..samples_per_sym).step_by(4) {
                        let phase = -omega * k as f64;
                        let rot = Complex32::new(phase.cos() as f32, phase.sin() as f32);
                        acc += audio[s0 + k] * rot;
                    }
                    let p = acc.re * acc.re + acc.im * acc.im;
                    if p > row_max {
                        row_max = p;
                    }
                }
                total += row_max.sqrt();
            }
            if total > best.0 {
                best = (total, dt, freq);
            }
            freq += 50.0;
        }
        dt += 0.25;
    }
    (best.1, best.2)
}

fn extract_tone_sequence(audio: &[Complex32], sr: f64, dt: f64, freq: f64) -> Vec<u8> {
    let p = &Q65_60C;
    let samples_per_sym = (p.tsym_s * sr).round() as usize;
    let fft_size = samples_per_sym.next_power_of_two().max(8192);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut scratch = vec![Complex32::new(0.0, 0.0); fft_size];
    let mut out = Vec::with_capacity(p.num_symbols);
    let center_bin = (freq * fft_size as f64 / sr).round() as isize;
    let bin_step = p.tone_spacing_hz * fft_size as f64 / sr;
    for slot in 0..p.num_symbols {
        let start = (dt * sr).round() as isize + (slot as isize) * (samples_per_sym as isize);
        if start < 0 || start as usize + samples_per_sym > audio.len() {
            out.push(0);
            continue;
        }
        for s in scratch.iter_mut().take(fft_size) {
            *s = Complex32::new(0.0, 0.0);
        }
        for k in 0..samples_per_sym {
            scratch[k] = audio[start as usize + k];
        }
        fft.process(&mut scratch);
        let mut best = 0u8;
        let mut best_p = -1.0f32;
        for t in 0..64usize {
            let b = center_bin + ((t as isize - 32) as f64 * bin_step).round() as isize;
            if b < 0 || b >= fft_size as isize {
                continue;
            }
            let v = scratch[b as usize];
            let p = v.re * v.re + v.im * v.im;
            if p > best_p {
                best_p = p;
                best = t as u8;
            }
        }
        out.push(best);
    }
    out
}

fn load_wav_mono(path: &str) -> Result<(Vec<f32>, f64), Box<dyn std::error::Error>> {
    let mut f = File::open(path)?;
    let mut hdr = [0u8; 44];
    f.read_exact(&mut hdr)?;
    let channels = u16::from_le_bytes([hdr[22], hdr[23]]) as usize;
    let sr = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]) as f64;
    let bits = u16::from_le_bytes([hdr[34], hdr[35]]);
    if bits != 16 {
        return Err(format!("expected 16-bit PCM, got {} bits", bits).into());
    }
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
