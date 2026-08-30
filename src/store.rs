//! In-memory ring buffer of recent measurements. 24 h of one-per-minute
//! samples fits in ~1440 entries; rather than parameterizing on time we
//! cap on entry count.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// `Deserialize` because this is a wire type in both directions now: it
/// goes out in `GET /api/measurements` and in the sync status frame
/// (`sync.rs`), and the round-trip tests read it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMeasurement {
    pub measured_at: String, // UTC ISO-8601
    pub noise_floor_dbfs: f64,
    pub signal_peak_dbfs: f64,
    pub signal_avg_dbfs: f64,
    pub snr_peak_db: f64,
    pub snr_avg_db: f64,
    pub signal_active_fraction: f64,
}

/// 24 hours at one measurement per minute. The server cap on `?limit=`
/// matches this so the worst-case response stays bounded.
pub const MAX_ENTRIES: usize = 1_440;

pub struct MeasurementStore {
    entries: VecDeque<StoredMeasurement>,
}

impl Default for MeasurementStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MeasurementStore {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_ENTRIES),
        }
    }

    pub fn push(&mut self, m: StoredMeasurement) {
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(m);
    }

    /// Returns the last `limit` measurements in chronological order
    /// (oldest first). Used by `GET /api/measurements`.
    pub fn recent(&self, limit: usize) -> Vec<StoredMeasurement> {
        let n = limit.min(self.entries.len());
        self.entries
            .iter()
            .skip(self.entries.len() - n)
            .cloned()
            .collect()
    }

    pub fn last(&self) -> Option<StoredMeasurement> {
        self.entries.back().cloned()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(seq: i32) -> StoredMeasurement {
        StoredMeasurement {
            measured_at: format!("2026-05-13T00:{:02}:00Z", seq.rem_euclid(60)),
            noise_floor_dbfs: -110.0,
            signal_peak_dbfs: -90.0 + seq as f64,
            signal_avg_dbfs: -92.0,
            snr_peak_db: 20.0,
            snr_avg_db: 18.0,
            signal_active_fraction: 1.0,
        }
    }

    #[test]
    fn caps_at_max_entries_and_drops_oldest() {
        let mut s = MeasurementStore::new();
        for i in 0..(MAX_ENTRIES as i32 + 10) {
            s.push(fake(i));
        }
        assert_eq!(s.len(), MAX_ENTRIES);
        // The very first entries should be gone; the back is the most
        // recent push.
        let last = s.last().unwrap();
        assert_eq!(last.signal_peak_dbfs, -90.0 + (MAX_ENTRIES as f64 + 9.0));
    }

    #[test]
    fn default_is_equivalent_to_new() {
        let s: MeasurementStore = Default::default();
        assert_eq!(s.len(), 0);
        assert!(s.last().is_none());
        assert!(s.recent(10).is_empty());
    }

    #[test]
    fn recent_with_zero_limit_returns_empty() {
        let mut s = MeasurementStore::new();
        s.push(fake(0));
        assert!(s.recent(0).is_empty());
    }

    #[test]
    fn recent_caps_at_actual_length() {
        let mut s = MeasurementStore::new();
        for i in 0..3 {
            s.push(fake(i));
        }
        // Asking for more than we have returns all of them.
        assert_eq!(s.recent(100).len(), 3);
    }

    #[test]
    fn last_returns_most_recent_push() {
        let mut s = MeasurementStore::new();
        s.push(fake(1));
        s.push(fake(2));
        assert_eq!(s.last().unwrap().signal_peak_dbfs, -90.0 + 2.0);
    }

    #[test]
    fn recent_returns_chronological_order_capped_by_limit() {
        let mut s = MeasurementStore::new();
        for i in 0..10 {
            s.push(fake(i));
        }
        let out = s.recent(3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].signal_peak_dbfs, -90.0 + 7.0);
        assert_eq!(out[2].signal_peak_dbfs, -90.0 + 9.0);
    }
}
