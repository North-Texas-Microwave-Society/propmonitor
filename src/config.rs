use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Usb,
    Lsb,
    Am,
    Nfm,
    Wfm,
    Cw,
}

impl Mode {
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
        }
    }
}

fn default_sample_rate() -> f64 {
    2_000_000.0
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub frequency: f64,
    pub mode: Mode,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    #[serde(default)]
    pub gain: Option<f64>,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&text)?)
    }
}
