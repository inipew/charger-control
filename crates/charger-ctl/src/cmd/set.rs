use std::path::Path;

use charger_core::config::schema::{Config, DEFAULT_CONFIG_PATH};
use charger_core::error::ChargerError;

use crate::display;

fn config_path() -> std::path::PathBuf {
    Path::new(DEFAULT_CONFIG_PATH).to_path_buf()
}

fn load_config() -> Result<Config, ChargerError> {
    let path = config_path();

    Config::load(&path).map_err(|e| {
        display::error(&format!("Failed to load config: {e}"));

        e
    })
}

fn save_config(cfg: &Config) -> Result<(), ChargerError> {
    let path = config_path();

    cfg.save(&path)
}

fn notify_daemon() {
    match crate::client::IpcClient::send_command(b"reload", std::time::Duration::from_secs(2)) {
        Ok(response) if response.starts_with("OK") => {
            display::info("Daemon configuration reloaded.");
        }
        Ok(_) => {
            display::warn("Config saved, but daemon returned an error.");
        }
        Err(_) => {
            display::warn("Config saved, but daemon is not running.");
        }
    }
}

pub fn limit(value: u8) -> Result<(), ChargerError> {
    if !(50..=100).contains(&value) {
        let msg = "Limit must be between 50 and 100%";
        display::error(msg);
        return Err(ChargerError::InvalidInput(msg.to_string()));
    }

    let mut cfg = load_config()?;

    /*
     * resume_limit must always be strictly below charge_limit.
     *
     * Example:
     *   old limit = 80
     *   resume = 75
     *
     * set limit 70
     * -> resume automatically becomes 65
     */
    if cfg.resume_limit >= value {
        let new_resume = value.saturating_sub(5).max(40);

        if new_resume >= value {
            let msg = "Unable to create a valid resume limit for this charge limit.";
            display::error(msg);
            return Err(ChargerError::InvalidInput(msg.to_string()));
        }

        cfg.resume_limit = new_resume;

        display::warn(&format!(
            "Resume limit automatically adjusted to {}% \
             because it must remain below charge limit.",
            new_resume
        ));
    }

    cfg.charge_limit = value;

    save_config(&cfg)?;

    display::success(&format!("Charge limit set to {}%", value));

    notify_daemon();

    Ok(())
}

pub fn resume(value: u8) -> Result<(), ChargerError> {
    if !(40..=99).contains(&value) {
        let msg = "Resume limit must be between 40 and 99%";
        display::error(msg);
        return Err(ChargerError::InvalidInput(msg.to_string()));
    }

    let mut cfg = load_config()?;

    if value >= cfg.charge_limit {
        let msg = format!(
            "Resume limit ({}%) must be less than charge limit ({}%).",
            value, cfg.charge_limit
        );
        display::error(&msg);

        return Err(ChargerError::InvalidInput(msg));
    }

    cfg.resume_limit = value;

    save_config(&cfg)?;

    display::success(&format!("Resume limit set to {}%", value));

    notify_daemon();

    Ok(())
}

pub fn thermal(enabled: bool) -> Result<(), ChargerError> {
    let mut cfg = load_config()?;

    cfg.thermal_cutoff = enabled;

    save_config(&cfg)?;

    display::success(&format!(
        "Thermal cutoff {}",
        if enabled { "enabled" } else { "disabled" }
    ));

    notify_daemon();

    Ok(())
}

pub fn max_temp(value: i32) -> Result<(), ChargerError> {
    if !(30..=60).contains(&value) {
        let msg = "Max temp must be between 30 and 60 °C";
        display::error(msg);
        return Err(ChargerError::InvalidInput(msg.to_string()));
    }

    let mut cfg = load_config()?;

    cfg.max_temp_dc = value.saturating_mul(10);

    save_config(&cfg)?;

    display::success(&format!("Max temperature set to {} °C", value));

    notify_daemon();

    Ok(())
}

pub fn max_current(value: u32) -> Result<(), ChargerError> {
    if value != 0 && !(500..=10000).contains(&value) {
        let msg = "Max charge current must be 0 (unconstrained) or between 500 and 10000 mA";
        display::error(msg);
        return Err(ChargerError::InvalidInput(msg.to_string()));
    }

    let mut cfg = load_config()?;
    cfg.max_charge_current_ma = value;
    save_config(&cfg)?;

    if value == 0 {
        display::success("Max charge current set to Unconstrained (Full Speed)");
    } else {
        display::success(&format!("Max charge current set to {value} mA"));
    }

    notify_daemon();
    Ok(())
}

pub fn thermal_throttle(enabled: bool) -> Result<(), ChargerError> {
    let mut cfg = load_config()?;
    cfg.thermal_throttling_enabled = enabled;
    save_config(&cfg)?;

    display::success(&format!(
        "Stepped thermal throttling {}",
        if enabled { "enabled" } else { "disabled" }
    ));

    notify_daemon();
    Ok(())
}

pub fn enable(enabled: bool) -> Result<(), ChargerError> {
    let mut cfg = load_config()?;
    cfg.enabled = enabled;
    save_config(&cfg)?;

    display::success(&format!(
        "Charging control {}",
        if enabled { "enabled" } else { "disabled" }
    ));

    notify_daemon();
    Ok(())
}

pub fn fast_charge(enabled: bool) -> Result<(), ChargerError> {
    let mut cfg = load_config()?;
    cfg.fast_charge = enabled;
    save_config(&cfg)?;

    display::success(&format!(
        "Fast charge bypass {}",
        if enabled { "enabled" } else { "disabled" }
    ));

    notify_daemon();
    Ok(())
}

pub fn fast_charge_max_soc(value: u8) -> Result<(), ChargerError> {
    if !(50..=100).contains(&value) {
        let msg = "Fast charge max SOC must be between 50 and 100%";
        display::error(msg);
        return Err(ChargerError::InvalidInput(msg.to_string()));
    }

    let mut cfg = load_config()?;
    cfg.fast_charge_max_soc = value;
    save_config(&cfg)?;

    display::success(&format!("Fast charge max SOC set to {}%", value));

    notify_daemon();
    Ok(())
}
