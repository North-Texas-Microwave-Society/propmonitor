//! Verify the q65 crate's RS(63,13) encoder against a q65sim codeword.

use q65::rs::encode;

fn main() {
    let info: [u8; 13] = [2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18];
    let expected: [u8; 63] = [
        2, 27, 55, 35, 20, 6, 5, 9, 55, 0, 33, 22, 18, // message
        42, 63, 28, 8, 23, 17, 17, 8, 38, 37, 22, 31, 17, 23, 45, 45, 59, 31, 9, 40, 63, 57, 56, 57,
        43, 21, 7, 54, 45, 59, 12, 12, 3, 6, 3, 40, 8, 10, 46, 24, 24, 26, 6, 44, 18, 4, 51, 7, 50,
        19,
    ];
    let got = encode(&info);
    let message_matches = got[..13] == info[..];
    let parity_matches = got[13..] == expected[13..];
    println!("message section matches: {}", message_matches);
    println!("parity  section matches: {}", parity_matches);
    if !parity_matches {
        println!("\nfirst 10 expected parity: {:?}", &expected[13..23]);
        println!("first 10 got parity:      {:?}", &got[13..23]);
        let mut diff_count = 0;
        for i in 13..63 {
            if got[i] != expected[i] {
                diff_count += 1;
            }
        }
        println!("total differing parity symbols: {} / 50", diff_count);
    }
}
