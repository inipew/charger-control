mod ipc;
mod logging;
mod monitor;

use std::{path::Path, sync::Arc};
use tokio::{signal, sync::RwLock};
use charger_core::config::schema::{Config, DEFAULT_CONFIG_PATH};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // 1. Load config
    let config_path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e}. Using default.");
            Config::default()
        }
    };
    
    // 2. Init logging
    if let Err(e) = logging::init_logger(&config.log_path) {
        eprintln!("Warning: Failed to initialize logger at {:?}: {}", config.log_path, e);
    }
    tracing::info!("Charger daemon starting up");

    let config = Arc::new(RwLock::new(config));

    // 3. Setup shutdown channel
    let (shutdown_tx_ipc, shutdown_rx_ipc) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx_mon, shutdown_rx_mon) = tokio::sync::mpsc::channel(1);

    // 4. Start IPC server (conditionally on unix to allow compilation on Windows during dev)
    #[cfg(unix)]
    let ipc_handle = tokio::spawn(ipc::start_ipc_server(Arc::clone(&config), shutdown_rx_ipc));
    #[cfg(not(unix))]
    let ipc_handle = tokio::spawn(async move { let _ = shutdown_rx_ipc; tracing::warn!("IPC server not supported on non-unix"); });

    // 5. Start Monitor Loop
    let monitor_handle = tokio::spawn(monitor::run_monitor_loop(Arc::clone(&config), shutdown_rx_mon));

    // 6. Wait for termination signal
    match signal::ctrl_c().await {
        Ok(()) => {
            tracing::info!("Received Ctrl-C, shutting down");
        }
        Err(err) => {
            tracing::error!("Unable to listen for shutdown signal: {}", err);
        }
    }

    // 7. Graceful shutdown
    let _ = shutdown_tx_ipc.send(()).await;
    let _ = shutdown_tx_mon.send(()).await;
    
    let _ = tokio::join!(ipc_handle, monitor_handle);
    tracing::info!("Charger daemon stopped");
}
