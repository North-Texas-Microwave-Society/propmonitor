mod config;
mod error;
mod measure;
mod server;
mod store;
mod timefmt;
mod uploader;
mod worker;
mod yaml;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

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

    let port = bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5760);
    eprintln!(
        "propmonitor: listening on http://127.0.0.1:{} (Ctrl+C to exit)",
        port
    );

    // Block the main thread until Ctrl+C
    rt.block_on(async {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("propmonitor: shutting down");
    });

    Ok(())
}

async fn boot(cfg: Config, config_path: String) -> Result<Arc<AppState>> {
    let broadcaster = new_broadcaster();
    let (upload_tx, upload_rx) = new_event_channel();
    let config_arc = Arc::new(RwLock::new(cfg.clone()));
    let uploader_status_arc = Arc::new(RwLock::new(UploaderStatus::default()));
    let device_info_arc = Arc::new(RwLock::new(None));

    let state = Arc::new(AppState {
        config_path,
        config: config_arc.clone(),
        broadcaster: broadcaster.clone(),
        store: RwLock::new(MeasurementStore::new()),
        last_raw_dbfs: RwLock::new(None),
        device_info: device_info_arc.clone(),
        uploader_status: uploader_status_arc.clone(),
        worker_handle: StdMutex::new(None),
    });

    {
        let handle = spawn_worker_and_bridge(state.clone(), cfg.clone());
        *state.worker_handle.lock().unwrap() = Some(handle);
    }

    {
        let cfg = config_arc.clone();
        let device_info = device_info_arc.clone();
        let status = uploader_status_arc.clone();
        let measurements_rx = broadcaster.subscribe();
        let upload_tx = upload_tx.clone();
        tokio::spawn(async move {
            crate::uploader::run(cfg, device_info, measurements_rx, upload_tx, status).await;
        });
    }

    {
        let state_for_uploads = state.clone();
        tokio::spawn(forward_upload_events(state_for_uploads, upload_rx));
    }

    Ok(state)
}
