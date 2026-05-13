//! axum HTTP + WebSocket server.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::thread;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::error::Error;
use crate::store::{MeasurementStore, MAX_ENTRIES};
use crate::uploader::{UploaderStatus, UploadEvent};
use crate::worker::{run_worker, WorkerEvent};

/// Maximum payload we'll forward to a single WebSocket client without
/// dropping. broadcast::Receiver lag is reported back to the client as a
/// gap; that's normal during waterfall bursts.
const BROADCAST_CAPACITY: usize = 256;

/// Tagged JSON events sent to WebSocket clients. The "type" field is the
/// discriminator the browser uses to dispatch.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    DeviceInfo {
        actual_sample_rate: f64,
        actual_frequency: f64,
        actual_gain: f64,
        gain_elements: Vec<String>,
    },
    RawLevel {
        dbfs: f64,
    },
    Waterfall {
        f0_hz: f32,
        bin_hz: f32,
        bins: Vec<f32>,
    },
    PeriodStarted {
        at: String,
    },
    Measurement {
        measured_at: String,
        noise_floor_dbfs: f64,
        signal_peak_dbfs: f64,
        signal_avg_dbfs: f64,
        snr_peak_db: f64,
        snr_avg_db: f64,
    },
    Upload {
        at: String,
        status: String,
        queued: usize,
    },
    Error {
        message: String,
    },
}

/// Per-process state shared by all axum handlers.
///
/// `config` and `uploader_status` are `Arc<RwLock<…>>` so the uploader
/// task can share the same instances rather than maintaining its own and
/// mirroring back. The other fields don't need outside-of-axum readers.
pub struct AppState {
    pub config_path: String,
    pub config: Arc<RwLock<Config>>,
    pub broadcaster: broadcast::Sender<WsEvent>,
    pub store: RwLock<MeasurementStore>,
    pub last_raw_dbfs: RwLock<Option<f64>>,
    pub device_info: RwLock<Option<WsEvent>>,
    pub uploader_status: Arc<RwLock<UploaderStatus>>,
    /// Handle to the running worker thread. Replaced on PUT /api/config.
    pub worker_handle: StdMutex<Option<WorkerHandle>>,
}

pub struct WorkerHandle {
    pub stop: Arc<AtomicBool>,
    pub join: Option<thread::JoinHandle<()>>,
    pub bridge: Option<thread::JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        if let Some(b) = self.bridge.take() {
            let _ = b.join();
        }
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/assets/app.js", get(asset_js))
        .route("/assets/style.css", get(asset_css))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/devices", get(get_devices))
        .route("/api/status", get(get_status))
        .route("/api/measurements", get(get_measurements))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

pub async fn run(state: Arc<AppState>, bind: &str) -> Result<(), Error> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| Error::msg(format!("invalid http.bind {:?}: {}", bind, e)))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| Error::msg(format!("bind {}: {}", addr, e)))?;
    eprintln!("propmonitor: listening on http://{}", addr);
    let app = build_router(state);
    axum::serve(listener, app)
        .await
        .map_err(|e| Error::msg(format!("axum::serve: {}", e)))?;
    Ok(())
}

async fn index_html() -> Response {
    static HTML: &str = include_str!("web/index.html");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        HTML,
    )
        .into_response()
}

async fn asset_js() -> Response {
    static JS: &str = include_str!("web/app.js");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        JS,
    )
        .into_response()
}

async fn asset_css() -> Response {
    static CSS: &str = include_str!("web/style.css");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        CSS,
    )
        .into_response()
}

#[derive(Serialize)]
struct ConfigView {
    frequency: f64,
    mode: String,
    driver: String,
    sample_rate: f64,
    gain: Option<f64>,
    ppm: f64,
    period_seconds: u32,
    beacon: Option<BeaconView>,
    http: HttpView,
    microwaveprop: Option<MicrowavepropView>,
}

#[derive(Serialize)]
struct BeaconView {
    offset_hz: f64,
    bandwidth_hz: f64,
}

#[derive(Serialize)]
struct HttpView {
    bind: String,
}

#[derive(Serialize)]
struct MicrowavepropView {
    enabled: bool,
    monitor_token: String, // always "redacted" on output
    beacon_callsign: String,
}

