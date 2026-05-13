//! Standalone diagnostic. Opens an SDRplay (or any SoapySDR device per
//! args), streams a configurable amount of IQ, runs an FFT identical to
//! propmonitor's waterfall path, and prints spectral findings to stdout.
//!
//! Goal: figure out — without going through the main app — why a signal
//! at the expected offset isn't showing up in the waterfall. If this tool
//! reproduces the same anomaly, we've isolated the bug to the SDR-device
//! handling layer (not the web UI or the DSP, which have unit tests).
//!
//! Usage:
//!   sdr_diag                                    # use defaults
//!   sdr_diag --freq 144500000 --rate 250000     # override
//!   sdr_diag --iqcorr false                     # disable IQ correction
//!   sdr_diag --duration 3                       # capture seconds

use std::time::{Duration, Instant};

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use soapysdr::Direction::Rx;

const FFT_N: usize = 1024;

struct Args {
    driver: String,
    freq: f64,
    rate: f64,
    gain: f64,
    duration_s: f64,
    iqcorr: Option<bool>,
    dc_removal: Option<bool>,
    bandwidth: Option<f64>,
    agc_mode: bool,
    extra_device_args: String,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            driver: "sdrplay".into(),
            freq: 144_500_000.0,
            rate: 250_000.0,
            gain: 40.0,
            duration_s: 3.0,
            iqcorr: None,
            dc_removal: None,
            bandwidth: None,
            agc_mode: false,
            extra_device_args: String::new(),
        }
    }
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--driver" => a.driver = it.next().unwrap(),
            "--freq" => a.freq = it.next().unwrap().parse().unwrap(),
            "--rate" => a.rate = it.next().unwrap().parse().unwrap(),
            "--gain" => a.gain = it.next().unwrap().parse().unwrap(),
            "--duration" => a.duration_s = it.next().unwrap().parse().unwrap(),
            "--iqcorr" => a.iqcorr = Some(it.next().unwrap().parse().unwrap()),
            "--dc-removal" => a.dc_removal = Some(it.next().unwrap().parse().unwrap()),
            "--bandwidth" => a.bandwidth = Some(it.next().unwrap().parse().unwrap()),
            "--agc" => a.agc_mode = it.next().unwrap().parse().unwrap(),
            "--extra-args" => a.extra_device_args = it.next().unwrap(),
            other => panic!("unknown arg {other}"),
        }
    }
    a
}

fn hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let phase = 2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32;
            0.5 - 0.5 * phase.cos()
        })
        .collect()
}

fn compute_bins(
    ring: &[Complex32],
    pos: usize,
    hann: &[f32],
    fft: &dyn Fft<f32>,
    scratch: &mut [Complex32],
) -> Vec<f32> {
    let mut work: Vec<Complex32> = Vec::with_capacity(FFT_N);
    for i in 0..FFT_N {
        let idx = (pos + i) % FFT_N;
        work.push(ring[idx] * hann[i]);
    }
    fft.process_with_scratch(&mut work, scratch);
    let mut bins = Vec::with_capacity(FFT_N);
    for c in &work[FFT_N / 2..] {
        bins.push(c.norm_sqr());
    }
    for c in &work[..FFT_N / 2] {
        bins.push(c.norm_sqr());
    }
    bins
}

fn bin_to_hz(bin: usize, f0_hz: f32, bin_hz: f32) -> f32 {
    f0_hz + bin_hz * bin as f32
}

