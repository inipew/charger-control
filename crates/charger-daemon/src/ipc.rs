use std::{
    io::{Read, Write},
    os::unix::net::{UnixDatagram, UnixListener, UnixStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use charger_core::battery::{control, reader};
use charger_core::config::schema::Config;
use libc::{poll, pollfd, POLLIN};

pub const SOCKET_PATH: &str = charger_core::config::schema::DEFAULT_SOCKET_PATH;

const IPC_READ_TIMEOUT: Duration = Duration::from_millis(750);

const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

const IPC_POLL_TIMEOUT_MS: i32 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonCommand {
    BypassOn,
    BypassOff,
    Reload,
    Status,
    Shutdown,
}

impl DaemonCommand {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let command = std::str::from_utf8(bytes).ok()?.trim();

        match command {
            "bypass on" => Some(Self::BypassOn),
            "bypass off" => Some(Self::BypassOff),
            "reload" => Some(Self::Reload),
            "status" => Some(Self::Status),
            "shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }
}

fn get_process_stats() -> (u32, f32, f32) {
    (std::process::id(), read_rss_mb(), read_cpu_percent())
}

fn read_rss_mb() -> f32 {
    let status = match std::fs::read_to_string("/proc/self/status") {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<f32>().ok())
        })
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn read_cpu_percent() -> f32 {
    let stat = match std::fs::read_to_string("/proc/self/stat") {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let parts: Vec<&str> = stat.split_whitespace().collect();

    if parts.len() < 22 {
        return 0.0;
    }

    let utime = match parts[13].parse::<f64>() {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let stime = match parts[14].parse::<f64>() {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let starttime = match parts[21].parse::<f64>() {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };

    if clk_tck <= 0 {
        return 0.0;
    }

    let clk_tck = clk_tck as f64;

    let uptime = match std::fs::read_to_string("/proc/uptime") {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let system_uptime = match uptime.split_whitespace().next() {
        Some(value) => match value.parse::<f64>() {
            Ok(value) => value,
            Err(_) => return 0.0,
        },

        None => return 0.0,
    };

    let process_uptime = system_uptime - (starttime / clk_tck);

    if process_uptime <= 0.0 {
        return 0.0;
    }

    let cpu_time = (utime + stime) / clk_tck;

    ((cpu_time / process_uptime) * 100.0).clamp(0.0, 100.0) as f32
}

/// Start the IPC server.
///
/// The listener is non-blocking and uses `poll()` with a short timeout.
/// Shutdown is controlled by `shutdown`.
///
/// The function only returns after:
///
/// 1. the listener has stopped;
/// 2. the listener has been dropped;
/// 3. the socket has been cleaned up.
pub fn start_ipc_server(config: Arc<RwLock<Config>>, tx: UnixDatagram, shutdown: Arc<AtomicBool>) {
    let socket_path = Path::new(SOCKET_PATH);

    if !prepare_socket_path(socket_path) {
        return;
    }

    let listener = match UnixListener::bind(socket_path) {
        Ok(listener) => listener,

        Err(error) => {
            tracing::error!(
                path = SOCKET_PATH,
                error = %error,
                "Failed to bind IPC socket"
            );

            cleanup_socket(socket_path);
            return;
        }
    };

    if let Err(error) = listener.set_nonblocking(true) {
        tracing::error!(
            error = %error,
            "Failed to configure IPC listener"
        );

        drop(listener);
        cleanup_socket(socket_path);
        return;
    }

    set_socket_permissions(socket_path);

    tracing::info!(path = SOCKET_PATH, "IPC server ready");

    let listener_fd = {
        use std::os::unix::io::AsRawFd;

        listener.as_raw_fd()
    };

    while !shutdown.load(Ordering::Acquire) {
        let mut poll_fd = pollfd {
            fd: listener_fd,
            events: POLLIN,
            revents: 0,
        };

        let result = unsafe { poll(&mut poll_fd, 1, IPC_POLL_TIMEOUT_MS) };

        if result < 0 {
            let error = std::io::Error::last_os_error();

            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }

            tracing::error!(
                error = %error,
                "IPC poll failed"
            );

            break;
        }

        if result == 0 {
            continue;
        }

        if poll_fd.revents & POLLIN == 0 {
            continue;
        }

        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    handle_client(&mut stream, &config, &tx, &shutdown);

                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }

                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    break;
                }

                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "IPC accept failed"
                    );

                    break;
                }
            }
        }
    }

    drop(listener);

    cleanup_socket(socket_path);

    tracing::info!("IPC server stopped");
}

fn prepare_socket_path(path: &Path) -> bool {
    if path.exists() {
        match std::fs::remove_file(path) {
            Ok(()) => {
                tracing::debug!(
                    path = %path.display(),
                    "Removed stale IPC socket"
                );
            }

            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    error = %error,
                    "Failed to remove stale IPC socket"
                );

                return false;
            }
        }
    }

    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::error!(
                path = %parent.display(),
                error = %error,
                "Failed to create IPC directory"
            );

            return false;
        }
    }

    true
}

fn set_socket_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        match std::fs::metadata(path) {
            Ok(metadata) => {
                let mut permissions = metadata.permissions();

                permissions.set_mode(0o666);

                if let Err(error) = std::fs::set_permissions(path, permissions) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "Failed to set IPC permissions"
                    );
                }
            }

            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Failed to read IPC socket metadata"
                );
            }
        }
    }
}

fn cleanup_socket(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}

        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}

        Err(error) => {
            tracing::debug!(
                path = %path.display(),
                error = %error,
                "Failed to remove IPC socket"
            );
        }
    }
}

