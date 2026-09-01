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
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, watch, Notify, RwLock};

use crate::config::Config;
use crate::error::Error;
use crate::store::{MeasurementStore, MAX_ENTRIES};
use crate::update::{Manifest, Phase, UpdateRequest, UpdateState};
use crate::uploader::{UploadEvent, UploaderStatus};
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
        signal_active_fraction: f64,
    },
    Upload {
        at: String,
        status: String,
        queued: usize,
    },
    Error {
        message: String,
    },
    /// Self-update channel progress. `latest` is the build the channel
    /// offers when it differs from the running one; `error` is why the
    /// last check or install failed.
    Update {
        phase: Phase,
        latest: Option<Manifest>,
        error: Option<String>,
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
    /// Shared with the uploader so it can stamp each outgoing measurement
    /// with the SDR's actual reported gain at upload time.
    pub device_info: Arc<RwLock<Option<WsEvent>>>,
    pub uploader_status: Arc<RwLock<UploaderStatus>>,
    /// Handle to the running worker thread. Replaced on PUT /api/config.
    pub worker_handle: StdMutex<Option<WorkerHandle>>,
    /// Handle to the running HTTP server. Replaced on `http.bind` change so
    /// the listener can be rebound without restarting the process.
    pub http_server: StdMutex<Option<ServerHandle>>,
    /// Bumped once per successful **local** config edit (`PUT /api/config`).
    /// The microwaveprop sync task watches this and pushes the edit
    /// upstream. Configs that arrived *from* microwaveprop don't bump it —
    /// echoing them back would just mint versions for no reason.
    pub sync_notify: watch::Sender<u64>,
    /// Serializes config writes. `PUT /api/config` and the microwaveprop
    /// sync task are independent writers, and both read the active config,
    /// build a whole yaml document and write it back — interleaved, one
    /// edit would silently vanish while its version was recorded.
    pub config_write: tokio::sync::Mutex<()>,
    /// Self-update channel state, shared with the update task and read by
    /// `GET /api/update`.
    pub update_state: Arc<RwLock<UpdateState>>,
    /// Wakes the update task for an explicit "check now" / "install now"
    /// from the UI. `watch` coalesces, so a burst of clicks is one wake.
    pub update_notify: watch::Sender<UpdateRequest>,
    /// Absolute path of the running binary — where a new one is installed.
    /// Resolved once at boot, before anything can rename it.
    pub install_path: std::path::PathBuf,
    /// Release-channel manifest URL. A field rather than a constant so
    /// tests can point the channel at a local server.
    pub manifest_url: String,
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
        .route("/api/update", get(get_update))
        .route("/api/update/check", post(post_update_check))
        .route("/api/update/install", post(post_update_install))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

/// Handle to the running HTTP server task.
pub struct ServerHandle {
    shutdown: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

/// (Re)bind the HTTP listener to `bind`. The new address is bound eagerly so
/// bind errors surface to the caller without disturbing the running server;
/// on success the old server is signalled to stop (and reaped off-thread) and
/// the new one starts serving. On failure the running server is left
/// untouched.
///
/// The old server is never awaited here: `put_config` runs *inside* the
/// server it is rebinding, and awaiting that server's graceful shutdown from
/// within a request it is serving would deadlock (the shutdown waits for the
/// in-flight request to finish). Instead we signal it and let a background
/// task reap it once its connections drain.
pub async fn restart_server(state: &Arc<AppState>, bind: &str) -> Result<(), Error> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| Error::msg(format!("invalid http.bind {:?}: {}", bind, e)))?;

    // Bind first — if the new address is taken (including by our own running
    // server on an overlapping address, e.g. 0.0.0.0:5760 → 127.0.0.1:5760),
    // fail cleanly without tearing anything down.
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| Error::msg(format!("bind {}: {}", addr, e)))?;

    if let Some(old) = take_server(state) {
        old.shutdown.notify_one();
        tokio::spawn(async move {
            let _ = old.task.await;
        });
    }

    spawn_server(state, addr, listener);
    Ok(())
}

fn take_server(state: &Arc<AppState>) -> Option<ServerHandle> {
    state
        .http_server
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

fn spawn_server(state: &Arc<AppState>, addr: SocketAddr, listener: tokio::net::TcpListener) {
    eprintln!("propmonitor: listening on http://{}", addr);
    let app = build_router(state.clone());
    let shutdown = Arc::new(Notify::new());
    let shutdown2 = shutdown.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown2.notified().await;
            })
            .await
        {
            eprintln!("propmonitor: server exited: {}", e);
        }
    });
    let mut guard = state
        .http_server
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(ServerHandle { shutdown, task });
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
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
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
    update: UpdateView,
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
    beacon_id: String,
    gridsquare: String,
}

#[derive(Serialize)]
struct UpdateView {
    enabled: bool,
    auto: bool,
    check_interval: u32,
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
            beacon_id: m.beacon_id.clone(),
            gridsquare: m.gridsquare.clone(),
        }),
        update: UpdateView {
            enabled: cfg.update.enabled,
            auto: cfg.update.auto,
            check_interval: cfg.update.check_interval,
        },
    }
}

