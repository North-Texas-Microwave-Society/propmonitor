mod config;
mod error;
mod measure;
mod server;
mod store;
mod timefmt;
mod tray;
mod uploader;
mod worker;
mod yaml;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::{Context, Error, Result};
use crate::server::{
    forward_upload_events, new_broadcaster, spawn_worker_and_bridge, AppState,
};
use crate::store::MeasurementStore;
use crate::uploader::{new_event_channel, UploaderStatus};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "config.yaml".to_string());
    let cfg = Config::load(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path))?;
    let bind = cfg.http.bind.clone();
    let browser_url = browser_url_from_bind(&bind);

    // Tokio runs on a worker thread because the OS tray on Windows and
    // macOS *requires* the event loop on the process's main thread. The
    // server, worker thread, bridge, and uploader all live inside this
    // runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::msg(format!("tokio runtime: {}", e)))?;

    // Set up state and spawn background tasks while we still have the
    // runtime handle directly available.
    let state = rt.block_on(async { boot(cfg, config_path).await })?;

    {
        let state = state.clone();
        let bind = bind.clone();
        rt.spawn(async move {
            if let Err(e) = server::run(state, &bind).await {
                eprintln!("propmonitor: server exited: {}", e);
                std::process::exit(1);
            }
        });
    }

    // Auto-open browser shortly after the server starts. A short delay
    // is enough for axum to be listening on the port.
    //
    // Suppressed by `PROPMONITOR_NO_BROWSER=1` for headless deployments
    // and for any future integration tests that boot the server without
    // wanting to pop a real window. (Today's `cargo test` doesn't invoke
    // `main`, so this gate is belt-and-braces.)
    if std::env::var("PROPMONITOR_NO_BROWSER").is_err() {
        let url = browser_url.clone();
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = webbrowser::open(&url) {
                eprintln!("propmonitor: could not open browser: {}", e);
            }
        });
    }

    // Keep the tokio runtime alive for the lifetime of the process. The
    // tray event loop will own the main thread; when "Quit" is selected
    // it calls process::exit which tears the runtime down with it.
    //
    // The runtime is moved into a `Box::leak`-style static so it isn't
    // dropped at the end of main; otherwise `tray::run` would `loop {}`
    // happily but the spawned tasks would be cancelled by Drop.
    let _rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(rt));

    // Run the tray on the main thread. If the OS doesn't have a tray
    // (headless Linux over SSH, missing libayatana-appindicator, etc.)
    // fall back to a Ctrl+C-driven park so the server still runs.
    match tray::run(browser_url) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!(
                "propmonitor: tray unavailable ({}), running headless — Ctrl+C to exit",
                e
            );
            std::thread::park();
            Ok(())
        }
    }
}

async fn boot(cfg: Config, config_path: String) -> Result<Arc<AppState>> {
    let broadcaster = new_broadcaster();
    let (upload_tx, upload_rx) = new_event_channel();
    let config_arc = Arc::new(RwLock::new(cfg.clone()));
    let uploader_status_arc = Arc::new(RwLock::new(UploaderStatus::default()));

    let state = Arc::new(AppState {
        config_path,
        config: config_arc.clone(),
        broadcaster: broadcaster.clone(),
        store: RwLock::new(MeasurementStore::new()),
        last_raw_dbfs: RwLock::new(None),
        device_info: RwLock::new(None),
        uploader_status: uploader_status_arc.clone(),
        worker_handle: StdMutex::new(None),
    });

    {
        let handle = spawn_worker_and_bridge(state.clone(), cfg.clone());
        *state.worker_handle.lock().unwrap() = Some(handle);
    }

    {
        let cfg = config_arc.clone();
        let status = uploader_status_arc.clone();
        let measurements_rx = broadcaster.subscribe();
        let upload_tx = upload_tx.clone();
        tokio::spawn(async move {
            crate::uploader::run(cfg, measurements_rx, upload_tx, status).await;
        });
    }

    {
        let state_for_uploads = state.clone();
        tokio::spawn(forward_upload_events(state_for_uploads, upload_rx));
    }

    Ok(state)
}

/// Turn a config `http.bind` value into a URL the user's browser can
/// actually reach. `0.0.0.0` and `::` mean "listen on all interfaces"
/// to the OS, but browsers can't connect to those — they need a real
/// loopback or hostname.
fn browser_url_from_bind(bind: &str) -> String {
    let port = bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5760);
    format!("http://127.0.0.1:{}", port)
}
