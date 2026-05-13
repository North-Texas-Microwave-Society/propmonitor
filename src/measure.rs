use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

pub const FFT_SIZE: usize = 16_384;

#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    pub noise_dbfs: f64,
    /// Strongest single-frame in-band power across the capture window,
    /// computed over frames where the beacon was detected on-air (in-band
    /// power > noise_floor + 3 dB). For a keyed CW beacon this matches the
    /// keyed-down level; for a continuous carrier it's the same as avg.
    pub signal_peak_dbfs: f64,
    /// Mean in-band power across detected-on-air frames in the capture
    /// window. Mode-invariant: a 30% keyed CW beacon and a continuous
    /// carrier with the same TX power produce the same value.
    pub signal_avg_dbfs: f64,
    pub snr_peak_db: f64,
    pub snr_avg_db: f64,
    /// Fraction of frames whose in-band power exceeded `noise + 3 dB`.
    /// 1.0 for a continuous carrier, ~0.5 for a 50%-keyed CW beacon, 0.0
    /// when nothing crossed the threshold. The server uses this to weight
    /// or filter measurements for long-term trend analysis.
    pub signal_active_fraction: f64,
}

pub struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Sum of window samples squared — used to normalize the periodogram.
    window_energy: f64,
    sample_rate: f64,

    // Streaming accumulator state. `start_window` resets these and locks
    // in a passband; `add_frame` updates them; `finalize` consumes them
    // and produces a Measurement.
    window_open: bool,
    sig_lo: usize,
    sig_hi: usize,
    half_width_bins: usize,
    fft_buf: Vec<Complex32>,
    psd_sum: Vec<f64>,
    sig_peak_power: f64,
    sig_sum_power: f64,
    /// Per-frame in-band powers. Held so `finalize` can re-walk them and
    /// compute gated peak/avg over only the frames that exceeded
    /// `noise + 3 dB`, plus the active-fraction. Memory cost is bounded
    /// (~900 frames × 8 B for a 60 s window at 250 kS/s).
    frame_powers: Vec<f64>,
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
            window_open: false,
            sig_lo: 0,
            sig_hi: 0,
            half_width_bins: 0,
            fft_buf: vec![Complex32::new(0.0, 0.0); FFT_SIZE],
            psd_sum: vec![0.0f64; FFT_SIZE],
            sig_peak_power: 0.0,
            sig_sum_power: 0.0,
            frame_powers: Vec::new(),
            frame_count: 0,
        }
    }

    /// Begin a new measurement window. Resets all accumulators and
    /// pre-computes the signal-bin range from the supplied passband.
    pub fn start_window(&mut self, offset_hz: f64, width_hz: f64) {
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
        self.frame_powers.clear();
        self.frame_count = 0;
        self.window_open = true;
    }

    /// Process exactly one FFT_SIZE-sample frame. The signal-bin range from
    /// `start_window` is used to track per-frame in-band peak and mean.
    /// Returns the just-processed frame's in-band power (linear, normalized
    /// the same way as `signal_avg_dbfs`/`signal_peak_dbfs`) so callers can
    /// drive a live activity meter without re-running the FFT.
    pub fn add_frame(&mut self, frame: &[Complex32]) -> f64 {
        debug_assert_eq!(frame.len(), FFT_SIZE);
        debug_assert!(self.window_open, "start_window not called");

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
        self.frame_powers.push(frame_sig_power);
        self.frame_count += 1;
        frame_sig_power
    }

    /// Finalize the current measurement window and return the result.
    /// Clears `window_open` so a stale window can't be accidentally reused
    /// without calling `start_window` again.
    pub fn finalize(&mut self) -> Option<Measurement> {
        if self.frame_count == 0 || !self.window_open {
            self.window_open = false;
            return None;
        }
        if self.sig_hi <= self.sig_lo {
            self.window_open = false;
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
            self.window_open = false;
            return None;
        }
        noise_bins.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let noise_per_bin = noise_bins[noise_bins.len() / 2];
        let noise_power = (noise_per_bin * signal_bin_count as f64).max(1e-30);

        // Signal-present gating. Only frames whose in-band power exceeded
        // `noise + 3 dB` (×2 linear) count toward peak/avg — silence
        // between CW dits or Q65 transmissions doesn't dilute the signal
        // level. `signal_active_fraction` reports the duty cycle.
        //
        // If nothing crossed the threshold (`active_count == 0`) we fall
        // back to ungated stats so the upload still carries something
        // meaningful (peak/avg will be near the noise floor, SNR ~0 dB) —
        // the server uses `signal_active_fraction == 0` to filter or flag.
        let threshold = noise_power * 2.0;
        let mut active_count: usize = 0;
        let mut active_peak: f64 = 0.0;
        let mut active_sum: f64 = 0.0;
        for &p in &self.frame_powers {
            if p > threshold {
                active_count += 1;
                if p > active_peak {
                    active_peak = p;
                }
                active_sum += p;
            }
        }

        let (sig_peak_power, sig_avg_power) = if active_count > 0 {
            (
                active_peak.max(1e-30),
                (active_sum / active_count as f64).max(1e-30),
            )
        } else {
            (
                self.sig_peak_power.max(1e-30),
                (self.sig_sum_power / frames as f64).max(1e-30),
            )
        };
        let signal_active_fraction = active_count as f64 / frames as f64;

        self.window_open = false;

        Some(Measurement {
            noise_dbfs: 10.0 * noise_power.log10(),
            signal_peak_dbfs: 10.0 * sig_peak_power.log10(),
            signal_avg_dbfs: 10.0 * sig_avg_power.log10(),
            snr_peak_db: 10.0 * (sig_peak_power / noise_power).log10(),
            snr_avg_db: 10.0 * (sig_avg_power / noise_power).log10(),
            signal_active_fraction,
        })
    }

    /// Batch convenience: start a window, feed every full FFT_SIZE chunk
    /// from `samples`, finalize. Used by tests and as a sanity wrapper.
    #[cfg(test)]
    pub fn analyze(
        &mut self,
        samples: &[Complex32],
        offset_hz: f64,
        width_hz: f64,
    ) -> Option<Measurement> {
        self.start_window(offset_hz, width_hz);
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

        let m = analyzer.analyze(&samples, 1_500.0, 2_700.0).unwrap();
        assert!(
            m.snr_peak_db > 40.0,
            "clean tone should give >40 dB peak SNR, got {}",
            m.snr_peak_db
        );
    }

    /// Half-keyed CW: signal-present gating should give the same peak AND
    /// the same avg as a fully-on carrier, because silent frames don't
    /// count. The duty cycle shows up only in `signal_active_fraction`.
    /// This is the whole point of the gating change — long-term trends
    /// should not depend on the operator's keying pattern.
    #[test]
    fn keyed_cw_gated_avg_matches_fully_on() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        let frames = 32;
        let n = FFT_SIZE * frames;
        let tone_hz = 700.0;

        let mut always_on = Vec::with_capacity(n);
        let mut half_keyed = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * tone_hz as f32 * t;
            let s = Complex32::new(phase.cos(), phase.sin());
            always_on.push(s);
            if i < n / 2 {
                half_keyed.push(s);
            } else {
                half_keyed.push(Complex32::new(0.0, 0.0));
            }
        }

        let full = analyzer.analyze(&always_on, 700.0, 500.0).unwrap();
        let keyed = analyzer.analyze(&half_keyed, 700.0, 500.0).unwrap();

        let peak_delta = (full.signal_peak_dbfs - keyed.signal_peak_dbfs).abs();
        assert!(
            peak_delta < 1.0,
            "gated peak should match fully-on within 1 dB, got {} dB (full={}, keyed={})",
            peak_delta,
            full.signal_peak_dbfs,
            keyed.signal_peak_dbfs
        );

        let avg_delta = (full.signal_avg_dbfs - keyed.signal_avg_dbfs).abs();
        assert!(
            avg_delta < 1.0,
            "gated avg should match fully-on within 1 dB, got {} dB (full={}, keyed={})",
            avg_delta,
            full.signal_avg_dbfs,
            keyed.signal_avg_dbfs
        );
    }

    /// Continuous tone → every frame is active → fraction = 1.0.
    #[test]
    fn signal_active_fraction_is_one_for_continuous_tone() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        let n = FFT_SIZE * 16;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * 700.0 * t;
            samples.push(Complex32::new(phase.cos(), phase.sin()));
        }

        let m = analyzer.analyze(&samples, 700.0, 500.0).unwrap();
        assert!(
            m.signal_active_fraction > 0.99,
            "continuous tone should give fraction ~1.0, got {}",
            m.signal_active_fraction
        );
    }

    /// 50% keyed CW: roughly half of frames are above the noise+3dB
    /// threshold. Tolerate alignment slop (one frame may straddle the
    /// boundary).
    #[test]
    fn signal_active_fraction_is_half_for_50_percent_keyed() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        let frames = 32;
        let n = FFT_SIZE * frames;
        let mut keyed = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * 700.0 * t;
            if i < n / 2 {
                keyed.push(Complex32::new(phase.cos(), phase.sin()));
            } else {
                keyed.push(Complex32::new(0.0, 0.0));
            }
        }

        let m = analyzer.analyze(&keyed, 700.0, 500.0).unwrap();
        assert!(
            (m.signal_active_fraction - 0.5).abs() < 0.1,
            "50%-keyed CW should give fraction ~0.5, got {}",
            m.signal_active_fraction
        );
    }

    #[test]
    fn finalize_without_start_window_returns_none() {
        let mut a = SpectrumAnalyzer::new(250_000.0);
        // No start_window, no add_frame.
        assert!(a.finalize().is_none());
    }

    #[test]
    fn finalize_with_zero_frames_returns_none() {
        let mut a = SpectrumAnalyzer::new(250_000.0);
        a.start_window(0.0, 50.0);
        // start_window called but no add_frame — frame_count stays 0.
        assert!(a.finalize().is_none());
    }

    /// An offset so far outside the band that clamping forces
    /// `sig_lo > sig_hi`. `finalize` should bail with `None` rather
    /// than producing nonsense numbers.
    #[test]
    fn finalize_returns_none_when_passband_is_outside_fft() {
        let mut a = SpectrumAnalyzer::new(250_000.0);
        a.start_window(1_000_000_000.0, 1.0);
        let frame = vec![Complex32::new(0.1, 0.1); FFT_SIZE];
        a.add_frame(&frame);
        assert!(a.finalize().is_none());
    }

    /// A passband as wide as the full sample rate leaves no out-of-band
    /// bins for the noise floor; `finalize` returns `None`.
    #[test]
    fn finalize_returns_none_when_passband_covers_everything() {
        let sr = 250_000.0;
        let mut a = SpectrumAnalyzer::new(sr);
        a.start_window(0.0, sr);
        let frame = vec![Complex32::new(0.1, 0.1); FFT_SIZE];
        a.add_frame(&frame);
        assert!(a.finalize().is_none());
    }

    #[test]
    fn finalize_after_finalize_returns_none() {
        // Once finalized, window_open is cleared — a second finalize
        // call must not return stale state.
        let sr = 2_000_000.0;
        let mut a = SpectrumAnalyzer::new(sr);
        let n = FFT_SIZE * 2;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let phase = 2.0 * PI * 700.0 * t;
            samples.push(Complex32::new(phase.cos(), phase.sin()));
        }
        assert!(a.analyze(&samples, 700.0, 500.0).is_some());
        assert!(a.finalize().is_none());
    }

    /// Pure silence → no frames cross threshold → fraction = 0.0, and
    /// peak/avg fall back to the ungated values (effectively the noise
    /// floor — SNR ~0 dB).
    #[test]
    fn signal_active_fraction_is_zero_for_silence() {
        let sr = 2_000_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sr);

        let n = FFT_SIZE * 8;
        let samples = vec![Complex32::new(0.0, 0.0); n];

        let m = analyzer.analyze(&samples, 700.0, 500.0).unwrap();
        assert_eq!(
            m.signal_active_fraction, 0.0,
            "silence should give fraction 0.0, got {}",
            m.signal_active_fraction
        );
        assert!(
            m.snr_peak_db.abs() < 1.0,
            "silence SNR should be near 0 dB, got {}",
            m.snr_peak_db
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
        analyzer.start_window(700.0, 500.0);

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

        let m = analyzer.analyze(&samples, 1_500.0, 2_700.0).unwrap();
        assert!(
            m.snr_peak_db < 20.0,
            "out-of-band tone should not score as signal, got peak SNR {}",
            m.snr_peak_db
        );
    }
}
