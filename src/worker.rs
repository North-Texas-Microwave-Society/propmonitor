use std::os::raw::{c_char, c_uint};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Note: Instant is used below for the per-window deadline.

use num_complex::Complex32;
use rustfft::FftPlanner;
use soapysdr::Direction::Rx;

use crate::config::{passband_for, Config};
use crate::error::{Context, Error, Result};
use crate::measure::{Measurement, SpectrumAnalyzer, FFT_SIZE};

// Minimal FFI so we can swap SoapySDR's default stderr log handler for a
// no-op. The SDRplay driver chats about devIdx/SerNo/hwVer when a device
// is opened, which is just noise in the headless server logs.
extern "C" {
    fn SoapySDR_registerLogHandler(
        handler: Option<unsafe extern "C" fn(level: c_uint, message: *const c_char)>,
    );
}

unsafe extern "C" fn silent_log_handler(_level: c_uint, _message: *const c_char) {}

fn silence_soapysdr_logs() {
    unsafe { SoapySDR_registerLogHandler(Some(silent_log_handler)) };
}

/// FFT length used by the live waterfall. 1024 bins at 250 kS/s gives ~244
/// Hz/bin — fine enough to see narrow beacon carriers in their passband
/// while still covering the full tuned bandwidth on screen.
pub const WATERFALL_FFT_N: usize = 1024;

/// Standard Hann window of length [`WATERFALL_FFT_N`]. Public for tests
/// so the synthetic-signal path uses the exact same window the worker
/// thread uses.
pub fn waterfall_hann_window() -> Vec<f32> {
    (0..WATERFALL_FFT_N)
        .map(|i| {
            let phase =
                2.0 * std::f32::consts::PI * i as f32 / (WATERFALL_FFT_N - 1) as f32;
            0.5 - 0.5 * phase.cos()
        })
        .collect()
}

/// Linearize a ring buffer of complex samples (oldest first), apply a
/// Hann window, run a forward FFT, and return fftshift'd per-bin power
/// values (|·|²). `wf_pos` is the next-write position in the ring —
/// equivalently, the index of the oldest sample.
pub fn compute_waterfall_bins(
    wf_ring: &[Complex32],
    wf_pos: usize,
    hann: &[f32],
    fft: &dyn rustfft::Fft<f32>,
    scratch: &mut [Complex32],
) -> Vec<f32> {
    debug_assert_eq!(wf_ring.len(), WATERFALL_FFT_N);
    debug_assert_eq!(hann.len(), WATERFALL_FFT_N);

    let mut work: Vec<Complex32> = Vec::with_capacity(WATERFALL_FFT_N);
    for (i, window) in hann.iter().enumerate() {
        let idx = (wf_pos + i) % WATERFALL_FFT_N;
        work.push(wf_ring[idx] * *window);
    }
    fft.process_with_scratch(&mut work, scratch);
    let mut bins: Vec<f32> = Vec::with_capacity(WATERFALL_FFT_N);
    // fftshift: negative-freq half first, then DC + positive.
    for c in &work[WATERFALL_FFT_N / 2..] {
        bins.push(c.norm_sqr());
    }
    for c in &work[..WATERFALL_FFT_N / 2] {
        bins.push(c.norm_sqr());
    }
    bins
}

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    /// A new measurement window just began.
    PeriodStarted,
    /// An integration window finished — the single most important event
    /// produced by the worker.
    PeriodMeasurement(Measurement),
    /// One waterfall row: per-bin power values (linear), the frequency of
    /// bin 0, and the bin width in Hz.
    WaterfallRow {
        bins: Vec<f32>,
        f0_hz: f32,
        bin_hz: f32,
    },
    /// Live RMS of the raw IQ stream in dBFS, sent ~5 Hz.
    RawLevel { dbfs: f64 },
    /// Sent once after the device is fully configured. Carries the values
    /// the device actually accepted, plus the list of available gain
    /// elements — both useful for diagnosing why a config didn't take
    /// effect.
    DeviceInfo {
        actual_sample_rate: f64,
        actual_frequency: f64,
        actual_gain: f64,
        gain_elements: Vec<String>,
    },
    /// Unrecoverable worker error. The bridge logs it and surfaces it via
    /// the status endpoint; the worker thread exits.
    Error(String),
}

