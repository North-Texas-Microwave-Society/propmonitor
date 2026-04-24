//! 77-bit message pack/unpack for Q65 (shared with FT8).
//!
//! STATUS: functional for i3=1 standard messages and i3=0 n3=0 free-text.
//! i3=4 (nonstandard hashed callsigns) is handled as a passthrough —
//! real hashed-callsign resolution requires a rolling callhash table
//! (see `callhash`). CRC handling is stubbed: we pack/unpack 13 six-bit
//! RS symbols from 78 bits (77 payload + 1 trailing zero). Cross-check
//! against WSJT-X reference captures is tracked in Stage 6.

use crate::gf64::Gf;
use crate::rs::K;

pub const PAYLOAD_BITS: usize = 77;

/// 77 payload bits, packed into 10 bytes, MSB-first, bit 77+ = 0.
pub type Payload77 = [u8; 10];

/// Convert a 77-bit payload into 13 six-bit RS(63,13) information symbols.
/// Layout: 77 payload bits, then 1 tail bit (zero). Symbol 0 is the high-order
/// six bits of the packed sequence.
pub fn payload_to_rs_symbols(payload: &Payload77) -> [Gf; K] {
    let mut out = [0u8; K];
    for (i, v) in out.iter_mut().enumerate() {
        let bit_start = i * 6;
        *v = read_bits(payload, bit_start, 6);
    }
    out
}

/// Inverse of `payload_to_rs_symbols`: 13 symbols → 77-bit payload + 1 tail bit.
pub fn rs_symbols_to_payload(symbols: &[Gf; K]) -> Payload77 {
    let mut out = [0u8; 10];
    for (i, &s) in symbols.iter().enumerate() {
        write_bits(&mut out, i * 6, 6, s);
    }
    out
}

fn read_bits(buf: &[u8], start_bit: usize, n: usize) -> u8 {
    let mut v = 0u16;
    for i in 0..n {
        let bit = start_bit + i;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = (buf[byte] >> off) & 1;
        v = (v << 1) | b as u16;
    }
    v as u8
}

fn write_bits(buf: &mut [u8], start_bit: usize, n: usize, value: u8) {
    for i in 0..n {
        let bit = start_bit + i;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = (value >> (n - 1 - i)) & 1;
        buf[byte] &= !(1 << off);
        buf[byte] |= b << off;
    }
}

/// Read an N-bit integer (up to 64 bits) from a bit buffer, MSB-first.
fn read_u64(buf: &[u8], start_bit: usize, n: usize) -> u64 {
    debug_assert!(n <= 64);
    let mut v = 0u64;
    for i in 0..n {
        let bit = start_bit + i;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = (buf[byte] >> off) & 1;
        v = (v << 1) | b as u64;
    }
    v
}

/// Decoded message as a structured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// i3=1, the standard CALL1 CALL2 {GRID|REPORT|RRR|RR73|73} message.
    Standard {
        call1: String,
        call2: String,
        extra: StandardExtra,
    },
    /// i3=0 n3=0: arbitrary short text, up to 13 characters.
    FreeText(String),
    /// i3=4: nonstandard callsigns with hashed references. Real resolution
    /// requires a callhash table; for MVP we render `<h12>` placeholders.
    Nonstandard { raw: String },
    /// Anything else (i3 = 2,3,5,...) — rendered as hex for debugging.
    Unsupported { i3: u8, raw: [u8; 10] },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandardExtra {
    Grid(String),          // 4-char Maidenhead
    Report(i8),            // dB, signed
    Rogered(i8),           // R-dB
    Rrr,
    Rr73,
    Seventy3,
    /// Reserved / unrecognized 15-bit field.
    Raw(u16),
}

