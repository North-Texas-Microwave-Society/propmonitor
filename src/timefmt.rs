//! Tiny `chrono` replacement: just enough to format a `SystemTime` as
//! `HH:MM:SS` in local time and to read the current sub-minute fractional
//! second for UTC alignment. Uses libc's `localtime_r` directly so we
//! don't pull in a calendar crate for a single date format.

use std::os::raw::{c_char, c_int, c_long};
use std::time::{SystemTime, UNIX_EPOCH};

#[repr(C)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: *const c_char,
}

extern "C" {
    fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
}

/// Local wall-clock time captured at construction. We only ever need
/// HH:MM:SS for the UI table, so we store nothing fancier.
#[derive(Debug, Clone, Copy)]
pub struct LocalHms {
    pub h: u8,
    pub m: u8,
    pub s: u8,
}

impl LocalHms {
    pub fn now() -> Self {
        Self::from_unix(unix_now_secs())
    }

    pub fn from_unix(secs: i64) -> Self {
        // SAFETY: localtime_r writes into a stack-owned Tm. The pointer
        // we pass is non-null and well-aligned, and we only read the
        // integer fields that POSIX guarantees are populated.
        unsafe {
            let mut tm: Tm = std::mem::zeroed();
            let res = localtime_r(&secs as *const i64, &mut tm as *mut Tm);
            if res.is_null() {
                // localtime_r failure on a valid time_t is essentially
                // impossible on the platforms we run on. Fall back to
                // UTC HH:MM:SS so we never panic in the UI thread.
                let s = secs.rem_euclid(60) as u8;
                let m = (secs.rem_euclid(3600) / 60) as u8;
                let h = (secs.rem_euclid(86400) / 3600) as u8;
                return LocalHms { h, m, s };
            }
            LocalHms {
                h: tm.tm_hour as u8,
                m: tm.tm_min as u8,
                s: tm.tm_sec as u8,
            }
        }
    }

    pub fn format_hms(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.h, self.m, self.s)
    }
}

/// UTC seconds since the Unix epoch.
pub fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Returns (seconds, subsec_fraction) modulo 60 in UTC. Used by the Q65
/// worker to align the first capture period to the next UTC minute.
pub fn utc_seconds_into_minute() -> f64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = (d.as_secs() % 60) as f64;
    let frac = d.subsec_nanos() as f64 * 1e-9;
    secs + frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_zero_padded_hms() {
        let t = LocalHms { h: 3, m: 4, s: 9 };
        assert_eq!(t.format_hms(), "03:04:09");
    }

    #[test]
    fn from_unix_handles_known_epoch() {
        // 1970-01-01 00:00:00 UTC. Local time will vary by TZ, but we can
        // at least confirm it produces a sane HMS without crashing.
        let t = LocalHms::from_unix(0);
        assert!(t.h < 24);
        assert!(t.m < 60);
        assert!(t.s < 60);
    }

    #[test]
    fn utc_seconds_into_minute_in_range() {
        let f = utc_seconds_into_minute();
        assert!((0.0..60.0).contains(&f));
    }
}
