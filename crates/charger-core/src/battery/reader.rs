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

/// Read input current in microamps from the charger.
/// Returns positive i64 if power is drawn.
pub fn read_input_current_ua() -> Result<i64, ChargerError> {
    let nodes = [
        "/sys/class/power_supply/main/current_now",
        "/sys/class/power_supply/main/input_current_now",
        "/sys/class/power_supply/usb/current_now",
    ];
    for p in nodes {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<i64>() {
                if val != 0 { return Ok(val); }
            }
        }
    }
    Ok(0)
}

/// Read battery current in microamps.
/// Returns signed i64 (negative = discharging).
pub fn read_battery_current_ua() -> Result<i64, ChargerError> {
    let nodes = [
        "/sys/class/power_supply/battery/current_now",
        "/sys/class/power_supply/battery/batt_current_now",
    ];
    for p in nodes {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<i64>() {
                if val != 0 { return Ok(val); }
            }
        }
    }
    Ok(0)
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

/// Check if the charger/power source is physically connected
pub fn is_power_connected() -> Result<bool, ChargerError> {
    // 1. Check typec_mode first (Hardware-level Sink detection)
    let path = Path::new("/sys/class/power_supply/battery/typec_mode");
    if let Ok(mode) = read_sysfs(path) {
        if mode.contains("Sink attached") {
            return Ok(true);
        } else if mode.contains("Powered cable w/ sink") {
            return Ok(false);
        }
    }

    // 2. Check input current (if > 0, we are definitely drawing power)
    if let Ok(current) = read_input_current_ua() {
        if current > 0 {
            return Ok(true);
        }
    }

    // 3. Fallback to present nodes (VBUS physical detection)
    let present_nodes = [
        "/sys/class/power_supply/usb/present",
        "/sys/class/power_supply/ac/present",
        "/sys/class/power_supply/wireless/present",
    ];
    let mut present_supported = false;
    for p in present_nodes {
        if let Ok(val) = read_sysfs(Path::new(p)) {
            present_supported = true;
            if val == "1" {
                return Ok(true);
            }
        }
    }
    if present_supported {
        return Ok(false);
    }

    // 4. Fallback to online nodes
    let nodes = [
        "/sys/class/power_supply/ac/online",
        "/sys/class/power_supply/usb/online",
        "/sys/class/power_supply/wireless/online",
    ];
    
    let mut online_supported = false;
    let mut any_online = false;
    
    for p in nodes {
        if let Ok(val) = read_sysfs(Path::new(p)) {
            online_supported = true;
            if val == "1" {
                any_online = true;
                break;
            }
        }
    }

    if online_supported {
        return Ok(any_online);
    }

    Ok(true) // Safe default, do not rely on battery/status discharging
}