async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = state.config.read().await;
    Json(cfg_to_view(&cfg)).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConfigUpdate {
    pub(crate) frequency: f64,
    pub(crate) mode: String,
    pub(crate) driver: String,
    pub(crate) sample_rate: f64,
    pub(crate) gain: Option<f64>,
    pub(crate) ppm: Option<f64>,
    pub(crate) period_seconds: Option<u32>,
    pub(crate) beacon: Option<BeaconUpdate>,
    pub(crate) http: Option<HttpUpdate>,
    pub(crate) microwaveprop: Option<MicrowavepropUpdate>,
    /// Omitted entirely (an older client, or the sync task) means "leave
    /// the self-update settings alone" — a config push from microwaveprop
    /// must never reset a node's update channel.
    pub(crate) update: Option<UpdateUpdate>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BeaconUpdate {
    pub(crate) offset_hz: f64,
    pub(crate) bandwidth_hz: f64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HttpUpdate {
    pub(crate) bind: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MicrowavepropUpdate {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    pub(crate) monitor_token: String,
    pub(crate) beacon_id: String,
    #[serde(default)]
    pub(crate) gridsquare: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateUpdate {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default = "default_true")]
    pub(crate) auto: bool,
    #[serde(default = "default_check_interval")]
    pub(crate) check_interval: u32,
}

fn default_check_interval() -> u32 {
    crate::config::DEFAULT_CHECK_INTERVAL
}

fn default_true() -> bool {
    true
}

/// Why a config apply failed. Keeps the status split `put_config` had
/// before its body moved into [`apply_config`]: a rejected body is the
/// caller's fault, a failed write or rebind is ours.
#[derive(Debug)]
pub enum ApplyError {
    Invalid(Error),
    Internal(Error),
}

impl ApplyError {
    fn status(&self) -> StatusCode {
        match self {
            ApplyError::Invalid(_) => StatusCode::BAD_REQUEST,
            ApplyError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Invalid(e) | ApplyError::Internal(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Validate, persist and activate a config update. Shared by
/// `PUT /api/config` and the microwaveprop sync task, so both surfaces get
/// the same ordering guarantees: rebind before persist (a bad bind fails
/// without touching the file on disk), persist before the in-memory swap,
/// worker restart last.
///
/// `config_version` is the version to record under
/// `microwaveprop.config_version`: `Some(v)` when the config came *from*
/// microwaveprop, `None` to carry the persisted version forward (local
/// edits — microwaveprop mints their version afterwards).
pub async fn apply_config(
    state: &Arc<AppState>,
    body: &ConfigUpdate,
    config_version: Option<u64>,
) -> Result<Config, ApplyError> {
    let _write = state.config_write.lock().await;
    let current = state.config.read().await.clone();

    let yaml =
        build_yaml_from_update(body, &current, config_version).map_err(ApplyError::Invalid)?;
    let new_cfg = Config::from_yaml_str(&yaml).map_err(ApplyError::Invalid)?;

    // If the bind address changed, rebind the HTTP listener *before*
    // persisting so a bad bind fails cleanly and leaves config on disk
    // untouched. `restart_server` never awaits the old server from inside
    // this request (see its doc comment).
    if new_cfg.http.bind != current.http.bind {
        restart_server(state, &new_cfg.http.bind)
            .await
            .map_err(ApplyError::Internal)?;
    }

    crate::yaml::write_atomic(&state.config_path, &yaml).map_err(ApplyError::Internal)?;

    {
        let mut active = state.config.write().await;
        *active = new_cfg.clone();
    }
    restart_worker(state, &new_cfg);

    Ok(new_cfg)
}

/// Stop the running worker + bridge and start a fresh pair on `cfg`. The
/// SoapySDR device is released and reopened.
fn restart_worker(state: &Arc<AppState>, cfg: &Config) {
    let mut handle = state
        .worker_handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(mut h) = handle.take() {
        h.stop_and_join();
    }
    *handle = Some(spawn_worker_and_bridge(state.clone(), cfg.clone()));
}

/// Record a microwaveprop-minted config version without disturbing the
/// running config. Used when a pushed config is content-identical to the
/// active one, and when microwaveprop mints a version for a local edit:
/// neither case justifies dropping and reopening the SDR.
pub async fn persist_config_version(state: &Arc<AppState>, version: u64) -> Result<(), Error> {
    let _write = state.config_write.lock().await;
    let mut active = state.config.write().await;
    let Some(mw) = active.microwaveprop.as_mut() else {
        return Err(Error::msg(
            "cannot record a config version without a `microwaveprop:` block",
        ));
    };
    if mw.config_version == version {
        return Ok(());
    }
    mw.config_version = version;

    let yaml = build_yaml_from_update(&cfg_to_update(&active), &active, Some(version))?;
    crate::yaml::write_atomic(&state.config_path, &yaml)
}

async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ConfigUpdate>,
) -> Response {
    match apply_config(&state, &body, None).await {
        Ok(cfg) => {
            // Wake the sync task so the edit gets pushed up to
            // microwaveprop (which mints the version for it).
            state.sync_notify.send_modify(|n| *n += 1);
            Json(cfg_to_view(&cfg)).into_response()
        }
        Err(e) => error_response(e.status(), e.to_string()),
    }
}

/// Round-trip the active config back into the update shape, so the yaml
/// writer has a single input type. Carries the *real* token rather than the
/// `"redacted"` sentinel — the value never leaves the process.
pub(crate) fn cfg_to_update(cfg: &Config) -> ConfigUpdate {
    ConfigUpdate {
        frequency: cfg.frequency,
        mode: cfg.mode.as_str().to_string(),
        driver: cfg.driver.clone(),
        sample_rate: cfg.sample_rate,
        gain: cfg.gain,
        ppm: Some(cfg.ppm),
        period_seconds: Some(cfg.period_seconds),
        beacon: cfg.beacon.as_ref().map(|b| BeaconUpdate {
            offset_hz: b.offset_hz,
            bandwidth_hz: b.bandwidth_hz,
        }),
        http: Some(HttpUpdate {
            bind: cfg.http.bind.clone(),
        }),
        microwaveprop: cfg.microwaveprop.as_ref().map(|m| MicrowavepropUpdate {
            enabled: m.enabled,
            monitor_token: m.monitor_token.clone(),
            beacon_id: m.beacon_id.clone(),
            gridsquare: m.gridsquare.clone(),
        }),
        update: Some(UpdateUpdate {
            enabled: cfg.update.enabled,
            auto: cfg.update.auto,
            check_interval: cfg.update.check_interval,
        }),
    }
}

/// Render an update as the on-disk yaml document. Fields the UI doesn't
/// necessarily send are resolved against `current`:
///
///   - `monitor_token`: the `"redacted"` sentinel keeps the existing token.
///   - `period_seconds`: absent means "keep", not "reset to default".
///   - `microwaveprop.config_version`: `config_version` when the caller has
///     a newly-minted one, otherwise the persisted value carries forward.
fn build_yaml_from_update(
    body: &ConfigUpdate,
    current: &Config,
    config_version: Option<u64>,
) -> Result<String, Error> {
    use crate::yaml::YamlWriter;
    let preserved_token = current
        .microwaveprop
        .as_ref()
        .map(|m| m.monitor_token.as_str())
        .unwrap_or("");
    let version = config_version.unwrap_or_else(|| {
        current
            .microwaveprop
            .as_ref()
            .map(|m| m.config_version)
            .unwrap_or(0)
    });

    let mut w = YamlWriter::new();
    w.scalar("frequency", &body.frequency.to_string());
    w.scalar("mode", &body.mode);
    w.string("driver", &body.driver);
    w.scalar("sample_rate", &body.sample_rate.to_string());
    if let Some(g) = body.gain {
        w.scalar("gain", &format!("{}", g));
    }
    w.scalar("ppm", &format!("{}", body.ppm.unwrap_or(0.0)));
    w.scalar(
        "period_seconds",
        &format!("{}", body.period_seconds.unwrap_or(current.period_seconds)),
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
        w.nested_string("beacon_id", &m.beacon_id);
        w.nested_string("gridsquare", &m.gridsquare);
        w.nested_scalar("config_version", &version.to_string());
    }

    // The update channel is node-local: microwaveprop never carries it,
    // so an absent block means "keep what is running" rather than "reset
    // to defaults".
    let update = body
        .update
        .as_ref()
        .map(|u| crate::config::UpdateConfig {
            enabled: u.enabled,
            auto: u.auto,
            check_interval: u.check_interval,
        })
        .unwrap_or_else(|| current.update.clone());
    w.nested_open("update");
    w.nested_scalar("enabled", if update.enabled { "true" } else { "false" });
    w.nested_scalar("auto", if update.auto { "true" } else { "false" });
    w.nested_scalar("check_interval", &update.check_interval.to_string());

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
        let get = |k: &str| args.get(k).map(|s| s.to_string()).filter(|s| !s.is_empty());
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

/// One JSON snapshot of the self-update channel: the running build, what
/// the channel last offered, where a new binary would land, and the
/// operator's three settings. Every update endpoint answers with this
/// shape so the UI has exactly one thing to render.
async fn update_snapshot(state: &Arc<AppState>) -> Json<serde_json::Value> {
    let cfg = state.config.read().await.update.clone();
    let s = state.update_state.read().await;
    Json(json!({
        "enabled": cfg.enabled,
        "auto": cfg.auto,
        "check_interval": cfg.check_interval,
        "current": s.current,
        "latest": s.latest,
        "phase": s.phase,
        "last_check_at": s.last_check_at,
        "last_error": s.last_error,
        "install_path": state.install_path.to_string_lossy(),
    }))
}

async fn get_update(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    update_snapshot(&state).await.into_response()
}

/// Ask for a check now. Returns the snapshot as it stands — the check
/// itself runs on the channel task and reports through `WsEvent::Update`.
/// Works even with `update.enabled: false`: the flag governs the timer,
/// not the operator.
async fn post_update_check(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.update_notify.send_replace(UpdateRequest::Check);
    update_snapshot(&state).await.into_response()
}

/// Install the build the last check found. Rejected — rather than queued —
/// when there is nothing to install, something is already in flight, or
/// the platform has no in-place activation, so the UI can say why.
async fn post_update_install(State(state): State<Arc<AppState>>) -> Response {
    {
        let s = state.update_state.read().await;
        if s.phase.is_busy() {
            return error_response(
                StatusCode::CONFLICT,
                "an update check or install is already running".to_string(),
            );
        }
        if !s.current.can_self_update {
            return error_response(
                StatusCode::CONFLICT,
                "in-place self-update requires a Linux install".to_string(),
            );
        }
        if s.latest.is_none() {
            return error_response(
                StatusCode::CONFLICT,
                "no update available — check for updates first".to_string(),
            );
        }
    }
    state.update_notify.send_replace(UpdateRequest::Install);
    update_snapshot(&state).await.into_response()
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

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.broadcaster.subscribe();

    // Replay the most recent device info so newly-connected clients have
    // header data immediately rather than waiting for the next event.
    if let Some(ev) = state.device_info.read().await.clone() {
        if let Ok(s) = serde_json::to_string(&ev) {
            let _ = socket.send(Message::Text(s.into())).await;
        }
    }

    loop {
        match rx.recv().await {
            Ok(ev) => match serde_json::to_string(&ev) {
                Ok(s) => {
                    if socket.send(Message::Text(s.into())).await.is_err() {
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

/// Outcome of converting one `WorkerEvent` into the WS frame + any
/// derived state writes the bridge thread needs to perform. Extracted
/// from `bridge_loop` so the pure mapping logic can be unit-tested
/// without standing up a tokio runtime or an `AppState`.
pub(crate) struct BridgeStep {
    pub(crate) ws_event: WsEvent,
    /// When set, replace `AppState.device_info` with this value (always
    /// matches `ws_event`).
    pub(crate) update_device_info: bool,
    /// When set, replace `AppState.last_raw_dbfs` with this value.
    pub(crate) update_last_raw_dbfs: Option<f64>,
    /// When set, push this `StoredMeasurement` onto the ring buffer.
    pub(crate) push_measurement: Option<crate::store::StoredMeasurement>,
}

/// Pure transformation from a worker event to its WS-stream frame plus
/// the state mutations to apply. `now_iso` is the current UTC time in
/// ISO-8601, supplied by the caller so tests can pin the value.
pub(crate) fn convert_worker_event(ev: &WorkerEvent, now_iso: &str) -> BridgeStep {
    match ev {
        WorkerEvent::DeviceInfo {
            actual_sample_rate,
            actual_frequency,
            actual_gain,
            gain_elements,
        } => {
            let ws = WsEvent::DeviceInfo {
                actual_sample_rate: *actual_sample_rate,
                actual_frequency: *actual_frequency,
                actual_gain: *actual_gain,
                gain_elements: gain_elements.clone(),
            };
            BridgeStep {
                ws_event: ws,
                update_device_info: true,
                update_last_raw_dbfs: None,
                push_measurement: None,
            }
        }
        WorkerEvent::RawLevel { dbfs } => BridgeStep {
            ws_event: WsEvent::RawLevel { dbfs: *dbfs },
            update_device_info: false,
            update_last_raw_dbfs: Some(*dbfs),
            push_measurement: None,
        },
        WorkerEvent::WaterfallRow {
            bins,
            f0_hz,
            bin_hz,
        } => BridgeStep {
            ws_event: WsEvent::Waterfall {
                f0_hz: *f0_hz,
                bin_hz: *bin_hz,
                bins: bins.clone(),
            },
            update_device_info: false,
            update_last_raw_dbfs: None,
            push_measurement: None,
        },
        WorkerEvent::PeriodStarted => BridgeStep {
            ws_event: WsEvent::PeriodStarted {
                at: now_iso.to_string(),
            },
            update_device_info: false,
            update_last_raw_dbfs: None,
            push_measurement: None,
        },
        WorkerEvent::PeriodMeasurement(m) => {
            let stored = crate::store::StoredMeasurement {
                measured_at: now_iso.to_string(),
                noise_floor_dbfs: m.noise_dbfs,
                signal_peak_dbfs: m.signal_peak_dbfs,
                signal_avg_dbfs: m.signal_avg_dbfs,
                snr_peak_db: m.snr_peak_db,
                snr_avg_db: m.snr_avg_db,
                signal_active_fraction: m.signal_active_fraction,
            };
            BridgeStep {
                ws_event: WsEvent::Measurement {
                    measured_at: stored.measured_at.clone(),
                    noise_floor_dbfs: stored.noise_floor_dbfs,
                    signal_peak_dbfs: stored.signal_peak_dbfs,
                    signal_avg_dbfs: stored.signal_avg_dbfs,
                    snr_peak_db: stored.snr_peak_db,
                    snr_avg_db: stored.snr_avg_db,
                    signal_active_fraction: stored.signal_active_fraction,
                },
                update_device_info: false,
                update_last_raw_dbfs: None,
                push_measurement: Some(stored),
            }
        }
        WorkerEvent::Error(msg) => BridgeStep {
            ws_event: WsEvent::Error {
                message: msg.clone(),
            },
            update_device_info: false,
            update_last_raw_dbfs: None,
            push_measurement: None,
        },
    }
}

/// Choose the timestamp to stamp an event with. `PeriodStarted` records the
/// current time as the start of a new integration window; the following
/// `PeriodMeasurement` reuses that recorded start so `measured_at` reflects
/// the beginning of the window, not its end (matching api.md §4).
fn stamp_event(ev: &WorkerEvent, now: &str, period_start: &mut Option<String>) -> String {
    match ev {
        WorkerEvent::PeriodStarted => {
            *period_start = Some(now.to_string());
            now.to_string()
        }
        WorkerEvent::PeriodMeasurement(_) => {
            period_start.clone().unwrap_or_else(|| now.to_string())
        }
        _ => now.to_string(),
    }
}

fn bridge_loop(
    state: Arc<AppState>,
    rx: std::sync::mpsc::Receiver<WorkerEvent>,
    rt: tokio::runtime::Handle,
) {
    let mut period_start: Option<String> = None;
    while let Ok(ev) = rx.recv() {
        let now = crate::timefmt::format_utc_iso8601(crate::timefmt::unix_now_secs());
        let stamp = stamp_event(&ev, &now, &mut period_start);
        let step = convert_worker_event(&ev, &stamp);

        if step.update_device_info {
            let v = step.ws_event.clone();
            rt.block_on(async {
                *state.device_info.write().await = Some(v);
            });
        }
        if let Some(dbfs) = step.update_last_raw_dbfs {
            rt.block_on(async {
                *state.last_raw_dbfs.write().await = Some(dbfs);
            });
        }
        if let Some(m) = step.push_measurement {
            rt.block_on(async {
                state.store.write().await.push(m);
            });
        }

        let _ = state.broadcaster.send(step.ws_event);
    }
}

/// Subscribe to upload-status events emitted by the uploader task and
/// re-broadcast them on the WS channel. The uploader has already updated
/// `state.uploader_status` (it owns the same Arc<RwLock<…>>), so this
/// function just forwards.
pub async fn forward_upload_events(state: Arc<AppState>, mut rx: broadcast::Receiver<UploadEvent>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BeaconConfig, HttpConfig, MicrowavepropConfig, Mode};
    use crate::measure::Measurement;

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
            microwaveprop: Some(MicrowavepropConfig {
                enabled: true,
                monitor_token: "secret-token".to_string(),
                beacon_id: "00000000-0000-0000-0000-000000000001".to_string(),
                gridsquare: "EM12il".to_string(),
                config_version: 7,
            }),
            update: crate::config::UpdateConfig::default(),
        }
    }

    #[test]
    fn cfg_to_view_redacts_token_and_preserves_other_fields() {
        let cfg = sample_cfg();
        let v = cfg_to_view(&cfg);
        assert_eq!(v.frequency, 28_330_000.0);
        assert_eq!(v.mode, "beacon");
        assert_eq!(v.driver, "rtlsdr");
        assert_eq!(v.period_seconds, 60);
        let mw = v.microwaveprop.unwrap();
        assert_eq!(mw.monitor_token, "redacted");
        assert_eq!(mw.beacon_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(mw.gridsquare, "EM12il");
        assert!(mw.enabled);
        let b = v.beacon.unwrap();
        assert_eq!(b.bandwidth_hz, 300.0);
    }

    #[test]
    fn cfg_to_view_handles_missing_microwaveprop() {
        let mut cfg = sample_cfg();
        cfg.microwaveprop = None;
        let v = cfg_to_view(&cfg);
        assert!(v.microwaveprop.is_none());
    }

    #[test]
    fn cfg_to_view_serializes_to_json_with_expected_keys() {
        let v = cfg_to_view(&sample_cfg());
        let j = serde_json::to_value(&v).unwrap();
        let obj = j.as_object().unwrap();
        for key in [
            "frequency",
            "mode",
            "driver",
            "sample_rate",
            "gain",
            "ppm",
            "period_seconds",
            "beacon",
            "http",
            "microwaveprop",
        ] {
            assert!(obj.contains_key(key), "missing key {}", key);
        }
    }

    fn sample_update() -> ConfigUpdate {
        ConfigUpdate {
            frequency: 28_330_000.0,
            mode: "beacon".to_string(),
            driver: "rtlsdr".to_string(),
            sample_rate: 250_000.0,
            gain: Some(10.0),
            ppm: Some(0.0),
            period_seconds: None,
            beacon: Some(BeaconUpdate {
                offset_hz: 0.0,
                bandwidth_hz: 300.0,
            }),
            http: Some(HttpUpdate {
                bind: "0.0.0.0:5760".to_string(),
            }),
            microwaveprop: Some(MicrowavepropUpdate {
                enabled: true,
                monitor_token: "new-token".to_string(),
                beacon_id: "uuid-xyz".to_string(),
                gridsquare: "EM12il".to_string(),
            }),
            // The UI always sends this block; the `None` path has its own
            // test.
            update: Some(UpdateUpdate {
                enabled: true,
                auto: false,
                check_interval: 1800,
            }),
        }
    }

    #[test]
    fn build_yaml_from_update_round_trips_through_parser() {
        let yaml = build_yaml_from_update(&sample_update(), &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.frequency, 28_330_000.0);
        assert_eq!(parsed.mode, Mode::Beacon);
        assert_eq!(parsed.period_seconds, 60);
        let mw = parsed.microwaveprop.unwrap();
        assert_eq!(mw.monitor_token, "new-token");
        assert_eq!(mw.beacon_id, "uuid-xyz");
        assert_eq!(mw.gridsquare, "EM12il");
        assert!(mw.enabled);
    }

    #[test]
    fn build_yaml_from_update_preserves_fractional_tuning_values() {
        let mut body = sample_update();
        body.frequency = 28_330_000.5;
        body.sample_rate = 250_000.25;
        let yaml = build_yaml_from_update(&body, &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.frequency, 28_330_000.5);
        assert_eq!(parsed.sample_rate, 250_000.25);
    }

    #[test]
    fn build_yaml_from_update_preserves_token_on_redacted_sentinel() {
        let mut body = sample_update();
        body.microwaveprop.as_mut().unwrap().monitor_token = "redacted".to_string();
        let yaml = build_yaml_from_update(&body, &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.microwaveprop.unwrap().monitor_token, "secret-token");
    }

    #[test]
    fn build_yaml_from_update_preserves_period_when_missing_from_body() {
        let body = sample_update(); // period_seconds: None
        let mut current = sample_cfg();
        current.period_seconds = 45;
        let yaml = build_yaml_from_update(&body, &current, None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.period_seconds, 45);
    }

    #[test]
    fn build_yaml_from_update_uses_explicit_period_when_provided() {
        let mut body = sample_update();
        body.period_seconds = Some(120);
        let mut current = sample_cfg();
        current.period_seconds = 45;
        let yaml = build_yaml_from_update(&body, &current, None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.period_seconds, 120);
    }

    #[test]
    fn build_yaml_from_update_omits_microwaveprop_block_when_absent() {
        let mut body = sample_update();
        body.microwaveprop = None;
        let yaml = build_yaml_from_update(&body, &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert!(parsed.microwaveprop.is_none());
    }

    #[test]
    fn build_yaml_from_update_handles_missing_http() {
        let mut body = sample_update();
        body.http = None;
        let yaml = build_yaml_from_update(&body, &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.http.bind, "0.0.0.0:5760");
    }

    #[test]
    fn build_yaml_from_update_handles_no_gain_for_agc() {
        let mut body = sample_update();
        body.gain = None;
        let yaml = build_yaml_from_update(&body, &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert!(parsed.gain.is_none());
    }

    #[test]
    fn build_yaml_from_update_handles_disabled_microwaveprop() {
        let mut body = sample_update();
        body.microwaveprop.as_mut().unwrap().enabled = false;
        let yaml = build_yaml_from_update(&body, &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert!(!parsed.microwaveprop.unwrap().enabled);
    }

    /// A local edit must not silently reset the version microwaveprop last
    /// handed the node, or the next reconnect looks like a node that never
    /// applied its config.
    #[test]
    fn build_yaml_from_update_carries_the_persisted_config_version_forward() {
        let yaml = build_yaml_from_update(&sample_update(), &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.microwaveprop.unwrap().config_version, 7);
    }

    #[test]
    fn build_yaml_from_update_records_a_newly_minted_config_version() {
        let yaml = build_yaml_from_update(&sample_update(), &sample_cfg(), Some(12)).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.microwaveprop.unwrap().config_version, 12);
    }

    #[test]
    fn cfg_to_update_round_trips_the_active_config_through_the_writer() {
        let cfg = sample_cfg();
        let yaml = build_yaml_from_update(&cfg_to_update(&cfg), &cfg, None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.frequency, cfg.frequency);
        assert_eq!(parsed.mode, cfg.mode);
        assert_eq!(parsed.driver, cfg.driver);
        assert_eq!(parsed.sample_rate, cfg.sample_rate);
        assert_eq!(parsed.gain, cfg.gain);
        assert_eq!(parsed.period_seconds, cfg.period_seconds);
        assert_eq!(parsed.http.bind, cfg.http.bind);
        assert_eq!(parsed.beacon.unwrap().bandwidth_hz, 300.0);
        let mw = parsed.microwaveprop.unwrap();
        assert_eq!(mw.monitor_token, "secret-token");
        assert_eq!(mw.beacon_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(mw.config_version, 7);
    }

    fn measurement(active: f64) -> Measurement {
        Measurement {
            noise_dbfs: -110.0,
            signal_peak_dbfs: -88.0,
            signal_avg_dbfs: -89.0,
            snr_peak_db: 22.0,
            snr_avg_db: 21.0,
            signal_active_fraction: active,
        }
    }

    #[test]
    fn convert_worker_event_device_info_sets_update_flag() {
        let ev = WorkerEvent::DeviceInfo {
            actual_sample_rate: 250_000.0,
            actual_frequency: 28_330_000.0,
            actual_gain: 10.0,
            gain_elements: vec!["TUNER".to_string()],
        };
        let step = convert_worker_event(&ev, "2026-05-13T15:30:00Z");
        assert!(step.update_device_info);
        assert!(step.update_last_raw_dbfs.is_none());
        assert!(step.push_measurement.is_none());
        match step.ws_event {
            WsEvent::DeviceInfo { actual_gain, .. } => assert_eq!(actual_gain, 10.0),
            _ => panic!("expected DeviceInfo"),
        }
    }

    #[test]
    fn convert_worker_event_raw_level_updates_last_raw() {
        let ev = WorkerEvent::RawLevel { dbfs: -34.2 };
        let step = convert_worker_event(&ev, "t");
        assert_eq!(step.update_last_raw_dbfs, Some(-34.2));
        match step.ws_event {
            WsEvent::RawLevel { dbfs } => assert_eq!(dbfs, -34.2),
            _ => panic!("expected RawLevel"),
        }
    }

    #[test]
    fn convert_worker_event_waterfall_passes_through() {
        let bins = vec![0.1, 0.2, 0.3];
        let ev = WorkerEvent::WaterfallRow {
            bins: bins.clone(),
            f0_hz: -125_000.0,
            bin_hz: 244.14,
        };
        let step = convert_worker_event(&ev, "t");
        match step.ws_event {
            WsEvent::Waterfall {
                f0_hz,
                bin_hz,
                bins: b,
            } => {
                assert_eq!(f0_hz, -125_000.0);
                assert_eq!(bin_hz, 244.14);
                assert_eq!(b, bins);
            }
            _ => panic!("expected Waterfall"),
        }
    }

    #[test]
    fn convert_worker_event_period_started_uses_supplied_time() {
        let step = convert_worker_event(&WorkerEvent::PeriodStarted, "2026-05-13T15:30:00Z");
        match step.ws_event {
            WsEvent::PeriodStarted { at } => assert_eq!(at, "2026-05-13T15:30:00Z"),
            _ => panic!("expected PeriodStarted"),
        }
    }

    #[test]
    fn measurement_carries_period_start_timestamp() {
        let mut period_start: Option<String> = None;
        // A period begins at T1.
        assert_eq!(
            stamp_event(&WorkerEvent::PeriodStarted, "T1", &mut period_start),
            "T1"
        );
        // The measurement closing that period is stamped with T1 (window
        // start), even though the bridge clock has since advanced to T2.
        let ev = WorkerEvent::PeriodMeasurement(measurement(1.0));
        assert_eq!(stamp_event(&ev, "T2", &mut period_start), "T1");
        // Unrelated events carry the current clock, not the stale start.
        assert_eq!(
            stamp_event(
                &WorkerEvent::RawLevel { dbfs: -34.0 },
                "T2",
                &mut period_start
            ),
            "T2"
        );
    }

    #[test]
    fn convert_worker_event_measurement_pushes_to_store_and_carries_fraction() {
        let ev = WorkerEvent::PeriodMeasurement(measurement(0.48));
        let step = convert_worker_event(&ev, "2026-05-13T15:30:00Z");
        let pushed = step.push_measurement.unwrap();
        assert_eq!(pushed.signal_active_fraction, 0.48);
        assert_eq!(pushed.measured_at, "2026-05-13T15:30:00Z");
        match step.ws_event {
            WsEvent::Measurement {
                signal_active_fraction,
                ..
            } => assert_eq!(signal_active_fraction, 0.48),
            _ => panic!("expected Measurement"),
        }
    }

    #[test]
    fn convert_worker_event_error_passes_message() {
        let step = convert_worker_event(&WorkerEvent::Error("boom".to_string()), "t");
        match step.ws_event {
            WsEvent::Error { message } => assert_eq!(message, "boom"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn ws_event_serializes_with_snake_case_type_tag() {
        let ev = WsEvent::RawLevel { dbfs: -34.0 };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"raw_level\""), "got {}", s);
        let ev2 = WsEvent::PeriodStarted {
            at: "x".to_string(),
        };
        let s2 = serde_json::to_string(&ev2).unwrap();
        assert!(s2.contains("\"type\":\"period_started\""), "got {}", s2);
    }

    #[test]
    fn ws_event_measurement_includes_active_fraction() {
        let ev = WsEvent::Measurement {
            measured_at: "t".to_string(),
            noise_floor_dbfs: -110.0,
            signal_peak_dbfs: -88.0,
            signal_avg_dbfs: -89.0,
            snr_peak_db: 22.0,
            snr_avg_db: 21.0,
            signal_active_fraction: 0.5,
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["signal_active_fraction"], 0.5);
        assert_eq!(v["type"], "measurement");
    }

    #[test]
    fn new_broadcaster_round_trips_an_event() {
        let tx = new_broadcaster();
        let mut rx = tx.subscribe();
        tx.send(WsEvent::RawLevel { dbfs: -50.0 }).unwrap();
        let got = rx.try_recv().unwrap();
        match got {
            WsEvent::RawLevel { dbfs } => assert_eq!(dbfs, -50.0),
            _ => panic!("expected RawLevel"),
        }
    }

    /// `enumerate_devices` calls into SoapySDR which may or may not be
    /// present in the test environment. Either way the function must
    /// return without panicking — at worst an empty list.
    #[test]
    fn enumerate_devices_does_not_panic() {
        let _ = enumerate_devices();
    }

    /// Builds an `AppState` with no worker thread — suitable for hitting
    /// handler logic via `tower::ServiceExt::oneshot` without opening an
    /// SDR. The store, last-raw, and device-info fields can be seeded
    /// before the call to drive the read-only handlers.
    fn test_state(cfg: Config, config_path: &str) -> Arc<AppState> {
        test_state_with_channel(cfg, config_path, crate::update::MANIFEST_URL)
    }

    /// `test_state`, with the release channel pointed somewhere else — a
    /// local test server, so a check can be driven end to end without
    /// touching GitHub.
    fn test_state_with_channel(
        cfg: Config,
        config_path: &str,
        manifest_url: &str,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            config_path: config_path.to_string(),
            config: Arc::new(RwLock::new(cfg)),
            broadcaster: new_broadcaster(),
            store: RwLock::new(MeasurementStore::new()),
            last_raw_dbfs: RwLock::new(None),
            device_info: Arc::new(RwLock::new(None)),
            uploader_status: Arc::new(RwLock::new(UploaderStatus::default())),
            worker_handle: StdMutex::new(None),
            http_server: StdMutex::new(None),
            sync_notify: watch::Sender::new(0),
            config_write: tokio::sync::Mutex::new(()),
            update_state: Arc::new(RwLock::new(UpdateState::new())),
            update_notify: watch::Sender::new(UpdateRequest::Idle),
            install_path: std::env::temp_dir().join("propmonitor-test-binary"),
            manifest_url: manifest_url.to_string(),
        })
    }

    async fn body_bytes(resp: axum::http::Response<axum::body::Body>) -> Vec<u8> {
        let body = resp.into_body();
        let collected = axum::body::to_bytes(body, 64 * 1024).await.unwrap();
        collected.to_vec()
    }

    #[tokio::test]
    async fn get_index_returns_html() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(sample_cfg(), "config.yaml"));
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/html"));
        let body = body_bytes(resp).await;
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn get_app_js_returns_javascript() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(sample_cfg(), "config.yaml"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/assets/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().contains("javascript"));
    }

    #[tokio::test]
    async fn get_style_css_returns_css() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(sample_cfg(), "config.yaml"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/assets/style.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().contains("text/css"));
    }

    #[tokio::test]
    async fn get_api_config_redacts_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(sample_cfg(), "config.yaml"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["microwaveprop"]["monitor_token"], "redacted");
        assert_eq!(
            v["microwaveprop"]["beacon_id"],
            "00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(v["microwaveprop"]["gridsquare"], "EM12il");
    }

    #[tokio::test]
    async fn get_api_status_returns_snapshot() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let state = test_state(sample_cfg(), "config.yaml");
        *state.last_raw_dbfs.write().await = Some(-42.0);
        state
            .store
            .write()
            .await
            .push(crate::store::StoredMeasurement {
                measured_at: "2026-05-13T15:30:00Z".to_string(),
                noise_floor_dbfs: -110.0,
                signal_peak_dbfs: -88.0,
                signal_avg_dbfs: -89.0,
                snr_peak_db: 22.0,
                snr_avg_db: 21.0,
                signal_active_fraction: 0.5,
            });
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["last_raw_dbfs"], -42.0);
        assert_eq!(v["last_measurement"]["signal_active_fraction"], 0.5);
    }

    #[tokio::test]
    async fn get_api_measurements_returns_recent_ring() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let state = test_state(sample_cfg(), "config.yaml");
        for i in 0..3 {
            state
                .store
                .write()
                .await
                .push(crate::store::StoredMeasurement {
                    measured_at: format!("2026-05-13T00:{:02}:00Z", i),
                    noise_floor_dbfs: -110.0,
                    signal_peak_dbfs: -90.0 + i as f64,
                    signal_avg_dbfs: -91.0,
                    snr_peak_db: 20.0,
                    snr_avg_db: 18.0,
                    signal_active_fraction: 1.0,
                });
        }
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/measurements?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v["measurements"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn get_api_measurements_uses_default_limit_when_query_missing() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let state = test_state(sample_cfg(), "config.yaml");
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/measurements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_api_devices_returns_devices_array() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(sample_cfg(), "config.yaml"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["devices"].is_array());
    }

    #[tokio::test]
    async fn put_api_config_rejects_malformed_json() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(sample_cfg(), "/tmp/propmonitor-test-put.yaml"));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn put_api_config_rejects_invalid_config() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router(test_state(
            sample_cfg(),
            "/tmp/propmonitor-test-put-bad.yaml",
        ));
        // mode=beacon with no beacon block — should fail validation.
        let body = serde_json::json!({
            "frequency": 28330000,
            "mode": "beacon",
            "driver": "rtlsdr",
            "sample_rate": 250000,
            "gain": 10,
            "ppm": 0,
            "http": { "bind": "0.0.0.0:5760" }
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = body_bytes(resp).await;
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn persist_config_version_writes_disk_without_restarting_the_worker() {
        let path = std::env::temp_dir().join(format!(
            "propmonitor-config-version-{}.yaml",
            std::process::id()
        ));
        let state = test_state(sample_cfg(), path.to_str().unwrap());

        persist_config_version(&state, 12).await.unwrap();

        assert_eq!(
            state
                .config
                .read()
                .await
                .microwaveprop
                .as_ref()
                .unwrap()
                .config_version,
            12
        );
        let on_disk = Config::from_yaml_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mw = on_disk.microwaveprop.unwrap();
        assert_eq!(mw.config_version, 12);
        // The rest of the config — token included — survives the rewrite.
        assert_eq!(mw.monitor_token, "secret-token");
        assert_eq!(on_disk.frequency, 28_330_000.0);
        assert_eq!(on_disk.beacon.unwrap().bandwidth_hz, 300.0);
        // No SDR churn: a version-only update never spawns a worker.
        assert!(state
            .worker_handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none());

        let _ = std::fs::remove_file(&path);
    }

    /// Recording the version we already have must not rewrite config.yaml
    /// — an unwritable path is the cheapest proof that no write happened.
    #[tokio::test]
    async fn persist_config_version_skips_the_write_when_unchanged() {
        let state = test_state(sample_cfg(), "/nonexistent-dir/propmonitor.yaml");
        persist_config_version(&state, 7).await.unwrap();
    }

    #[tokio::test]
    async fn persist_config_version_fails_without_a_microwaveprop_block() {
        let mut cfg = sample_cfg();
        cfg.microwaveprop = None;
        let state = test_state(cfg, "/nonexistent-dir/propmonitor.yaml");
        assert!(persist_config_version(&state, 3).await.is_err());
    }

    #[tokio::test]
    async fn forward_upload_events_emits_ws_upload_frame() {
        let state = test_state(sample_cfg(), "config.yaml");
        let mut ws_rx = state.broadcaster.subscribe();

        let (up_tx, up_rx) = broadcast::channel::<UploadEvent>(4);
        let s = state.clone();
        let handle = tokio::spawn(forward_upload_events(s, up_rx));

        up_tx
            .send(UploadEvent {
                at: "2026-05-13T15:30:01Z".to_string(),
                ok: true,
                queued: 0,
            })
            .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_millis(200), ws_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match got {
            WsEvent::Upload { status, .. } => assert_eq!(status, "ok"),
            other => panic!("expected upload, got {:?}", other),
        }

        drop(up_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), handle).await;
    }

    #[test]
    fn error_response_carries_status_and_json_message() {
        let r = error_response(StatusCode::BAD_REQUEST, "bad input".to_string());
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn default_true_helper() {
        assert!(default_true());
    }

    #[test]
    fn worker_handle_stop_and_join_is_idempotent() {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let join = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let s3 = stop.clone();
        let bridge = std::thread::spawn(move || {
            while !s3.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let mut h = WorkerHandle {
            stop,
            join: Some(join),
            bridge: Some(bridge),
        };
        h.stop_and_join();
        // Calling it again must not panic.
        h.stop_and_join();
    }

    // ---------------- self-update surface -----------------------------

    /// Stands in for the release channel: serves one manifest at
    /// `/manifest`. Returns the URL to point `AppState::manifest_url` at.
    async fn fake_channel(manifest: serde_json::Value) -> String {
        let body = manifest.to_string();
        let app = Router::new().route(
            "/manifest",
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/manifest")
    }

    async fn get_json(state: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let resp = build_router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = body_bytes(resp).await;
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post_json(state: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let resp = build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = body_bytes(resp).await;
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn get_api_update_reports_the_running_build_and_settings() {
        let state = test_state(sample_cfg(), "config.yaml");
        let (status, v) = get_json(state, "/api/update").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["current"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["current"]["commit"], crate::update::CURRENT_COMMIT);
        assert_eq!(v["phase"], "idle");
        assert!(v["latest"].is_null(), "nothing checked yet");
        assert!(v["last_check_at"].is_null());
        assert!(v["last_error"].is_null());
        // The settings the UI renders its form from.
        assert_eq!(v["enabled"], true);
        assert_eq!(v["auto"], true);
        assert_eq!(v["check_interval"], 3600);
        assert!(
            v["install_path"].as_str().is_some_and(|p| !p.is_empty()),
            "the UI shows where a new binary lands"
        );
    }

    /// The whole check path, driven through the endpoint the UI calls:
    /// POST wakes the channel task, the task fetches the manifest from a
    /// local stand-in, and the result shows up on GET.
    #[tokio::test]
    async fn post_api_update_check_records_an_available_build() {
        let url = fake_channel(serde_json::json!({
            "version": "9.9.9",
            "commit": "1111111111111111111111111111111111111111",
            "built_at": "2026-08-30T12:00:00Z",
            "assets": { "propmonitor-x86_64-linux": "abc123" },
        }))
        .await;
        let state = test_state_with_channel(sample_cfg(), "config.yaml", &url);
        let task = tokio::spawn(crate::update::run(state.clone()));

        let (status, _) = post_json(state.clone(), "/api/update/check").await;
        assert_eq!(status, StatusCode::OK);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let latest = loop {
            let (_, v) = get_json(state.clone(), "/api/update").await;
            if !v["latest"].is_null() {
                break v;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the check never completed: {v}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        assert_eq!(latest["latest"]["version"], "9.9.9");
        assert_eq!(
            latest["latest"]["commit"],
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(latest["phase"], "idle", "the check finished");
        assert!(latest["last_check_at"].is_string());
        assert!(latest["last_error"].is_null());

        task.abort();
    }

    /// A manifest describing the build we are already running means "up to
    /// date", not "update available".
    #[tokio::test]
    async fn a_check_against_our_own_commit_reports_up_to_date() {
        let url = fake_channel(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": crate::update::CURRENT_COMMIT,
            "built_at": "2026-08-30T12:00:00Z",
            "assets": { "propmonitor-x86_64-linux": "abc123" },
        }))
        .await;
        let state = test_state_with_channel(sample_cfg(), "config.yaml", &url);
        let task = tokio::spawn(crate::update::run(state.clone()));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (_, v) = get_json(state.clone(), "/api/update").await;
            if v["last_check_at"].is_string() {
                assert!(v["latest"].is_null(), "already current: {v}");
                assert!(v["last_error"].is_null());
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no check ran: {v}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        task.abort();
    }

    /// An unreachable channel is reported, not fatal — and the next check
    /// is still allowed to run.
    ///
    /// The message has to name the transport failure: `reqwest::Error`'s
    /// own `Display` stops at "error sending request for url (…)", which
    /// tells an operator that something failed and nothing about what, so
    /// the reported string must carry the wrapped cause too.
    #[tokio::test]
    async fn a_failed_check_surfaces_an_error_and_stays_idle() {
        // Port 1 on loopback: refused immediately, no waiting.
        let state = test_state_with_channel(sample_cfg(), "config.yaml", "http://127.0.0.1:1/x");
        let task = tokio::spawn(crate::update::run(state.clone()));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, v) = get_json(state.clone(), "/api/update").await;
            if let Some(err) = v["last_error"].as_str() {
                assert!(err.contains("release channel"), "unexpected error: {err}");
                assert!(
                    err.contains("Connection refused") || err.contains("os error 61"),
                    "error dropped the transport cause: {err}"
                );
                assert_eq!(v["phase"], "idle");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no failure seen: {v}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        task.abort();
    }

    #[tokio::test]
    async fn post_api_update_install_refuses_when_there_is_nothing_to_install() {
        let state = test_state(sample_cfg(), "config.yaml");
        let (status, v) = post_json(state, "/api/update/install").await;
        assert_eq!(status, StatusCode::CONFLICT);
        let err = v["error"].as_str().unwrap();
        // Off-Linux the platform check answers first; either way the UI
        // gets a reason rather than a silent no-op.
        assert!(
            err.contains("no update available") || err.contains("requires a Linux install"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn post_api_update_install_refuses_while_a_check_is_running() {
        let state = test_state(sample_cfg(), "config.yaml");
        state.update_state.write().await.phase = crate::update::Phase::Downloading;
        let (status, v) = post_json(state, "/api/update/install").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(v["error"].as_str().unwrap().contains("already running"));
    }

    #[tokio::test]
    async fn ws_update_events_reach_open_clients() {
        let state = test_state(sample_cfg(), "config.yaml");
        let mut rx = state.broadcaster.subscribe();
        state
            .broadcaster
            .send(WsEvent::Update {
                phase: crate::update::Phase::Checking,
                latest: None,
                error: None,
            })
            .unwrap();

        let got = rx.try_recv().unwrap();
        let json = serde_json::to_value(&got).unwrap();
        assert_eq!(json["type"], "update");
        assert_eq!(json["phase"], "checking");
        assert!(json["latest"].is_null());
    }

    #[test]
    fn cfg_to_view_exposes_the_update_settings() {
        let mut cfg = sample_cfg();
        cfg.update = crate::config::UpdateConfig {
            enabled: false,
            auto: false,
            check_interval: 7200,
        };
        let v = serde_json::to_value(cfg_to_view(&cfg)).unwrap();
        assert_eq!(v["update"]["enabled"], false);
        assert_eq!(v["update"]["auto"], false);
        assert_eq!(v["update"]["check_interval"], 7200);
    }

    #[test]
    fn build_yaml_from_update_writes_the_update_block() {
        let yaml = build_yaml_from_update(&sample_update(), &sample_cfg(), None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        // sample_update() asks for auto: false, 1800 s.
        assert!(parsed.update.enabled);
        assert!(!parsed.update.auto);
        assert_eq!(parsed.update.check_interval, 1800);
    }

    /// A body without an `update` block — the sync task's shape — must
    /// leave the node's own update settings exactly as they were.
    #[test]
    fn build_yaml_from_update_preserves_update_settings_when_absent() {
        let mut current = sample_cfg();
        current.update = crate::config::UpdateConfig {
            enabled: true,
            auto: false,
            check_interval: 900,
        };
        let mut body = sample_update();
        body.update = None;

        let yaml = build_yaml_from_update(&body, &current, None).unwrap();
        let parsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed.update, current.update);
    }
}
