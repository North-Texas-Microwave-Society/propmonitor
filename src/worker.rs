use std::os::raw::{c_char, c_uint};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use num_complex::Complex32;
use soapysdr::Direction::Rx;

use crate::config::{Config, Mode};
use crate::error::{Context, Error, Result};
use crate::measure::{Measurement, SpectrumAnalyzer, FFT_SIZE};
use crate::timefmt::{utc_seconds_into_minute, LocalHms};
use crate::ui::Q65Row;

// Minimal FFI so we can swap SoapySDR's default stderr log handler for a
// no-op. The SDRplay driver chats about devIdx/SerNo/hwVer when a device
// is opened, and that output corrupts the ratatui alternate screen.
extern "C" {
    fn SoapySDR_registerLogHandler(
        handler: Option<unsafe extern "C" fn(level: c_uint, message: *const c_char)>,
    );
}

unsafe extern "C" fn silent_log_handler(_level: c_uint, _message: *const c_char) {}

fn silence_soapysdr_logs() {
    unsafe { SoapySDR_registerLogHandler(Some(silent_log_handler)) };
}

/// One in every TICK_EVERY frames the worker emits a FrameTick. At the
/// default 2 MS/s with FFT_SIZE=16384 we get ~122 frames/sec, so dividing
/// by 12 gives the UI ~10 updates/sec for the live activity bar.
const TICK_EVERY: usize = 12;

const WINDOW: Duration = Duration::from_secs(60);

pub enum WorkerEvent {
    /// A new measurement window (analog) or T/R period (Q65) just began.
    WindowStarted { at: Instant },
    /// Per-frame in-band power, sent ~10 Hz. Analog modes only.
    FrameTick { in_band_dbfs: f64 },
    /// A 60-second analog measurement window finished.
    WindowComplete(Measurement),
    /// A Q65 T/R period finished and produced zero or more decodes.
    Q65Decodes(Vec<Q65Row>),
    /// Unrecoverable worker error. The UI shows the message and exits.
    Error(String),
}

/// Open the SDR, run the capture/analyze loop, push events to `tx` until
/// the `stop` flag is set or an error occurs. Errors are reported as
/// `WorkerEvent::Error` so the UI can display them after restoring the
/// terminal — this function never returns an error directly.
pub fn run_worker(cfg: Config, tx: Sender<WorkerEvent>, stop: Arc<AtomicBool>) {
    if let Err(e) = run_inner(&cfg, &tx, &stop) {
        let _ = tx.send(WorkerEvent::Error(format!("{}", e)));
    }
}

