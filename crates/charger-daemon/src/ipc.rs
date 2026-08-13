#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use charger_core::config::schema::Config;
#[cfg(unix)]
use libc::{poll, pollfd, POLLIN};

pub const SOCKET_PATH: &str = charger_core::config::schema::DEFAULT_SOCKET_PATH;

const IPC_READ_TIMEOUT: Duration = Duration::from_millis(750);

const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

const IPC_POLL_TIMEOUT_MS: i32 = -1;

#[derive(Debug)]
pub struct DaemonDiagnostics {
    pub netlink_available: AtomicBool,
    pub is_idle: AtomicBool,
    pub poll_interval_ms: AtomicU64,
    pub error_backoff_ms: AtomicU64,
    pub battery_level_percent: AtomicU8,
    pub battery_temperature_dc: AtomicI32,
    pub power_state: RwLock<String>,
    pub hardware_state: RwLock<String>,
}

impl DaemonDiagnostics {
    pub fn new() -> Self {
        Self {
            netlink_available: AtomicBool::new(false),
            is_idle: AtomicBool::new(false),
            poll_interval_ms: AtomicU64::new(0),
            error_backoff_ms: AtomicU64::new(0),
            battery_level_percent: AtomicU8::new(255),
            battery_temperature_dc: AtomicI32::new(i32::MIN),
            power_state: RwLock::new("Unknown".to_string()),
            hardware_state: RwLock::new("Unknown".to_string()),
        }
    }
}

impl Default for DaemonDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonCommand {
    BypassOn,
    BypassOff,
    DisableOn,
    DisableOff,
    Reload,
    Status,
    StatusJson,
    Shutdown,
}

impl DaemonCommand {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let command = std::str::from_utf8(bytes).ok()?.trim();