fn cfg_to_view(cfg: &Config) -> ConfigView {
    ConfigView {
        frequency: cfg.frequency,
        mode: cfg.mode.as_str().to_string(),
        driver: cfg.driver.clone(),
        sample_rate: cfg.sample_rate,
        gain: cfg.gain,
        ppm: cfg.ppm,
        period_seconds: cfg.period_seconds,
        beacon: cfg.beacon.as_ref().map(|b| BeaconView {
            offset_hz: b.offset_hz,
            bandwidth_hz: b.bandwidth_hz,
        }),
        http: HttpView {
            bind: cfg.http.bind.clone(),
        },
        microwaveprop: cfg.microwaveprop.as_ref().map(|m| MicrowavepropView {
            enabled: m.enabled,
            monitor_token: "redacted".to_string(),
            beacon_callsign: m.beacon_callsign.clone(),
        }),
    }
}

async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = state.config.read().await;
    Json(cfg_to_view(&cfg)).into_response()
}

#[derive(Deserialize)]
struct ConfigUpdate {
    frequency: f64,
    mode: String,
    driver: String,
    sample_rate: f64,
    gain: Option<f64>,
    ppm: Option<f64>,
    period_seconds: Option<u32>,
    beacon: Option<BeaconUpdate>,
    http: Option<HttpUpdate>,
    microwaveprop: Option<MicrowavepropUpdate>,
}

#[derive(Deserialize)]
struct BeaconUpdate {
    offset_hz: f64,
    bandwidth_hz: f64,
}

#[derive(Deserialize)]
struct HttpUpdate {
    bind: String,
}

#[derive(Deserialize)]
struct MicrowavepropUpdate {
    #[serde(default = "default_true")]
    enabled: bool,
    monitor_token: String,
    beacon_callsign: String,
}

fn default_true() -> bool {
    true
}

async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ConfigUpdate>,
) -> Response {
    // Resolve fields the UI doesn't necessarily send:
    //   - monitor_token: "redacted" sentinel preserves the existing one.
    //   - period_seconds: not in the UI form, so missing-on-PUT means
    //     "keep the existing value", not "reset to default".
    let (preserved_token, preserved_period) = {
        let cfg = state.config.read().await;
        let token = cfg
            .microwaveprop
            .as_ref()
            .map(|m| m.monitor_token.clone())
            .unwrap_or_default();
        (token, cfg.period_seconds)
    };

    let yaml = match build_yaml_from_update(&body, &preserved_token, preserved_period) {
        Ok(y) => y,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e.to_string()),
    };

    let new_cfg = match Config::from_yaml_str(&yaml) {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e.to_string()),
    };

    // Persist atomically.
    if let Err(e) = crate::yaml::write_atomic(&state.config_path, &yaml) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Swap in new config and restart the worker.
    {
        let mut current = state.config.write().await;
        *current = new_cfg.clone();
    }
    {
        let mut handle = state.worker_handle.lock().unwrap();
        if let Some(mut h) = handle.take() {
            h.stop_and_join();
        }
        let new_handle = spawn_worker_and_bridge(state.clone(), new_cfg.clone());
        *handle = Some(new_handle);
    }

    let cfg = state.config.read().await;
    Json(cfg_to_view(&cfg)).into_response()
}

fn build_yaml_from_update(
    body: &ConfigUpdate,
    preserved_token: &str,
    preserved_period: u32,
) -> Result<String, Error> {
    use crate::yaml::YamlWriter;
    let mut w = YamlWriter::new();
    w.scalar("frequency", &format!("{}", body.frequency as i64));
    w.scalar("mode", &body.mode);
    w.string("driver", &body.driver);
    w.scalar("sample_rate", &format!("{}", body.sample_rate as i64));
    if let Some(g) = body.gain {
        w.scalar("gain", &format!("{}", g));
    }
    w.scalar("ppm", &format!("{}", body.ppm.unwrap_or(0.0)));
    w.scalar(
        "period_seconds",
        &format!("{}", body.period_seconds.unwrap_or(preserved_period)),
    );

    if let Some(b) = &body.beacon {
        w.nested_open("beacon");
        w.nested_scalar("offset_hz", &format!("{}", b.offset_hz));
        w.nested_scalar("bandwidth_hz", &format!("{}", b.bandwidth_hz));
    }

    if let Some(h) = &body.http {
        w.nested_open("http");
        w.nested_string("bind", &h.bind);
    } else {
        w.nested_open("http");
        w.nested_string("bind", "0.0.0.0:5760");
    }

    if let Some(m) = &body.microwaveprop {
        let token = if m.monitor_token == "redacted" {
            preserved_token.to_string()
        } else {
            m.monitor_token.clone()
        };
        w.nested_open("microwaveprop");
        w.nested_scalar("enabled", if m.enabled { "true" } else { "false" });
        w.nested_string("monitor_token", &token);
        w.nested_string("beacon_callsign", &m.beacon_callsign);
    }

    Ok(w.finish())
}