pub fn format_message(msg: &Message) -> String {
    match msg {
        Message::Standard { call1, call2, extra } => {
            let tail = match extra {
                StandardExtra::Grid(g) => g.clone(),
                StandardExtra::Report(r) => format!("{:+03}", r),
                StandardExtra::Rogered(r) => format!("R{:+03}", r),
                StandardExtra::Rrr => "RRR".into(),
                StandardExtra::Rr73 => "RR73".into(),
                StandardExtra::Seventy3 => "73".into(),
                StandardExtra::Raw(v) => format!("?{}", v),
            };
            format!("{} {} {}", call1, call2, tail)
        }
        Message::FreeText(s) => s.clone(),
        Message::Nonstandard { raw } => raw.clone(),
        Message::Unsupported { i3, raw } => {
            format!("[i3={} raw={:02x?}]", i3, raw)
        }
    }
}

/// Unpack a 77-bit payload into a Message.
pub fn unpack(payload: &Payload77) -> Message {
    let i3 = read_u64(payload, 0, 3) as u8;
    match i3 {
        0 => {
            // Free-text subfield layout: 3 bits i3, 3 bits n3, then body.
            let n3 = read_u64(payload, 3, 3) as u8;
            if n3 == 0 {
                Message::FreeText(unpack_free_text(payload))
            } else {
                Message::Unsupported {
                    i3,
                    raw: *payload,
                }
            }
        }
        1 | 2 => {
            // Standard message. For both i3=1 and (legacy) i3=2: c28 r1 c28 r1 R1 g15.
            let c1 = read_u64(payload, 3, 28) as u32;
            let _r1 = read_u64(payload, 31, 1) as u8;
            let c2 = read_u64(payload, 32, 28) as u32;
            let _r2 = read_u64(payload, 60, 1) as u8;
            let _big_r = read_u64(payload, 61, 1) as u8;
            let g15 = read_u64(payload, 62, 15) as u16;
            Message::Standard {
                call1: unpack_c28(c1),
                call2: unpack_c28(c2),
                extra: unpack_g15(g15),
            }
        }
        4 => {
            // Hashed-callsign message: placeholder rendering.
            Message::Nonstandard {
                raw: format!("<{:010x?}>", payload),
            }
        }
        _ => Message::Unsupported {
            i3,
            raw: *payload,
        },
    }
}

pub fn pack_standard(call1: &str, call2: &str, extra: &StandardExtra) -> Payload77 {
    let mut p = [0u8; 10];
    let c1 = pack_c28(call1);
    let c2 = pack_c28(call2);
    let g15 = pack_g15(extra);
    write_bits_u64(&mut p, 0, 3, 1); // i3=1
    write_bits_u64(&mut p, 3, 28, c1 as u64);
    write_bits_u64(&mut p, 31, 1, 0);
    write_bits_u64(&mut p, 32, 28, c2 as u64);
    write_bits_u64(&mut p, 60, 1, 0);
    write_bits_u64(&mut p, 61, 1, 0);
    write_bits_u64(&mut p, 62, 15, g15 as u64);
    p
}

pub fn pack_free_text(text: &str) -> Payload77 {
    let mut p = [0u8; 10];
    write_bits_u64(&mut p, 0, 3, 0); // i3=0
    write_bits_u64(&mut p, 3, 3, 0); // n3=0
    let chars = encode_free_text_13(text);
    // Pack 13 characters * 42 symbols -> 71-bit integer.
    let mut acc: u128 = 0;
    for c in chars {
        acc = acc * 42 + c as u128;
    }
    // Write 71 bits starting at bit 6.
    for i in 0..71 {
        let bit = 6 + i;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = ((acc >> (70 - i)) & 1) as u8;
        p[byte] |= b << off;
    }
    p
}

fn unpack_free_text(payload: &Payload77) -> String {
    let mut acc: u128 = 0;
    for i in 0..71 {
        let bit = 6 + i;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = ((payload[byte] >> off) & 1) as u128;
        acc = (acc << 1) | b;
    }
    // Extract 13 base-42 symbols, LSB-first (so reverse at the end).
    let mut chars = Vec::with_capacity(13);
    for _ in 0..13 {
        let c = (acc % 42) as u8;
        acc /= 42;
        chars.push(c);
    }
    chars.reverse();
    let s: String = chars.into_iter().map(decode_free_text_char).collect();
    s.trim().to_string()
}

