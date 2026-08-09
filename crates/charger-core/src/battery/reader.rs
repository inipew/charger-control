use std::{fs, path::Path};
use crate::error::ChargerError;

/// Read a sysfs node as a raw String, trimmed.
pub fn read_sysfs(path: &Path) -> Result<String, ChargerError> {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| ChargerError::SysfsRead { path: path.to_owned(), source: e })
}

/// Read battery level (0..=100) from sysfs capacity node.
pub fn read_capacity() -> Result<u8, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/capacity");
    read_sysfs(path)?
        .parse::<u8>()
        .map_err(|_| ChargerError::ParseError("capacity"))
}

pub fn read_capacity_raw() -> Result<f32, ChargerError> {
    if let Ok(raw) = read_sysfs(Path::new("/sys/class/power_supply/bms/capacity_raw")) {
        if let Ok(val) = raw.parse::<f32>() {
            return Ok(if val > 100.0 { val / 100.0 } else { val });
        }
    }
    if let Ok(real) = read_sysfs(Path::new("/sys/class/power_supply/battery/real_soc")) {
        if let Ok(val) = real.parse::<f32>() {
            return Ok(val);
        }
    }
    // Fallback to integer capacity
    read_capacity().map(|v| v as f32)
}

/// Read input current in microamps from the charger.
/// Returns positive i64 if power is drawn.
pub fn read_input_current_ua() -> Result<i64, ChargerError> {
    for p in crate::battery::nodes::INPUT_CURRENT_NODES {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<i64>() {
                return Ok(val);
            }
        }
    }
    Err(ChargerError::ParseError("input_current_ua"))
}

/// Read battery current in microamps.
/// Returns signed i64 (negative = discharging).
pub fn read_battery_current_ua() -> Result<i64, ChargerError> {
    for p in crate::battery::nodes::BATTERY_CURRENT_NODES {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<i64>() {
                return Ok(val);
            }
        }
    }
    Err(ChargerError::ParseError("battery_current_ua"))
}

pub fn read_voltage_uv() -> Result<u32, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/voltage_now");
    read_sysfs(path)?
        .parse::<u32>()
        .map_err(|_| ChargerError::ParseError("voltage_now"))
}

pub fn read_temperature_dc() -> Result<i32, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/temp");
    read_sysfs(path)?
        .parse::<i32>()
        .map_err(|_| ChargerError::ParseError("temp"))
}

pub fn read_charge_full_design() -> Result<u32, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/charge_full_design",
        "/sys/class/power_supply/bms/charge_full_design",
        "/sys/class/power_supply/battery/capacity_design_uah",
    ];
    for p in paths {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<u32>() {
                if val > 0 {
                    let mah = if val > 100_000 { val / 1000 } else { val };
                    return Ok(mah);
                }
            }
        }
    }
    Err(ChargerError::ParseError("charge_full_design"))
}

pub fn read_cycle_count() -> Result<u32, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/cycle_count",
        "/sys/class/power_supply/bms/cycle_count",
        "/sys/class/power_supply/main/cycle_count",
    ];
    for p in paths {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<u32>() {
                if val > 0 { return Ok(val); }
            }
        }
    }
    Err(ChargerError::ParseError("cycle_count"))
}

pub fn read_technology() -> Result<String, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/technology",
        "/sys/class/power_supply/battery/type",
        "/sys/class/power_supply/bms/battery_type",
    ];
    for p in paths {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if !raw.is_empty() { return Ok(raw); }
        }
    }
    Ok("Li-ion".to_string())
}

pub fn calc_wattage_w(voltage_uv: u32, current_ma: f32) -> f32 {
    (voltage_uv as f32 / 1_000_000.0) * (current_ma / 1000.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Disconnected,
    Attached,
    Connected,
    Charging,
    Unknown,
}

impl PowerState {
    pub fn is_plugged_in(&self) -> bool {
        matches!(self, PowerState::Connected | PowerState::Charging)
    }
}

/// Get the current 4-Tier power state
pub fn get_power_state() -> Result<PowerState, ChargerError> {
    // 1. Check AC Online (Source of Truth for Connected)
    let ac_online = read_sysfs(Path::new("/sys/class/power_supply/ac/online"))?;
    if ac_online == "1" {
        // Validation: Is it actually charging?
        let status = read_sysfs(Path::new("/sys/class/power_supply/battery/status")).unwrap_or_else(|_| "Unknown".into());
        if status == "Charging" {
            return Ok(PowerState::Charging);
        }
        return Ok(PowerState::Connected);
    }
    
    // 2. Fallback to early attach hint if AC is offline
    if let Ok(typec) = read_sysfs(Path::new("/sys/class/power_supply/usb/typec_mode")) {
        if typec.contains("Source attached") {
            return Ok(PowerState::Attached);
        }
    }
    
    // 3. Completely disconnected
    Ok(PowerState::Disconnected)
}