fn handle_client(
    stream: &mut UnixStream,
    config: &Arc<RwLock<Config>>,
    tx: &UnixDatagram,
    shutdown: &Arc<AtomicBool>,
) {
    let _ = stream.set_read_timeout(Some(IPC_READ_TIMEOUT));

    let _ = stream.set_write_timeout(Some(IPC_WRITE_TIMEOUT));

    let mut buffer = [0u8; 1024];

    let size = match stream.read(&mut buffer) {
        Ok(0) => return,

        Ok(size) => size,

        Err(error) => {
            tracing::debug!(
                error = %error,
                "IPC client read failed"
            );

            return;
        }
    };

    let command = match DaemonCommand::from_bytes(&buffer[..size]) {
        Some(command) => command,

        None => {
            let _ = stream.write_all(b"Error: Unknown command");

            return;
        }
    };

    tracing::debug!(
        command = ?command,
        "IPC command received"
    );

    match command {
        DaemonCommand::BypassOn => match control::enter_bypass_mode() {
            Ok(()) => {
                let _ = tx.send(&[3]);

                let _ = stream.write_all(b"OK: Bypass ON");
            }

            Err(error) => {
                tracing::error!(
                    error = %error,
                    "Failed enabling bypass"
                );

                write_error(stream, &format!("Error: {error}"));
            }
        },

        DaemonCommand::BypassOff => match control::exit_bypass_mode() {
            Ok(()) => {
                let _ = tx.send(&[4]);

                let _ = stream.write_all(b"OK: Bypass OFF");
            }

            Err(error) => {
                tracing::error!(
                    error = %error,
                    "Failed disabling bypass"
                );

                write_error(stream, &format!("Error: {error}"));
            }
        },

        DaemonCommand::Reload => {
            handle_reload(stream, config, tx);
        }

        DaemonCommand::Status => {
            handle_status(stream, config);
        }

        DaemonCommand::Shutdown => {
            /*
             * ACK FIRST.
             *
             * charger-ctl waits for this response.
             */
            if let Err(error) = stream.write_all(b"OK: Shutting down") {
                tracing::warn!(
                    error = %error,
                    "Failed sending shutdown acknowledgement"
                );
            }

            /*
             * Tell monitor to exit gracefully.
             */
            if let Err(error) = tx.send(&[2]) {
                tracing::error!(
                    error = %error,
                    "Failed notifying monitor about shutdown"
                );
            }

            /*
             * Stop accepting new IPC clients.
             */
            shutdown.store(true, Ordering::Release);

            tracing::info!("Graceful shutdown requested through IPC");
        }
    }
}

fn write_error(stream: &mut UnixStream, message: &str) {
    let _ = stream.write_all(message.as_bytes());
}

fn handle_reload(stream: &mut UnixStream, config: &Arc<RwLock<Config>>, tx: &UnixDatagram) {
    let config_path = std::path::PathBuf::from(charger_core::config::schema::DEFAULT_CONFIG_PATH);

    let new_config = match Config::load(&config_path) {
        Ok(config) => config,

        Err(error) => {
            tracing::error!(
                error = %error,
                "Failed to reload configuration"
            );

            write_error(stream, &format!("Error loading config: {error}"));

            return;
        }
    };

    match config.write() {
        Ok(mut current) => {
            *current = new_config;

            let _ = tx.send(&[1]);

            let _ = stream.write_all(b"OK: Config reloaded");

            tracing::info!("Configuration reloaded successfully");
        }

        Err(_) => {
            write_error(stream, "Error: Failed to lock config");
        }
    }
}

fn handle_status(stream: &mut UnixStream, config: &Arc<RwLock<Config>>) {
    let (pid, rss, cpu) = get_process_stats();

    let config_guard = match config.read() {
        Ok(config) => config,

        Err(_) => {
            write_error(stream, "Error: Failed to lock config");

            return;
        }
    };

    let hardware = control::get_actual_charging_state();

    let battery = reader::read_capacity()
        .ok()
        .map(|value| format!("{value}%"))
        .unwrap_or_else(|| "N/A".to_string());

    let temperature = reader::read_temperature_dc()
        .ok()
        .map(|value| format!("{:.1} C", value as f32 / 10.0))
        .unwrap_or_else(|| "N/A".to_string());

    let power_state = reader::get_power_state()
        .ok()
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "Unknown".to_string());

    let message = format!(
        "OK:\n\
         [ DAEMON STATUS ]\n\
         • Status       : {}\n\
         • PID          : {}\n\
         • Memory (RSS) : {:.2} MB\n\
         • CPU Average  : {:.3}%\n\
         • Hardware     : {:?}\n\
         \n\
         [ BATTERY ]\n\
         • Level        : {}\n\
         • Temperature  : {}\n\
         • Power State  : {}\n\
         \n\
         [ CONFIG ]\n\
         • Enabled      : {}\n\
         • Charge Limit : {}%\n\
         • Resume Limit : {}%\n\
         • Thermal Cut  : {}\n\
         • Max Temp     : {:.1} C",
        if config_guard.enabled {
            "Active"
        } else {
            "Standby"
        },
        pid,
        rss,
        cpu,
        hardware,
        battery,
        temperature,
        power_state,
        config_guard.enabled,
        config_guard.charge_limit,
        config_guard.resume_limit,
        if config_guard.thermal_cutoff {
            "ON"
        } else {
            "OFF"
        },
        config_guard.max_temp_dc as f32 / 10.0,
    );

    let _ = stream.write_all(message.as_bytes());
}