/// Open the SDR, run the capture/analyze loop, push events to `tx` until
/// the `stop` flag is set or an error occurs. Errors are reported as
/// `WorkerEvent::Error` so the bridge can log them.
pub fn run_worker(cfg: Config, tx: Sender<WorkerEvent>, stop: Arc<AtomicBool>) {
    if let Err(e) = run_inner(&cfg, &tx, &stop) {
        let _ = tx.send(WorkerEvent::Error(format!("{}", e)));
    }
}

fn open_device(cfg: &Config) -> Result<soapysdr::Device> {
    let args = format!("driver={}", cfg.driver);
    let devs = soapysdr::enumerate(args.as_str())
        .with_context(|| format!("SoapySDR enumerate failed for driver={}", cfg.driver))?;
    if devs.is_empty() {
        return Err(Error::msg(format!(
            "no {} device found — is it plugged in and the Soapy{} module installed?",
            cfg.driver,
            cfg.driver.to_uppercase()
        )));
    }
    soapysdr::Device::new(args.as_str())
        .with_context(|| format!("failed to open device (driver={})", cfg.driver))
}

fn run_inner(cfg: &Config, tx: &Sender<WorkerEvent>, stop: &Arc<AtomicBool>) -> Result<()> {
    silence_soapysdr_logs();

    let dev = open_device(cfg)?;

    dev.set_sample_rate(Rx, 0, cfg.sample_rate)
        .context("set_sample_rate failed")?;
    dev.set_frequency(Rx, 0, cfg.frequency, ())
        .context("set_frequency failed")?;

    if cfg.ppm != 0.0 {
        let _ = dev.set_component_frequency(Rx, 0, "CORR", cfg.ppm, ());
    }

    let gain_elements = dev.list_gains(Rx, 0).unwrap_or_default();

    match cfg.gain {
        Some(g) => {
            // Explicitly take the device out of auto-gain mode. Some drivers
            // ignore set_gain() if AGC is still on, leaving the dongle deaf.
            let _ = dev.set_gain_mode(Rx, 0, false);
            // For RTL-SDR the meaningful stage is "TUNER" (R820T). Use the
            // element setter when we can see that name; fall back to the
            // master set_gain for other drivers.
            if gain_elements.iter().any(|n| n == "TUNER") {
                dev.set_gain_element(Rx, 0, "TUNER", g)
                    .context("set_gain_element TUNER failed")?;
            } else {
                dev.set_gain(Rx, 0, g).context("set_gain failed")?;
            }
        }
        None => {
            let _ = dev.set_gain_mode(Rx, 0, true);
        }
    }

    let actual_sample_rate = dev.sample_rate(Rx, 0).unwrap_or(cfg.sample_rate);
    let actual_frequency = dev.frequency(Rx, 0).unwrap_or(cfg.frequency);
    let actual_gain = dev.gain(Rx, 0).unwrap_or(cfg.gain.unwrap_or(0.0));
    let _ = tx.send(WorkerEvent::DeviceInfo {
        actual_sample_rate,
        actual_frequency,
        actual_gain,
        gain_elements: gain_elements.clone(),
    });

    let mut stream = dev
        .rx_stream::<Complex32>(&[0])
        .context("failed to create rx stream")?;
    stream.activate(None).context("stream activate failed")?;

    let mut analyzer = SpectrumAnalyzer::new(actual_sample_rate);
    let mtu = stream.mtu().context("stream mtu failed")?;
    let mut scratch = vec![Complex32::new(0.0, 0.0); mtu];
    let mut frame_buf: Vec<Complex32> = Vec::with_capacity(FFT_SIZE);

    let (offset_hz, width_hz) = passband_for(cfg.mode, cfg.beacon.as_ref());
    let window_duration = Duration::from_secs(cfg.period_seconds as u64);

    // Waterfall FFT setup.
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(WATERFALL_FFT_N);
    let mut wf_ring: Vec<Complex32> = vec![Complex32::new(0.0, 0.0); WATERFALL_FFT_N];
    let mut wf_pos: usize = 0;
    let mut wf_filled: usize = 0;
    let mut wf_since_emit: usize = 0;
    let wf_emit_every: usize = (actual_sample_rate / 10.0) as usize;
    let mut fft_scratch =
        vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let hann = waterfall_hann_window();
    let bin_hz = (actual_sample_rate / WATERFALL_FFT_N as f64) as f32;
    let f0_hz = -((actual_sample_rate as f32) / 2.0);

    // Raw IQ RMS for the live-level diagnostic — emitted ~5 Hz.
    let mut raw_power_sum: f64 = 0.0;
    let mut raw_power_n: usize = 0;
    let raw_emit_every: usize = (actual_sample_rate / 5.0) as usize;

    while !stop.load(Ordering::Relaxed) {
        let window_start = Instant::now();
        analyzer.start_window(offset_hz, width_hz);
        frame_buf.clear();
        if tx.send(WorkerEvent::PeriodStarted).is_err() {
            return Ok(());
        }

        while window_start.elapsed() < window_duration {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            let want = (FFT_SIZE - frame_buf.len()).min(scratch.len());
            let n = match stream.read(&mut [&mut scratch[..want]], 100_000) {
                Ok(n) => n,
                Err(e) if e.code == soapysdr::ErrorCode::Timeout => continue,
                Err(e) => return Err(Error::msg(format!("stream read failed: {}", e))),
            };
            if n == 0 {
                continue;
            }

            // Raw RMS + waterfall ring fill, sample-by-sample over the
            // chunk we just read.
            for s in &scratch[..n] {
                raw_power_sum += s.norm_sqr() as f64;
                raw_power_n += 1;
                if raw_power_n >= raw_emit_every {
                    let mean = raw_power_sum / raw_power_n as f64;
                    let dbfs = 10.0 * mean.max(1e-30).log10();
                    if tx.send(WorkerEvent::RawLevel { dbfs }).is_err() {
                        return Ok(());
                    }
                    raw_power_sum = 0.0;
                    raw_power_n = 0;
                }

                wf_ring[wf_pos] = *s;
                wf_pos = (wf_pos + 1) % WATERFALL_FFT_N;
                if wf_filled < WATERFALL_FFT_N {
                    wf_filled += 1;
                }
                wf_since_emit += 1;
                if wf_since_emit >= wf_emit_every && wf_filled >= WATERFALL_FFT_N {
                    wf_since_emit = 0;
                    let bins = compute_waterfall_bins(
                        &wf_ring,
                        wf_pos,
                        &hann,
                        &*fft,
                        &mut fft_scratch,
                    );
                    if tx
                        .send(WorkerEvent::WaterfallRow {
                            bins,
                            f0_hz,
                            bin_hz,
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }

            frame_buf.extend_from_slice(&scratch[..n]);

            // Process complete FFT_SIZE frames out of the buffer.
            while frame_buf.len() >= FFT_SIZE {
                let _ = analyzer.add_frame(&frame_buf[..FFT_SIZE]);
                frame_buf.drain(..FFT_SIZE);
            }
        }

        if let Some(m) = analyzer.finalize() {
            if tx.send(WorkerEvent::PeriodMeasurement(m)).is_err() {
                return Ok(());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waterfall_peak_for_tone(sample_rate: f64, tone_hz: f64) -> (usize, f32) {
        let omega = 2.0 * std::f64::consts::PI * tone_hz / sample_rate;
        let samples: Vec<Complex32> = (0..WATERFALL_FFT_N)
            .map(|n| {
                let phase = omega * n as f64;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let hann = waterfall_hann_window();
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(WATERFALL_FFT_N);
        let mut scratch =
            vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let bins = compute_waterfall_bins(&samples, 0, &hann, &*fft, &mut scratch);
        let (peak_idx, _) = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();
        let bin_hz = (sample_rate / WATERFALL_FFT_N as f64) as f32;
        let f0_hz = -((sample_rate as f32) / 2.0);
        let peak_freq = f0_hz + bin_hz * peak_idx as f32;
        (peak_idx, peak_freq)
    }

    #[test]
    fn waterfall_locates_positive_tone_at_expected_bin() {
        let (_idx, freq) = waterfall_peak_for_tone(250_000.0, 1_669.0);
        assert!((freq - 1669.0).abs() < 250.0);
    }

    #[test]
    fn waterfall_locates_negative_tone_at_expected_bin() {
        let (_idx, freq) = waterfall_peak_for_tone(250_000.0, -1_669.0);
        assert!((freq - (-1669.0)).abs() < 250.0);
    }

    #[test]
    fn waterfall_dc_lands_at_center_bin() {
        let (idx, _freq) = waterfall_peak_for_tone(250_000.0, 0.0);
        assert_eq!(idx, WATERFALL_FFT_N / 2);
    }
}
