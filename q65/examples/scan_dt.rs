//! Scan DT at a fixed frequency, reporting how many of the 63 codeword
//! symbols the demodulator's argmax recovers correctly. Finds the sub-symbol
//! DT where the signal is actually aligned.

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use q65::sync::data_positions;
use q65::Q65_60C;
use rustfft::FftPlanner;

const EXPECTED: [u8; 63] = [
    2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18, 42, 63, 28, 8, 23, 17, 17, 8, 38, 37, 22, 31,
    17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56, 57, 43, 21, 7, 54, 45, 59, 12, 12, 3, 6, 3, 40, 8,
    10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7, 50, 19,
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).ok_or("usage: scan_dt <path.wav> [freq_hz]")?;
    let freq: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1710.0);

    let (samples, sr) = load_wav_mono(path)?;
    let audio: Vec<Complex32> = samples.iter().map(|&s| Complex32::new(s, 0.0)).collect();

    println!("scanning dt for freq={} Hz", freq);
    let mut best = (0u32, 0.0f64);
    let mut dt = 0.0f64;
    while dt <= 2.5 {
        let n = count_correct(&audio, sr, dt, freq);
        if n > best.0 {
            best = (n, dt);
        }
        if n >= 10 {
            println!("  dt={:.3} s -> {}/63 correct", dt, n);
        }
        dt += 0.005;
    }
    println!("best: dt={:.3} s, {}/63 correct", best.1, best.0);
    Ok(())
}

fn count_correct(audio: &[Complex32], sr: f64, dt: f64, freq_tone0: f64) -> u32 {
    let p = &Q65_60C;
    let samples_per_sym = (p.tsym_s * sr).round() as usize;
    let fft_size = samples_per_sym.next_power_of_two().max(8192);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut scratch = vec![Complex32::new(0.0, 0.0); fft_size];

    let center_bin = (freq_tone0 * fft_size as f64 / sr).round() as isize;
    let bin_step = p.tone_spacing_hz * fft_size as f64 / sr;

    let data_pos = data_positions();
    let mut n_correct = 0u32;

    for (rs_idx, &slot) in data_pos.iter().enumerate() {
        let start = (dt * sr).round() as isize + (slot as isize) * (samples_per_sym as isize);
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
        // Find argmax over the 64 data tones. tone = v+1, freq = f0 - tone*spacing.
        let mut best = 0u8;
        let mut best_p = -1.0f32;
        for v in 0..64 {
            let tone_idx = (v + 1) as f64;
            // Tones go UPWARD in freq: audio_freq(tone) = f_tone0 + tone * spacing.
            let b_signed = center_bin + (tone_idx * bin_step).round() as isize;
            if b_signed < 0 || b_signed >= fft_size as isize {
                continue;
            }
            let z = scratch[b_signed as usize];
            let p = z.re * z.re + z.im * z.im;
            if p > best_p {
                best_p = p;
                best = v as u8;
            }
        }
        if best == EXPECTED[rs_idx] {
            n_correct += 1;
        }
    }
    n_correct
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
