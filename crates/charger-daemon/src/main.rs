mod ipc;
mod logging;
mod monitor;

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, RwLock};

use charger_core::config::schema::{
    Config,
    DEFAULT_CONFIG_PATH,
};

fn setup_environment() {
    unsafe {
        libc::umask(0o022);

        let root = b"/\0";

        libc::chdir(
            root.as_ptr() as *const libc::c_char
        );
    }
}

fn acquire_lock()
    -> Result<std::fs::File, String>
{
    let lock_path =
        "/data/adb/charger-control/daemon.lock";

    if let Some(parent) =
        Path::new(lock_path).parent()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| {
                format!(
                    "Gagal membuat directory lock: {e}"
                )
            })?;
    }

    let file =
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(lock_path)
            .map_err(|e| {
                format!(
                    "Gagal membuka file lock: {e}"
                )
            })?;

    unsafe {
        let fd = file.as_raw_fd();

        if libc::flock(
            fd,
            libc::LOCK_EX | libc::LOCK_NB,
        ) != 0
        {
            return Err(
                "Daemon sudah berjalan! \
                 (flock gagal)"
                    .to_string(),
            );
        }
    }

    Ok(file)
}

fn main() {
    setup_environment();

    /*
     * Keep the lock alive for the entire daemon lifetime.
     */
    let _lock_file =
        match acquire_lock() {
            Ok(file) => file,

            Err(e) => {
                eprintln!("FATAL: {e}");
                std::process::exit(1);
            }
        };

    let config_path =
        Path::new(DEFAULT_CONFIG_PATH)
            .to_path_buf();

    let config =
        match Config::load(&config_path) {
            Ok(config) => config,

            Err(e) => {
                eprintln!(
                    "Failed to load config: {e}"
                );

                let default_config =
                    Config::default();

                if let Some(parent) =
                    config_path.parent()
                {
                    let _ =
                        std::fs::create_dir_all(
                            parent,
                        );
                }

                if let Err(save_error) =
                    default_config.save(
                        &config_path,
                    )
                {
                    eprintln!(
                        "Failed to save default config: \
                         {save_error}"
                    );
                }

                default_config
            }
        };

    let log_path =
        config.log_path.clone();

    if let Err(e) =
        logging::init_logger(&log_path)
    {
        eprintln!(
            "Failed to initialize logging: {e}"
        );
        return;
    }

    tracing::info!(
        "ChargerControl daemon starting"
    );

    tracing::info!(
        "Config path: {}",
        config_path.display()
    );

    tracing::info!(
        "Log path: {}",
        log_path.display()
    );

    let shared_config =
        Arc::new(RwLock::new(config));

    let (tx, rx) =
        match std::os::unix::net::UnixDatagram::pair()
        {
            Ok(pair) => pair,

            Err(e) => {
                tracing::error!(
                    "Failed to create internal IPC: {}",
                    e
                );
                return;
            }
        };

    /*
     * Signal handling.
     *
     * The monitor is responsible for restoring charging
     * during normal shutdown. Signal handler is only a
     * last-resort emergency path.
     */
    if let Ok(mut signals) =
        signal_hook::iterator::Signals::new([
            signal_hook::consts::signal::SIGTERM,
            signal_hook::consts::signal::SIGINT,
        ])
    {
        std::thread::spawn(move || {
            if let Some(signal) =
                signals.forever().next()
            {
                tracing::warn!(
                    "Received signal {}",
                    signal
                );

                let _ =
                    charger_core::battery::control::set_charging(
                        true,
                    );

                let _ =
                    std::fs::remove_file(
                        ipc::SOCKET_PATH,
                    );

                /*
                 * Do not remove daemon.lock manually.
                 *
                 * The lock file itself can remain on disk;
                 * flock is released automatically when the
                 * process exits.
                 */

                std::process::exit(0);
            }
        });
    }

    /*
     * IPC server.
     */
    let config_for_ipc =
        Arc::clone(&shared_config);

    std::thread::spawn(move || {
        ipc::start_ipc_server(
            config_for_ipc,
            tx,
        );
    });

    /*
     * Main monitor loop.
     */
    monitor::run_monitor_loop(
        Arc::clone(&shared_config),
        rx,
    );

    /*
     * Graceful shutdown.
     */
    tracing::info!(
        "Daemon shutting down; \
         restoring charging state..."
    );

    if let Err(e) =
        charger_core::battery::control::set_charging(
            true,
        )
    {
        tracing::error!(
            "Failed to restore charging: {}",
            e
        );
    } else {
        tracing::info!(
            "Charging restored successfully"
        );
    }

    let _ =
        std::fs::remove_file(
            ipc::SOCKET_PATH,
        );

    tracing::info!(
        "ChargerControl daemon exited gracefully"
    );
}