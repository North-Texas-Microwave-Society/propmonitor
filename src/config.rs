use crate::error::{Error, Result};
use crate::yaml;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Usb,
    Lsb,
    Am,
    Nfm,
    Wfm,
    Cw,
    Beacon,
}

impl Mode {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "usb" => Mode::Usb,
            "lsb" => Mode::Lsb,
            "am" => Mode::Am,
            "nfm" => Mode::Nfm,
            "wfm" => Mode::Wfm,
            "cw" => Mode::Cw,
            "beacon" => Mode::Beacon,
            other => {
                return Err(Error::msg(format!(
                    "config: unknown mode {:?} (expected usb|lsb|am|nfm|wfm|cw|beacon)",
                    other
                )));
            }
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Usb => "usb",
            Mode::Lsb => "lsb",
            Mode::Am => "am",
            Mode::Nfm => "nfm",
            Mode::Wfm => "wfm",
            Mode::Cw => "cw",
            Mode::Beacon => "beacon",
        }
    }
}

/// Per-mode (offset_hz, bandwidth_hz). For beacon mode the caller passes in
/// the user-configured beacon offset+bandwidth; everything else has a
/// fixed default.
pub fn passband_for(mode: Mode, beacon: Option<&BeaconConfig>) -> (f64, f64) {
    match mode {
        Mode::Usb => (1_500.0, 2_700.0),
        Mode::Lsb => (-1_500.0, 2_700.0),
        Mode::Am => (0.0, 6_000.0),
        Mode::Nfm => (0.0, 12_500.0),
        Mode::Wfm => (0.0, 150_000.0),
        Mode::Cw => (700.0, 500.0),
        Mode::Beacon => beacon
            .map(|b| (b.offset_hz, b.bandwidth_hz))
            .unwrap_or((0.0, 50.0)),
    }
}

#[derive(Debug, Clone)]
pub struct BeaconConfig {
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: String,
}

