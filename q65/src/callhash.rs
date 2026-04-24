//! Rolling callsign hash table for i3=4 (nonstandard hashed callsigns).
//!
//! WSJT-X maintains a table mapping 22-/12-/10-bit hashes of full
//! callsigns to the callsigns themselves. When an i3=4 message carries
//! a hashed callsign, the UI shows the real callsign if it's been seen
//! recently — the hash itself is one-way.
//!
//! STATUS: minimal — 22-bit hash only, insert and lookup, LRU-evicted
//! at capacity.

use std::collections::VecDeque;

#[derive(Default)]
pub struct CallhashTable {
    entries: VecDeque<(u32, String)>,
    capacity: usize,
}

impl CallhashTable {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    pub fn insert(&mut self, call: &str) {
        let h = hash22(call);
        // Move-to-front if already present.
        if let Some(pos) = self.entries.iter().position(|(hh, _)| *hh == h) {
            self.entries.remove(pos);
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_back();
        }
        self.entries.push_front((h, call.to_string()));
    }

    pub fn lookup22(&self, h: u32) -> Option<&str> {
        self.entries
            .iter()
            .find(|(hh, _)| *hh == h)
            .map(|(_, s)| s.as_str())
    }
}

/// 22-bit hash of a callsign. WSJT-X uses a specific multiply-and-mask
/// construction; the exact constants must match for cross-compatibility
/// (tracked in Stage 6). This implementation is a stable placeholder:
/// FNV-1a folded to 22 bits. Callers MUST NOT rely on exact hash values
/// for on-air interop until this is switched to the WSJT-X hash.
pub fn hash22(call: &str) -> u32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in call.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h as u32) & 0x003F_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_lookup_roundtrip() {
        let mut t = CallhashTable::new(4);
        t.insert("K1ABC");
        let h = hash22("K1ABC");
        assert_eq!(t.lookup22(h), Some("K1ABC"));
    }

    #[test]
    fn lru_eviction() {
        let mut t = CallhashTable::new(2);
        t.insert("K1ABC");
        t.insert("W2DEF");
        t.insert("VE7XYZ"); // evicts K1ABC
        assert_eq!(t.lookup22(hash22("K1ABC")), None);
        assert_eq!(t.lookup22(hash22("W2DEF")), Some("W2DEF"));
        assert_eq!(t.lookup22(hash22("VE7XYZ")), Some("VE7XYZ"));
    }

    #[test]
    fn hash22_is_22_bits() {
        let h = hash22("K1ABC");
        assert!(h < (1 << 22));
    }
}
