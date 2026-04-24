//! Q65 encoder/decoder. MVP: Q65-60C only. Architecture is parameterized
//! on `Q65Params` so additional submodes/variants are constants + test
//! fixtures, not a rewrite.
//!
//! See `docs/plans/2026-04-16-q65-decoder-design.md` in the parent repo.

pub mod callhash;
pub mod decode;
pub mod demod;
pub mod encode;
pub mod gf64;
pub mod kv;
pub mod message;
pub mod params;
pub mod bp;
pub mod qra;
pub mod qra_tables;
pub mod rs;
pub mod sync;

pub use decode::{decode, Decode, DecodeError, DecodeSearch};
pub use encode::encode_message;
pub use params::{Q65Params, Submode, Variant, Q65_60C};
