//! Two-way config sync with microwaveprop for **managed** monitors.
//!
//! A managed monitor is created on the microwaveprop website, which then
//! holds the authoritative config plus a version counter. This module keeps
//! the node and the website in agreement:
//!
//! - **Down:** microwaveprop pushes `config_push` over the WebSocket (or the
//!   node pulls it on the polling fallback); the node applies it and reports
//!   back with `config_applied`.
//! - **Up:** a local edit through `PUT /api/config` bumps
//!   `AppState::sync_notify`; this task sends `config_report`, microwaveprop
//!   mints the next version and answers `config_accepted`.
//!
//! Two invariants shape everything here:
//!
//! 1. **Versions are minted only by microwaveprop.** The node persists the
//!    last version it was handed (`microwaveprop.config_version` in
//!    config.yaml) and only ever echoes it, so a reboot doesn't look like a
//!    node that never applied its config.
//! 2. **`monitor_token` never appears in a sync payload.** The wire shape is
//!    [`SyncConfig`], a distinct struct with no token field, so a leak isn't
//!    possible by construction rather than by review.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::config::Config;
use crate::error::Error;
use crate::server::{
    apply_config, persist_config_version, AppState, BeaconUpdate, ConfigUpdate, HttpUpdate,
    MicrowavepropUpdate,
};
use crate::store::StoredMeasurement;

/// Sync socket. Same host as the measurement ingest endpoint in
/// `uploader.rs`; every install talks to the same server.
pub const MICROWAVEPROP_SYNC_ENDPOINT: &str = "wss://prop.w5isp.com/api/v1/beacon-monitor/socket";
/// Polling fallback: config pull (`GET`) and local-edit push-up (`POST`).
pub const MICROWAVEPROP_CONFIG_ENDPOINT: &str =
    "https://prop.w5isp.com/api/v1/beacon-monitor/config";
/// Polling fallback: status heartbeat.
pub const MICROWAVEPROP_STATUS_ENDPOINT: &str =
    "https://prop.w5isp.com/api/v1/beacon-monitor/status";

/// Envelope version stamped on every outgoing frame.
const PROTOCOL_VERSION: u8 = 1;

/// Status heartbeat cadence. Doubles as the app-level keepalive.
const STATUS_INTERVAL: Duration = Duration::from_secs(60);
/// WebSocket protocol ping cadence. Together with the status frame this
/// keeps the connection under Cloudflare's ~100 s idle timeout.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Polling cadence while the socket is down.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// How often to re-check for a token while sync is idle. The watch channel
/// wakes us the moment one is pasted into the LAN UI; this is just a
/// backstop.
const IDLE_RECHECK: Duration = Duration::from_secs(30);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;

// ---------------- wire types ------------------------------------------

/// `{"v":1,"type":"…", …body}` — the shape of every node→prop frame. The
/// polling fallback posts the bodies on their own, without the envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Envelope<T> {
    pub(crate) v: u8,
    #[serde(rename = "type")]
    pub(crate) ty: String,
    #[serde(flatten)]
    pub(crate) body: T,
}

impl<T> Envelope<T> {
    fn new(ty: &str, body: T) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ty: ty.to_string(),
            body,
        }
    }
}

/// First frame on every connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HelloBody {
    pub(crate) client_version: String,
    pub(crate) applied_config_version: u64,
    pub(crate) local_ip: Option<String>,
    pub(crate) local_port: u16,
    pub(crate) config: SyncConfig,
}

/// Outcome of a `config_push`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AppliedBody {
    pub(crate) version: u64,
    pub(crate) ok: bool,
    pub(crate) error: Option<String>,
}

/// A local edit travelling up. `base_version` is the version the node was
/// running when the edit happened; microwaveprop mints the next one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReportBody {
    pub(crate) base_version: u64,
    pub(crate) config: SyncConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StatusBody {
    pub(crate) local_ip: Option<String>,
    pub(crate) local_port: u16,
    pub(crate) uploader: UploaderView,
    pub(crate) last_measurement: Option<StoredMeasurement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UploaderView {
    pub(crate) enabled: bool,
    pub(crate) last_status: Option<String>,
    pub(crate) queued: usize,
}

/// Body of `GET /config` and of the `config_push` frame's payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConfigPayload {
    pub(crate) version: u64,
    pub(crate) config: SyncConfig,
}

/// Body of the `POST /config` response — the newly minted version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AcceptedPayload {
    pub(crate) version: u64,
}

/// The config shape microwaveprop syncs.
///
/// Deliberately **not** `ConfigView`: no `monitor_token` (it must never
/// appear in a sync payload in either direction) and no `http` block —
/// pushing a bad `bind` from the website would strand the node's LAN UI
/// with no remote way back in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SyncConfig {
    pub(crate) frequency: f64,
    pub(crate) mode: String,
    pub(crate) driver: String,
    pub(crate) sample_rate: f64,
    #[serde(default)]
    pub(crate) gain: Option<f64>,
    #[serde(default)]
    pub(crate) ppm: f64,
    pub(crate) period_seconds: u32,
    #[serde(default)]
    pub(crate) beacon: Option<SyncBeacon>,
    /// Maps to `microwaveprop.enabled` locally — whether the uploader
    /// actually POSTs measurements.
    #[serde(default = "default_upload_enabled")]
    pub(crate) upload_enabled: bool,
    #[serde(default)]
    pub(crate) beacon_id: String,
    #[serde(default)]
    pub(crate) gridsquare: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SyncBeacon {
    pub(crate) offset_hz: f64,
    pub(crate) bandwidth_hz: f64,
}

