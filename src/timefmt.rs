//! Tiny date helpers. The headless server only ever needs UTC ISO-8601
//! timestamps, so we compute them manually from Unix seconds rather than
//! pulling in `chrono` for one format.
//!
//! Hand-rolled instead of using libc's `gmtime_r` because that symbol is
//! POSIX-only — Windows MSVC ships `gmtime_s` with a different signature.
//! The math here is ~30 lines and runs identically on every platform.

use std::time::{SystemTime, UNIX_EPOCH};

/// Format Unix seconds as a UTC ISO-8601 string: `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Valid for any non-negative `unix_secs`. Negative inputs (pre-1970) are
/// clamped to the epoch — we never report dates before 1970.
pub fn format_utc_iso8601(unix_secs: i64) -> String {
    let secs = unix_secs.max(0) as u64;
    let (year, month, day) = days_to_ymd((secs / 86_400) as u32);
    let sod = (secs % 86_400) as u32;
    let hour = sod / 3_600;
    let min = (sod % 3_600) / 60;
    let sec = sod % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, sec
    )
}

/// Convert "days since 1970-01-01" to (year, month, day). Handles Gregorian
/// leap years correctly. Algorithm: Howard Hinnant's date-from-days, the
/// one used by C++20 <chrono>.
fn days_to_ymd(days_since_epoch: u32) -> (i32, u32, u32) {
    // Shift to the proleptic Gregorian calendar where year 0 = March-based.
    // Reference: https://howardhinnant.github.io/date_algorithms.html#civil_from_days
    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}

/// UTC seconds since the Unix epoch.
pub fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_as_iso8601() {
        assert_eq!(format_utc_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_a_known_recent_timestamp() {
        // 2026-05-13T15:10:00Z = 1_778_685_000
        assert_eq!(format_utc_iso8601(1_778_685_000), "2026-05-13T15:10:00Z");
    }

    #[test]
    fn handles_leap_day_2024() {
        // 2024-02-29T00:00:00Z = 1_709_164_800
        assert_eq!(format_utc_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn handles_year_2000_century_leap() {
        // 2000-02-29T00:00:00Z = 951_782_400 (2000 is a leap year despite %100)
        assert_eq!(format_utc_iso8601(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn handles_year_2100_non_leap() {
        // 2100-03-01T00:00:00Z = 4_107_542_400 (2100 is NOT a leap year)
        assert_eq!(format_utc_iso8601(4_107_542_400), "2100-03-01T00:00:00Z");
    }

    #[test]
    fn negative_input_clamps_to_epoch() {
        assert_eq!(format_utc_iso8601(-1), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn unix_now_secs_is_after_year_2020() {
        // Sanity: the wall clock is post-2020 anywhere this builds.
        let now = unix_now_secs();
        assert!(now > 1_577_836_800, "got {}", now); // 2020-01-01
    }

    #[test]
    fn formats_now_round_trip() {
        let s = format_utc_iso8601(unix_now_secs());
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20); // "YYYY-MM-DDTHH:MM:SSZ"
    }
}
