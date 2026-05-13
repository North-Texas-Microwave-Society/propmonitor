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

#[derive(Debug, Clone, Serialize)]
struct WireMeasurement {
    beacon_callsign: String,
    frequency_hz: u64,
    measured_at: String,
    integration_s: u32,
    passband_hz: f64,
    noise_floor_dbfs: f64,
    signal_peak_dbfs: f64,
    signal_avg_dbfs: f64,
    snr_peak_db: f64,
    snr_avg_db: f64,
}

pub fn new_event_channel() -> (broadcast::Sender<UploadEvent>, broadcast::Receiver<UploadEvent>) {
    let (tx, rx) = broadcast::channel::<UploadEvent>(64);
    (tx, rx)
}

/// Long-running task. Spawn this with `tokio::spawn`. When `microwaveprop`
/// is not configured the task immediately marks the uploader disabled and
/// drains the measurement stream without doing anything else.
pub async fn run(
    cfg: Arc<RwLock<Config>>,
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
                    .filter(|m| m.enabled && !m.monitor_token.is_empty())
                    .map(|m| m.monitor_token.clone())
            };
            if let Some(token) = token {
                let m = queue.front().unwrap().clone();
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
                            // Permanent failure (4xx) — drop the head so
                            // we don't retry it forever.
                            queue.pop_front();
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                }
            }
        }

        // Then wait for the next measurement event or a short tick.
        let recv = tokio::time::timeout(Duration::from_secs(5), measurements_rx.recv()).await;
        match recv {
            Ok(Ok(WsEvent::Measurement {
                measured_at,
                noise_floor_dbfs,
                signal_peak_dbfs,
                signal_avg_dbfs,
                snr_peak_db,
                snr_avg_db,
            })) => {
                let c = cfg.read().await;
                let Some(mw) = c
                    .microwaveprop
                    .as_ref()
                    .filter(|m| m.enabled && !m.monitor_token.is_empty())
                else {
                    let mut s = status.write().await;
                    s.enabled = false;
                    continue;
                };
                {
                    let mut s = status.write().await;
                    s.enabled = true;
                }
                let m = WireMeasurement {
                    beacon_callsign: mw.beacon_callsign.clone(),
                    frequency_hz: c.frequency as u64,
                    measured_at,
                    integration_s: c.period_seconds,
                    passband_hz: crate::config::passband_for(c.mode, c.beacon.as_ref()).1,
                    noise_floor_dbfs,
                    signal_peak_dbfs,
                    signal_avg_dbfs,
                    snr_peak_db,
                    snr_avg_db,
                };
                if queue.len() >= MAX_ENTRIES {
                    queue.pop_front();
                }
                queue.push_back(m);
            }
            Ok(Ok(_)) => continue,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_timeout) => continue, // tick to retry queue
        }
    }
}

/// POST one measurement. Returns `Ok(())` on 2xx. Returns `Err(true)` for
/// transient failures (5xx, timeout, network) — keep in queue. Returns
/// `Err(false)` for permanent failures (4xx) — drop from queue.
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
        Ok(r) => {
            let status = r.status();
            if status.is_success() {
                Ok(())
            } else if status.is_client_error() {
                Err(false) // permanent — bad body or revoked token
            } else {
                Err(true) // server error — retry
            }
        }
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
        s.last_status = Some(if ev.ok { "ok".to_string() } else { "error".to_string() });
        s.queued = ev.queued;
    }
    let _ = tx.send(ev);
}

fn now_iso() -> String {
    crate::timefmt::format_utc_iso8601(crate::timefmt::unix_now_secs())
}