#[derive(Debug, Clone)]
pub struct MicrowavepropConfig {
    /// Whether the uploader actually POSTs measurements. Lets the
    /// operator save credentials but pause reporting without losing
    /// them (e.g. during station maintenance, antenna swaps, testing).
    pub enabled: bool,
    pub monitor_token: String,
    /// UUID of the beacon being monitored. Canonical key on the
    /// microwaveprop side — replaces callsign-based identification so
    /// duplicate or unregistered callsigns can't confuse routing.
    pub beacon_id: String,
    /// Maidenhead grid square of the RECEIVER location. Required for
    /// uploads — the server uses it to correlate signal strength with
    /// propagation paths. Up to 20 characters. Leave empty to disable
    /// uploads.
    pub gridsquare: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub frequency: f64,
    pub mode: Mode,
    pub sample_rate: f64,
    pub gain: Option<f64>,
    /// SoapySDR driver name (e.g. "sdrplay", "rtlsdr"). Defaults to "rtlsdr".
    pub driver: String,
    /// Frequency-error correction in PPM applied to the tuner. 0 for TCXO devices.
    pub ppm: f64,
    /// Measurement integration window in seconds.
    pub period_seconds: u32,
    pub beacon: Option<BeaconConfig>,
    pub http: HttpConfig,
    pub microwaveprop: Option<MicrowavepropConfig>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_yaml_str(&text)
    }

    pub fn from_yaml_str(text: &str) -> Result<Self> {
        let map = yaml::parse(text)?;

        let frequency = yaml::parse_f64(yaml::require_scalar(&map, "frequency")?, "frequency")?;
        let mode = Mode::parse(yaml::require_scalar(&map, "mode")?)?;

        let sample_rate = match map.get("sample_rate") {
            Some(v) => yaml::parse_f64(
                v.as_scalar()
                    .ok_or_else(|| Error::msg("config: `sample_rate` must be a scalar"))?,
                "sample_rate",
            )?,
            None => 250_000.0,
        };

        let gain = match map.get("gain") {
            Some(v) => Some(yaml::parse_f64(
                v.as_scalar()
                    .ok_or_else(|| Error::msg("config: `gain` must be a scalar"))?,
                "gain",
            )?),
            None => None,
        };

        let driver = match map.get("driver") {
            Some(v) => v
                .as_scalar()
                .ok_or_else(|| Error::msg("config: `driver` must be a scalar"))?
                .to_string(),
            None => "rtlsdr".to_string(),
        };

        let ppm = match map.get("ppm") {
            Some(v) => yaml::parse_f64(
                v.as_scalar()
                    .ok_or_else(|| Error::msg("config: `ppm` must be a scalar"))?,
                "ppm",
            )?,
            None => 0.0,
        };

        let period_seconds = match map.get("period_seconds") {
            Some(v) => u32::try_from(yaml::parse_usize(
                v.as_scalar()
                    .ok_or_else(|| Error::msg("config: `period_seconds` must be a scalar"))?,
                "period_seconds",
            )?)
            .map_err(|_| Error::msg("config: `period_seconds` is too large"))?,
            None => 60,
        };

        let beacon = match map.get("beacon") {
            Some(v) => {
                let m = v
                    .as_map()
                    .ok_or_else(|| Error::msg("config: `beacon` must be a mapping"))?;
                let offset_hz = match m.get("offset_hz") {
                    Some(v) => yaml::parse_f64(
                        v.as_scalar().ok_or_else(|| {
                            Error::msg("config: `beacon.offset_hz` must be a scalar")
                        })?,
                        "beacon.offset_hz",
                    )?,
                    None => 0.0,
                };
                let bandwidth_hz = match m.get("bandwidth_hz") {
                    Some(v) => yaml::parse_f64(
                        v.as_scalar().ok_or_else(|| {
                            Error::msg("config: `beacon.bandwidth_hz` must be a scalar")
                        })?,
                        "beacon.bandwidth_hz",
                    )?,
                    None => 50.0,
                };
                Some(BeaconConfig {
                    offset_hz,
                    bandwidth_hz,
                })
            }
            None => None,
        };

        let http = match map.get("http") {
            Some(v) => {
                let m = v
                    .as_map()
                    .ok_or_else(|| Error::msg("config: `http` must be a mapping"))?;
                let bind = match m.get("bind") {
                    Some(v) => v
                        .as_scalar()
                        .ok_or_else(|| Error::msg("config: `http.bind` must be a scalar"))?
                        .to_string(),
                    None => "0.0.0.0:5760".to_string(),
                };
                HttpConfig { bind }
            }
            None => HttpConfig {
                bind: "0.0.0.0:5760".to_string(),
            },
        };

        let microwaveprop = match map.get("microwaveprop") {
            Some(v) => {
                let m = v
                    .as_map()
                    .ok_or_else(|| Error::msg("config: `microwaveprop` must be a mapping"))?;
                // Default: enabled. The block being present at all is
                // already a signal of intent to report; the operator can
                // explicitly set `enabled: false` to pause without
                // erasing their credentials.
                let enabled = match m.get("enabled") {
                    Some(v) => v
                        .as_scalar()
                        .ok_or_else(|| {
                            Error::msg("config: `microwaveprop.enabled` must be a scalar")
                        })?
                        .parse::<bool>()
                        .map_err(|_| {
                            Error::msg("config: `microwaveprop.enabled` must be true or false")
                        })?,
                    None => true,
                };
                let monitor_token = m
                    .get("monitor_token")
                    .and_then(|v| v.as_scalar())
                    .unwrap_or("")
                    .to_string();
                let beacon_id = m
                    .get("beacon_id")
                    .and_then(|v| v.as_scalar())
                    .unwrap_or("")
                    .to_string();
                let gridsquare = m
                    .get("gridsquare")
                    .and_then(|v| v.as_scalar())
                    .unwrap_or("")
                    .to_string();
                Some(MicrowavepropConfig {
                    enabled,
                    monitor_token,
                    beacon_id,
                    gridsquare,
                })
            }
            None => None,
        };

        let cfg = Config {
            frequency,
            mode,
            sample_rate,
            gain,
            driver,
            ppm,
            period_seconds,
            beacon,
            http,
            microwaveprop,
        };

        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.frequency.is_finite() || self.frequency <= 0.0 {
            return Err(Error::msg("config: frequency must be finite and positive"));
        }
        if !self.sample_rate.is_finite() || self.sample_rate < 250_000.0 {
            return Err(Error::msg(
                "config: sample_rate must be finite and at least 250000 Hz (lower rates pin the waterfall bin width too high)",
            ));
        }
        if self.gain.is_some_and(|gain| !gain.is_finite()) {
            return Err(Error::msg("config: gain must be finite"));
        }
        if !self.ppm.is_finite() {
            return Err(Error::msg("config: ppm must be finite"));
        }
        if self.period_seconds < 5 {
            return Err(Error::msg("config: period_seconds must be at least 5"));
        }
        if self.mode == Mode::Beacon {
            let b = self
                .beacon
                .as_ref()
                .ok_or_else(|| Error::msg("config: mode=beacon requires a `beacon:` block"))?;
            if !b.offset_hz.is_finite() {
                return Err(Error::msg("config: beacon.offset_hz must be finite"));
            }
            if !b.bandwidth_hz.is_finite() || b.bandwidth_hz <= 0.0 {
                return Err(Error::msg(
                    "config: beacon.bandwidth_hz must be finite and positive",
                ));
            }
            if b.bandwidth_hz > self.sample_rate / 2.0 {
                return Err(Error::msg(
                    "config: beacon.bandwidth_hz exceeds sample_rate/2",
                ));
            }
            if b.offset_hz.abs() + b.bandwidth_hz / 2.0 > self.sample_rate / 2.0 {
                return Err(Error::msg(
                    "config: beacon passband extends outside the sampled spectrum",
                ));
            }
        }
        if let Some(mw) = &self.microwaveprop {
            if !mw.gridsquare.is_empty() && (mw.gridsquare.len() < 4 || mw.gridsquare.len() > 20) {
                return Err(Error::msg(
                    "config: microwaveprop.gridsquare must be 4–20 characters",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_beacon_config() {
        let yaml = "\
frequency: 28330000
mode: beacon
sample_rate: 250000
gain: 10
beacon:
  offset_hz: 0
  bandwidth_hz: 50
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.frequency, 28_330_000.0);
        assert_eq!(cfg.mode, Mode::Beacon);
        assert_eq!(cfg.gain, Some(10.0));
        let b = cfg.beacon.unwrap();
        assert_eq!(b.offset_hz, 0.0);
        assert_eq!(b.bandwidth_hz, 50.0);
        assert!(cfg.microwaveprop.is_none());
        assert_eq!(cfg.http.bind, "0.0.0.0:5760");
        assert_eq!(cfg.period_seconds, 60);
    }

    #[test]
    fn rejects_beacon_mode_without_beacon_block() {
        let yaml = "frequency: 28330000\nmode: beacon\nsample_rate: 250000\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_mode() {
        let yaml = "frequency: 28330000\nmode: bogus\nsample_rate: 250000\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn microwaveprop_defaults_to_enabled_when_block_present() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
microwaveprop:
  monitor_token: \"token123\"
  beacon_id: \"00000000-0000-0000-0000-000000000001\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let mw = cfg.microwaveprop.unwrap();
        assert!(mw.enabled);
        assert_eq!(mw.monitor_token, "token123");
        assert_eq!(mw.beacon_id, "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn rejects_zero_or_negative_frequency() {
        let yaml = "frequency: 0\nmode: cw\nsample_rate: 250000\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_non_finite_numeric_values() {
        for (field, value) in [
            ("frequency", "NaN"),
            ("sample_rate", "inf"),
            ("gain", "NaN"),
            ("ppm", "-inf"),
        ] {
            let yaml =
                format!("frequency: 28330000\nmode: cw\nsample_rate: 250000\n{field}: {value}\n");
            assert!(
                Config::from_yaml_str(&yaml).is_err(),
                "accepted {field}: {value}"
            );
        }
    }

    #[test]
    fn rejects_beacon_passband_extending_outside_sampled_spectrum() {
        let yaml = "frequency: 28330000\nmode: beacon\nsample_rate: 250000\nbeacon:\n  offset_hz: 124900\n  bandwidth_hz: 300\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_sample_rate_below_250k() {
        let yaml = "frequency: 28330000\nmode: cw\nsample_rate: 100000\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_period_seconds_below_five() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
period_seconds: 1
";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_period_seconds_that_overflows_u32() {
        let yaml = "frequency: 28330000\nmode: cw\nperiod_seconds: 4294967300\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_invalid_uploader_enabled_value() {
        let yaml = "frequency: 28330000\nmode: cw\nmicrowaveprop:\n  enabled: maybe\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_beacon_bandwidth_zero_or_negative() {
        let yaml = "\
frequency: 28330000
mode: beacon
sample_rate: 250000
beacon:
  offset_hz: 0
  bandwidth_hz: 0
";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn rejects_beacon_bandwidth_above_nyquist() {
        let yaml = "\
frequency: 28330000
mode: beacon
sample_rate: 250000
beacon:
  offset_hz: 0
  bandwidth_hz: 200000
";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn uses_default_sample_rate_when_missing() {
        // No sample_rate key — defaults to 250000.
        let yaml = "frequency: 28330000\nmode: cw\n";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.sample_rate, 250_000.0);
    }

    #[test]
    fn beacon_block_omitting_offset_uses_default_zero() {
        // beacon block present, offset_hz missing → defaults to 0.0.
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
beacon:
  bandwidth_hz: 50
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let b = cfg.beacon.unwrap();
        assert_eq!(b.offset_hz, 0.0);
        assert_eq!(b.bandwidth_hz, 50.0);
    }

    #[test]
    fn http_block_omitting_bind_uses_default() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
http:
  bind: \"127.0.0.1:1234\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.http.bind, "127.0.0.1:1234");

        // Empty http block (no bind key) → default bind.
        // The minimal YAML parser requires a value on the parent if
        // there's no nested content, so we can't write a truly empty
        // block. We can write one with an unrelated key — but the parser
        // would reject unknown keys at that nesting level. The default
        // path is therefore only reachable by omitting `http:` entirely,
        // which is already covered by `applies_defaults_when_optional_fields_missing`.
    }

    #[test]
    fn applies_defaults_when_optional_fields_missing() {
        let yaml = "frequency: 28330000\nmode: cw\nsample_rate: 250000\n";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.driver, "rtlsdr");
        assert_eq!(cfg.ppm, 0.0);
        assert_eq!(cfg.period_seconds, 60);
        assert!(cfg.gain.is_none());
        assert_eq!(cfg.http.bind, "0.0.0.0:5760");
    }

    #[test]
    fn parses_every_mode_keyword() {
        for (s, m) in [
            ("usb", Mode::Usb),
            ("lsb", Mode::Lsb),
            ("am", Mode::Am),
            ("nfm", Mode::Nfm),
            ("wfm", Mode::Wfm),
            ("cw", Mode::Cw),
            ("beacon", Mode::Beacon),
        ] {
            assert_eq!(Mode::parse(s).unwrap(), m);
            assert_eq!(m.as_str(), s);
        }
    }

    #[test]
    fn passband_for_returns_mode_defaults() {
        assert_eq!(passband_for(Mode::Usb, None), (1500.0, 2700.0));
        assert_eq!(passband_for(Mode::Lsb, None), (-1500.0, 2700.0));
        assert_eq!(passband_for(Mode::Am, None), (0.0, 6000.0));
        assert_eq!(passband_for(Mode::Nfm, None), (0.0, 12500.0));
        assert_eq!(passband_for(Mode::Wfm, None), (0.0, 150000.0));
        assert_eq!(passband_for(Mode::Cw, None), (700.0, 500.0));
    }

    #[test]
    fn passband_for_beacon_uses_user_config() {
        let b = BeaconConfig {
            offset_hz: 100.0,
            bandwidth_hz: 50.0,
        };
        assert_eq!(passband_for(Mode::Beacon, Some(&b)), (100.0, 50.0));
    }

    #[test]
    fn passband_for_beacon_without_block_falls_back_to_default() {
        // Belt-and-braces — validation should reject this, but
        // `passband_for` itself doesn't panic on `None`.
        assert_eq!(passband_for(Mode::Beacon, None), (0.0, 50.0));
    }

    #[test]
    fn parses_quoted_driver_string() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
driver: \"rtlsdr,serial=ABC\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.driver, "rtlsdr,serial=ABC");
    }

    #[test]
    fn parses_non_default_ppm_and_period() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
ppm: 2.5
period_seconds: 30
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.ppm, 2.5);
        assert_eq!(cfg.period_seconds, 30);
    }

    #[test]
    fn parses_custom_http_bind() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
