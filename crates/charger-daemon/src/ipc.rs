use charger_core::config::schema::Config;
use std::{
    io::{Read, Write},
    os::unix::net::{UnixDatagram, UnixListener},
    path::Path,
    sync::{Arc, RwLock},
};

pub const SOCKET_PATH: &str = "/data/adb/charger-control/daemon.sock";

#[derive(Debug, PartialEq, Eq)]
pub enum DaemonCommand {
    BypassOn,
    BypassOff,
    Reload,
    Status,
    Shutdown,
}

impl DaemonCommand {
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        match std::str::from_utf8(b).unwrap_or("").trim() {
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
    let mut rss_mb = 0.0;
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<f32>() {
                        rss_mb = kb / 1024.0;
                    }
                }
                break;
            }
        }
    }

    let mut cpu_percent = 0.0;
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        let parts: Vec<&str> = stat.split_whitespace().collect();
        if parts.len() >= 22 {
            if let (Ok(utime), Ok(stime), Ok(starttime)) = (
                parts[13].parse::<f32>(),
                parts[14].parse::<f32>(),
                parts[21].parse::<f32>(),
            ) {
                let clk_tck = 100.0;
                let total_time_sec = (utime + stime) / clk_tck;

                if let Ok(uptime_str) = std::fs::read_to_string("/proc/uptime") {
                    let sys_uptime: f32 = uptime_str
                        .split_whitespace()
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0.0);
                    let process_uptime = sys_uptime - (starttime / clk_tck);
                    if process_uptime > 0.0 {
                        cpu_percent = (total_time_sec / process_uptime) * 100.0;
                    }
                }
            }
        }
    }

    (pid, rss_mb, cpu_percent)
}

pub fn start_ipc_server(config: Arc<RwLock<Config>>, tx: UnixDatagram) {
    let socket_path = Path::new(SOCKET_PATH);
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }

    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind to socket: {e}");
            return;
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(socket_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o666);
            let _ = std::fs::set_permissions(socket_path, perms);
        }
    }

    tracing::info!("IPC server listening on {:?}", socket_path);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                handle_client(&mut stream, &config, &tx);
            }
            Err(e) => {
                tracing::error!("Failed to accept socket connection: {e}");
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);
}

fn handle_client(
    stream: &mut std::os::unix::net::UnixStream,
    config: &Arc<RwLock<Config>>,
    tx: &UnixDatagram,
) {
    // BUG FIX 1: Set read timeout to prevent infinite blocking on bad clients
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));

    let mut buf = [0u8; 1024];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => {
            if let Some(cmd) = DaemonCommand::from_bytes(&buf[..n]) {
                tracing::info!("Received IPC command: {:?}", cmd);
                match cmd {
                    DaemonCommand::BypassOn => {
                        match charger_core::battery::control::enter_bypass_mode() {
                            Ok(res) if res.all_succeeded() => {
                                let _ = stream.write_all(b"OK: Bypass ON");
                            }
                            Ok(res) => {
                                let msg = format!("Error: Partial failure ({} succeeded, {} failed)", res.succeeded, res.failed);
                                let _ = stream.write_all(msg.as_bytes());
                            }
                            Err(e) => {
                                let _ = stream.write_all(format!("Error: {e}").as_bytes());
                            }
                        }
                    }
                    DaemonCommand::BypassOff => {
                        match charger_core::battery::control::exit_bypass_mode() {
                            Ok(res) if res.all_succeeded() => {
                                let _ = stream.write_all(b"OK: Bypass OFF");
                            }
                            Ok(res) => {
                                let msg = format!("Error: Partial failure ({} succeeded, {} failed)", res.succeeded, res.failed);
                                let _ = stream.write_all(msg.as_bytes());
                            }
                            Err(e) => {
                                let _ = stream.write_all(format!("Error: {e}").as_bytes());
                            }
                        }
                    }
                    DaemonCommand::Reload => {
                        let cfg_path = Path::new(charger_core::config::schema::DEFAULT_CONFIG_PATH)
                            .to_path_buf();
                        match Config::load(&cfg_path) {
                            Ok(new_cfg) => {
                                if let Ok(mut c) = config.write() {
                                    *c = new_cfg;
                                }
                                let _ = tx.send(&[1]); // 1 = Reload
                                let _ = stream.write_all(b"OK: Config reloaded");
                            }
                            Err(e) => {
                                let _ = stream
                                    .write_all(format!("Error loading config: {e}").as_bytes());
                            }
                        }
                    }
                    DaemonCommand::Status => {
                        let (pid, rss, cpu) = get_process_stats();
                        let msg = if let Ok(cfg) = config.read() {
                            format!(
                                "OK:\n\
                                 [ DAEMON STATUS ]\n\
                                 • Status       : {}\n\
                                 • PID          : {}\n\
                                 • Memory (RSS) : {:.2} MB\n\
                                 • CPU Usage    : {:.3}%\n\
                                 \n\
                                 [ CURRENT CONFIG ]\n\
                                 • Charge Limit : {}%\n\
                                 • Resume Limit : {}%\n\
                                 • Thermal Cut  : {}",
                                if cfg.enabled {
                                    "Active (Monitoring)"
                                } else {
                                    "Standby (Disabled)"
                                },
                                pid,
                                rss,
                                cpu,
                                cfg.charge_limit,
                                cfg.resume_limit,
                                if cfg.thermal_cutoff { "ON" } else { "OFF" }
                            )
                        } else {
                            "Error: Failed to lock config".to_string()
                        };
                        let _ = stream.write_all(msg.as_bytes());
                    }
                    DaemonCommand::Shutdown => {
                        let _ = stream.write_all(b"OK: Shutting down");
                        let _ = tx.send(&[2]); // 2 = Shutdown
                                               // BUG FIX 3: Remove socket file before forceful exit
                        let _ = std::fs::remove_file(SOCKET_PATH);
                        // Exit early via process exit to force immediate shutdown across all threads
                        std::process::exit(0);
                    }
                }
            } else {
                let _ = stream.write_all(b"Error: Unknown command");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to read from socket: {e}"),
    }
}
