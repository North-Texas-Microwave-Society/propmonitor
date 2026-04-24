//! GF(2^6) arithmetic using primitive polynomial p(x) = x^6 + x + 1.
//!
//! Element representation: 6-bit value 0..=63 stored in a `u8`. The
//! primitive element is alpha = x (value 2). `EXP[i] = alpha^i` for
//! i in 0..63 (plus EXP[63..126] duplicated for branchless mul).
//! `LOG[v] = k` such that alpha^k = v, for v in 1..=63. LOG[0] is
//! defined as 0 but is invalid and must not be used.

pub const FIELD_SIZE: usize = 64;
pub const FIELD_ORDER: usize = 63;
pub const PRIMITIVE_POLY: u16 = 0b1000011; // x^6 + x + 1 = 67

pub type Gf = u8;

struct Tables {
    exp: [u8; 126],
    log: [u8; 64],
}

static TABLES: Tables = build_tables();

const fn build_tables() -> Tables {
    let mut exp = [0u8; 126];
    let mut log = [0u8; 64];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 63 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x40 != 0 {
            x ^= PRIMITIVE_POLY;
        }
        i += 1;
    }
    // Duplicate the cycle so mul can skip a modulo.
    let mut j = 63;
    while j < 126 {
        exp[j] = exp[j - 63];
        j += 1;
    }
    Tables { exp, log }
}

#[inline]
pub fn add(a: Gf, b: Gf) -> Gf {
    a ^ b
}

#[inline]
pub fn sub(a: Gf, b: Gf) -> Gf {
    a ^ b
}

#[inline]
pub fn mul(a: Gf, b: Gf) -> Gf {
    if a == 0 || b == 0 {
        return 0;
    }
    let la = TABLES.log[a as usize] as usize;
    let lb = TABLES.log[b as usize] as usize;
    TABLES.exp[la + lb]
}

#[inline]
pub fn inv(a: Gf) -> Gf {
    debug_assert!(a != 0, "GF inv(0) is undefined");
    let la = TABLES.log[a as usize] as usize;
    TABLES.exp[FIELD_ORDER - la]
}

#[inline]
pub fn div(a: Gf, b: Gf) -> Gf {
    debug_assert!(b != 0, "GF div by 0");
    if a == 0 {
        return 0;
    }
    let la = TABLES.log[a as usize] as usize;
    let lb = TABLES.log[b as usize] as usize;
    TABLES.exp[(la + FIELD_ORDER - lb) % FIELD_ORDER]
}

#[inline]
pub fn pow(a: Gf, e: i32) -> Gf {
    if a == 0 {
        return if e == 0 { 1 } else { 0 };
    }
    let la = TABLES.log[a as usize] as i32;
    let k = la * e;
    let k = k.rem_euclid(FIELD_ORDER as i32) as usize;
    TABLES.exp[k]
}

/// `alpha^i` for any integer i.
#[inline]
pub fn alpha_pow(e: i32) -> Gf {
    let k = e.rem_euclid(FIELD_ORDER as i32) as usize;
    TABLES.exp[k]
}

/// Discrete log. Caller must ensure `a != 0`.
#[inline]
pub fn log(a: Gf) -> u8 {
    debug_assert!(a != 0);
    TABLES.log[a as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_is_xor() {
        for a in 0..64u8 {
            for b in 0..64u8 {
                assert_eq!(add(a, b), a ^ b);
            }
        }
    }

    #[test]
    fn mul_commutes() {
        for a in 0..64u8 {
            for b in 0..64u8 {
                assert_eq!(mul(a, b), mul(b, a));
            }
        }
    }

    #[test]
    fn mul_associates() {
        for a in 0..16u8 {
            for b in 0..16u8 {
                for c in 0..16u8 {
                    assert_eq!(mul(mul(a, b), c), mul(a, mul(b, c)));
                }
            }
        }
    }

    #[test]
    fn mul_distributes() {
        for a in 0..16u8 {
            for b in 0..16u8 {
                for c in 0..16u8 {
                    assert_eq!(mul(a, add(b, c)), add(mul(a, b), mul(a, c)));
                }
            }
        }
    }

    #[test]
    fn zero_and_one() {
        for a in 0..64u8 {
            assert_eq!(mul(a, 0), 0);
            assert_eq!(mul(a, 1), a);
            assert_eq!(add(a, 0), a);
        }
    }

    #[test]
    fn inv_roundtrips() {
        for a in 1..64u8 {
            let ai = inv(a);
            assert_eq!(mul(a, ai), 1, "a={} inv(a)={}", a, ai);
        }
    }

    #[test]
    fn div_is_mul_inv() {
        for a in 0..64u8 {
            for b in 1..64u8 {
                assert_eq!(div(a, b), mul(a, inv(b)));
            }
        }
    }

    #[test]
    fn alpha_has_order_63() {
        // alpha^63 == 1 and no smaller positive power equals 1.
        let alpha: Gf = 2;
        let mut x: Gf = 1;
        for i in 1..63 {
            x = mul(x, alpha);
            assert_ne!(x, 1, "alpha^{} = 1 too early", i);
        }
        x = mul(x, alpha);
        assert_eq!(x, 1, "alpha^63 must be 1");
    }

    #[test]
    fn exp_table_covers_field() {
        // alpha^0..alpha^62 must be all 63 nonzero elements.
        let mut seen = [false; 64];
        for i in 0..63 {
            let v = alpha_pow(i);
            assert!(!seen[v as usize], "duplicate alpha^{} = {}", i, v);
            assert_ne!(v, 0);
            seen[v as usize] = true;
        }
        for (v, &s) in seen.iter().enumerate().take(64).skip(1) {
            assert!(s, "missing element {}", v);
        }
    }

    #[test]
    fn pow_matches_repeated_mul() {
        for a in 1..8u8 {
            let mut expected: Gf = 1;
            for e in 0..10 {
                assert_eq!(pow(a, e), expected, "a={} e={}", a, e);
                expected = mul(expected, a);
            }
        }
    }
}
