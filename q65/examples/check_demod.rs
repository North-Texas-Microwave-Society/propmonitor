//! Demodulate a clean q65sim WAV and verify we recover the known codeword.

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use q65::demod::{demodulate, FineSync};
use q65::kv::hard_from_reliability;
use q65::Q65_60C;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).ok_or("usage: check_demod <path.wav>")?;
    let (samples, sr) = load_wav_mono(path)?;
    let audio: Vec<Complex32> = samples.iter().map(|&s| Complex32::new(s, 0.0)).collect();

    let expected: [u8; 63] = [
        2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18,
        42, 63, 28, 8, 23, 17, 17, 8, 38, 37, 22, 31, 17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56, 57,
        43, 21, 7, 54, 45, 59, 12, 12, 3, 6, 3, 40, 8, 10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7, 50,
        19,
    ];

    let mut best = (0usize, 0.0f32, 0.0f32);
    let mut dt = 0.0f32;
    while dt <= 2.0 {
        let mut f = 1700.0f32;
        while f <= 1720.0 {
            let fine = FineSync { dt_s: dt, freq_hz: f };
            let rel = demodulate(&Q65_60C, &audio, sr, fine);
            let got = hard_from_reliability(&rel);
            let n: usize = got.iter().zip(expected.iter()).filter(|(a, b)| a == b).count();
            if n > best.0 {
                best = (n, dt, f);
            }
            f += 0.1;
        }
        dt += 0.005;
    }
    println!(
        "best: {}/63 matches at dt={:.3} s, freq={:.2} Hz",
        best.0, best.1, best.2
    );

    let fine = FineSync { dt_s: best.1, freq_hz: best.2 };
    let rel = demodulate(&Q65_60C, &audio, sr, fine);
    let got = hard_from_reliability(&rel);
    println!("\nfirst 20 expected: {:?}", &expected[..20]);
    println!("first 20 got:      {:?}", &got[..20]);
    Ok(())
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
        if &tag == b"data" { data_size = size as usize; break; }
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
