mod ipc;
mod logging;
mod monitor;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

use charger_core::config::schema::{Config, DEFAULT_CONFIG_PATH};

#[cfg(unix)]
fn setup_environment() {
    unsafe {
        libc::umask(0o022);

        let root = b"/\0";

        libc::chdir(root.as_ptr() as *const libc::c_char);
    }
}

#[cfg(unix)]
fn acquire_lock() -> Result<std::fs::File, String> {
    let lock_path = charger_core::config::schema::DEFAULT_LOCK_PATH;
    let pid_path = charger_core::config::schema::DEFAULT_PID_PATH;

    if let Some(parent) = Path::new(lock_path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed creating lock directory: {error}"))?;
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| format!("Failed opening lock file: {error}"))?;

    unsafe {
        let fd = file.as_raw_fd();

        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return Err("Daemon is already running (flock failed)".to_string());
        }
    }

    let pid = std::process::id();
    if let Err(error) = std::fs::write(pid_path, pid.to_string()) {
        eprintln!("Warning: Failed writing PID file: {error}");
    }

    Ok(file)
}

#[cfg(unix)]
fn cleanup_pid_file() {
    let _ = std::fs::remove_file(charger_core::config::schema::DEFAULT_PID_PATH);
}

#[cfg(unix)]
fn main() {
    setup_environment();

    let _lock_file = match acquire_lock() {
        Ok(file) => file,

        Err(error) => {
            eprintln!("FATAL: {error}");

            std::process::exit(1);
        }
    };

    let config_path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();

    let config = match Config::load(&config_path) {
        Ok(config) => config,

        Err(error) => {
            eprintln!("Failed to load config: {error}");

            let mut default_config = Config::default();
            default_config.validate();

            if let Some(parent) = config_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            if let Err(save_error) = default_config.save(&config_path) {
                eprintln!(
                    "Failed to save default config: \
                         {save_error}"
                );
            }

            default_config
        }
    };

    let log_path = config.log_path.clone();

    if let Err(error) = logging::init_logger(&log_path) {
        eprintln!("Failed to initialize logging: {error}");

        cleanup_pid_file();
        return;
    }

    tracing::info!(
        "ChargerControl daemon starting (PID {})",
        std::process::id()
    );

    tracing::info!("Config path: {}", config_path.display());

    tracing::info!("Log path: {}", log_path.display());

    let shared_config = Arc::new(RwLock::new(config));

    let (tx, rx) = match std::os::unix::net::UnixDatagram::pair() {
        Ok(pair) => pair,

        Err(error) => {
            tracing::error!(
                error = %error,
                "Failed creating internal IPC"
            );

            cleanup_pid_file();
            return;
        }
    };

    if let Err(error) = rx.set_nonblocking(true) {
        tracing::error!(
            error = %error,
            "Failed setting internal IPC receiver to nonblocking"
        );

        cleanup_pid_file();
        return;
    }

    let ipc_shutdown = Arc::new(AtomicBool::new(false));

    let shared_diagnostics = Arc::new(ipc::DaemonDiagnostics::new());

    let tx_for_signal = match tx.try_clone() {
        Ok(cloned) => Some(cloned),
        Err(error) => {
            tracing::warn!("Failed cloning IPC datagram for signal handler: {error}");
            None
        }
    };

    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::signal::SIGTERM,
        signal_hook::consts::signal::SIGINT,
    ]) {
        let _ = std::thread::Builder::new()
            .name("signal-handler".to_string())
            .stack_size(256 * 1024)
            .spawn(move || {
                if let Some(signal) = signals.forever().next() {
                    tracing::warn!("Received signal {}, initiating graceful shutdown", signal);

                    if let Some(signal_tx) = tx_for_signal {
                        let _ = signal_tx.send(&[2]);
                    }
                }
            });
    }

    let config_for_ipc = Arc::clone(&shared_config);

    let shutdown_for_ipc = Arc::clone(&ipc_shutdown);

    let diagnostics_for_ipc = Arc::clone(&shared_diagnostics);

    let ipc_thread = match std::thread::Builder::new()
        .name("ipc-server".to_string())
        .stack_size(512 * 1024)
        .spawn(move || {
            ipc::start_ipc_server(config_for_ipc, tx, shutdown_for_ipc, diagnostics_for_ipc);
        }) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::error!(error = %error, "Failed spawning IPC server thread");
            cleanup_pid_file();
            return;
        }
    };

    monitor::run_monitor_loop(Arc::clone(&shared_config), rx, shared_diagnostics);

    tracing::info!("Monitor requested daemon shutdown");

    if let Err(error) = charger_core::battery::control::set_charging(true) {
        tracing::error!(
            error = %error,
            "Failed to restore charging"
        );
    } else {
        tracing::info!("Charging restored successfully");
    }

    ipc_shutdown.store(true, Ordering::Release);
    let _ = std::os::unix::net::UnixStream::connect(ipc::SOCKET_PATH);

    if let Err(error) = ipc_thread.join() {
        tracing::error!(?error, "IPC thread terminated unexpectedly");
    }

    cleanup_pid_file();
    let _ = std::fs::remove_file(ipc::SOCKET_PATH);

    tracing::info!("ChargerControl daemon exited gracefully");
}

#[cfg(not(unix))]
fn main() {
    eprintln!("charger-daemon is only supported on Linux/Android.");
}