fn default_upload_enabled() -> bool {
    true
}

/// Frames microwaveprop sends. The `v` field is accepted and ignored —
/// a protocol bump will arrive as new frame types, and unknown types are
/// logged and skipped rather than killing the session.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerFrame {
    HelloAck { config_version: u64 },
    ConfigPush { version: u64, config: SyncConfig },
    ConfigAccepted { version: u64 },
    Error { code: String, message: String },
}

impl SyncConfig {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        let mw = cfg.microwaveprop.as_ref();
        Self {
            frequency: cfg.frequency,
            mode: cfg.mode.as_str().to_string(),
            driver: cfg.driver.clone(),
            sample_rate: cfg.sample_rate,
            gain: cfg.gain,
            ppm: cfg.ppm,
            period_seconds: cfg.period_seconds,
            beacon: cfg.beacon.as_ref().map(|b| SyncBeacon {
                offset_hz: b.offset_hz,
                bandwidth_hz: b.bandwidth_hz,
            }),
            upload_enabled: mw.is_some_and(|m| m.enabled),
            beacon_id: mw.map(|m| m.beacon_id.clone()).unwrap_or_default(),
            gridsquare: mw.map(|m| m.gridsquare.clone()).unwrap_or_default(),
        }
    }

    /// Build the local update that applies this config, preserving
    /// everything sync deliberately doesn't carry: the `monitor_token` (via
    /// the existing `"redacted"` sentinel), the whole `http` block, and the
    /// self-update settings.
    pub(crate) fn to_config_update(&self, current: &Config) -> ConfigUpdate {
        ConfigUpdate {
            frequency: self.frequency,
            mode: self.mode.clone(),
            driver: self.driver.clone(),
            sample_rate: self.sample_rate,
            gain: self.gain,
            ppm: Some(self.ppm),
            period_seconds: Some(self.period_seconds),
            beacon: self.beacon.as_ref().map(|b| BeaconUpdate {
                offset_hz: b.offset_hz,
                bandwidth_hz: b.bandwidth_hz,
            }),
            http: Some(HttpUpdate {
                bind: current.http.bind.clone(),
            }),
            microwaveprop: Some(MicrowavepropUpdate {
                enabled: self.upload_enabled,
                monitor_token: "redacted".to_string(),
                beacon_id: self.beacon_id.clone(),
                gridsquare: self.gridsquare.clone(),
            }),
            // Node-local: `None` means "keep the running values". The
            // website has no say in when a node updates itself.
            update: None,
        }
    }
}

// ---------------- pure decision logic ---------------------------------

/// What to do with a config microwaveprop pushed down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushDecision {
    /// `version <= applied` — already seen, or older than what's running.
    Stale,
    /// Content identical to the running config: persist the version and
    /// skip the apply. A no-op push must not drop and reopen the SDR.
    VersionOnly,
    /// Genuinely new config: apply it and restart the worker.
    Apply,
}

pub(crate) fn decide_push(
    pushed_version: u64,
    applied_version: u64,
    pushed: &SyncConfig,
    active: &SyncConfig,
) -> PushDecision {
    if pushed_version <= applied_version {
        PushDecision::Stale
    } else if pushed == active {
        PushDecision::VersionOnly
    } else {
        PushDecision::Apply
    }
}

/// Version this node last accepted from microwaveprop. 0 for a node that
/// has never been configured from the website.
pub(crate) fn applied_version(cfg: &Config) -> u64 {
    cfg.microwaveprop
        .as_ref()
        .map(|m| m.config_version)
        .unwrap_or(0)
}

/// Port the LAN UI is reachable on, parsed out of `http.bind`
/// (`0.0.0.0:5760`, `127.0.0.1:5760`, `[::]:5760`).
pub(crate) fn port_from_bind(bind: &str) -> Option<u16> {
    let (_, port) = bind.trim().rsplit_once(':')?;
    port.parse().ok()
}

/// Best guess at this node's LAN address, for the "Open node UI" link on
/// the website. `connect` on a UDP socket sends no packets — it just makes
/// the kernel pick a route, which reveals the default-route interface IP.
/// The literal fallback keeps this working when DNS is broken.
pub(crate) fn detect_local_ip() -> Option<Ipv4Addr> {
    ["prop.w5isp.com:443", "8.8.8.8:80"]
        .into_iter()
        .find_map(local_ip_towards)
}

fn local_ip_towards(target: &str) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(target).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

// ---------------- frame builders --------------------------------------

fn hello_frame(cfg: &Config, local_ip: Option<String>, local_port: u16) -> Envelope<HelloBody> {
    Envelope::new(
        "hello",
        HelloBody {
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            applied_config_version: applied_version(cfg),
            local_ip,
            local_port,
            config: SyncConfig::from_config(cfg),
        },
    )
}

fn applied_frame(version: u64, ok: bool, error: Option<String>) -> Envelope<AppliedBody> {
    Envelope::new("config_applied", AppliedBody { version, ok, error })
}

fn report_body(cfg: &Config) -> ReportBody {
    ReportBody {
        base_version: applied_version(cfg),
        config: SyncConfig::from_config(cfg),
    }
}

// ---------------- the task --------------------------------------------

/// A local edit that still owes microwaveprop a `config_report`.
///
/// `pending` outlives a session so an edit made while the socket was down
/// still gets delivered (over the polling POST if need be); `awaiting_ack`
/// keeps one session from re-sending it on every loop turn.
#[derive(Debug, Default)]
struct ReportState {
    pending: bool,
    awaiting_ack: bool,
}

impl ReportState {
    fn mark_edit(&mut self) {
        self.pending = true;
        self.awaiting_ack = false;
    }

