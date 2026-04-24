//! Decode a Q65-60C WAV file via the q65 crate.
//!
//! Usage: cargo run --release --example decode_wav -- path/to/capture.wav

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use q65::{decode, DecodeSearch, Q65_60C};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(1)
        .ok_or("usage: decode_wav <path.wav>")?;

    let (samples, sr) = load_wav_mono(path)?;
    println!("loaded {} samples at {} Hz ({:.2} s)", samples.len(), sr, samples.len() as f64 / sr);

    // Real-valued audio -> complex with imag=0. Mirror image at -f will
    // appear but the decoder searches around +1500 Hz, well separated.
    let audio: Vec<Complex32> = samples.iter().map(|&s| Complex32::new(s, 0.0)).collect();

    // Broad search around the standard WSJT-X audio passband.
    let search = DecodeSearch {
        dt_range_s: 2.5,
        dt_step_s: 0.1,
        freq_center_hz: 1500.0,
        freq_range_hz: 500.0,
        freq_step_hz: 2.0,
        max_decodes: 10,
    };

    println!("running decode...");
    let t0 = std::time::Instant::now();
    let decs = decode(&Q65_60C, &audio, sr, &search)?;
    let elapsed = t0.elapsed();
    println!("decode finished in {:.2} s", elapsed.as_secs_f32());
    println!();

    if decs.is_empty() {
        println!("(no decodes)");
    } else {
        println!("SNR   DT    HZ    MESSAGE");
        for d in &decs {
            println!("{:>4.0} {:>5.2} {:>5.0}  {}", d.snr_db, d.dt_s, d.freq_hz, d.message);
        }
    }

    // Diagnostic: report the top 5 sync-correlation peaks regardless of
    // whether a decode succeeded, so we can tell whether sync is even
    // latching onto the signal.
    println!();
    println!("top sync-correlation peaks (for diagnosis):");
    let peaks = diagnose_sync(&audio, sr, 1500.0, 500.0);
    for (rank, (score, dt, freq)) in peaks.iter().take(5).enumerate() {
        println!(
            "  #{}: score {:>7.1}  dt {:>5.2} s  freq {:>6.1} Hz",
            rank + 1,
            score,
            dt,
            freq
        );
    }

    Ok(())
}

fn load_wav_mono(path: &str) -> Result<(Vec<f32>, f64), Box<dyn std::error::Error>> {
    let mut f = File::open(path)?;
    let mut hdr = [0u8; 44];
    f.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let channels = u16::from_le_bytes([hdr[22], hdr[23]]) as usize;
    let sr = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]) as f64;
    let bits = u16::from_le_bytes([hdr[34], hdr[35]]);
    if bits != 16 {
        return Err(format!("expected 16-bit PCM, got {} bits", bits).into());
    }
    // Walk chunks until we find "data".
    // (Simple WSJT-X WAVs put data right after fmt; keep it simple.)
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

// Coarse sync correlation across (dt, freq) to report top peaks, useful for
// diagnosing whether the sync pattern constants are right.
fn diagnose_sync(
    audio: &[Complex32],
    sr: f64,
    center_hz: f64,
    range_hz: f64,
) -> Vec<(f32, f32, f32)> {
    let p = &Q65_60C;
    let samples_per_sym = (p.tsym_s * sr).round() as usize;
    let sync_pos = q65::sync::SYNC_POSITIONS;
    let sync_tones = q65::sync::SYNC_TONES;
    let mut peaks = Vec::new();
    let mut dt = -2.5f64;
    while dt <= 2.5 {
        let mut df = -range_hz;
        while df <= range_hz {
            let freq = center_hz + df;
            let mut total = 0.0f32;
            for (i, &pos) in sync_pos.iter().enumerate() {
                let start = (dt * sr).round() as isize
                    + (pos as isize) * (samples_per_sym as isize);
                if start < 0 || start as usize + samples_per_sym > audio.len() {
                    continue;
                }
                let tone = sync_tones[i] as f64;
                let f = freq + (tone - 32.0) * p.tone_spacing_hz;
                let omega = 2.0 * std::f64::consts::PI * f / sr;
                let mut acc = Complex32::new(0.0, 0.0);
                for k in 0..samples_per_sym {
                    let phase = -omega * k as f64;
                    let rot = Complex32::new(phase.cos() as f32, phase.sin() as f32);
                    acc += audio[start as usize + k] * rot;
                }
                total += (acc.re * acc.re + acc.im * acc.im).sqrt();
            }
            peaks.push((total, dt as f32, freq as f32));
            df += 5.0;
        }
        dt += 0.2;
    }
    peaks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    peaks
}