#[derive(Serialize)]
struct DeviceOption {
    /// Value to put into `driver:` in config.yaml. Format: `driver,key=val,…`.
    value: String,
    /// Human-friendly label for the dropdown.
    label: String,
}

/// Enumerate every SoapySDR device the host currently sees. Runs the
/// blocking enumerate call on a worker thread so the axum runtime stays
/// responsive — `soapysdr::enumerate` typically takes ~100–500 ms while
/// it scans USB.
async fn get_devices() -> Response {
    let devices = tokio::task::spawn_blocking(enumerate_devices)
        .await
        .unwrap_or_default();
    Json(json!({ "devices": devices })).into_response()
}

fn enumerate_devices() -> Vec<DeviceOption> {
    let args_list = match soapysdr::enumerate("") {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::with_capacity(args_list.len());
    for args in args_list {
        let get = |k: &str| {
            args.get(k)
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        };
        let driver = match get("driver") {
            Some(d) => d,
            None => continue, // skip entries without a driver name
        };
        let serial = get("serial");

        // Build the config-string value: `driver[,serial=…]`. We keep it
        // narrow on purpose — extra SoapySDR kwargs (like `dev_index` for
        // RTL-SDR) tend to drift over reboots, so we only persist the
        // stable fields. Users can edit the saved value if they need more.
        let mut value = driver.clone();
        if let Some(s) = &serial {
            value.push_str(&format!(",serial={}", s));
        }

        // Build the dropdown label. Prefer the driver-supplied human name
        // (`label`), then manufacturer+product, then just the driver name.
        let display = get("label")
            .or_else(|| match (get("manufacturer"), get("product")) {
                (Some(m), Some(p)) => Some(format!("{} {}", m, p)),
                (Some(m), None) => Some(m),
                (None, Some(p)) => Some(p),
                (None, None) => None,
            })
            .unwrap_or_else(|| driver.clone());
        let label = match &serial {
            Some(s) => format!("{} ({})", display, s),
            None => display,
        };

        out.push(DeviceOption { value, label });
    }
    out
}

async fn get_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let device = state.device_info.read().await.clone();
    let last_raw = *state.last_raw_dbfs.read().await;
    let last_meas = state.store.read().await.last();
    let uploader = state.uploader_status.read().await.clone();
    Json(json!({
        "device": device,
        "last_raw_dbfs": last_raw,
        "last_measurement": last_meas,
        "uploader": uploader,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct MeasurementsQuery {
    limit: Option<usize>,
}

async fn get_measurements(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<MeasurementsQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(MAX_ENTRIES);
    let store = state.store.read().await;
    Json(json!({ "measurements": store.recent(limit) })).into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.broadcaster.subscribe();

    // Replay the most recent device info so newly-connected clients have
    // header data immediately rather than waiting for the next event.
    if let Some(ev) = state.device_info.read().await.clone() {
        if let Ok(s) = serde_json::to_string(&ev) {
            let _ = socket.send(Message::Text(s)).await;
        }
    }

    loop {
        match rx.recv().await {
            Ok(ev) => match serde_json::to_string(&ev) {
                Ok(s) => {
                    if socket.send(Message::Text(s)).await.is_err() {
                        break;
                    }
                }
                Err(_) => continue,
            },
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Slow client; just keep going from the latest available.
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

// ---------------- worker + bridge plumbing ----------------------------

/// Spawn the worker thread and a sync→async bridge thread that forwards
/// `WorkerEvent`s into `state.broadcaster` and updates derived state.
/// Returns the `WorkerHandle` that lets a future PUT /api/config stop them.
///
/// Must be called from inside a tokio runtime (the bridge needs the runtime
/// handle to update tokio RwLocks).
pub fn spawn_worker_and_bridge(state: Arc<AppState>, cfg: Config) -> WorkerHandle {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker_cfg = cfg.clone();
    let rt_handle = tokio::runtime::Handle::current();

    let join = thread::Builder::new()
        .name("worker".into())
        .spawn(move || run_worker(worker_cfg, tx, worker_stop))
        .expect("spawn worker thread");

    let bridge_state = state.clone();
    let bridge = thread::Builder::new()
        .name("bridge".into())
        .spawn(move || bridge_loop(bridge_state, rx, rt_handle))
        .expect("spawn bridge thread");

    WorkerHandle {
        stop,
        join: Some(join),
        bridge: Some(bridge),
    }
}

fn bridge_loop(
    state: Arc<AppState>,
    rx: std::sync::mpsc::Receiver<WorkerEvent>,
    rt: tokio::runtime::Handle,
) {
    while let Ok(ev) = rx.recv() {
        let ws_ev: Option<WsEvent> = match &ev {
            WorkerEvent::DeviceInfo {
                actual_sample_rate,
                actual_frequency,
                actual_gain,
                gain_elements,
            } => {
                let v = WsEvent::DeviceInfo {
                    actual_sample_rate: *actual_sample_rate,
                    actual_frequency: *actual_frequency,
                    actual_gain: *actual_gain,
                    gain_elements: gain_elements.clone(),
                };
                rt.block_on(async {
                    *state.device_info.write().await = Some(v.clone());
                });
                Some(v)
            }
            WorkerEvent::RawLevel { dbfs } => {
                rt.block_on(async {
                    *state.last_raw_dbfs.write().await = Some(*dbfs);
                });
                Some(WsEvent::RawLevel { dbfs: *dbfs })
            }
            WorkerEvent::WaterfallRow {
                bins,
                f0_hz,
                bin_hz,
            } => Some(WsEvent::Waterfall {
                f0_hz: *f0_hz,
                bin_hz: *bin_hz,
                bins: bins.clone(),
            }),
            WorkerEvent::PeriodStarted => {
                let at = crate::timefmt::format_utc_iso8601(
                    crate::timefmt::unix_now_secs(),
                );
                Some(WsEvent::PeriodStarted { at })
            }
            WorkerEvent::PeriodMeasurement(m) => {
                let at = crate::timefmt::format_utc_iso8601(
                    crate::timefmt::unix_now_secs(),
                );
                let stored = crate::store::StoredMeasurement {
                    measured_at: at.clone(),
                    noise_floor_dbfs: m.noise_dbfs,
                    signal_peak_dbfs: m.signal_peak_dbfs,
                    signal_avg_dbfs: m.signal_avg_dbfs,
                    snr_peak_db: m.snr_peak_db,
                    snr_avg_db: m.snr_avg_db,
                };
                let stored_clone = stored.clone();
                rt.block_on(async {
                    state.store.write().await.push(stored_clone);
                });
                Some(WsEvent::Measurement {
                    measured_at: stored.measured_at,
                    noise_floor_dbfs: stored.noise_floor_dbfs,
                    signal_peak_dbfs: stored.signal_peak_dbfs,
                    signal_avg_dbfs: stored.signal_avg_dbfs,
                    snr_peak_db: stored.snr_peak_db,
                    snr_avg_db: stored.snr_avg_db,
                })
            }
            WorkerEvent::Error(msg) => Some(WsEvent::Error {
                message: msg.clone(),
            }),
        };

        // Forward measurements to the uploader as well — it subscribes to
        // the same broadcast channel for `Measurement` events.
        if let Some(ev) = ws_ev {
            let _ = state.broadcaster.send(ev);
        }
    }
}

/// Subscribe to upload-status events emitted by the uploader task and
/// re-broadcast them on the WS channel. The uploader has already updated
/// `state.uploader_status` (it owns the same Arc<RwLock<…>>), so this
/// function just forwards.
pub async fn forward_upload_events(
    state: Arc<AppState>,
    mut rx: broadcast::Receiver<UploadEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let status_str = if ev.ok { "ok" } else { "error" };
                let _ = state.broadcaster.send(WsEvent::Upload {
                    at: ev.at,
                    status: status_str.to_string(),
                    queued: ev.queued,
                });
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

pub fn new_broadcaster() -> broadcast::Sender<WsEvent> {
    let (tx, _rx) = broadcast::channel::<WsEvent>(BROADCAST_CAPACITY);
    tx
}