fn run_inner(cfg: &Config, tx: &Sender<WorkerEvent>, stop: &Arc<AtomicBool>) -> Result<()> {
    silence_soapysdr_logs();

    if cfg.mode == Mode::Q65 {
        return run_q65(cfg, tx, stop);
    }

    let devs = soapysdr::enumerate("driver=sdrplay").context("SoapySDR enumerate failed")?;
    if devs.is_empty() {
        return Err(Error::msg(
            "no SDRplay device found — is the RSP plugged in and SoapySDRPlay installed?",
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
    let mut frame_buf: Vec<Complex32> = Vec::with_capacity(FFT_SIZE);

    while !stop.load(Ordering::Relaxed) {
        let window_start = Instant::now();
        analyzer.start_window(cfg.mode);
        frame_buf.clear();
        let mut frames_in_window: usize = 0;
        if tx
            .send(WorkerEvent::WindowStarted { at: window_start })
            .is_err()
        {
            return Ok(());
        }

        while window_start.elapsed() < WINDOW {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            let want = (FFT_SIZE - frame_buf.len()).min(scratch.len());
            // Short timeout so we re-check the stop flag and the window
            // deadline frequently rather than blocking for a full second.
            let n = match stream.read(&mut [&mut scratch[..want]], 100_000) {
                Ok(n) => n,
                Err(e) if e.code == soapysdr::ErrorCode::Timeout => continue,
                Err(e) => return Err(Error::msg(format!("stream read failed: {}", e))),
            };
            if n == 0 {
                continue;
            }
            frame_buf.extend_from_slice(&scratch[..n]);

            if frame_buf.len() == FFT_SIZE {
                let frame_power = analyzer.add_frame(&frame_buf);
                frame_buf.clear();
                frames_in_window += 1;

                if frames_in_window.is_multiple_of(TICK_EVERY) {
                    let dbfs = 10.0 * frame_power.max(1e-30).log10();
                    if tx.send(WorkerEvent::FrameTick { in_band_dbfs: dbfs }).is_err() {
                        return Ok(());
                    }
                }
            }
        }

        if let Some(m) = analyzer.finalize() {
            if tx.send(WorkerEvent::WindowComplete(m)).is_err() {
                return Ok(());
            }
        }
    }

    Ok(())
}

// ---------------- Q65-60C live capture path ----------------------------
//
// Pipeline: IQ at `cfg.sample_rate` -> shift to baseband at the Q65 audio
// offset -> decimate to 12 kHz complex -> fill a 64-second period buffer
// aligned to UTC minute boundaries -> hand to q65::decode each period.
//
// STATUS: plumbed. The decoder itself (q65 crate) has a provisional sync
// pattern and a placeholder OSD soft decoder rather than full Koetter-
// Vardy, so live-RF sensitivity will be well below WSJT-X's jt9. See
// IMPLEMENTATION_PLAN.md for the remaining work.

fn run_q65(cfg: &Config, tx: &Sender<WorkerEvent>, stop: &Arc<AtomicBool>) -> Result<()> {
    let q = cfg
        .q65
        .as_ref()
        .ok_or_else(|| Error::msg("mode q65 but no q65: config block present"))?;

    let devs = soapysdr::enumerate("driver=sdrplay").context("SoapySDR enumerate failed")?;
    if devs.is_empty() {
        return Err(Error::msg(
            "no SDRplay device found — is the RSP plugged in and SoapySDRPlay installed?",
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
            dev.set_gain_mode(Rx, 0, true).ok();
        }
    }

    let mut stream = dev
        .rx_stream::<Complex32>(&[0])
        .context("failed to create rx stream")?;
    stream.activate(None).context("stream activate failed")?;

    let mtu = stream.mtu().context("stream mtu failed")?;
    let mut scratch = vec![Complex32::new(0.0, 0.0); mtu];

    // Decimation: pick a target rate near 12 kHz that divides the source rate
    // evenly. For 2_000_000 the nearest integer divisor near 12 kHz is 166
    // (~12.048 kHz) — acceptable for MVP.
    let decim = (cfg.sample_rate / 12_000.0).round() as usize;
    let audio_sr = cfg.sample_rate / decim as f64;

    // Mixer state: rotate IQ by -2πf/fs per sample where f = dial offset of the
    // Q65 audio-center. The dial is tuned to cfg.frequency; the signal sits at
    // cfg.frequency + (audio_center_hz - 1500) in IF terms. Treat the SDR IF as
    // zero-IF and just shift the 0 Hz of IQ to the audio-center.
    let mix_freq = q.audio_center_hz - 1_500.0;
    let mut mix_phase: f64 = 0.0;
    let mix_dphase = -2.0 * std::f64::consts::PI * mix_freq / cfg.sample_rate;

    // Period buffer sized for 64 s of complex audio at `audio_sr` (4 s margin
    // over the 60 s T/R period for DT search).
    let period_samples = (audio_sr * 64.0).ceil() as usize;
    let mut audio_buf: Vec<Complex32> = Vec::with_capacity(period_samples);

    // State for the running decimator.
    let mut decim_counter: usize = 0;
    let mut decim_accum = Complex32::new(0.0, 0.0);

    // Align first period to the next UTC minute boundary. Seconds-since-minute
    // > 0 means we skip the current partial period.
    let secs_past = utc_seconds_into_minute();
    let mut next_period_start_wall = Instant::now()
        + Duration::from_secs_f64((60.0 - secs_past).max(0.1));
    // Drop incoming samples until we cross into the next period.
    let mut buffering = false;

    let tick = Duration::from_millis(100);

    while !stop.load(Ordering::Relaxed) {
        // If we just passed the period start, announce it and begin buffering.
        if !buffering && Instant::now() >= next_period_start_wall {
            let _ = tx.send(WorkerEvent::WindowStarted {
                at: Instant::now(),
            });
            audio_buf.clear();
            decim_counter = 0;
            decim_accum = Complex32::new(0.0, 0.0);
            buffering = true;
        }

        // Read a chunk.
        let n = match stream.read(&mut [&mut scratch[..]], 100_000) {
            Ok(n) => n,
            Err(e) if e.code == soapysdr::ErrorCode::Timeout => continue,
            Err(e) => return Err(Error::msg(format!("stream read failed: {}", e))),
        };

        if buffering && n > 0 {
            for s in &scratch[..n] {
                // Mix down.
                let (si, co) = mix_phase.sin_cos();
                let rot = Complex32::new(co as f32, si as f32);
                mix_phase += mix_dphase;
                if mix_phase > std::f64::consts::TAU {
                    mix_phase -= std::f64::consts::TAU;
                } else if mix_phase < -std::f64::consts::TAU {
                    mix_phase += std::f64::consts::TAU;
                }
                let mixed = *s * rot;
                decim_accum += mixed;
                decim_counter += 1;
                if decim_counter >= decim {
                    let avg =
                        decim_accum * Complex32::new(1.0 / decim as f32, 0.0);
                    audio_buf.push(avg);
                    decim_accum = Complex32::new(0.0, 0.0);
                    decim_counter = 0;
                    if audio_buf.len() >= period_samples {
                        // One period's worth in the bag — decode and reset.
                        let decodes = decode_q65_period(q, &audio_buf, audio_sr);
                        let _ = tx.send(WorkerEvent::Q65Decodes(decodes));
                        audio_buf.clear();
                        buffering = false;
                        next_period_start_wall += Duration::from_secs(60);
                        break;
                    }
                }
            }
        }

        // Give the stop flag a chance to be seen.
        std::thread::sleep(Duration::ZERO);
        let _ = tick;
    }

    Ok(())
}

fn decode_q65_period(
    q: &crate::config::Q65Config,
    audio: &[Complex32],
    audio_sr: f64,
) -> Vec<Q65Row> {
    let search = q65::DecodeSearch {
        dt_range_s: 2.0,
        dt_step_s: 0.2,
        freq_center_hz: q.audio_center_hz as f32,
        freq_range_hz: q.audio_search_hz as f32,
        freq_step_hz: 2.0,
        max_decodes: q.max_decodes_per_period,
    };
    let now = LocalHms::now();
    match q65::decode(&q65::Q65_60C, audio, audio_sr, &search) {
        Ok(decs) => decs
            .into_iter()
            .map(|d| Q65Row {
                at: now,
                snr_db: d.snr_db,
                dt_s: d.dt_s,
                freq_hz: d.freq_hz,
                message: d.message,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