    fn acked(&mut self) {
        self.pending = false;
        self.awaiting_ack = false;
    }
}

/// Long-running task. Spawn with `tokio::spawn`. Idles (costing nothing)
/// until a `monitor_token` exists, so a self-service install never talks to
/// the sync endpoint, and pasting a token into the LAN UI starts sync
/// without a process restart.
pub async fn run(state: Arc<AppState>) {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("reqwest client");
    let endpoints = PollEndpoints::default();
    let mut edits = state.sync_notify.subscribe();
    let mut report = ReportState::default();
    let mut backoff = MIN_BACKOFF;
    let mut next_poll = Instant::now();

    loop {
        let token = wait_for_token(&state, &mut edits).await;
        latch_local_edit(&mut edits, &mut report);

        match connect(&token).await {
            Ok(ws) => {
                backoff = MIN_BACKOFF;
                if let Err(e) = run_session(&state, ws, &token, &mut edits, &mut report).await {
                    eprintln!("propmonitor: sync session ended: {}", e);
                }
            }
            Err(e) => eprintln!("propmonitor: sync connect failed: {}", e),
        }

        // Polling fallback: only while the socket is down, and no more than
        // once per POLL_INTERVAL however fast the reconnects churn.
        if Instant::now() >= next_poll {
            poll_cycle(&state, &client, &token, &endpoints, &mut report).await;
            next_poll = Instant::now() + POLL_INTERVAL;
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Block until a `monitor_token` is configured, then return it. Woken by
/// the config-edit watch channel, with a slow re-check as a backstop.
async fn wait_for_token(state: &Arc<AppState>, edits: &mut watch::Receiver<u64>) -> String {
    loop {
        if let Some(token) = current_token(state).await {
            return token;
        }
        let _ = tokio::time::timeout(IDLE_RECHECK, edits.changed()).await;
    }
}

/// Notice a local edit that landed while the socket was down. The watch
/// channel remembers the change, but only a read marks it seen — and the
/// session's `select!` is the only other place that reads it, so without
/// this an edit made between reconnects would never be reported and the
/// polling push-up would have nothing to flush.
fn latch_local_edit(edits: &mut watch::Receiver<u64>, report: &mut ReportState) {
    if edits.has_changed().unwrap_or(false) {
        let _ = edits.borrow_and_update();
        report.mark_edit();
    }
}

async fn current_token(state: &Arc<AppState>) -> Option<String> {
    state
        .config
        .read()
        .await
        .microwaveprop
        .as_ref()
        .map(|m| m.monitor_token.clone())
        .filter(|t| !t.is_empty())
}

async fn connect(token: &str) -> Result<WsStream, Error> {
    let mut request = MICROWAVEPROP_SYNC_ENDPOINT
        .into_client_request()
        .map_err(|e| Error::msg(format!("sync endpoint: {}", e)))?;
    let value = HeaderValue::from_str(&format!("Bearer {}", token))
        .map_err(|_| Error::msg("monitor_token is not a valid HTTP header value"))?;
    request.headers_mut().insert(AUTHORIZATION, value);

    let (ws, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| Error::msg(format!("{}: {}", MICROWAVEPROP_SYNC_ENDPOINT, e)))?;
    Ok(ws)
}

async fn run_session(
    state: &Arc<AppState>,
    ws: WsStream,
    token: &str,
    edits: &mut watch::Receiver<u64>,
    report: &mut ReportState,
) -> Result<(), Error> {
    let (mut tx, mut rx) = ws.split();
    report.awaiting_ack = false;

    let (local_ip, local_port) = local_endpoint(state).await;
    let hello = {
        let cfg = state.config.read().await;
        hello_frame(&cfg, local_ip, local_port)
    };
    send_frame(&mut tx, &hello).await?;

    // The first tick of a tokio interval fires immediately: that puts a
    // status frame right behind `hello` (so the website shows the node
    // online without waiting a minute) and costs one early ping.
    let mut status_tick = tokio::time::interval(STATUS_INTERVAL);
    status_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping_tick = tokio::time::interval(PING_INTERVAL);
    ping_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if report.pending && !report.awaiting_ack {
            let body = {
                let cfg = state.config.read().await;
                report_body(&cfg)
            };
            send_frame(&mut tx, &Envelope::new("config_report", body)).await?;
            report.awaiting_ack = true;
        }

        tokio::select! {
            incoming = rx.next() => {
                let Some(message) = incoming else { return Ok(()) };
                let message = message.map_err(|e| Error::msg(format!("sync socket: {}", e)))?;
                match message {
                    Message::Text(text) => {
                        match serde_json::from_str::<ServerFrame>(&text) {
                            Ok(frame) => {
                                if let Some(reply) = handle_server_frame(state, frame, report).await {
                                    send_frame(&mut tx, &reply).await?;
                                }
                            }
                            Err(e) => eprintln!("propmonitor: sync frame ignored: {}", e),
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            _ = status_tick.tick() => {
                let body = status_body(state).await;
                send_frame(&mut tx, &Envelope::new("status", body)).await?;
            }
            _ = ping_tick.tick() => {
                tx.send(Message::Ping(Default::default()))
                    .await
                    .map_err(|e| Error::msg(format!("sync ping: {}", e)))?;
            }
            changed = edits.changed() => {
                if changed.is_err() {
                    return Ok(()); // process is shutting down
                }
                report.mark_edit();
                // A token change needs a fresh handshake, not a report on
                // a socket authenticated with the old one.
                if current_token(state).await.as_deref() != Some(token) {
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_server_frame(
    state: &Arc<AppState>,
    frame: ServerFrame,
    report: &mut ReportState,
) -> Option<Envelope<AppliedBody>> {
    match frame {
        ServerFrame::HelloAck { config_version } => {
            // A `config_push` follows if we're behind; nothing to do here.
            eprintln!(
                "propmonitor: sync connected (microwaveprop config v{})",
                config_version
            );
            None
        }
        ServerFrame::ConfigPush { version, config } => {
            match apply_push(state, version, &config).await {
                Ok(PushDecision::Stale) => None,
                Ok(_) => Some(applied_frame(version, true, None)),
                Err(e) => {
                    eprintln!("propmonitor: sync config v{} rejected: {}", version, e);
                    Some(applied_frame(version, false, Some(e.to_string())))
                }
            }
        }
        ServerFrame::ConfigAccepted { version } => {
            record_version(state, version).await;
            report.acked();
            None
        }
        ServerFrame::Error { code, message } => {
            eprintln!("propmonitor: sync error [{}] {}", code, message);
            None
        }
    }
}

/// Apply a config microwaveprop handed us, honouring the version compare
/// and the content no-op skip.
async fn apply_push(
    state: &Arc<AppState>,
    version: u64,
    pushed: &SyncConfig,
) -> Result<PushDecision, Error> {
    let current = state.config.read().await.clone();
    let decision = decide_push(
        version,
        applied_version(&current),
        pushed,
        &SyncConfig::from_config(&current),
    );

    match decision {
        PushDecision::Stale => {}
        PushDecision::VersionOnly => persist_config_version(state, version).await?,
        PushDecision::Apply => {
            apply_config(state, &pushed.to_config_update(&current), Some(version))
                .await
                .map_err(|e| Error::msg(e.to_string()))?;
        }
    }
    Ok(decision)
}

async fn record_version(state: &Arc<AppState>, version: u64) {
    if let Err(e) = persist_config_version(state, version).await {
        eprintln!("propmonitor: could not persist config version: {}", e);
    }
}

async fn local_endpoint(state: &Arc<AppState>) -> (Option<String>, u16) {
    let bind = state.config.read().await.http.bind.clone();
    let port = port_from_bind(&bind).unwrap_or(0);
    // `detect_local_ip` resolves a hostname, so keep it off the runtime.
    let ip = tokio::task::spawn_blocking(detect_local_ip)
        .await
        .ok()
        .flatten();
    (ip.map(|ip| ip.to_string()), port)
}

async fn status_body(state: &Arc<AppState>) -> StatusBody {
    let (local_ip, local_port) = local_endpoint(state).await;
    let uploader = state.uploader_status.read().await.clone();
    let last_measurement = state.store.read().await.last();
    StatusBody {
        local_ip,
        local_port,
        uploader: UploaderView {
            enabled: uploader.enabled,
            last_status: uploader.last_status,
            queued: uploader.queued,
        },
        last_measurement,
    }
}

async fn send_frame<T: Serialize>(tx: &mut WsSink, frame: &Envelope<T>) -> Result<(), Error> {
    let text = serde_json::to_string(frame)
        .map_err(|e| Error::msg(format!("serialize sync frame: {}", e)))?;
    tx.send(Message::text(text))
        .await
        .map_err(|e| Error::msg(format!("send sync frame: {}", e)))
}

// ---------------- polling fallback ------------------------------------

/// One polling round, used only while the WebSocket is down. Push-up runs
/// before the pull: microwaveprop accepts node reports unconditionally, so
/// reporting first keeps a local edit from being clobbered by the version
/// we're about to read.
async fn poll_cycle(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    token: &str,
    endpoints: &PollEndpoints,
    report: &mut ReportState,
) {
    if report.pending {
        match post_report(state, client, token, endpoints).await {
            Ok(version) => {
                record_version(state, version).await;
                report.acked();
            }
            Err(e) => eprintln!("propmonitor: sync poll push-up failed: {}", e),
        }
    }
    if let Err(e) = pull_config(state, client, token, endpoints).await {
        eprintln!("propmonitor: sync poll pull failed: {}", e);
    }
    if let Err(e) = post_status(state, client, token, endpoints).await {
        eprintln!("propmonitor: sync poll status failed: {}", e);
    }
}

/// URLs the polling fallback talks to. Held in a struct rather than read
/// straight off the constants so the poll cycle can be driven against a
/// local server in tests.
#[derive(Debug, Clone)]
pub(crate) struct PollEndpoints {
    config: String,
    status: String,
}

impl Default for PollEndpoints {
    fn default() -> Self {
        Self {
            config: MICROWAVEPROP_CONFIG_ENDPOINT.to_string(),
            status: MICROWAVEPROP_STATUS_ENDPOINT.to_string(),
        }
    }
}

async fn pull_config(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    token: &str,
    endpoints: &PollEndpoints,
) -> Result<(), Error> {
    let known = applied_version(&state.config.read().await.clone());
    // `known_version` is a plain integer, so no query-string escaping
    // machinery is needed here.
    let url = format!("{}?known_version={}", endpoints.config, known);
    let response = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| Error::msg(e.to_string()))?;

    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(());
    }
    if !response.status().is_success() {
        return Err(Error::msg(format!(
            "GET config: HTTP {}",
            response.status()
        )));
    }

    let payload: ConfigPayload = response
        .json()
        .await
        .map_err(|e| Error::msg(format!("GET config body: {}", e)))?;
    apply_push(state, payload.version, &payload.config).await?;
    Ok(())
}

async fn post_report(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    token: &str,
    endpoints: &PollEndpoints,
) -> Result<u64, Error> {
    let body = {
        let cfg = state.config.read().await;
        report_body(&cfg)
    };
    let response = client
        .post(&endpoints.config)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::msg(e.to_string()))?;
    if !response.status().is_success() {
        return Err(Error::msg(format!(
            "POST config: HTTP {}",
            response.status()
        )));
    }
    let accepted: AcceptedPayload = response
        .json()
        .await
        .map_err(|e| Error::msg(format!("POST config body: {}", e)))?;
    Ok(accepted.version)
}

async fn post_status(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    token: &str,
    endpoints: &PollEndpoints,
) -> Result<(), Error> {
    let body = status_body(state).await;
    let response = client
        .post(&endpoints.status)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::msg(e.to_string()))?;
    if !response.status().is_success() {
        return Err(Error::msg(format!(
            "POST status: HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BeaconConfig, HttpConfig, MicrowavepropConfig, Mode};

    fn sample_cfg() -> Config {
        Config {
            frequency: 28_330_000.0,
            mode: Mode::Beacon,
            sample_rate: 250_000.0,
            gain: Some(10.0),
            driver: "rtlsdr,serial=03340219".to_string(),
            ppm: 0.0,
            period_seconds: 60,
            beacon: Some(BeaconConfig {
                offset_hz: 0.0,
                bandwidth_hz: 300.0,
            }),
            http: HttpConfig {
                bind: "0.0.0.0:5760".to_string(),
            },
            microwaveprop: Some(MicrowavepropConfig {
                enabled: true,
                monitor_token: "super-secret-token".to_string(),
                beacon_id: "00000000-0000-0000-0000-000000000001".to_string(),
                gridsquare: "EM12il".to_string(),
                config_version: 7,
            }),
            update: crate::config::UpdateConfig::default(),
        }
    }

    /// The whole point of `SyncConfig` being its own struct: there is no
    /// field a token could travel in. Mirrors the uploader's
    /// `wire_measurement_serializes_with_expected_keys` guard.
    #[test]
    fn sync_config_has_no_token_and_no_http_block() {
        let value = serde_json::to_value(SyncConfig::from_config(&sample_cfg())).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("monitor_token"));
        assert!(!obj.contains_key("http"));
        for key in [
            "frequency",
            "mode",
            "driver",
            "sample_rate",
            "gain",
            "ppm",
            "period_seconds",
            "beacon",
            "upload_enabled",
            "beacon_id",
            "gridsquare",
        ] {
            assert!(obj.contains_key(key), "missing key {}", key);
        }
    }

    #[test]
    fn no_frame_carries_the_monitor_token() {
        let cfg = sample_cfg();
        let frames = [
            serde_json::to_string(&hello_frame(&cfg, Some("192.168.1.50".into()), 5760)).unwrap(),
            serde_json::to_string(&Envelope::new("config_report", report_body(&cfg))).unwrap(),
            serde_json::to_string(&applied_frame(9, true, None)).unwrap(),
            serde_json::to_string(&Envelope::new(
                "status",
                StatusBody {
                    local_ip: Some("192.168.1.50".into()),
                    local_port: 5760,
                    uploader: UploaderView {
                        enabled: true,
                        last_status: Some("ok".into()),
                        queued: 0,
                    },
                    last_measurement: None,
                },
            ))
            .unwrap(),
        ];
        for frame in frames {
            assert!(
                !frame.contains("monitor_token") && !frame.contains("super-secret-token"),
                "token leaked into {}",
                frame
            );
        }
    }

    #[test]
    fn hello_frame_round_trips() {
        let cfg = sample_cfg();
        let frame = hello_frame(&cfg, Some("192.168.1.50".to_string()), 5760);
        let text = serde_json::to_string(&frame).unwrap();

        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["type"], "hello");
        assert_eq!(value["applied_config_version"], 7);
        assert_eq!(value["local_ip"], "192.168.1.50");
        assert_eq!(value["local_port"], 5760);
        assert_eq!(value["config"]["mode"], "beacon");
        assert_eq!(value["config"]["beacon"]["bandwidth_hz"], 300.0);
        assert!(value["client_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));

        let back: Envelope<HelloBody> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn config_applied_frame_round_trips() {
        let frame = applied_frame(9, false, Some("bad mode".to_string()));
        let text = serde_json::to_string(&frame).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["type"], "config_applied");
        assert_eq!(value["version"], 9);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "bad mode");
        assert_eq!(
            serde_json::from_str::<Envelope<AppliedBody>>(&text).unwrap(),
            frame
        );
    }

    #[test]
    fn config_report_frame_round_trips() {
        let frame = Envelope::new("config_report", report_body(&sample_cfg()));
        let text = serde_json::to_string(&frame).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "config_report");
        assert_eq!(value["base_version"], 7);
        assert_eq!(value["config"]["frequency"], 28_330_000.0);
        assert_eq!(
            serde_json::from_str::<Envelope<ReportBody>>(&text).unwrap(),
            frame
        );
    }

    #[test]
    fn status_frame_round_trips() {
        let frame = Envelope::new(
            "status",
            StatusBody {
                local_ip: Some("192.168.1.50".to_string()),
                local_port: 5760,
                uploader: UploaderView {
                    enabled: true,
                    last_status: Some("ok".to_string()),
                    queued: 3,
                },
                last_measurement: Some(StoredMeasurement {
                    measured_at: "2026-05-13T15:30:00Z".to_string(),
                    noise_floor_dbfs: -110.2,
                    signal_peak_dbfs: -88.4,
                    signal_avg_dbfs: -89.1,
                    snr_peak_db: 21.8,
                    snr_avg_db: 21.1,
                    signal_active_fraction: 0.48,
                }),
            },
        );
        let text = serde_json::to_string(&frame).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "status");
        assert_eq!(value["uploader"]["queued"], 3);
        assert_eq!(value["uploader"]["last_status"], "ok");
        assert_eq!(value["last_measurement"]["snr_avg_db"], 21.1);
        assert_eq!(
            serde_json::from_str::<Envelope<StatusBody>>(&text).unwrap(),
            frame
        );
    }

    #[test]
    fn status_body_posts_without_the_envelope() {
        // The polling fallback POSTs the body alone — no `v`, no `type`.
        let body = StatusBody {
            local_ip: None,
            local_port: 5760,
            uploader: UploaderView {
                enabled: false,
                last_status: None,
                queued: 0,
            },
            last_measurement: None,
        };
        let value = serde_json::to_value(&body).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("v"));
        assert!(!obj.contains_key("type"));
        assert!(obj["local_ip"].is_null());
        assert_eq!(obj["local_port"], 5760);
    }

    #[test]
    fn server_frames_deserialize() {
        let hello_ack: ServerFrame =
            serde_json::from_str(r#"{"v":1,"type":"hello_ack","config_version":9}"#).unwrap();
        assert_eq!(hello_ack, ServerFrame::HelloAck { config_version: 9 });

        let accepted: ServerFrame =
            serde_json::from_str(r#"{"v":1,"type":"config_accepted","version":10}"#).unwrap();
        assert_eq!(accepted, ServerFrame::ConfigAccepted { version: 10 });

        let error: ServerFrame = serde_json::from_str(
            r#"{"v":1,"type":"error","code":"bad_frame","message":"unparsable"}"#,
        )
        .unwrap();
        assert_eq!(
            error,
            ServerFrame::Error {
                code: "bad_frame".to_string(),
                message: "unparsable".to_string(),
            }
        );

        let push: ServerFrame = serde_json::from_str(
            r#"{"v":1,"type":"config_push","version":9,"config":{
                 "frequency":28330000,"mode":"beacon","driver":"rtlsdr",
                 "sample_rate":250000,"gain":10,"ppm":0,"period_seconds":60,
                 "beacon":{"offset_hz":0,"bandwidth_hz":300},
                 "upload_enabled":true,"beacon_id":"uuid","gridsquare":"EM12il"}}"#,
        )
        .unwrap();
        let ServerFrame::ConfigPush { version, config } = push else {
            panic!("expected config_push");
        };
        assert_eq!(version, 9);
        assert_eq!(config.mode, "beacon");
        assert_eq!(config.beacon.unwrap().bandwidth_hz, 300.0);
    }

    #[test]
    fn config_push_tolerates_a_minimal_config() {
        // Optional fields default rather than failing the frame: a parse
        // error would cost us the version and the ability to reply.
        let config: SyncConfig = serde_json::from_str(
            r#"{"frequency":28330000,"mode":"cw","driver":"rtlsdr",
                "sample_rate":250000,"period_seconds":60}"#,
        )
        .unwrap();
        assert!(config.gain.is_none());
        assert_eq!(config.ppm, 0.0);
        assert!(config.beacon.is_none());
        assert!(config.upload_enabled);
        assert!(config.beacon_id.is_empty());
    }

    #[test]
    fn unknown_frame_types_are_rejected_not_panicked() {
        assert!(serde_json::from_str::<ServerFrame>(r#"{"v":1,"type":"whats_this"}"#).is_err());
    }

    #[test]
    fn decide_push_ignores_versions_already_applied() {
        let cfg = sample_cfg();
        let active = SyncConfig::from_config(&cfg);
        let mut newer = active.clone();
        newer.frequency = 28_331_000.0;

        assert_eq!(decide_push(7, 7, &newer, &active), PushDecision::Stale);
        assert_eq!(decide_push(6, 7, &newer, &active), PushDecision::Stale);
        assert_eq!(decide_push(8, 7, &newer, &active), PushDecision::Apply);
    }

    #[test]
    fn decide_push_skips_the_apply_when_content_is_unchanged() {
        let cfg = sample_cfg();
        let active = SyncConfig::from_config(&cfg);
        assert_eq!(
            decide_push(8, 7, &active.clone(), &active),
            PushDecision::VersionOnly
        );
    }

    #[test]
    fn to_config_update_preserves_token_and_http_bind() {
        let mut current = sample_cfg();
        current.http.bind = "127.0.0.1:9000".to_string();

        let mut pushed = SyncConfig::from_config(&current);
        pushed.frequency = 50_000_000.0;
        pushed.upload_enabled = false;
        pushed.gridsquare = "FN31pr".to_string();

        let update = pushed.to_config_update(&current);
        assert_eq!(update.frequency, 50_000_000.0);
        assert_eq!(update.http.as_ref().unwrap().bind, "127.0.0.1:9000");
        let mw = update.microwaveprop.as_ref().unwrap();
        assert_eq!(mw.monitor_token, "redacted");
        assert!(!mw.enabled);
        assert_eq!(mw.gridsquare, "FN31pr");
        assert_eq!(mw.beacon_id, "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn from_config_maps_upload_enabled_and_identity_fields() {
        let mut cfg = sample_cfg();
        cfg.microwaveprop.as_mut().unwrap().enabled = false;
        let sync = SyncConfig::from_config(&cfg);
        assert!(!sync.upload_enabled);
        assert_eq!(sync.gridsquare, "EM12il");

        cfg.microwaveprop = None;
        let sync = SyncConfig::from_config(&cfg);
        assert!(!sync.upload_enabled);
        assert!(sync.beacon_id.is_empty());
    }

    #[test]
    fn applied_version_defaults_to_zero_without_a_microwaveprop_block() {
        let mut cfg = sample_cfg();
        assert_eq!(applied_version(&cfg), 7);
        cfg.microwaveprop = None;
        assert_eq!(applied_version(&cfg), 0);
    }

    #[test]
    fn latch_local_edit_catches_an_edit_made_while_disconnected_exactly_once() {
        let edits = watch::Sender::new(0);
        let mut rx = edits.subscribe();
        let mut report = ReportState::default();

        latch_local_edit(&mut rx, &mut report);
        assert!(!report.pending, "nothing edited yet");

        edits.send_modify(|n| *n += 1);
        latch_local_edit(&mut rx, &mut report);
        assert!(report.pending);

        report.acked();
        latch_local_edit(&mut rx, &mut report);
        assert!(!report.pending, "the same edit must not be reported twice");
    }

    #[test]
    fn port_from_bind_handles_the_shapes_config_allows() {
        assert_eq!(port_from_bind("0.0.0.0:5760"), Some(5760));
        assert_eq!(port_from_bind("127.0.0.1:80"), Some(80));
        assert_eq!(port_from_bind("[::]:5760"), Some(5760));
        assert_eq!(port_from_bind("0.0.0.0"), None);
        assert_eq!(port_from_bind("0.0.0.0:http"), None);
    }

    // ---------------- live session over a real socket ------------------

    fn test_state(cfg: Config, config_path: &str) -> Arc<AppState> {
        use std::sync::Mutex as StdMutex;
        use tokio::sync::RwLock;
        Arc::new(AppState {
            config_path: config_path.to_string(),
            config: Arc::new(RwLock::new(cfg)),
            broadcaster: crate::server::new_broadcaster(),
            store: RwLock::new(crate::store::MeasurementStore::new()),
            last_raw_dbfs: RwLock::new(None),
            device_info: Arc::new(RwLock::new(None)),
            uploader_status: Arc::new(RwLock::new(crate::uploader::UploaderStatus::default())),
            worker_handle: StdMutex::new(None),
            http_server: StdMutex::new(None),
            sync_notify: watch::Sender::new(0),
            config_write: tokio::sync::Mutex::new(()),
            update_state: Arc::new(RwLock::new(crate::update::UpdateState::new())),
            update_notify: watch::Sender::new(crate::update::UpdateRequest::Idle),
            install_path: std::env::temp_dir().join("propmonitor-test-binary"),
            manifest_url: crate::update::MANIFEST_URL.to_string(),
        })
    }

    /// Stand-in for microwaveprop's `MonitorSocket`: records every frame
    /// the node sends and answers per the protocol in `api.md` §5.
    async fn scripted_server(
        mut socket: axum::extract::ws::WebSocket,
        seen: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) {
        use axum::extract::ws::Message as Frame;

        while let Some(Ok(message)) = socket.recv().await {
            let Frame::Text(text) = message else { continue };
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            let ty = frame["type"].as_str().unwrap_or_default().to_string();
            let _ = seen.send(frame.clone());

            let replies: Vec<String> = match ty.as_str() {
                "hello" => vec![
                    serde_json::json!({"v": 1, "type": "hello_ack", "config_version": 9})
                        .to_string(),
                    // Content-identical push: the node must persist the
                    // version and skip the apply.
                    serde_json::json!({
                        "v": 1, "type": "config_push",
                        "version": 9, "config": frame["config"],
                    })
                    .to_string(),
                    // Stale: below the version just applied. Ignored.
                    serde_json::json!({
                        "v": 1, "type": "config_push",
                        "version": 5, "config": frame["config"],
                    })
                    .to_string(),
                ],
                "config_report" => {
                    vec![
                        serde_json::json!({"v": 1, "type": "config_accepted", "version": 10})
                            .to_string(),
                    ]
                }
                _ => vec![],
            };
            for reply in replies {
                socket.send(Frame::Text(reply.into())).await.unwrap();
            }
        }
    }

    /// Drives `run_session` against a real WebSocket server: hello,
    /// a no-op push, a stale push, and a local edit pushed up. Proves the
    /// dispatcher, the version bookkeeping, and the on-disk persistence
    /// line up with the protocol.
    #[tokio::test]
    async fn live_session_syncs_versions_in_both_directions() {
        let path = std::env::temp_dir().join(format!(
            "propmonitor-sync-session-{}.yaml",
            std::process::id()
        ));
        let state = test_state(sample_cfg(), path.to_str().unwrap());

        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/socket",
            axum::routing::get(
                move |upgrade: axum::extract::ws::WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |socket| scripted_server(socket, seen_tx))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/socket", port))
            .await
            .unwrap();

        let node_state = state.clone();
        let session = tokio::spawn(async move {
            let mut edits = node_state.sync_notify.subscribe();
            let mut report = ReportState::default();
            run_session(
                &node_state,
                ws,
                "super-secret-token",
                &mut edits,
                &mut report,
            )
            .await
        });

        // hello → the node announces the version it already has.
        let hello = next_frame(&mut seen, "hello").await;
        assert_eq!(hello["v"], 1);
        assert_eq!(hello["applied_config_version"], 7);
        assert_eq!(hello["config"]["mode"], "beacon");
        assert_eq!(hello["local_port"], 5760);
        assert!(hello["config"].get("monitor_token").is_none());

        // The no-op push is acknowledged and persisted without a restart.
        let applied = next_frame(&mut seen, "config_applied").await;
        assert_eq!(applied["version"], 9);
        assert_eq!(applied["ok"], true);
        assert!(state
            .worker_handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none());

        // A local edit gets reported with the version we just persisted as
        // its base — which also proves the stale v5 push was ignored.
        state.sync_notify.send_modify(|n| *n += 1);
        let report = next_frame(&mut seen, "config_report").await;
        assert_eq!(report["base_version"], 9);
        assert!(report["config"].get("monitor_token").is_none());

        // …and the version microwaveprop mints for it lands on disk.
        wait_for_version(&state, 10).await;
        let on_disk = Config::from_yaml_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mw = on_disk.microwaveprop.unwrap();
        assert_eq!(mw.config_version, 10);
        assert_eq!(mw.monitor_token, "super-secret-token");

        session.abort();
        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// Pull the next frame of `ty` off the server's record, skipping the
    /// status heartbeat that rides along on connect.
    async fn next_frame(
        seen: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        ty: &str,
    ) -> serde_json::Value {
        let deadline = Duration::from_secs(10);
        loop {
            let frame = tokio::time::timeout(deadline, seen.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for a {} frame", ty))
                .expect("socket closed before the frame arrived");
            assert_ne!(frame["version"], 5, "node answered a stale push: {}", frame);
            if frame["type"] == ty {
                return frame;
            }
        }
    }

    async fn wait_for_version(state: &Arc<AppState>, want: u64) {
        for _ in 0..100 {
            if applied_version(&state.config.read().await.clone()) == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("config version never reached {}", want);
    }

    // ---------------- polling fallback over real HTTP -------------------

    #[derive(Clone)]
    struct PollFixture {
        seen: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        /// The config the node last reported, replayed by `GET /config` so
        /// the pull is content-identical and skips the SDR restart.
        reported: Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
    }

    fn auth_of(headers: &axum::http::HeaderMap) -> String {
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    async fn fixture_get_config(
        axum::extract::State(fx): axum::extract::State<PollFixture>,
        headers: axum::http::HeaderMap,
        axum::extract::RawQuery(query): axum::extract::RawQuery,
    ) -> axum::Json<serde_json::Value> {
        let _ = fx.seen.send(serde_json::json!({
            "route": "get_config",
            "auth": auth_of(&headers),
            "query": query,
        }));
        let config = fx
            .reported
            .lock()
            .await
            .clone()
            .expect("node reports before it pulls");
        axum::Json(serde_json::json!({ "version": 13, "config": config }))
    }

    async fn fixture_post_config(
        axum::extract::State(fx): axum::extract::State<PollFixture>,
        headers: axum::http::HeaderMap,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        *fx.reported.lock().await = Some(body["config"].clone());
        let _ = fx.seen.send(serde_json::json!({
            "route": "post_config",
            "auth": auth_of(&headers),
            "body": body,
        }));
        axum::Json(serde_json::json!({ "version": 12 }))
    }

    async fn fixture_post_status(
        axum::extract::State(fx): axum::extract::State<PollFixture>,
        headers: axum::http::HeaderMap,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::http::StatusCode {
        let _ = fx.seen.send(serde_json::json!({
            "route": "post_status",
            "auth": auth_of(&headers),
            "body": body,
        }));
        axum::http::StatusCode::NO_CONTENT
    }

    /// The whole polling cycle against a real HTTP server: pending local
    /// edit up, config down, status heartbeat — in that order, with the
    /// version microwaveprop mints at each step persisted as we go.
    #[tokio::test]
    async fn polling_fallback_pushes_up_then_pulls_down_then_heartbeats() {
        let path =
            std::env::temp_dir().join(format!("propmonitor-sync-poll-{}.yaml", std::process::id()));
        let state = test_state(sample_cfg(), path.to_str().unwrap());

        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let fixture = PollFixture {
            seen: seen_tx,
            reported: Arc::new(tokio::sync::Mutex::new(None)),
        };
        let app = axum::Router::new()
            .route(
                "/config",
                axum::routing::get(fixture_get_config).post(fixture_post_config),
            )
            .route("/status", axum::routing::post(fixture_post_status))
            .with_state(fixture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = PollEndpoints {
            config: format!("http://127.0.0.1:{}/config", port),
            status: format!("http://127.0.0.1:{}/status", port),
        };
        let mut report = ReportState {
            pending: true,
            awaiting_ack: false,
        };

        poll_cycle(
            &state,
            &reqwest::Client::new(),
            "super-secret-token",
            &endpoints,
            &mut report,
        )
        .await;

        let post = seen.recv().await.unwrap();
        assert_eq!(post["route"], "post_config");
        assert_eq!(post["auth"], "Bearer super-secret-token");
        assert_eq!(post["body"]["base_version"], 7);
        assert!(post["body"]["config"].get("monitor_token").is_none());
        // Accepted at v12 → the pull must announce v12 as known.
        assert!(!report.pending);

        let get = seen.recv().await.unwrap();
        assert_eq!(get["route"], "get_config");
        assert_eq!(get["query"], "known_version=12");

        let status = seen.recv().await.unwrap();
        assert_eq!(status["route"], "post_status");
        assert_eq!(status["auth"], "Bearer super-secret-token");
        assert_eq!(status["body"]["local_port"], 5760);
        assert_eq!(status["body"]["uploader"]["queued"], 0);
        assert!(status["body"].get("type").is_none());

        // v13 came down content-identical: persisted, no worker restart.
        let on_disk = Config::from_yaml_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.microwaveprop.unwrap().config_version, 13);
        assert!(state
            .worker_handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none());

        server.abort();
        let _ = std::fs::remove_file(&path);
    }
}