fn main() {
    let args = parse_args();

    let mut device_args = format!("driver={}", args.driver);
    if !args.extra_device_args.is_empty() {
        device_args.push(',');
        device_args.push_str(&args.extra_device_args);
    }
    if let Some(iqcorr) = args.iqcorr {
        device_args.push_str(&format!(",iqcorr_ctrl={iqcorr}"));
    }

    eprintln!("# device args: {device_args}");
    let dev =
        soapysdr::Device::new(device_args.as_str()).expect("failed to open device");

    dev.set_sample_rate(Rx, 0, args.rate)
        .expect("set_sample_rate failed");
    dev.set_frequency(Rx, 0, args.freq, ())
        .expect("set_frequency failed");

    if let Some(bw) = args.bandwidth {
        dev.set_bandwidth(Rx, 0, bw)
            .expect("set_bandwidth failed");
    }

    if let Some(_dc) = args.dc_removal {
        let _ = dev.set_dc_offset_mode(Rx, 0, args.dc_removal.unwrap_or(true));
    }

    if args.agc_mode {
        let _ = dev.set_gain_mode(Rx, 0, true);
    } else {
        let _ = dev.set_gain_mode(Rx, 0, false);
        // Try TUNER element first (RTL-SDR); fall back to master.
        let gain_elements = dev.list_gains(Rx, 0).unwrap_or_default();
        if gain_elements.iter().any(|n| n == "TUNER") {
            dev.set_gain_element(Rx, 0, "TUNER", args.gain)
                .expect("set_gain_element failed");
        } else {
            dev.set_gain(Rx, 0, args.gain).expect("set_gain failed");
        }
    }

    eprintln!(
        "# actual: freq={:.0} Hz, rate={:.0} Hz, gain={:.1} dB, gains={:?}",
        dev.frequency(Rx, 0).unwrap_or(0.0),
        dev.sample_rate(Rx, 0).unwrap_or(0.0),
        dev.gain(Rx, 0).unwrap_or(0.0),
        dev.list_gains(Rx, 0).unwrap_or_default(),
    );
    eprintln!(
        "# bandwidth={:.0} Hz, antenna={:?}, antennas={:?}",
        dev.bandwidth(Rx, 0).unwrap_or(0.0),
        dev.antenna(Rx, 0).unwrap_or_default(),
        dev.antennas(Rx, 0).unwrap_or_default(),
    );

    let mut stream = dev
        .rx_stream::<Complex32>(&[0])
        .expect("failed to create rx stream");
    stream.activate(None).expect("stream activate failed");

    let mtu = stream.mtu().expect("stream mtu failed");
    let mut scratch_iq = vec![Complex32::new(0.0, 0.0); mtu];

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_N);
    let mut fft_scratch =
        vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let hann = hann_window(FFT_N);

    let mut ring = vec![Complex32::new(0.0, 0.0); FFT_N];
    let mut ring_pos: usize = 0;
    let mut ring_filled: usize = 0;
    let mut samples_since_fft: usize = 0;
    let fft_interval: usize = (args.rate / 10.0) as usize;
    let bin_hz = (args.rate / FFT_N as f64) as f32;
    let f0_hz = -((args.rate as f32) / 2.0);

    // Per-bin max-hold across the whole capture window.
    let mut max_hold: Vec<f32> = vec![0.0; FFT_N];
    let mut total_iq_power: f64 = 0.0;
    let mut total_iq_n: usize = 0;
    let mut fft_rows: usize = 0;

    // Per-second RMS bucket so we can spot TX/RX cycling.
    let sec_samples_target: usize = args.rate as usize;
    let mut sec_power: f64 = 0.0;
    let mut sec_n: usize = 0;
    let mut sec_idx: usize = 0;

    let start = Instant::now();
    let target = Duration::from_secs_f64(args.duration_s);
    while start.elapsed() < target {
        let n = match stream.read(&mut [&mut scratch_iq[..]], 100_000) {
            Ok(n) => n,
            Err(e) if e.code == soapysdr::ErrorCode::Timeout => continue,
            Err(e) => {
                eprintln!("# stream read error: {e}");
                break;
            }
        };
        for s in &scratch_iq[..n] {
            let p = s.norm_sqr() as f64;
            total_iq_power += p;
            total_iq_n += 1;
            sec_power += p;
            sec_n += 1;
            if sec_n >= sec_samples_target {
                let db = 10.0 * (sec_power / sec_n as f64).max(1e-30).log10();
                eprintln!("# t={sec_idx:3}s rms={db:+6.2} dBFS");
                sec_power = 0.0;
                sec_n = 0;
                sec_idx += 1;
            }
            ring[ring_pos] = *s;
            ring_pos = (ring_pos + 1) % FFT_N;
            if ring_filled < FFT_N {
                ring_filled += 1;
            }
            samples_since_fft += 1;
            if samples_since_fft >= fft_interval && ring_filled >= FFT_N {
                samples_since_fft = 0;
                let bins =
                    compute_bins(&ring, ring_pos, &hann, &*fft, &mut fft_scratch);
                for (i, &p) in bins.iter().enumerate() {
                    if p > max_hold[i] {
                        max_hold[i] = p;
                    }
                }
                fft_rows += 1;
            }
        }
    }

    println!("# captured: rows={fft_rows} duration={:.1}s", start.elapsed().as_secs_f32());
    let rms_dbfs = 10.0 * (total_iq_power / total_iq_n.max(1) as f64).max(1e-30).log10();
    println!("# raw RMS: {rms_dbfs:+.2} dBFS over {total_iq_n} samples");

    // Find DC bin (must be center for our convention)
    let dc_bin = FFT_N / 2;
    let dc_pow = max_hold[dc_bin];
    let dc_db = 10.0 * dc_pow.max(1e-30).log10();
    println!(
        "# DC bin (center {dc_bin}, freq {:.0} Hz): max power {dc_db:+.2} dB",
        bin_to_hz(dc_bin, f0_hz, bin_hz)
    );

    // Floor estimate (median of max-hold)
    let mut sorted = max_hold.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor_db = 10.0 * sorted[FFT_N / 2].max(1e-30).log10();
    println!("# median (noise-floor estimate): {floor_db:+.2} dB");

    // Top peaks
    let mut indexed: Vec<(usize, f32)> = max_hold.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    println!("# top 8 peaks (excluding DC ±5 bins):");
    let mut shown = 0;
    for (idx, pow) in indexed {
        if (idx as isize - dc_bin as isize).abs() <= 5 {
            continue;
        }
        let db = 10.0 * pow.max(1e-30).log10();
        let above_floor = db - floor_db;
        let freq = bin_to_hz(idx, f0_hz, bin_hz);
        println!(
            "    bin {:4}  freq {:+9.1} Hz   {:+6.1} dB  ({:+5.1} dB over floor)",
            idx, freq, db, above_floor
        );
        shown += 1;
        if shown >= 8 {
            break;
        }
    }

    // Print bins in a narrow band around DC for fine-grained inspection
    println!("# bins near DC (±20 of center), max-hold dB above floor:");
    for offset in (-20i32..=20).step_by(2) {
        let idx = (dc_bin as i32 + offset) as usize;
        let db = 10.0 * max_hold[idx].max(1e-30).log10();
        let above = db - floor_db;
        let freq = bin_to_hz(idx, f0_hz, bin_hz);
        let bar = "#".repeat(((above / 2.0).max(0.0) as usize).min(40));
        println!(
            "    bin {:4}  freq {:+8.0} Hz   {:+5.1} dB  {bar}",
            idx, freq, above
        );
    }

    let _ = stream.deactivate(None);
}
