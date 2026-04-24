#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submode {
    Tr15,
    Tr30,
    Tr60,
    Tr120,
    Tr300,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    A,
    B,
    C,
    D,
    E,
}

impl Variant {
    pub fn tone_spacing_multiplier(self) -> f64 {
        match self {
            Variant::A => 1.0,
            Variant::B => 2.0,
            Variant::C => 4.0,
            Variant::D => 8.0,
            Variant::E => 16.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Q65Params {
    pub submode: Submode,
    pub variant: Variant,
    pub tsym_s: f64,
    pub baud_hz: f64,
    pub tone_spacing_hz: f64,
    pub total_bw_hz: f64,
    pub num_symbols: usize,
    pub num_data_symbols: usize,
    pub num_sync_symbols: usize,
    pub num_tones: usize,
    pub tr_period_s: f64,
}

/// Q65-60C: 60 s T/R period, variant C tone spacing (4 x baud).
///
/// Nominal values from Franke/Somerville/Taylor ("The FT4 and FT8
/// Communication Protocols and the New Q65 Mode", QEX 2020) and from
/// the WSJT-X documentation. These are the *target* numbers we tune to;
/// exact values (tsym, baud) must agree with the reference to
/// sub-percent precision for sync correlation to work at design SNR.
pub const Q65_60C: Q65Params = Q65Params {
    submode: Submode::Tr60,
    variant: Variant::C,
    tsym_s: 0.60400,
    baud_hz: 1.65563,
    tone_spacing_hz: 6.62252,
    total_bw_hz: 430.464,
    num_symbols: 85,
    num_data_symbols: 63,
    num_sync_symbols: 22,
    num_tones: 65,
    tr_period_s: 60.0,
};