        match command {
            "bypass on" => Some(Self::BypassOn),
            "bypass off" => Some(Self::BypassOff),
            "disable on" | "disable" => Some(Self::DisableOn),
            "disable off" | "enable" => Some(Self::DisableOff),
            "reload" => Some(Self::Reload),
            "status" => Some(Self::Status),
            "status json" | "status_json" => Some(Self::StatusJson),
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

    let after_comm = match stat.rfind(')') {
        Some(idx) => &stat[idx + 1..],
        None => return 0.0,
    };

    let parts: Vec<&str> = after_comm.split_whitespace().collect();

    if parts.len() < 20 {
        return 0.0;
    }

    let utime = match parts[11].parse::<f64>() {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let stime = match parts[12].parse::<f64>() {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    let starttime = match parts[19].parse::<f64>() {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    #[cfg(unix)]
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    #[cfg(not(unix))]
    let clk_tck = 100;

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
#[cfg(unix)]
pub fn start_ipc_server(
    config: Arc<RwLock<Config>>,
    tx: UnixDatagram,
    shutdown: Arc<AtomicBool>,
    diagnostics: Arc<DaemonDiagnostics>,
) {
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
                    handle_client(&mut stream, &config, &tx, &shutdown, &diagnostics);

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

#[cfg(not(unix))]
pub fn start_ipc_server(
    _config: Arc<RwLock<Config>>,
    _shutdown: Arc<AtomicBool>,
    _diagnostics: Arc<DaemonDiagnostics>,
) {
}

fn prepare_socket_path(path: &Path) -> bool {
    if path.exists() {
        match std::fs::remove_file(path) {
            Ok(()) => {}

            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    error = %error,
                    "Failed removing stale IPC socket"
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
                "Failed creating IPC socket directory"
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

        let permissions = std::fs::Permissions::from_mode(0o660);

        if let Err(error) = std::fs::set_permissions(path, permissions) {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed setting IPC socket permissions"
            );
        }
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn cleanup_socket(path: &Path) {
    if path.exists() {
        if let Err(error) = std::fs::remove_file(path) {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed cleaning up IPC socket"
            );
        }
    }
}

#[cfg(unix)]
fn handle_client(
    stream: &mut UnixStream,
    config: &Arc<RwLock<Config>>,
    tx: &UnixDatagram,
    shutdown: &Arc<AtomicBool>,
    diagnostics: &Arc<DaemonDiagnostics>,
) {
    let _ = stream.set_read_timeout(Some(IPC_READ_TIMEOUT));

    let _ = stream.set_write_timeout(Some(IPC_WRITE_TIMEOUT));

    let mut buffer = [0u8; 128];

    let size = match stream.read(&mut buffer) {
        Ok(size) if size > 0 => size,

        Ok(_) => return,

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
        DaemonCommand::BypassOn => {
            let _ = tx.send(&[3]);
            let _ = stream.write_all(b"OK: Bypass ON");
        }

        DaemonCommand::BypassOff => {
            let _ = tx.send(&[4]);
            let _ = stream.write_all(b"OK: Bypass OFF");
        }

        DaemonCommand::DisableOn => {
            let _ = tx.send(&[5]);
            let _ = stream.write_all(b"OK: Daemon Disabled");
        }

        DaemonCommand::DisableOff => {
            let _ = tx.send(&[6]);
            let _ = stream.write_all(b"OK: Daemon Enabled");
        }

        DaemonCommand::Reload => {
            handle_reload(stream, config, tx);
        }

        DaemonCommand::Status => {
            handle_status(stream, config, diagnostics);
        }

        DaemonCommand::StatusJson => {
            handle_status_json(stream, config, diagnostics);
        }

        DaemonCommand::Shutdown => {
            if let Err(error) = stream.write_all(b"OK: Shutting down") {
                tracing::warn!(
                    error = %error,
                    "Failed sending shutdown acknowledgement"
                );
            }

            if let Err(error) = tx.send(&[2]) {
                tracing::error!(
                    error = %error,
                    "Failed notifying monitor about shutdown"
                );
            }

            shutdown.store(true, Ordering::Release);

            tracing::info!("Graceful shutdown requested through IPC");
        }
    }
}

#[cfg(unix)]
fn write_error(stream: &mut UnixStream, message: &str) {
    let _ = stream.write_all(message.as_bytes());
}

#[cfg(unix)]
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

#[cfg(unix)]
fn handle_status(
    stream: &mut UnixStream,
    config: &Arc<RwLock<Config>>,
    diagnostics: &Arc<DaemonDiagnostics>,
) {
    let (pid, rss, cpu) = get_process_stats();

    let config_guard = match config.read() {
        Ok(config) => config,

        Err(_) => {
            write_error(stream, "Error: Failed to lock config");

            return;
        }
    };

    let hw_guard = diagnostics
        .hardware_state
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let hardware = hw_guard.clone();
    drop(hw_guard);

    let level_val = diagnostics.battery_level_percent.load(Ordering::Relaxed);
    let battery = if level_val == 255 {
        "N/A".to_string()
    } else {
        format!("{level_val}%")
    };

    let temp_val = diagnostics.battery_temperature_dc.load(Ordering::Relaxed);
    let temperature = if temp_val == i32::MIN {
        "N/A".to_string()
    } else {
        format!("{:.1} C", temp_val as f32 / 10.0)
    };

    let ps_guard = diagnostics
        .power_state
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let power_state = ps_guard.clone();
    drop(ps_guard);

    let netlink_available = diagnostics.netlink_available.load(Ordering::Relaxed);
    let is_idle = diagnostics.is_idle.load(Ordering::Relaxed);
    let interval_ms = diagnostics.poll_interval_ms.load(Ordering::Relaxed);
    let backoff_ms = diagnostics.error_backoff_ms.load(Ordering::Relaxed);

    let mode_str = if !config_guard.enabled {
        "Disabled (Standby)"
    } else if is_idle {
        "Ultra-Low-Power Idle"
    } else {
        "Active Monitoring"
    };

    let netlink_str = if netlink_available {
        "Enabled"
    } else {
        "Unavailable (Fallback poll)"
    };

    let interval_str = if is_idle && netlink_available {
        "Infinite (-1 / kernel sleep)".to_string()
    } else if interval_ms == u64::MAX {
        "Infinite (-1)".to_string()
    } else {
        format!("{:.1}s", interval_ms as f32 / 1000.0)
    };

    let backoff_str = format!("{:.1}s", backoff_ms as f32 / 1000.0);

    let message = format!(
        "OK:\n\
         [ DAEMON STATUS ]\n\
         • Status       : {}\n\
         • PID          : {}\n\
         • Memory (RSS) : {:.2} MB\n\
         • CPU Average  : {:.3}%\n\
         • Hardware     : {:?}\n\
         \n\
         [ MONITOR DIAGNOSTICS ]\n\
         • Mode         : {}\n\
         • Netlink      : {}\n\
         • Poll Interval: {}\n\
         • Error Backoff: {}\n\
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
        mode_str,
        netlink_str,
        interval_str,
        backoff_str,
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

#[derive(serde::Serialize)]
pub struct DaemonStatusResponse {
    pub pid: u32,
    pub memory_rss_mb: f32,
    pub cpu_percent: f32,
    pub enabled: bool,
    pub mode: String,
    pub netlink_available: bool,
    pub poll_interval_ms: u64,
    pub error_backoff_ms: u64,
    pub battery_level_percent: Option<u8>,
    pub battery_temperature_c: Option<f32>,
    pub power_state: String,
    pub charge_limit: u8,
    pub resume_limit: u8,
    pub thermal_cutoff: bool,
    pub max_temp_c: f32,
}

#[cfg(unix)]
fn handle_status_json(
    stream: &mut UnixStream,
    config: &Arc<RwLock<Config>>,
    diagnostics: &Arc<DaemonDiagnostics>,
) {
    let (pid, rss, cpu) = get_process_stats();

    let config_guard = match config.read() {
        Ok(config) => config,
        Err(_) => {
            write_error(stream, "Error: Failed to lock config");
            return;
        }
    };

    let level_val = diagnostics.battery_level_percent.load(Ordering::Relaxed);
    let battery = if level_val == 255 {
        None
    } else {
        Some(level_val)
    };

    let temp_val = diagnostics.battery_temperature_dc.load(Ordering::Relaxed);
    let temperature = if temp_val == i32::MIN {
        None
    } else {
        Some(temp_val as f32 / 10.0)
    };

    let ps_guard = diagnostics
        .power_state
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let power_state = ps_guard.clone();
    drop(ps_guard);

    let netlink_available = diagnostics.netlink_available.load(Ordering::Relaxed);
    let is_idle = diagnostics.is_idle.load(Ordering::Relaxed);
    let interval_ms = diagnostics.poll_interval_ms.load(Ordering::Relaxed);
    let backoff_ms = diagnostics.error_backoff_ms.load(Ordering::Relaxed);

    let mode_str = if !config_guard.enabled {
        "Disabled (Standby)"
    } else if is_idle {
        "Ultra-Low-Power Idle"
    } else {
        "Active Monitoring"
    };

    let status_data = DaemonStatusResponse {
        pid,
        memory_rss_mb: rss,
        cpu_percent: cpu,
        enabled: config_guard.enabled,
        mode: mode_str.to_string(),
        netlink_available,
        poll_interval_ms: interval_ms,
        error_backoff_ms: backoff_ms,
        battery_level_percent: battery,
        battery_temperature_c: temperature,
        power_state,
        charge_limit: config_guard.charge_limit,
        resume_limit: config_guard.resume_limit,
        thermal_cutoff: config_guard.thermal_cutoff,
        max_temp_c: config_guard.max_temp_dc as f32 / 10.0,
    };

    match serde_json::to_string_pretty(&status_data) {
        Ok(json) => {
            let response = format!("OK:\n{json}");
            let _ = stream.write_all(response.as_bytes());
        }
        Err(err) => {
            write_error(stream, &format!("Error serializing json: {err}"));
        }
    }
}