http:
  bind: \"127.0.0.1:9090\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.http.bind, "127.0.0.1:9090");
    }

    #[test]
    fn parses_beacon_block_with_defaults() {
        // beacon: with no offset/bandwidth keys gets the per-key defaults.
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
beacon:
  offset_hz: 100
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let b = cfg.beacon.unwrap();
        assert_eq!(b.offset_hz, 100.0);
        assert_eq!(b.bandwidth_hz, 50.0); // default
    }

    #[test]
    fn load_from_disk_reads_a_real_file() {
        let path = std::env::temp_dir().join(format!(
            "propmonitor-config-test-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "frequency: 28330000\nmode: cw\nsample_rate: 250000\n",
        )
        .unwrap();
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.mode, Mode::Cw);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn microwaveprop_enabled_false_persists_credentials_but_pauses_uploads() {
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
microwaveprop:
  enabled: false
  monitor_token: \"token123\"
  beacon_id: \"00000000-0000-0000-0000-000000000001\"
  gridsquare: \"FN31pr\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let mw = cfg.microwaveprop.unwrap();
        assert!(!mw.enabled);
        assert_eq!(mw.monitor_token, "token123");
        assert_eq!(mw.gridsquare, "FN31pr");
    }

    #[test]
    fn gridsquare_parses_and_defaults() {
        // Present → parsed.
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
microwaveprop:
  monitor_token: \"token123\"
  beacon_id: \"00000000-0000-0000-0000-000000000001\"
  gridsquare: \"EM12il\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.microwaveprop.unwrap().gridsquare, "EM12il");

        // Omitted → defaults to empty string.
        let yaml = "\
frequency: 28330000
mode: cw
sample_rate: 250000
microwaveprop:
  monitor_token: \"token123\"
  beacon_id: \"00000000-0000-0000-0000-000000000001\"
";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.microwaveprop.unwrap().gridsquare, "");
    }

    #[test]
    fn gridsquare_validation_rejects_bad_lengths() {
        // Empty gridsquare is allowed (pauses uploads), 4–20 is allowed.
        for gs in ["", "EM12", "EM12il", "EM12ilABCDEFGHIJKL"] {
            let yaml = format!(
                "frequency: 28330000\nmode: cw\nsample_rate: 250000\nmicrowaveprop:\n  monitor_token: \"t\"\n  beacon_id: \"b\"\n  gridsquare: \"{gs}\"\n"
            );
            assert!(
                Config::from_yaml_str(&yaml).is_ok(),
                "rejected valid gridsquare {gs:?}"
            );
        }

        // Too short (< 4) and too long (> 20) → rejected.
        for gs in ["EM1", "EM12ilABCDEFGHIJKLMNOP"] {
            let yaml = format!(
                "frequency: 28330000\nmode: cw\nsample_rate: 250000\nmicrowaveprop:\n  monitor_token: \"t\"\n  beacon_id: \"b\"\n  gridsquare: \"{gs}\"\n"
            );
            assert!(
                Config::from_yaml_str(&yaml).is_err(),
                "accepted invalid gridsquare {gs:?}"
            );
        }
    }
}
