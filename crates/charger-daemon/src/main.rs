mod ipc;
mod logging;
mod monitor;

use std::{
    os::unix::io::AsRawFd,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

use charger_core::config::schema::{Config, DEFAULT_CONFIG_PATH};

fn setup_environment() {
    unsafe {
        libc::umask(0o022);

        let root = b"/\0";

        libc::chdir(root.as_ptr() as *const libc::c_char);
    }
}

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

fn cleanup_pid_file() {
    let _ = std::fs::remove_file(charger_core::config::schema::DEFAULT_PID_PATH);
}

fn main() {
    setup_environment();

    /*
     * Keep the lock alive for the entire process lifetime.
     *
     * The lock file itself is NOT authoritative.
     * Kernel flock is authoritative.
     */
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

            let default_config = Config::default();

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

    /*
     * Signal handling.
     *
     * SIGTERM/SIGINT are emergency paths.
     * Normal CLI shutdown goes through IPC.
     */
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::signal::SIGTERM,
        signal_hook::consts::signal::SIGINT,
    ]) {
        std::thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                tracing::warn!("Received signal {}", signal);

                let _ = charger_core::battery::control::set_charging(true);

                cleanup_pid_file();
                let _ = std::fs::remove_file(ipc::SOCKET_PATH);

                /*
                 * Never remove daemon.lock manually.
                 *
                 * flock is released by the kernel when
                 * this process terminates.
                 */
                std::process::exit(0);
            }
        });
    }

    /*
     * Start IPC server.
     */
    let config_for_ipc = Arc::clone(&shared_config);

    let shutdown_for_ipc = Arc::clone(&ipc_shutdown);

    let ipc_thread = std::thread::spawn(move || {
        ipc::start_ipc_server(config_for_ipc, tx, shutdown_for_ipc);
    });

    /*
     * Monitor owns the normal daemon lifecycle.
     */
    monitor::run_monitor_loop(Arc::clone(&shared_config), rx);

    tracing::info!("Monitor requested daemon shutdown");

    /*
     * Restore safe charging state.
     */
    if let Err(error) = charger_core::battery::control::set_charging(true) {
        tracing::error!(
            error = %error,
            "Failed to restore charging"
        );
    } else {
        tracing::info!("Charging restored successfully");
    }

    /*
     * Stop IPC server.
     */
    ipc_shutdown.store(true, Ordering::Release);

    /*
     * Wait until IPC has:
     *
     * - stopped accepting clients;
     * - dropped listener;
     * - removed socket.
     */
    if let Err(error) = ipc_thread.join() {
        tracing::error!(?error, "IPC thread terminated unexpectedly");
    }

    /*
     * Defensive cleanup only.
     */
    cleanup_pid_file();
    let _ = std::fs::remove_file(ipc::SOCKET_PATH);

    tracing::info!("ChargerControl daemon exited gracefully");
}
