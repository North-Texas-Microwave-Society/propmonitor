//! Periodic POST to microwaveprop.
//!
//! Subscribes to the `WsEvent::Measurement` broadcast stream, attaches the
//! beacon callsign + tuned frequency + passband from config, and uploads
//! one measurement per POST. Failures go into a bounded retry queue with
//! exponential backoff capped at 5 minutes.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::server::WsEvent;
use crate::store::MAX_ENTRIES;

/// Single source of truth for the microwaveprop ingest URL. Hardcoded
/// because every propmonitor install reports to the same server — only
/// the per-station `monitor_token` and `beacon_callsign` vary. Edit
/// this constant (and rebuild) to point at a different server.
pub const MICROWAVEPROP_ENDPOINT: &str =
    "https://prop.w5isp.com/api/v1/beacon-monitor/measurements";

/// Re-broadcasted status of each upload attempt — the server forwards these
/// to WebSocket clients and updates the status endpoint.
#[derive(Debug, Clone)]
pub struct UploadEvent {
    pub at: String,
    pub ok: bool,
    pub queued: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploaderStatus {
    pub enabled: bool,
    pub last_post_at: Option<String>,
    pub last_status: Option<String>,
    pub queued: usize,
}

/// Outcome class for a single POST attempt — pulled out of [`post_one`] so
/// the dispatch logic is testable without standing up an HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseClass {
    /// 2xx — measurement accepted.
    Ok,
    /// Non-429 4xx — body or auth bad; never going to succeed without
    /// operator intervention. Drop the head of the queue.
    Permanent,
    /// 5xx, network error, timeout — try again after backoff.
    Transient,
}

/// Decide what to do with an HTTP response status. Network errors and
/// timeouts get classified by callers as `Transient`.
pub(crate) fn classify_status(status: reqwest::StatusCode) -> ResponseClass {
    if status.is_success() {
        ResponseClass::Ok
    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        ResponseClass::Transient
    } else if status.is_client_error() {
        ResponseClass::Permanent
    } else {
        ResponseClass::Transient
    }
}

/// Whether the configured microwaveprop block is fully populated enough
/// to attempt uploads. Used both at queue-drain time and at enqueue time.
pub(crate) fn should_upload(mw: &crate::config::MicrowavepropConfig) -> bool {
    mw.enabled
        && !mw.monitor_token.is_empty()
        && !mw.beacon_id.is_empty()
        && !mw.gridsquare.is_empty()
}

