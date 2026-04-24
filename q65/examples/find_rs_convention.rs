//! Brute-force search for the (primitive polynomial, first-root index)
//! combination that produces the q65sim codeword from the known info
//! symbols.

use std::convert::TryInto;

// Primitive polynomials of degree 6 over GF(2). Only these give a field
// (irreducible AND primitive). There are phi(63)/6 = 6 primitive polys.
const PRIM_POLYS: &[u16] = &[
    0b1000011, // x^6 + x + 1                   (= 67)
    0b1011011, // x^6 + x^4 + x^3 + x + 1       (= 91)
    0b1100001, // x^6 + x^5 + 1                 (= 97)
    0b1100111, // x^6 + x^5 + x^2 + x + 1       (= 103)
    0b1101101, // x^6 + x^5 + x^3 + x^2 + 1     (= 109)
    0b1110011, // x^6 + x^5 + x^4 + x + 1       (= 115)
];

fn build_tables(prim_poly: u16) -> (Vec<u8>, Vec<u8>) {
    let mut exp = vec![0u8; 126];
    let mut log = vec![0u8; 64];
    let mut x: u16 = 1;
    for (i, slot) in exp.iter_mut().enumerate().take(63) {
        *slot = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x40 != 0 {
            x ^= prim_poly;
        }
    }
    for j in 63..126 {
        exp[j] = exp[j - 63];
    }
    (exp, log)
}

fn mul(a: u8, b: u8, exp: &[u8], log: &[u8]) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let la = log[a as usize] as usize;
    let lb = log[b as usize] as usize;
    exp[la + lb]
}

fn alpha_pow(e: usize, exp: &[u8]) -> u8 {
    exp[e % 63]
}

/// Try to generate the RS(63, 13) parity with the given primitive polynomial,
/// a "generator element" beta = alpha^beta_exp (must be coprime with 63),
/// and first-root index.
fn try_encode(
    info: &[u8; 13],
    prim_poly: u16,
    first_root: usize,
    beta_exp: usize,
) -> [u8; 63] {
    let (exp, log) = build_tables(prim_poly);

    // Generator g(x) = prod_{i=0}^{49} (x - beta^(first_root+i)).
    let parity = 50usize;
    let mut g: Vec<u8> = vec![1];
    for i in 0..parity {
        let root = alpha_pow(beta_exp * (first_root + i), &exp);
        let mut next = vec![0u8; g.len() + 1];
        for (j, &c) in g.iter().enumerate() {
            next[j] ^= mul(c, root, &exp, &log);
            next[j + 1] ^= c;
        }
        g = next;
    }

    // Systematic encode: shift message up by parity, reduce mod g, remainder = parity.
    let mut work = [0u8; 63];
    work[parity..(parity + 13)].copy_from_slice(&info[..13]);
    for i in (parity..63).rev() {
        let coef = work[i];
        if coef != 0 {
            for (j, &gc) in g.iter().enumerate() {
                let idx = i - parity + j;
                work[idx] ^= mul(coef, gc, &exp, &log);
            }
        }
    }

    let mut cw = [0u8; 63];
    cw[..13].copy_from_slice(info);
    // Try both orderings: parity in positions 13..63 (MSB-first, little-endian) or reversed.
    cw[13..].copy_from_slice(&work[..parity]);
    cw
}

fn main() {
    let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
    let expected_parity: [u8; 50] = [
        42, 63, 28, 8, 23, 17, 17, 8, 38, 37, 22, 31, 17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56,
        57, 43, 21, 7, 54, 45, 59, 12, 12, 3, 6, 3, 40, 8, 10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7,
        50, 19,
    ];

    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let mut best: Vec<(u16, usize, usize, bool, usize)> = Vec::new();
    for &pp in PRIM_POLYS {
        for beta_exp in 1..63 {
            if gcd(beta_exp, 63) != 1 {
                continue;
            }
            for first_root in 0..63 {
                let cw = try_encode(&info, pp, first_root, beta_exp);
                let parity: [u8; 50] = cw[13..].try_into().unwrap();
                let n_fwd: usize = parity
                    .iter()
                    .zip(expected_parity.iter())
                    .filter(|(a, b)| a == b)
                    .count();
                let mut rev = expected_parity;
                rev.reverse();
                let n_rev: usize = parity
                    .iter()
                    .zip(rev.iter())
                    .filter(|(a, b)| a == b)
                    .count();
                best.push((pp, beta_exp, first_root, false, n_fwd));
                best.push((pp, beta_exp, first_root, true, n_rev));
            }
        }
    }
    best.sort_by(|a, b| b.4.cmp(&a.4));
    println!("top 10 matches (parity symbols correct out of 50):");
    for &(pp, be, fr, reversed, n) in best.iter().take(10) {
        println!(
            "  prim={:3}  beta=a^{:2}  first_root={:2}  {:8}  {}/50",
            pp,
            be,
            fr,
            if reversed { "reversed" } else { "forward" },
            n
        );
    }

    if let Some(&(pp, be, fr, reversed, _)) = best.iter().find(|e| e.4 == 50) {
        println!(
            "\nEXACT MATCH: prim_poly = {}, beta = alpha^{}, first_root = {}, {}",
            pp,
            be,
            fr,
            if reversed { "reversed" } else { "forward" }
        );
    } else {
        println!("\nno exact match found.");
    }
}
