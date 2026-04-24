use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use crate::config::Mode;

pub const FFT_SIZE: usize = 16_384;

#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    pub noise_dbfs: f64,
    /// Strongest single-frame in-band power across the capture window.
    /// This is what you want for keyed signals (CW beacons, SSB voice peaks)
    /// where averaging across silent frames would understate the signal.
    pub signal_peak_dbfs: f64,
    /// Mean in-band power across all frames in the capture window.
    /// This is the time-averaged signal level — useful for continuous
    /// carriers (FM, AM broadcasts) and for understanding duty cycle.
    pub signal_avg_dbfs: f64,
    pub snr_peak_db: f64,
    pub snr_avg_db: f64,
}

pub struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Sum of window samples squared — used to normalize the periodogram.
    window_energy: f64,
    sample_rate: f64,

    // Streaming accumulator state. `start_window` resets these and locks in
    // a mode; `add_frame` updates them; `finalize` consumes them and
    // produces a Measurement.
    accum_mode: Option<Mode>,
    sig_lo: usize,
    sig_hi: usize,
    half_width_bins: usize,
    fft_buf: Vec<Complex32>,
    psd_sum: Vec<f64>,
    sig_peak_power: f64,
    sig_sum_power: f64,
    frame_count: usize,
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: f64) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);

        // Hann window
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|n| {
                let x = std::f32::consts::PI * 2.0 * n as f32 / (FFT_SIZE as f32 - 1.0);
                0.5 * (1.0 - x.cos())
            })
            .collect();
        let window_energy: f64 = window.iter().map(|&w| (w as f64) * (w as f64)).sum();

        Self {
            fft,
            window,
            window_energy,
            sample_rate,
            accum_mode: None,
            sig_lo: 0,
            sig_hi: 0,
            half_width_bins: 0,
            fft_buf: vec![Complex32::new(0.0, 0.0); FFT_SIZE],
            psd_sum: vec![0.0f64; FFT_SIZE],
            sig_peak_power: 0.0,
            sig_sum_power: 0.0,
            frame_count: 0,
        }
    }

    /// Begin a new measurement window. Resets all accumulators and
    /// pre-computes the signal-bin range for `mode`.
    pub fn start_window(&mut self, mode: Mode) {
        let (offset_hz, width_hz) = mode.passband();
        let bin_hz = self.sample_rate / FFT_SIZE as f64;
        let center_bin = FFT_SIZE as isize / 2;
        let offset_bins = (offset_hz / bin_hz).round() as isize;
        let half_width_bins = ((width_hz / bin_hz) / 2.0).round().max(1.0) as isize;

        self.sig_lo = (center_bin + offset_bins - half_width_bins).max(0) as usize;
        self.sig_hi = (center_bin + offset_bins + half_width_bins)
            .min(FFT_SIZE as isize - 1) as usize;
        self.half_width_bins = half_width_bins as usize;

        for p in self.psd_sum.iter_mut() {
            *p = 0.0;
        }
        self.sig_peak_power = 0.0;
        self.sig_sum_power = 0.0;
        self.frame_count = 0;
        self.accum_mode = Some(mode);
    }

    /// Process exactly one FFT_SIZE-sample frame. The signal-bin range from
    /// `start_window` is used to track per-frame in-band peak and mean.
    /// Returns the just-processed frame's in-band power (linear, normalized
    /// the same way as `signal_avg_dbfs`/`signal_peak_dbfs`) so callers can
    /// drive a live activity meter without re-running the FFT.
    pub fn add_frame(&mut self, frame: &[Complex32]) -> f64 {
        debug_assert_eq!(frame.len(), FFT_SIZE);
        debug_assert!(self.accum_mode.is_some(), "start_window not called");

        for (i, &s) in frame.iter().enumerate() {
            let w = self.window[i];
            self.fft_buf[i] = Complex32::new(s.re * w, s.im * w);
        }
        self.fft.process(&mut self.fft_buf);

        // fftshift on the fly. Accumulate this frame's in-band signal power
        // so we can track peak and mean.
        let mut frame_sig_power = 0.0f64;
        for k in 0..FFT_SIZE {
            let shifted = (k + FFT_SIZE / 2) % FFT_SIZE;
            let v = self.fft_buf[shifted];
            let bin_power = (v.re as f64) * (v.re as f64) + (v.im as f64) * (v.im as f64);
            self.psd_sum[k] += bin_power;
            if k >= self.sig_lo && k <= self.sig_hi {
                frame_sig_power += bin_power;
            }
        }
        let frame_sig_power = frame_sig_power / self.window_energy;
        if frame_sig_power > self.sig_peak_power {
            self.sig_peak_power = frame_sig_power;
        }
        self.sig_sum_power += frame_sig_power;
        self.frame_count += 1;
        frame_sig_power
    }

    /// Finalize the current measurement window and return the result.
    /// Clears `accum_mode` so a stale window can't be accidentally reused
    /// without calling `start_window` again.
    pub fn finalize(&mut self) -> Option<Measurement> {
        if self.frame_count == 0 || self.accum_mode.is_none() {
            self.accum_mode = None;
            return None;
        }
        if self.sig_hi <= self.sig_lo {
            self.accum_mode = None;
            return None;
        }
        let signal_bin_count = self.sig_hi - self.sig_lo + 1;
        let frames = self.frame_count;

        // Time-average and normalize the periodogram for noise estimation.
        // (Done into a local copy so we don't trash psd_sum if the caller
        // wants to inspect it later — and so a future start_window can
        // re-zero in place.)
        let avg_norm = 1.0 / (frames as f64 * self.window_energy);
        let psd_avg: Vec<f64> = self.psd_sum.iter().map(|p| p * avg_norm).collect();

        // Noise floor: median of out-of-passband bins from the averaged
        // PSD. Guard region on each side avoids spectral leakage from a
        // strong in-band signal poisoning the estimate.
        let guard = self.half_width_bins;
        let guard_lo = self.sig_lo.saturating_sub(guard);
        let guard_hi = (self.sig_hi + guard).min(FFT_SIZE - 1);

        let mut noise_bins: Vec<f64> = psd_avg
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < guard_lo || *i > guard_hi)
            .map(|(_, &p)| p)
            .collect();
        if noise_bins.is_empty() {
            self.accum_mode = None;
            return None;
        }
        noise_bins.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let noise_per_bin = noise_bins[noise_bins.len() / 2];
        let noise_power = (noise_per_bin * signal_bin_count as f64).max(1e-30);

        let sig_avg_power = (self.sig_sum_power / frames as f64).max(1e-30);
        let sig_peak_power = self.sig_peak_power.max(1e-30);

        self.accum_mode = None;

        Some(Measurement {
            noise_dbfs: 10.0 * noise_power.log10(),
            signal_peak_dbfs: 10.0 * sig_peak_power.log10(),
            signal_avg_dbfs: 10.0 * sig_avg_power.log10(),
            snr_peak_db: 10.0 * (sig_peak_power / noise_power).log10(),
            snr_avg_db: 10.0 * (sig_avg_power / noise_power).log10(),
        })
    }

    /// Batch convenience: start a window, feed every full FFT_SIZE chunk
    /// from `samples`, finalize. Used by tests and as a sanity wrapper.
    #[cfg(test)]
    pub fn analyze(&mut self, samples: &[Complex32], mode: Mode) -> Option<Measurement> {
        self.start_window(mode);
        for frame in samples.chunks_exact(FFT_SIZE) {
            self.add_frame(frame);
        }
        self.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// A clean tone inside the USB passband should give a high SNR.
    #[test]
    fn tone_in_passband_has_high_snr() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        // 1500 Hz tone — smack in the middle of the USB passband.
        let n = FFT_SIZE * 4;
        let tone_hz = 1500.0;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * tone_hz as f32 * t;
            samples.push(Complex32::new(phase.cos(), phase.sin()));
        }

        let m = analyzer.analyze(&samples, Mode::Usb).unwrap();
        assert!(
            m.snr_peak_db > 40.0,
            "clean tone should give >40 dB peak SNR, got {}",
            m.snr_peak_db
        );
    }

    /// A CW carrier that's only keyed on for half the window should report
    /// a peak signal level close to a fully-on carrier — not 3 dB lower
    /// from averaging in the silent half.
    #[test]
    fn keyed_cw_signal_is_gated_to_active_frames() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        // Need many frames so we can split into "on" and "off" halves.
        let frames = 32;
        let n = FFT_SIZE * frames;
        let tone_hz = 700.0; // middle of the CW passband

        let mut always_on = Vec::with_capacity(n);
        let mut half_keyed = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * tone_hz as f32 * t;
            let s = Complex32::new(phase.cos(), phase.sin());
            always_on.push(s);
            // First half carrier on, second half silent (key up).
            if i < n / 2 {
                half_keyed.push(s);
            } else {
                half_keyed.push(Complex32::new(0.0, 0.0));
            }
        }

        let full = analyzer.analyze(&always_on, Mode::Cw).unwrap();
        let keyed = analyzer.analyze(&half_keyed, Mode::Cw).unwrap();

        let delta = (full.signal_peak_dbfs - keyed.signal_peak_dbfs).abs();
        assert!(
            delta < 1.0,
            "keyed peak signal should match fully-on within ~1 dB, got {} dB difference (full={}, keyed={})",
            delta,
            full.signal_peak_dbfs,
            keyed.signal_peak_dbfs
        );

        // And the average should be ~3 dB lower for the half-keyed case,
        // since it really is averaging carrier-on with silence — that's the
        // whole point of reporting both peak and avg.
        let avg_delta = full.signal_avg_dbfs - keyed.signal_avg_dbfs;
        assert!(
            (avg_delta - 3.0).abs() < 0.5,
            "half-keyed avg should be ~3 dB below full, got {} dB",
            avg_delta
        );
    }

    /// `add_frame` returns the just-processed frame's in-band power so the
    /// worker thread can drive a live activity meter without re-doing the
    /// FFT. A frame containing a tone in the passband should return a much
    /// larger value than a frame of zeros.
    #[test]
    fn add_frame_returns_per_frame_in_band_power() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);
        analyzer.start_window(Mode::Cw);

        // Frame 1: clean 700 Hz tone (middle of CW passband)
        let mut tone_frame = Vec::with_capacity(FFT_SIZE);
        for i in 0..FFT_SIZE {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * 700.0 * t;
            tone_frame.push(Complex32::new(phase.cos(), phase.sin()));
        }
        let tone_power = analyzer.add_frame(&tone_frame);

        // Frame 2: silence
        let zero_frame = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        let zero_power = analyzer.add_frame(&zero_frame);

        assert!(
            tone_power > 1e-3,
            "tone frame should return non-trivial in-band power, got {}",
            tone_power
        );
        assert!(
            zero_power < 1e-10,
            "zero frame should return ~0 in-band power, got {}",
            zero_power
        );
        assert!(
            tone_power > zero_power * 1e6,
            "tone power should dwarf zero power, tone={} zero={}",
            tone_power,
            zero_power
        );
    }

    /// A tone far outside the passband should not be counted as signal.
    #[test]
    fn tone_outside_passband_has_low_snr() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        // 200 kHz tone — way outside any SSB passband.
        let n = FFT_SIZE * 4;
        let tone_hz = 200_000.0;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * tone_hz as f32 * t;
            samples.push(Complex32::new(phase.cos(), phase.sin()));
        }

        let m = analyzer.analyze(&samples, Mode::Usb).unwrap();
        assert!(
            m.snr_peak_db < 20.0,
            "out-of-band tone should not score as signal, got peak SNR {}",
            m.snr_peak_db
        );
    }
}
