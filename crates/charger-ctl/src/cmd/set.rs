use std::path::Path;
use charger_core::error::ChargerError;
use charger_core::config::schema::{Config, DEFAULT_CONFIG_PATH};
use crate::display;

pub fn limit(value: u8) -> Result<(), ChargerError> {
    if value < 50 || value > 100 {
        display::error("Limit must be between 50 and 100");
        return Ok(());
    }

    let path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();
    let mut cfg = Config::load(&path).unwrap_or_default();
    cfg.charge_limit = value;
    cfg.save(&path)?;

    display::success(&format!("Charge limit set to {}%", value));
    notify_daemon();
    Ok(())
}

pub fn resume(value: u8) -> Result<(), ChargerError> {
    if value < 40 || value > 99 {
        display::error("Resume limit must be between 40 and 99%");
        return Ok(());
    }

    let path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();
    let mut cfg = Config::load(&path).unwrap_or_default();
    if value >= cfg.charge_limit {
        display::error(&format!("Resume limit ({}%) must be less than charge limit ({}%)", value, cfg.charge_limit));
        return Ok(());
    }
    cfg.resume_limit = value;
    cfg.save(&path)?;

    display::success(&format!("Resume limit set to {}%", value));
    notify_daemon();
    Ok(())
}

pub fn thermal(enabled: bool) -> Result<(), ChargerError> {
    let path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();
    let mut cfg = Config::load(&path).unwrap_or_default();
    cfg.thermal_cutoff = enabled;
    cfg.save(&path)?;

    display::success(&format!("Thermal cutoff {}", if enabled { "enabled" } else { "disabled" }));
    notify_daemon();
    Ok(())
}

pub fn max_temp(value: i32) -> Result<(), ChargerError> {
    if value < 30 || value > 60 {
        display::error("Max temp must be between 30 and 60 °C");
        return Ok(());
    }

    let path = Path::new(DEFAULT_CONFIG_PATH).to_path_buf();
    let mut cfg = Config::load(&path).unwrap_or_default();
    cfg.max_temp_dc = value * 10;
    cfg.save(&path)?;

    display::success(&format!("Max temperature set to {} °C", value));
    notify_daemon();
    Ok(())
}

fn notify_daemon() {
    // Send reload command to daemon via socket
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        use std::io::Write;
        
        if let Ok(mut stream) = UnixStream::connect(charger_core::config::schema::DEFAULT_CONFIG_PATH.replace("config.toml", "daemon.sock")) {
            let _ = stream.write_all(b"reload");
            display::info("Daemon configuration reloaded");
        } else {
            display::warn("Failed to contact daemon. Is it running?");
        }
    }
    
    #[cfg(not(unix))]
    display::warn("IPC not supported on this platform. Please restart daemon manually.");
}