/// Pick the gain value to stamp onto an upload. Prefers the SDR's
/// actually-reported `actual_gain` (post-AGC if applicable). Falls back
/// to the configured value, then to 0 — the server should treat 0 as
/// "no reading available" rather than literally 0 dB.
pub(crate) fn gain_from_device_info(dev: Option<&WsEvent>, cfg_gain: Option<f64>) -> f64 {
    match dev {
        Some(WsEvent::DeviceInfo { actual_gain, .. }) => *actual_gain,
        _ => cfg_gain.unwrap_or(0.0),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WireMeasurement {
    /// UUID of the beacon being monitored. Canonical key on the
    /// microwaveprop side.
    pub(crate) beacon_id: String,
    /// Maidenhead grid square of the receiver station. Used by the
    /// server to correlate signal level with propagation-path distance
    /// and bearing.
    pub(crate) gridsquare: String,
    pub(crate) frequency_hz: u64,
    pub(crate) measured_at: String,
    pub(crate) integration_s: u32,
    pub(crate) passband_hz: f64,
    /// Actual SDR gain at the time of measurement (reported by the
    /// device, not the configured value). Lets the server rebaseline
    /// dBFS trends across operator gain changes.
    pub(crate) gain_db: f64,
    pub(crate) noise_floor_dbfs: f64,
    pub(crate) signal_peak_dbfs: f64,
    pub(crate) signal_avg_dbfs: f64,
    pub(crate) snr_peak_db: f64,
    pub(crate) snr_avg_db: f64,
    /// Fraction of frames in the integration window where in-band power
    /// exceeded `noise + 3 dB`. Encodes duty cycle so the server can
    /// distinguish a 30%-keyed CW beacon from a continuous Q65
    /// transmission and weight or filter accordingly.
    pub(crate) signal_active_fraction: f64,
    /// Build version string of this propmonitor instance — diagnostic
    /// only; helps the server correlate stat-distribution shifts with
    /// client upgrades.
    pub(crate) propmonitor_version: String,
}

/// Build the wire payload from the active config + a measurement event.
/// Pure function — no I/O, no async — so it can be unit-tested.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_wire_measurement(
    cfg: &Config,
    mw: &crate::config::MicrowavepropConfig,
    actual_gain_db: f64,
    measured_at: String,
    noise_floor_dbfs: f64,
    signal_peak_dbfs: f64,
    signal_avg_dbfs: f64,
    snr_peak_db: f64,
    snr_avg_db: f64,
    signal_active_fraction: f64,
) -> WireMeasurement {
    WireMeasurement {
        beacon_id: mw.beacon_id.clone(),
        gridsquare: mw.gridsquare.clone(),
        frequency_hz: cfg.frequency as u64,
        measured_at,
        integration_s: cfg.period_seconds,
        passband_hz: crate::config::passband_for(cfg.mode, cfg.beacon.as_ref()).1,
        gain_db: actual_gain_db,
        noise_floor_dbfs,
        signal_peak_dbfs,
        signal_avg_dbfs,
        snr_peak_db,
        snr_avg_db,
        signal_active_fraction,
        propmonitor_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

pub fn new_event_channel() -> (
    broadcast::Sender<UploadEvent>,
    broadcast::Receiver<UploadEvent>,
) {
    let (tx, rx) = broadcast::channel::<UploadEvent>(64);
    (tx, rx)
}

/// Long-running task. Spawn this with `tokio::spawn`. When `microwaveprop`
/// is not configured the task immediately marks the uploader disabled and
/// drains the measurement stream without doing anything else.
pub async fn run(
    cfg: Arc<RwLock<Config>>,
    device_info: Arc<RwLock<Option<WsEvent>>>,
    measurements_rx: broadcast::Receiver<WsEvent>,
    upload_tx: broadcast::Sender<UploadEvent>,
    status: Arc<RwLock<UploaderStatus>>,
) {
    let mut measurements_rx = measurements_rx;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client");

    let mut queue: VecDeque<WireMeasurement> = VecDeque::with_capacity(MAX_ENTRIES);
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(300);

    loop {
        // First, drain the queue if anything is waiting and the uploader
        // is configured. We try one POST per tick to avoid starving the
        // event loop.
        if !queue.is_empty() {
            let token = {
                let c = cfg.read().await;
                c.microwaveprop
                    .as_ref()
                    .filter(|m| should_upload(m))
                    .map(|m| m.monitor_token.clone())
            };
            if let (Some(token), Some(m)) = (token, queue.front().cloned()) {
                match post_one(&client, MICROWAVEPROP_ENDPOINT, &token, &m).await {
                    Ok(()) => {
                        queue.pop_front();
                        backoff = Duration::from_secs(1);
                        emit(
                            &upload_tx,
                            &status,
                            UploadEvent {
                                at: now_iso(),
                                ok: true,
                                queued: queue.len(),
                            },
                        )
                        .await;
                    }
                    Err(transient) => {
                        emit(
                            &upload_tx,
                            &status,
                            UploadEvent {
                                at: now_iso(),
                                ok: false,
                                queued: queue.len(),
                            },
                        )
                        .await;
                        if !transient {
                            // Permanent failure (non-429 4xx) — drop the
                            // head so we don't retry it forever.
                            queue.pop_front();
                            backoff = Duration::from_secs(1);
                            continue;
                        }
                        if backoff_and_drain(
                            &cfg,
                            &device_info,
                            &status,
                            &mut queue,
                            &mut measurements_rx,
                            backoff,
                        )
                        .await
                        {
                            break;
                        }
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                }
            }
        }

        // Then wait for the next measurement event or a short tick.
        let recv = tokio::time::timeout(Duration::from_secs(5), measurements_rx.recv()).await;
        match recv {
            Ok(Ok(ev)) => {
                enqueue_measurement(&cfg, &device_info, &status, &mut queue, ev).await;
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_timeout) => continue, // tick to retry queue
        }
    }
}

/// Build and enqueue one measurement onto the retry queue, stamping the
/// uploader's enabled state from the live config. A no-op for any frame that
/// is not a completed `Measurement` (waterfall/raw frames share the channel).
async fn enqueue_measurement(
    cfg: &Arc<RwLock<Config>>,
    device_info: &Arc<RwLock<Option<WsEvent>>>,
    status: &Arc<RwLock<UploaderStatus>>,
    queue: &mut VecDeque<WireMeasurement>,
    ev: WsEvent,
) {
    let WsEvent::Measurement {
        measured_at,
        noise_floor_dbfs,
        signal_peak_dbfs,
        signal_avg_dbfs,
        snr_peak_db,
        snr_avg_db,
        signal_active_fraction,
    } = ev
    else {
        return;
    };
    let c = cfg.read().await;
    let Some(mw) = c.microwaveprop.as_ref().filter(|m| should_upload(m)) else {
        let mut s = status.write().await;
        s.enabled = false;
        return;
    };
    {
        let mut s = status.write().await;
        s.enabled = true;
    }
    let gain_db = gain_from_device_info(device_info.read().await.as_ref(), c.gain);
    let m = build_wire_measurement(
        &c,
        mw,
        gain_db,
        measured_at,
        noise_floor_dbfs,
        signal_peak_dbfs,
        signal_avg_dbfs,
        snr_peak_db,
        snr_avg_db,
        signal_active_fraction,
    );
    if queue.len() >= MAX_ENTRIES {
        queue.pop_front();
    }
    queue.push_back(m);
}

/// Wait out a transient-failure `backoff`, but keep reading `measurements_rx`
/// and enqueuing measurements the whole time.
///
/// The retry queue only earns its keep if an outage preserves the datapoints
/// generated during it. `measurements_rx` is a shared broadcast that also
/// carries high-rate waterfall/raw frames, so a plain `sleep` here would let
/// its capacity overflow within seconds and drop the very measurements the
/// outage produced. Returns `true` when the broadcast is closed (the task
/// should end).
async fn backoff_and_drain(
    cfg: &Arc<RwLock<Config>>,
    device_info: &Arc<RwLock<Option<WsEvent>>>,
    status: &Arc<RwLock<UploaderStatus>>,
    queue: &mut VecDeque<WireMeasurement>,
    measurements_rx: &mut broadcast::Receiver<WsEvent>,
    backoff: Duration,
) -> bool {
    let deadline = tokio::time::sleep(backoff);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return false,
            recv = measurements_rx.recv() => match recv {
                Ok(ev) => enqueue_measurement(cfg, device_info, status, queue, ev).await,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return true,
            },
        }
    }
}

/// POST one measurement. Returns `Ok(())` on 2xx. Returns `Err(true)` for
/// transient failures (5xx, timeout, network) — keep in queue. Returns
/// `Err(false)` for permanent failures (non-429 4xx) — drop from queue.
async fn post_one(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    body: &WireMeasurement,
) -> Result<(), bool> {
    let resp = client
        .post(endpoint)
        .bearer_auth(token)
        .json(body)
        .send()
        .await;
    match resp {
        Ok(r) => match classify_status(r.status()) {
            ResponseClass::Ok => Ok(()),
            ResponseClass::Permanent => Err(false),
            ResponseClass::Transient => Err(true),
        },
        Err(_) => Err(true),
    }
}

async fn emit(
    tx: &broadcast::Sender<UploadEvent>,
    status: &Arc<RwLock<UploaderStatus>>,
    ev: UploadEvent,
) {
    {
        let mut s = status.write().await;
        s.last_post_at = Some(ev.at.clone());
        s.last_status = Some(if ev.ok {
            "ok".to_string()
        } else {
            "error".to_string()
        });
        s.queued = ev.queued;
    }
    let _ = tx.send(ev);
}

fn now_iso() -> String {
    crate::timefmt::format_utc_iso8601(crate::timefmt::unix_now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BeaconConfig, HttpConfig, MicrowavepropConfig, Mode};
    use reqwest::StatusCode;

    fn sample_cfg() -> Config {
        Config {
            frequency: 28_330_000.0,
            mode: Mode::Beacon,
            sample_rate: 250_000.0,
            gain: Some(10.0),
            driver: "rtlsdr".to_string(),
            ppm: 0.0,
            period_seconds: 60,
            beacon: Some(BeaconConfig {
                offset_hz: 0.0,
                bandwidth_hz: 300.0,
            }),
            http: HttpConfig {
                bind: "0.0.0.0:5760".to_string(),
            },
            microwaveprop: None,
            update: crate::config::UpdateConfig::default(),
        }
    }

    fn sample_mw() -> MicrowavepropConfig {
        MicrowavepropConfig {
            config_version: 0,
            enabled: true,
            monitor_token: "token-123".to_string(),
            beacon_id: "00000000-0000-0000-0000-000000000001".to_string(),
            gridsquare: "EM12il".to_string(),
        }
    }

    #[test]
    fn classify_status_categorizes_each_response_class() {
        assert_eq!(classify_status(StatusCode::NO_CONTENT), ResponseClass::Ok);
        assert_eq!(classify_status(StatusCode::OK), ResponseClass::Ok);
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST),
            ResponseClass::Permanent
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            ResponseClass::Transient
        );
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            ResponseClass::Permanent
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            ResponseClass::Transient
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            ResponseClass::Transient
        );
        // Anything else (1xx/3xx) lands in transient — we'll try again
        // rather than dropping a measurement we can't classify.
        assert_eq!(classify_status(StatusCode::FOUND), ResponseClass::Transient);
    }

    #[test]
    fn should_upload_requires_all_fields() {
        let full = sample_mw();
        assert!(should_upload(&full));

        let disabled = MicrowavepropConfig {
            enabled: false,
            ..full.clone()
        };
        assert!(!should_upload(&disabled));

        let no_token = MicrowavepropConfig {
            monitor_token: String::new(),
            ..full.clone()
        };
        assert!(!should_upload(&no_token));

        let no_beacon = MicrowavepropConfig {
            beacon_id: String::new(),
            ..full.clone()
        };
        assert!(!should_upload(&no_beacon));

        let no_gridsquare = MicrowavepropConfig {
            gridsquare: String::new(),
            ..full
        };
        assert!(!should_upload(&no_gridsquare));
    }

    #[test]
    fn gain_from_device_info_prefers_reported_gain() {
        let dev = WsEvent::DeviceInfo {
            actual_sample_rate: 250_000.0,
            actual_frequency: 28_330_000.0,
            actual_gain: 12.5,
            gain_elements: vec!["TUNER".to_string()],
        };
        assert_eq!(gain_from_device_info(Some(&dev), Some(40.0)), 12.5);
    }

    #[test]
    fn gain_from_device_info_falls_back_to_configured() {
        assert_eq!(gain_from_device_info(None, Some(20.0)), 20.0);
        // Non-DeviceInfo event also falls back.
        let other = WsEvent::RawLevel { dbfs: -34.0 };
        assert_eq!(gain_from_device_info(Some(&other), Some(15.0)), 15.0);
    }

    #[test]
    fn gain_from_device_info_falls_back_to_zero_when_nothing_known() {
        assert_eq!(gain_from_device_info(None, None), 0.0);
    }

    #[test]
    fn build_wire_measurement_maps_every_field() {
        let cfg = sample_cfg();
        let mw = sample_mw();
        let m = build_wire_measurement(
            &cfg,
            &mw,
            10.0,
            "2026-05-13T15:30:00Z".to_string(),
            -110.2,
            -88.4,
            -89.1,
            21.8,
            21.1,
            0.48,
        );
        assert_eq!(m.beacon_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(m.gridsquare, "EM12il");
        assert_eq!(m.frequency_hz, 28_330_000);
        assert_eq!(m.measured_at, "2026-05-13T15:30:00Z");
        assert_eq!(m.integration_s, 60);
        assert_eq!(m.passband_hz, 300.0);
        assert_eq!(m.gain_db, 10.0);
        assert_eq!(m.noise_floor_dbfs, -110.2);
        assert_eq!(m.signal_peak_dbfs, -88.4);
        assert_eq!(m.signal_avg_dbfs, -89.1);
        assert_eq!(m.snr_peak_db, 21.8);
        assert_eq!(m.snr_avg_db, 21.1);
        assert_eq!(m.signal_active_fraction, 0.48);
        assert!(!m.propmonitor_version.is_empty());
    }

    /// The JSON shape is the wire contract with microwaveprop — guard it
    /// against accidental rename. Use `serde_json::Value` so we don't
    /// over-specify field order.
    #[test]
    fn wire_measurement_serializes_with_expected_keys() {
        let m = build_wire_measurement(
            &sample_cfg(),
            &sample_mw(),
            10.0,
            "2026-05-13T15:30:00Z".to_string(),
            -110.0,
            -88.0,
            -89.0,
            22.0,
            21.0,
            0.5,
        );
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "beacon_id",
            "gridsquare",
            "frequency_hz",
            "measured_at",
            "integration_s",
            "passband_hz",
            "gain_db",
            "noise_floor_dbfs",
            "signal_peak_dbfs",
            "signal_avg_dbfs",
            "snr_peak_db",
            "snr_avg_db",
            "signal_active_fraction",
            "propmonitor_version",
        ] {
            assert!(obj.contains_key(key), "missing key {} in wire payload", key);
        }
        assert!(!obj.contains_key("beacon_callsign"), "stale field present");
    }

    #[test]
    fn build_uses_configured_passband_for_beacon_mode() {
        // Beacon mode reads passband from the beacon block.
        let mut cfg = sample_cfg();
        cfg.beacon = Some(BeaconConfig {
            offset_hz: 100.0,
            bandwidth_hz: 50.0,
        });
        let m = build_wire_measurement(
            &cfg,
            &sample_mw(),
            0.0,
            "t".into(),
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        );
        assert_eq!(m.passband_hz, 50.0);
    }

    #[test]
    fn build_uses_mode_passband_when_not_beacon() {
        let mut cfg = sample_cfg();
        cfg.mode = Mode::Cw;
        cfg.beacon = None;
        let m = build_wire_measurement(
            &cfg,
            &sample_mw(),
            0.0,
            "t".into(),
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        );
        // CW: (700.0, 500.0) → bandwidth 500.
        assert_eq!(m.passband_hz, 500.0);
    }

    #[test]
    fn new_event_channel_works() {
        let (tx, mut rx) = new_event_channel();
        let ev = UploadEvent {
            at: "x".into(),
            ok: true,
            queued: 0,
        };
        tx.send(ev.clone()).unwrap();
        let got = rx.try_recv().unwrap();
        assert_eq!(got.at, "x");
        assert!(got.ok);
    }

    #[test]
    fn now_iso_returns_a_complete_timestamp() {
        let s = now_iso();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20); // "YYYY-MM-DDTHH:MM:SSZ"
    }

    #[tokio::test]
    async fn emit_updates_status_and_broadcasts() {
        let (tx, mut rx) = new_event_channel();
        let status = Arc::new(RwLock::new(UploaderStatus::default()));
        emit(
            &tx,
            &status,
            UploadEvent {
                at: "2026-05-13T15:30:00Z".to_string(),
                ok: true,
                queued: 3,
            },
        )
        .await;
        let got = rx.try_recv().unwrap();
        assert!(got.ok);
        assert_eq!(got.queued, 3);
        let s = status.read().await;
        assert_eq!(s.last_post_at.as_deref(), Some("2026-05-13T15:30:00Z"));
        assert_eq!(s.last_status.as_deref(), Some("ok"));
        assert_eq!(s.queued, 3);
    }

    #[tokio::test]
    async fn emit_records_error_status() {
        let (tx, _rx) = new_event_channel();
        let status = Arc::new(RwLock::new(UploaderStatus::default()));
        emit(
            &tx,
            &status,
            UploadEvent {
                at: "t".to_string(),
                ok: false,
                queued: 0,
            },
        )
        .await;
        let s = status.read().await;
        assert_eq!(s.last_status.as_deref(), Some("error"));
    }

    /// When `microwaveprop` is not configured, `run` should set
    /// `status.enabled = false` after the first measurement event and
    /// drain the channel without attempting any HTTP work. We close the
    /// channel after sending so `run` exits.
    #[tokio::test]
    async fn run_marks_disabled_when_uploads_not_configured() {
        let cfg = Arc::new(RwLock::new(Config {
            frequency: 28_330_000.0,
            mode: crate::config::Mode::Cw,
            sample_rate: 250_000.0,
            gain: Some(10.0),
            driver: "rtlsdr".to_string(),
            ppm: 0.0,
            period_seconds: 60,
            beacon: None,
            http: crate::config::HttpConfig {
                bind: "0.0.0.0:5760".to_string(),
            },
            microwaveprop: None, // not configured
            update: crate::config::UpdateConfig::default(),
        }));
        let device_info = Arc::new(RwLock::new(None));
        let (meas_tx, meas_rx) = tokio::sync::broadcast::channel::<WsEvent>(8);
        let (up_tx, _up_rx) = new_event_channel();
        let status = Arc::new(RwLock::new(UploaderStatus {
            enabled: true,
            ..Default::default()
        }));

        let handle = tokio::spawn(run(cfg, device_info, meas_rx, up_tx, status.clone()));

        meas_tx
            .send(WsEvent::Measurement {
                measured_at: "t".to_string(),
                noise_floor_dbfs: -110.0,
                signal_peak_dbfs: -88.0,
                signal_avg_dbfs: -89.0,
                snr_peak_db: 22.0,
                snr_avg_db: 21.0,
                signal_active_fraction: 0.5,
            })
            .unwrap();

        // Give run a moment to consume the event and flip the status.
        for _ in 0..20 {
            if !status.read().await.enabled {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!status.read().await.enabled);

        // Drop the sender so `run`'s receiver sees Closed and exits.
        drop(meas_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
    }

    /// `run` should ignore non-measurement events (Waterfall, RawLevel,
    /// etc.) — they're carried on the same broadcast channel but the
    /// uploader only cares about completed measurements.
    #[tokio::test]
    async fn run_ignores_non_measurement_events() {
        let cfg = Arc::new(RwLock::new(Config {
            frequency: 28_330_000.0,
            mode: crate::config::Mode::Cw,
            sample_rate: 250_000.0,
            gain: Some(10.0),
            driver: "rtlsdr".to_string(),
            ppm: 0.0,
            period_seconds: 60,
            beacon: None,
            http: crate::config::HttpConfig {
                bind: "0.0.0.0:5760".to_string(),
            },
            microwaveprop: None,
            update: crate::config::UpdateConfig::default(),
        }));
        let device_info = Arc::new(RwLock::new(None));
        let (meas_tx, meas_rx) = tokio::sync::broadcast::channel::<WsEvent>(8);
        let (up_tx, _up_rx) = new_event_channel();
        let status = Arc::new(RwLock::new(UploaderStatus::default()));

        let handle = tokio::spawn(run(cfg, device_info, meas_rx, up_tx, status.clone()));

        meas_tx.send(WsEvent::RawLevel { dbfs: -34.0 }).unwrap();
        meas_tx
            .send(WsEvent::Waterfall {
                f0_hz: 0.0,
                bin_hz: 1.0,
                bins: vec![],
            })
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Status should not have flipped — neither event matched.
        // (It defaults to `enabled: false`.)
        assert!(!status.read().await.enabled);

        drop(meas_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
    }

    /// The transient-failure backoff must keep draining `measurements_rx`
    /// while it waits. The channel shares space with high-rate waterfall and
    /// raw-level frames; sleeping through the backoff would overflow it and
    /// drop the measurements an outage produced — defeating the retry queue.
    ///
    /// A concurrent sender floods the channel beyond its capacity with
    /// interleaved noise + measurement frames *while* the receiver is inside
    /// `backoff_and_drain`; every measurement must still land in the queue.
    #[tokio::test]
    async fn backoff_and_drain_enqueues_measurements_arriving_during_the_wait() {
        let cfg = Arc::new(RwLock::new(Config {
            microwaveprop: Some(sample_mw()),
            ..sample_cfg()
        }));
        let device_info = Arc::new(RwLock::new(None));
        let status = Arc::new(RwLock::new(UploaderStatus::default()));
        // Small on purpose: the noise frames would overflow it if the
        // receiver were asleep instead of draining.
        let (tx, mut rx) = tokio::sync::broadcast::channel::<WsEvent>(4);
        let mut queue: VecDeque<WireMeasurement> = VecDeque::new();

        let sender_tx = tx.clone();
        let sender = tokio::spawn(async move {
            for i in 0..8 {
                let _ = sender_tx.send(WsEvent::RawLevel { dbfs: -50.0 });
                let _ = sender_tx.send(WsEvent::Measurement {
                    measured_at: format!("2026-05-13T15:30:{i:02}Z"),
                    noise_floor_dbfs: -110.0,
                    signal_peak_dbfs: -88.0,
                    signal_avg_dbfs: -89.0,
                    snr_peak_db: 22.0,
                    snr_avg_db: 21.0,
                    signal_active_fraction: 1.0,
                });
                // Let the draining receiver keep up, as it does in real use
                // (production frames arrive at ~15 Hz, far below drain speed).
                tokio::task::yield_now().await;
            }
        });

        let closed = backoff_and_drain(
            &cfg,
            &device_info,
            &status,
            &mut queue,
            &mut rx,
            std::time::Duration::from_millis(500),
        )
        .await;
        sender.await.unwrap();

        assert!(!closed);
        // Every measurement the sender published is enqueued; only the
        // raw-level noise frames are discarded as expected.
        assert_eq!(queue.len(), 8);
        assert!(status.read().await.enabled);
    }

    #[test]
    fn uploader_status_default_is_disabled() {
        let s = UploaderStatus::default();
        assert!(!s.enabled);
        assert!(s.last_post_at.is_none());
        assert!(s.last_status.is_none());
        assert_eq!(s.queued, 0);
    }
}