// 42-character alphabet used by FT8/Q65 free-text.
// Space, 0-9, A-Z, plus 5 punctuation marks. Exact ordering matches WSJT-X.
const FREE_ALPHABET: &[u8; 42] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";

fn encode_free_text_char(c: char) -> u8 {
    let c = c.to_ascii_uppercase() as u8;
    FREE_ALPHABET
        .iter()
        .position(|&b| b == c)
        .unwrap_or(0) as u8
}

fn decode_free_text_char(i: u8) -> char {
    FREE_ALPHABET[(i as usize).min(41)] as char
}

fn encode_free_text_13(text: &str) -> [u8; 13] {
    let mut out = [0u8; 13];
    for (i, c) in text.chars().take(13).enumerate() {
        out[i] = encode_free_text_char(c);
    }
    out
}

// --- 28-bit compressed callsign (subset of FT8's c28). -------------

// FT8/Q65 compressed-callsign alphabet elements.
//   A0 = ' ' + 0-9 + A-Z  (37 chars; used for char 0)
//   A1 = 0-9 + A-Z        (36 chars; used for char 1)
//   A2 = 0-9              (10 chars; used for char 2)
//   A3 = ' ' + A-Z        (27 chars; used for chars 3-5)
// Callsign packed as: n = (((c0*36 + c1)*10 + c2)*27 + c3)*27 + c4)*27 + c5.
const CALL_A0: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 37
const CALL_A1: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 36
const CALL_A2: &[u8] = b"0123456789"; // 10
const CALL_A3: &[u8] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 27

pub fn pack_c28(call: &str) -> u32 {
    let up = call.to_ascii_uppercase();
    // Special tokens.
    if up == "DE" {
        return 0;
    }
    if up == "QRZ" {
        return 1;
    }
    if up.starts_with("CQ ") || up == "CQ" {
        let tail = up.strip_prefix("CQ").unwrap().trim();
        if tail.is_empty() {
            return 2; // bare CQ
        }
        if let Ok(n) = tail.parse::<u32>() {
            if n < 1000 {
                return 3 + n;
            }
        }
        // CQ with 1-4 letter directional tag.
        if tail.len() <= 4 && tail.chars().all(|c| c.is_ascii_uppercase()) {
            let mut v = 0u32;
            let pad = 4 - tail.len();
            let padded: String = std::iter::repeat_n(' ', pad).chain(tail.chars()).collect();
            for c in padded.chars() {
                let ci = CALL_A3
                    .iter()
                    .position(|&b| b == c as u8)
                    .unwrap_or(0) as u32;
                v = v * 27 + ci;
            }
            return 3 + 1000 + v;
        }
    }
    // Plain callsign: pad to exactly 6 characters.
    let padded = pad_call(&up);
    pack_standard_call(&padded).wrapping_add(2063592) // offset past reserved tokens
}

fn pack_standard_call(call: &[u8; 6]) -> u32 {
    let c0 = CALL_A0.iter().position(|&b| b == call[0]).unwrap_or(0) as u32;
    let c1 = CALL_A1.iter().position(|&b| b == call[1]).unwrap_or(0) as u32;
    let c2 = CALL_A2.iter().position(|&b| b == call[2]).unwrap_or(0) as u32;
    let c3 = CALL_A3.iter().position(|&b| b == call[3]).unwrap_or(0) as u32;
    let c4 = CALL_A3.iter().position(|&b| b == call[4]).unwrap_or(0) as u32;
    let c5 = CALL_A3.iter().position(|&b| b == call[5]).unwrap_or(0) as u32;
    ((((c0 * 36 + c1) * 10 + c2) * 27 + c3) * 27 + c4) * 27 + c5
}

