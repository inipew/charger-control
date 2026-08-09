use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};

use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};

use charger_core::battery::{control, reader};
use charger_core::config::schema::Config;

pub const SOCKET_PATH: &str = "/data/adb/charger-control/daemon.sock";

const IPC_READ_TIMEOUT: Duration = Duration::from_millis(750);

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
    let pid = std::process::id();

    let rss_mb = read_rss_mb();
    let cpu_percent = read_cpu_percent();

    (pid, rss_mb, cpu_percent)
}

fn read_rss_mb() -> f32 {
    let status = match std::fs::read_to_string("/proc/self/status") {
        Ok(value) => value,
        Err(_) => return 0.0,
    };

    for line in status.lines() {
        if !line.starts_with("VmRSS:") {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();

        if parts.len() < 2 {
            return 0.0;
        }

        if let Ok(kb) = parts[1].parse::<f32>() {
            return kb / 1024.0;
        }
    }

    0.0
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
        Ok(v) => v,
        Err(_) => return 0.0,
    };

    let stime = match parts[14].parse::<f64>() {
        Ok(v) => v,
        Err(_) => return 0.0,
    };

    let starttime = match parts[21].parse::<f64>() {
        Ok(v) => v,
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
            Ok(v) => v,
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

pub fn start_ipc_server(config: Arc<RwLock<Config>>, tx: UnixDatagram) {
    let socket_path = Path::new(SOCKET_PATH);

    if socket_path.exists() {
        match std::fs::remove_file(socket_path) {
            Ok(_) => {}

            Err(e) => {
                tracing::warn!("Failed to remove stale socket {:?}: {}", socket_path, e);
            }
        }
    }

    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::error!("Failed creating IPC directory: {}", e);
            return;
        }
    }

    let listener = match UnixListener::bind(socket_path) {
        Ok(listener) => listener,

        Err(e) => {
            tracing::error!("Failed to bind IPC socket {:?}: {}", socket_path, e);
            return;
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = std::fs::metadata(socket_path) {
            let mut permissions = metadata.permissions();

            // Keep compatibility with charger-ctl.
            permissions.set_mode(0o666);

            if let Err(e) = std::fs::set_permissions(socket_path, permissions) {
                tracing::warn!("Failed setting socket permissions: {}", e);
            }
        }
    }

    tracing::info!("IPC server listening on {:?}", socket_path);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                handle_client(&mut stream, &config, &tx);
            }

            Err(e) => {
                tracing::error!("IPC accept failed: {}", e);
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);

    tracing::info!("IPC server stopped");
}

fn handle_client(stream: &mut UnixStream, config: &Arc<RwLock<Config>>, tx: &UnixDatagram) {
    let _ = stream.set_read_timeout(Some(IPC_READ_TIMEOUT));

    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let mut buf = [0u8; 1024];

    let received = match stream.read(&mut buf) {
        Ok(0) => return,

        Ok(n) => n,

        Err(e) => {
            tracing::warn!("Failed reading IPC client: {}", e);
            return;
        }
    };

    let command = match DaemonCommand::from_bytes(&buf[..received]) {
        Some(command) => command,

        None => {
            let _ = stream.write_all(b"Error: Unknown command");

            return;
        }
    };

    tracing::debug!("IPC command received: {:?}", command);

    match command {
        DaemonCommand::BypassOn => {
            /*
             * Apply hardware immediately so the CLI receives
             * a truthful response. Monitor is then notified and
             * will reconcile the logical state.
             */
            match control::enter_bypass_mode() {
                Ok(()) => {
                    let _ = tx.send(&[3]);

                    let _ = stream.write_all(b"OK: Bypass ON");
                }

                Err(e) => {
                    tracing::error!("Failed enabling bypass: {}", e);

                    let _ = stream.write_all(format!("Error: {e}").as_bytes());
                }
            }
        }

        DaemonCommand::BypassOff => match control::exit_bypass_mode() {
            Ok(()) => {
                let _ = tx.send(&[4]);

                let _ = stream.write_all(b"OK: Bypass OFF");
            }

            Err(e) => {
                tracing::error!("Failed disabling bypass: {}", e);

                let _ = stream.write_all(format!("Error: {e}").as_bytes());
            }
        },

        DaemonCommand::Reload => {
            let config_path =
                std::path::PathBuf::from(charger_core::config::schema::DEFAULT_CONFIG_PATH);

            match Config::load(&config_path) {
                Ok(new_cfg) => match config.write() {
                    Ok(mut c) => {
                        *c = new_cfg;

                        let _ = tx.send(&[1]);

                        let _ = stream.write_all(b"OK: Config reloaded");

                        tracing::info!("Configuration reloaded successfully");
                    }

                    Err(_) => {
                        let _ = stream.write_all(b"Error: Failed to lock config");

                        tracing::error!("Failed to acquire config write lock during reload");
                    }
                },

                Err(e) => {
                    tracing::error!("Failed to reload configuration: {}", e);

                    let _ = stream.write_all(format!("Error loading config: {e}").as_bytes());
                }
            }
        }

        DaemonCommand::Status => {
            let (pid, rss, cpu) = get_process_stats();

            let config_guard = match config.read() {
                Ok(cfg) => cfg,

                Err(_) => {
                    let _ = stream.write_all(b"Error: Failed to lock config");

                    return;
                }
            };

            let hardware = control::get_actual_charging_state();

            let battery = reader::read_capacity()
                .ok()
                .map(|v| format!("{}%", v))
                .unwrap_or_else(|| "N/A".to_string());

            let temperature = reader::read_temperature_dc()
                .ok()
                .map(|v| format!("{:.1} C", v as f32 / 10.0))
                .unwrap_or_else(|| "N/A".to_string());

            let power_state = reader::get_power_state()
                .ok()
                .map(|v| format!("{:?}", v))
                .unwrap_or_else(|| "Unknown".to_string());

            let msg = format!(
                "OK:\n\
                 [ DAEMON STATUS ]\n\
                 • Status       : {}\n\
                 • PID          : {}\n\
                 • Memory (RSS) : {:.2} MB\n\
                 • CPU Average   : {:.3}%\n\
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

            let _ = stream.write_all(msg.as_bytes());
        }

        DaemonCommand::Shutdown => {
            let _ = stream.write_all(b"OK: Shutting down");

            let _ = tx.send(&[2]);
        }
    }
}
