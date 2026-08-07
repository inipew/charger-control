use std::{fs, path::Path};
use crate::{battery::nodes::*, error::ChargerError};

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

/// Read current in microamps from first available node.
/// Returns signed i64 (negative = discharging).
pub fn read_current_ua() -> Result<i64, ChargerError> {
    for path in CURRENT_NODES {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if let Ok(val) = raw.parse::<i64>() {
                if val != 0 { return Ok(val); }
            }
        }
    }
    Ok(0)
}

/// Returns current in mA, sign-corrected.
pub fn read_current_ma() -> Result<f32, ChargerError> {
    let mut ua = read_current_ua()? as f32;
    // Normalize: if absolute value > 10_000, it's in µA → convert to mA
    if ua.abs() > 10_000.0 { ua /= 1000.0; }
    Ok(ua)
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
