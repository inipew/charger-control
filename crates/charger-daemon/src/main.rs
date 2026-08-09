mod ipc;
mod logging;
mod monitor;

use charger_core::config::schema::{Config, DEFAULT_CONFIG_PATH};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, RwLock};

fn setup_environment() {
    unsafe {
        libc::umask(0o022);
        libc::chdir(c"/".as_ptr());
    }
}

fn acquire_lock() -> Result<std::fs::File, String> {
    let lock_path = "/data/adb/charger-control/daemon.lock";
    if let Some(p) = std::path::Path::new(lock_path).parent() {
        let _ = std::fs::create_dir_all(p);
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| format!("Gagal membuka file lock: {e}"))?;

    unsafe {
        let fd = file.as_raw_fd();
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return Err("Daemon sudah berjalan! (flock gagal)".to_string());
        }
    }

    Ok(file)
}

fn main() {
    setup_environment();

    // Simpan file lock agar tidak di-drop selama daemon hidup
    let _lock_file = match acquire_lock() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("FATAL: {e}");
            std::process::exit(1);
        }
    };

    let config_path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();

    // Load config (synchronous)
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            let def = Config::default();
            // Try to create parent dirs and save default if it fails to load
            if let Some(p) = config_path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = def.save(&config_path);
            eprintln!("Failed to load config: {e}. Using defaults.");
            def
        }
    };

    let log_path = config.log_path.clone();
    let shared_config = Arc::new(RwLock::new(config));

    // Initialize synchronous logging
    if let Err(e) = logging::init_logger(&log_path) {
        eprintln!("Failed to initialize logging: {e}");
        return;
    }

    tracing::info!("ChargerControl Daemon Starting (Pure Native STD)");

    let (tx, rx) = std::os::unix::net::UnixDatagram::pair()
        .expect("Failed to create UnixDatagram pair for IPC");

    // Spawn Background Thread for SIGTERM / SIGINT Signal Handling
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::signal::SIGTERM,
        signal_hook::consts::signal::SIGINT,
    ]) {
        let tx_signal = tx.try_clone().expect("Failed to clone UnixDatagram for signals");
        std::thread::spawn(move || {
            if let Some(sig) = signals.forever().next() {
                tracing::info!(
                    "Received signal {}, initiating graceful shutdown...",
                    sig
                );
                let _ = tx_signal.send(&[2]);
                // The main thread will receive this, restore hardware, and exit
            }
        });
    }

    // Spawn Background Thread for IPC Server
    let config_for_ipc = Arc::clone(&shared_config);
    std::thread::spawn(move || {
        ipc::start_ipc_server(config_for_ipc, tx);
    });

    // Main Thread runs the Monitor Loop
    monitor::run_monitor_loop(shared_config, rx);

    tracing::info!("Daemon exited gracefully");
}
