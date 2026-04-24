//! Quick-and-dirty spectrogram of a WAV file, printed as text: rows are time
//! (1 s each), columns are frequency bins. '#' = strong energy, '.' = quiet.

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use rustfft::FftPlanner;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).ok_or("usage: spectrogram <path.wav>")?;
    let (samples, sr) = load_wav_mono(path)?;
    println!("loaded {} samples at {} Hz ({:.2} s)", samples.len(), sr, samples.len() as f64 / sr);

    // One column per 30 Hz bin, 0 to 3000 Hz = 100 columns.
    // One row per 1 s window.
    let win = sr as usize; // 12000 samples = 1 s
    let fft_size = 8192usize;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);

    let bin_hz = sr / fft_size as f64;
    let col_hz = 30.0;
    let n_cols = 100usize;
    let col_bins = (col_hz / bin_hz) as usize + 1;

    let n_rows = samples.len() / win;
    let mut grid = vec![0.0f32; n_rows * n_cols];

    for row in 0..n_rows {
        let mut buf = vec![Complex32::new(0.0, 0.0); fft_size];
        let start = row * win;
        let take = fft_size.min(samples.len() - start);
        for i in 0..take {
            buf[i] = Complex32::new(samples[start + i], 0.0);
        }
        fft.process(&mut buf);
        for col in 0..n_cols {
            let center_bin = (col as f64 * col_hz / bin_hz) as usize;
            let mut s = 0.0f32;
            let bin_start = center_bin.saturating_sub(col_bins / 2);
            let bin_end = (center_bin + col_bins / 2).min(fft_size / 2);
            for &v in &buf[bin_start..bin_end] {
                s += v.re * v.re + v.im * v.im;
            }
            grid[row * n_cols + col] = s;
        }
    }

    // Normalize to 0..1 per row, then threshold.
    let mut row_max = vec![0.0f32; n_rows];
    for row in 0..n_rows {
        let mut m = 0.0f32;
        for col in 0..n_cols {
            m = m.max(grid[row * n_cols + col]);
        }
        row_max[row] = m;
    }
    let global_max = row_max.iter().cloned().fold(0.0f32, f32::max);

    println!();
    println!("Spectrogram (30 Hz/col, 1 s/row, 0..3000 Hz):");
    println!(
        "    {}",
        (0..n_cols)
            .map(|c| {
                let hz = (c as f64 * col_hz) as usize;
                if hz.is_multiple_of(500) { "|" } else { " " }
            })
            .collect::<String>()
    );
    print!("    ");
    for c in 0..n_cols {
        let hz = (c as f64 * col_hz) as usize;
        if hz.is_multiple_of(500) {
            print!("{:<1}", hz / 1000);
        } else {
            print!(" ");
        }
    }
    println!();

    for row in 0..n_rows {
        let s: String = (0..n_cols)
            .map(|col| {
                let v = grid[row * n_cols + col];
                let r = (v / global_max).sqrt();
                if r > 0.5 {
                    '#'
                } else if r > 0.2 {
                    '+'
                } else if r > 0.05 {
                    '.'
                } else {
                    ' '
                }
            })
            .collect();
        println!("{:>3}s {}", row, s);
    }

    // Report the column (= frequency) that has the most energy overall.
    let mut col_totals = vec![0.0f32; n_cols];
    for col in 0..n_cols {
        for row in 0..n_rows {
            col_totals[col] += grid[row * n_cols + col];
        }
    }
    let best_col = col_totals
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .unwrap()
        .0;
    println!();
    println!(
        "strongest column: col {} -> ~{:.0} Hz",
        best_col,
        best_col as f64 * col_hz
    );

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
