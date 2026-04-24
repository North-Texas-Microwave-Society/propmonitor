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
    Q65,
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
            "q65" => Mode::Q65,
            other => {
                return Err(Error::msg(format!(
                    "config: unknown mode {:?} (expected usb|lsb|am|nfm|wfm|cw|q65)",
                    other
                )));
            }
        })
    }

    /// Returns (offset_hz, bandwidth_hz) for this mode's passband,
    /// measured relative to the tuned center frequency.
    pub fn passband(self) -> (f64, f64) {
        match self {
            Mode::Usb => (1_500.0, 2_700.0),
            Mode::Lsb => (-1_500.0, 2_700.0),
            Mode::Am => (0.0, 6_000.0),
            Mode::Nfm => (0.0, 12_500.0),
            Mode::Wfm => (0.0, 150_000.0),
            Mode::Cw => (700.0, 500.0),
            Mode::Q65 => (0.0, 430.0),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Q65Config {
    /// Only "60C" accepted in MVP.
    pub submode: String,
    pub audio_center_hz: f64,
    pub audio_search_hz: f64,
    pub max_decodes_per_period: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub frequency: f64,
    pub mode: Mode,
    pub sample_rate: f64,
    pub gain: Option<f64>,
    pub q65: Option<Q65Config>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let map = yaml::parse(&text)?;

        let frequency = yaml::parse_f64(yaml::require_scalar(&map, "frequency")?, "frequency")?;
        let mode = Mode::parse(yaml::require_scalar(&map, "mode")?)?;

        let sample_rate = match map.get("sample_rate") {
            Some(v) => yaml::parse_f64(
                v.as_scalar()
                    .ok_or_else(|| Error::msg("config: `sample_rate` must be a scalar"))?,
                "sample_rate",
            )?,
            None => 2_000_000.0,
        };

        let gain = match map.get("gain") {
            Some(v) => Some(yaml::parse_f64(
                v.as_scalar()
                    .ok_or_else(|| Error::msg("config: `gain` must be a scalar"))?,
                "gain",
            )?),
            None => None,
        };

        let q65 = match map.get("q65") {
            Some(v) => {
                let qm = v
                    .as_map()
                    .ok_or_else(|| Error::msg("config: `q65` must be a mapping"))?;
                Some(Q65Config {
                    submode: yaml::require_scalar(qm, "submode")?.to_string(),
                    audio_center_hz: match qm.get("audio_center_hz") {
                        Some(v) => yaml::parse_f64(
                            v.as_scalar().ok_or_else(|| {
                                Error::msg("config: `q65.audio_center_hz` must be a scalar")
                            })?,
                            "q65.audio_center_hz",
                        )?,
                        None => 1_500.0,
                    },
                    audio_search_hz: match qm.get("audio_search_hz") {
                        Some(v) => yaml::parse_f64(
                            v.as_scalar().ok_or_else(|| {
                                Error::msg("config: `q65.audio_search_hz` must be a scalar")
                            })?,
                            "q65.audio_search_hz",
                        )?,
                        None => 200.0,
                    },
                    max_decodes_per_period: match qm.get("max_decodes_per_period") {
                        Some(v) => yaml::parse_usize(
                            v.as_scalar().ok_or_else(|| {
                                Error::msg(
                                    "config: `q65.max_decodes_per_period` must be a scalar",
                                )
                            })?,
                            "q65.max_decodes_per_period",
                        )?,
                        None => 5,
                    },
                })
            }
            None => None,
        };

        let cfg = Config {
            frequency,
            mode,
            sample_rate,
            gain,
            q65,
        };

        if cfg.mode == Mode::Q65 {
            let q = cfg
                .q65
                .as_ref()
                .ok_or_else(|| Error::msg("mode: q65 requires a `q65:` config block"))?;
            if q.submode != "60C" {
                return Err(Error::msg(format!(
                    "q65.submode {:?} unsupported — MVP only ships Q65-60C",
                    q.submode
                )));
            }
        }

        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_analog_config() {
        let dir = std::env::temp_dir();
        let path = dir.join("propmonitor_test_minimal.yaml");
        std::fs::write(&path, "frequency: 101100000\nmode: wfm\n").unwrap();
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.frequency, 101_100_000.0);
        assert_eq!(cfg.mode, Mode::Wfm);
        assert_eq!(cfg.sample_rate, 2_000_000.0);
        assert!(cfg.gain.is_none());
        assert!(cfg.q65.is_none());
    }

    #[test]
    fn parses_full_q65_config() {
        let dir = std::env::temp_dir();
        let path = dir.join("propmonitor_test_q65.yaml");
        std::fs::write(
            &path,
            "frequency: 50211000\nmode: q65\nsample_rate: 2000000\ngain: 40\nq65:\n  submode: \"60C\"\n  audio_center_hz: 1500\n  audio_search_hz: 200\n  max_decodes_per_period: 5\n",
        )
        .unwrap();
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.mode, Mode::Q65);
        assert_eq!(cfg.gain, Some(40.0));
        let q = cfg.q65.unwrap();
        assert_eq!(q.submode, "60C");
        assert_eq!(q.audio_center_hz, 1500.0);
        assert_eq!(q.max_decodes_per_period, 5);
    }

    #[test]
    fn rejects_q65_mode_without_q65_block() {
        let dir = std::env::temp_dir();
        let path = dir.join("propmonitor_test_q65_missing.yaml");
        std::fs::write(&path, "frequency: 50211000\nmode: q65\n").unwrap();
        assert!(Config::load(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn rejects_unsupported_submode() {
        let dir = std::env::temp_dir();
        let path = dir.join("propmonitor_test_q65_30A.yaml");
        std::fs::write(
            &path,
            "frequency: 50211000\nmode: q65\nq65:\n  submode: \"30A\"\n",
        )
        .unwrap();
        assert!(Config::load(path.to_str().unwrap()).is_err());
    }
}