fn pad_call(call: &str) -> [u8; 6] {
    // Standard callsigns have the digit in position 2 or 3 (e.g. K1ABC, WA2DEF, W100XYZ...).
    // For MVP just right-pad with spaces to 6 characters, and left-pad with a space if
    // the first character is a digit.
    let bytes: Vec<u8> = call.bytes().collect();
    let mut out = [b' '; 6];
    // Find digit position; assume standard 1x2 or 2x2/2x3 call with digit in the middle.
    let digit_pos = bytes.iter().position(|&b| b.is_ascii_digit()).unwrap_or(0);
    // If digit is at position 0 or 1, shift right so digit lands at position 2.
    let shift = 2_usize.saturating_sub(digit_pos);
    for (i, &b) in bytes.iter().enumerate() {
        let pos = i + shift;
        if pos < 6 {
            out[pos] = b;
        }
    }
    out
}

fn unpack_c28(v: u32) -> String {
    match v {
        0 => "DE".into(),
        1 => "QRZ".into(),
        2 => "CQ".into(),
        3..=1002 => format!("CQ {}", v - 3),
        1003..=2063591 => {
            let x = v - 1003;
            let mut cs = [b' '; 4];
            let mut y = x;
            for i in (0..4).rev() {
                cs[i] = CALL_A3[(y % 27) as usize];
                y /= 27;
            }
            let tag: String = cs.iter().map(|&b| b as char).collect();
            format!("CQ {}", tag.trim())
        }
        _ => {
            let x = v.saturating_sub(2063592);
            let mut y = x;
            let c5 = CALL_A3[(y % 27) as usize];
            y /= 27;
            let c4 = CALL_A3[(y % 27) as usize];
            y /= 27;
            let c3 = CALL_A3[(y % 27) as usize];
            y /= 27;
            let c2 = CALL_A2[(y % 10) as usize];
            y /= 10;
            let c1 = CALL_A1[(y % 36) as usize];
            y /= 36;
            let c0 = CALL_A0[(y % 37) as usize];
            let call: String = [c0, c1, c2, c3, c4, c5].iter().map(|&b| b as char).collect();
            call.trim().to_string()
        }
    }
}

// --- 15-bit "grid or report" field. ---------------------------------

pub fn pack_g15(extra: &StandardExtra) -> u16 {
    match *extra {
        StandardExtra::Grid(ref g) => {
            // 4-char Maidenhead: [A-R][A-R][0-9][0-9].
            if g.len() == 4 {
                let b = g.as_bytes();
                if (b'A'..=b'R').contains(&b[0])
                    && (b'A'..=b'R').contains(&b[1])
                    && b[2].is_ascii_digit()
                    && b[3].is_ascii_digit()
                {
                    let v = ((b[0] - b'A') as u32) * 18 * 10 * 10
                        + ((b[1] - b'A') as u32) * 10 * 10
                        + ((b[2] - b'0') as u32) * 10
                        + (b[3] - b'0') as u32;
                    return v as u16;
                }
            }
            32400 // reserved
        }
        StandardExtra::Report(r) => (32400 + clamp_report(r) as i32 + 30) as u16,
        StandardExtra::Rogered(r) => (32400 + 62 + clamp_report(r) as i32 + 30) as u16,
        StandardExtra::Rrr => 32400 + 124,
        StandardExtra::Rr73 => 32400 + 125,
        StandardExtra::Seventy3 => 32400 + 126,
        StandardExtra::Raw(v) => v,
    }
}

fn clamp_report(r: i8) -> i8 {
    r.clamp(-30, 30)
}

pub fn unpack_g15(v: u16) -> StandardExtra {
    let v = v as i32;
    if v < 32400 {
        // Grid.
        let a = (v / (18 * 10 * 10)) as u8;
        let b = ((v / (10 * 10)) % 18) as u8;
        let c = ((v / 10) % 10) as u8;
        let d = (v % 10) as u8;
        if a < 18 && b < 18 {
            let g: String = [b'A' + a, b'A' + b, b'0' + c, b'0' + d]
                .iter()
                .map(|&b| b as char)
                .collect();
            return StandardExtra::Grid(g);
        }
    }
    let t = v - 32400;
    match t {
        0..=61 => StandardExtra::Report((t - 30) as i8),
        62..=123 => StandardExtra::Rogered((t - 62 - 30) as i8),
        124 => StandardExtra::Rrr,
        125 => StandardExtra::Rr73,
        126 => StandardExtra::Seventy3,
        _ => StandardExtra::Raw(v as u16),
    }
}

