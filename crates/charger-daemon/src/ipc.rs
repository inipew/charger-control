use std::{path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::RwLock,
};
use charger_core::config::schema::Config;

fn get_process_stats() -> (u32, f32, f32) {
    let pid = std::process::id();
    
    // Memory RSS (dari /proc/self/status)
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

    // CPU Usage kasaran (dari /proc/self/stat & /proc/uptime)
    let mut cpu_percent = 0.0;
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        let parts: Vec<&str> = stat.split_whitespace().collect();
        if parts.len() >= 22 {
            if let (Ok(utime), Ok(stime), Ok(starttime)) = (
                parts[13].parse::<f32>(), 
                parts[14].parse::<f32>(),
                parts[21].parse::<f32>()
            ) {
                // Rata-rata sistem Linux Android menggunakan 100 ticks per second
                let clk_tck = 100.0; 
                let total_time_sec = (utime + stime) / clk_tck;
                
                if let Ok(uptime_str) = std::fs::read_to_string("/proc/uptime") {
                    let sys_uptime: f32 = uptime_str.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0.0);
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

pub async fn start_ipc_server(config: Arc<RwLock<Config>>, mut shutdown_rx: tokio::sync::mpsc::Receiver<()>) {
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

    // Ensure everyone can write to the socket so charger-ctl works
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

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!("IPC server shutting down");
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _addr)) => {
                        let config = Arc::clone(&config);
                        tokio::spawn(async move {
                            handle_client(stream, config).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to accept socket connection: {e}");
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);
}

async fn handle_client(mut stream: UnixStream, config: Arc<RwLock<Config>>) {
    let mut buf = [0u8; 1024];
    match stream.read(&mut buf).await {
        Ok(n) if n > 0 => {
            if let Some(cmd) = DaemonCommand::from_bytes(&buf[..n]) {
                tracing::info!("Received IPC command: {:?}", cmd);
                match cmd {
                    DaemonCommand::BypassOn => {
                        if let Err(e) = charger_core::battery::control::enter_bypass_mode() {
                            let _ = stream.write_all(format!("Error: {e}").as_bytes()).await;
                        } else {
                            let _ = stream.write_all(b"OK: Bypass ON").await;
                        }
                    }
                    DaemonCommand::BypassOff => {
                        if let Err(e) = charger_core::battery::control::exit_bypass_mode() {
                            let _ = stream.write_all(format!("Error: {e}").as_bytes()).await;
                        } else {
                            let _ = stream.write_all(b"OK: Bypass OFF").await;
                        }
                    }
                    DaemonCommand::Reload => {
                        let cfg_path = Path::new(charger_core::config::schema::DEFAULT_CONFIG_PATH).to_path_buf();
                        
                        match Config::load(&cfg_path) {
                            Ok(new_cfg) => {
                                *config.write().await = new_cfg;
                                let _ = stream.write_all(b"OK: Config reloaded").await;
                            }
                            Err(e) => {
                                let _ = stream.write_all(format!("Error loading config: {e}").as_bytes()).await;
                            }
                        }
                    }
                    DaemonCommand::Status => {
                        let cfg = config.read().await;
                        let (pid, rss, cpu) = get_process_stats();
                        let msg = format!(
                            "OK:\n\
                             [ DAEMON STATUS ]\n\
                             • Status       : {}\n\
                             • PID          : {}\n\
                             • Memory (RSS) : {:.2} MB\n\
                             • CPU Usage    : {:.3}%\n\
                             \n\
                             [ CURRENT CONFIG ]\n\
                             • Charge Limit : {}%\n\
                             • Thermal Cut  : {}\n\
                             • Power Save   : {}",
                             if cfg.enabled { "Active (Monitoring)" } else { "Standby (Disabled)" },
                             pid,
                             rss,
                             cpu,
                             cfg.charge_limit,
                             if cfg.thermal_cutoff { "ON" } else { "OFF" },
                             if cfg.cpu_power_save { "ON" } else { "OFF" }
                        );
                        let _ = stream.write_all(msg.as_bytes()).await;
                    }
                    DaemonCommand::Shutdown => {
                        let _ = stream.write_all(b"OK: Shutting down").await;
                        std::process::exit(0);
                    }
                }
            } else {
                let _ = stream.write_all(b"Error: Unknown command").await;
            }
        }
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to read from socket: {e}"),
    }
}
