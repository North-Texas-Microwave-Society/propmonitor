mod config;
mod measure;

use anyhow::{anyhow, Context, Result};
use num_complex::Complex32;
use soapysdr::Direction::Rx;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::measure::{SpectrumAnalyzer, FFT_SIZE};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args.get(1).map(String::as_str).unwrap_or("config.yaml");
    let cfg = Config::load(config_path)
        .with_context(|| format!("failed to load config from {}", config_path))?;

    println!(
        "propmonitor: tuning {:.6} MHz, mode {:?}, sr {} Hz",
        cfg.frequency / 1e6,
        cfg.mode,
        cfg.sample_rate as u64
    );

    // Find an SDRplay device.
    let devs = soapysdr::enumerate("driver=sdrplay")
        .context("SoapySDR enumerate failed")?;
    if devs.is_empty() {
        return Err(anyhow!(
            "no SDRplay device found — is the RSP plugged in and SoapySDRPlay installed?"
        ));
    }
    let dev =
        soapysdr::Device::new("driver=sdrplay").context("failed to open SDRplay device")?;

    dev.set_sample_rate(Rx, 0, cfg.sample_rate)
        .context("set_sample_rate failed")?;
    dev.set_frequency(Rx, 0, cfg.frequency, ())
        .context("set_frequency failed")?;

    match cfg.gain {
        Some(g) => {
            dev.set_gain_mode(Rx, 0, false).ok();
            dev.set_gain(Rx, 0, g).context("set_gain failed")?;
        }
        None => {
            // AGC on if supported; otherwise the driver's default gain stands.
            dev.set_gain_mode(Rx, 0, true).ok();
        }
    }

    let mut stream = dev
        .rx_stream::<Complex32>(&[0])
        .context("failed to create rx stream")?;
    stream.activate(None).context("stream activate failed")?;

    let mut analyzer = SpectrumAnalyzer::new(cfg.sample_rate);

    let mtu = stream.mtu().context("stream mtu failed")?;
    let mut scratch = vec![Complex32::new(0.0, 0.0); mtu];

    // Streaming frame buffer: we accumulate samples here until we have
    // exactly FFT_SIZE, then hand them to the analyzer and reset. The
    // analyzer keeps running totals (psd, peak, sum, frame count) for the
    // whole minute — we never buffer the full minute of IQ.
    let mut frame_buf: Vec<Complex32> = Vec::with_capacity(FFT_SIZE);
    const WINDOW: Duration = Duration::from_secs(60);

    loop {
        let window_start = Instant::now();
        analyzer.start_window(cfg.mode);
        frame_buf.clear();

        while window_start.elapsed() < WINDOW {
            // Don't read past the end of the frame; otherwise we'd overflow
            // frame_buf and lose samples.
            let want = (FFT_SIZE - frame_buf.len()).min(scratch.len());
            let n = stream
                .read(&mut [&mut scratch[..want]], 1_000_000)
                .context("stream read failed")?;
            if n == 0 {
                continue;
            }
            frame_buf.extend_from_slice(&scratch[..n]);
            if frame_buf.len() == FFT_SIZE {
                analyzer.add_frame(&frame_buf);
                frame_buf.clear();
            }
        }

        match analyzer.finalize() {
            Some(m) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "{}  noise: {:7.2} dBFS   sig peak/avg: {:7.2} / {:7.2} dBFS   snr peak/avg: {:6.2} / {:6.2} dB",
                    ts,
                    m.noise_dbfs,
                    m.signal_peak_dbfs,
                    m.signal_avg_dbfs,
                    m.snr_peak_db,
                    m.snr_avg_db,
                );
            }
            None => eprintln!("measurement failed (no frames in window)"),
        }
    }
}