fn write_bits_u64(buf: &mut [u8], start_bit: usize, n: usize, value: u64) {
    for i in 0..n {
        let bit = start_bit + i;
        let byte = bit / 8;
        let off = 7 - (bit % 8);
        let b = ((value >> (n - 1 - i)) & 1) as u8;
        buf[byte] &= !(1 << off);
        buf[byte] |= b << off;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_bits_roundtrip() {
        let mut buf = [0u8; 10];
        write_bits(&mut buf, 0, 6, 0b101010);
        assert_eq!(read_bits(&buf, 0, 6), 0b101010);
        write_bits(&mut buf, 3, 6, 0b010101);
        assert_eq!(read_bits(&buf, 3, 6), 0b010101);
    }

    #[test]
    fn symbol_roundtrip() {
        let mut p = [0u8; 10];
        p[0] = 0xAB;
        p[1] = 0xCD;
        p[2] = 0xEF;
        p[3] = 0x12;
        p[4] = 0x34;
        p[5] = 0x56;
        p[6] = 0x78;
        p[7] = 0x9A;
        p[8] = 0xBC;
        p[9] = 0b11100000;
        let sy = payload_to_rs_symbols(&p);
        for &v in sy.iter() {
            assert!(v < 64);
        }
        let back = rs_symbols_to_payload(&sy);
        assert_eq!(back, p);
    }

    #[test]
    fn c28_roundtrip_simple() {
        // Standard FT8 callsign shape: up to 2-char prefix, 1 digit, up to
        // 3-char suffix, total <= 6. "N0CALL" (4-char suffix) is nonstandard
        // and would be sent as i3=4, not packed here.
        for call in ["K1ABC", "W2DEF", "VE7XYZ", "N0CAL", "WA2DEF"] {
            let v = pack_c28(call);
            let back = unpack_c28(v);
            assert_eq!(back, call, "call = {}", call);
        }
    }

    #[test]
    fn g15_roundtrip() {
        let cases = [
            StandardExtra::Grid("FN42".into()),
            StandardExtra::Grid("AA00".into()),
            StandardExtra::Grid("RR99".into()),
            StandardExtra::Report(-18),
            StandardExtra::Report(0),
            StandardExtra::Report(15),
            StandardExtra::Rogered(-5),
            StandardExtra::Rrr,
            StandardExtra::Rr73,
            StandardExtra::Seventy3,
        ];
        for c in cases.iter() {
            let v = pack_g15(c);
            let back = unpack_g15(v);
            assert_eq!(&back, c, "case = {:?}", c);
        }
    }

    #[test]
    fn standard_roundtrip() {
        let msg = Message::Standard {
            call1: "K1ABC".into(),
            call2: "W2DEF".into(),
            extra: StandardExtra::Grid("FN42".into()),
        };
        let packed = pack_standard("K1ABC", "W2DEF", &StandardExtra::Grid("FN42".into()));
        let back = unpack(&packed);
        assert_eq!(back, msg);
    }

    #[test]
    fn free_text_roundtrip() {
        for text in ["HELLO WORLD", "CQ DX 50211", "TEST 123"] {
            let p = pack_free_text(text);
            let back = unpack(&p);
            if let Message::FreeText(s) = back {
                assert_eq!(s.trim_end(), text.trim_end());
            } else {
                panic!("expected FreeText, got {:?}", back);
            }
        }
    }

    #[test]
    fn free_text_alphabet_coverage() {
        // All 42 characters round-trip through one encode/decode.
        for i in 0..42u8 {
            assert_eq!(encode_free_text_char(decode_free_text_char(i)), i);
        }
    }
}
