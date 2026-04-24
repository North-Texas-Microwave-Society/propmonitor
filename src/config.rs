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
    Q65,
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
            Mode::Q65 => (0.0, 430.0),
        }
    }
}

fn default_sample_rate() -> f64 {
    2_000_000.0
}

fn default_q65_audio_center() -> f64 {
    1_500.0
}
fn default_q65_audio_search() -> f64 {
    200.0
}
fn default_q65_max_decodes() -> usize {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct Q65Config {
    /// Only "60C" accepted in MVP.
    pub submode: String,
    #[serde(default = "default_q65_audio_center")]
    pub audio_center_hz: f64,
    #[serde(default = "default_q65_audio_search")]
    pub audio_search_hz: f64,
    #[serde(default = "default_q65_max_decodes")]
    pub max_decodes_per_period: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub frequency: f64,
    pub mode: Mode,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    #[serde(default)]
    pub gain: Option<f64>,
    #[serde(default)]
    pub q65: Option<Q65Config>,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = serde_yaml::from_str(&text)?;
        if cfg.mode == Mode::Q65 {
            let q = cfg
                .q65
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("mode: q65 requires a `q65:` config block"))?;
            if q.submode != "60C" {
                anyhow::bail!(
                    "q65.submode {:?} unsupported — MVP only ships Q65-60C",
                    q.submode
                );
            }
        }
        Ok(cfg)
    }
}
