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

/// Check if the charger is currently plugged in
/// Defaults to true if status cannot be read to be safe.
pub fn is_plugged_in() -> Result<bool, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/status");
    if let Ok(status) = read_sysfs(path) {
        let s = status.to_lowercase();
        // If it's discharging or not charging explicitly, it's unplugged (or bypassing).
        // Wait, if it's bypassing, status might be "Not charging" or "Discharging".
        // Let's rely on 'Discharging' as the primary indicator of being unplugged.
        if s.contains("discharging") {
            return Ok(false);
        }
    }
    
    // Fallback: check ac/usb online
    let nodes = [
        "/sys/class/power_supply/ac/online",
        "/sys/class/power_supply/usb/online",
        "/sys/class/power_supply/wireless/online",
    ];
    for p in nodes {
        if let Ok(val) = read_sysfs(Path::new(p)) {
            if val == "1" {
                return Ok(true);
            }
        }
    }

    Ok(true) // Default safe assumption
}

use std::io::{Read, Seek, SeekFrom};
use std::fs::File;

/// A stateful reader that holds open File Descriptors for zero-allocation polling.
pub struct CachedReader {
    capacity_fd: Option<File>,
    temp_fd: Option<File>,
    current_fd: Option<File>,
    status_fd: Option<File>,
    buf: [u8; 32],
}

impl CachedReader {
    pub fn new() -> Self {
        let current_path = CURRENT_NODES.iter().find(|&&p| Path::new(p).exists()).copied().unwrap_or("/sys/class/power_supply/battery/current_now");
        Self {
            capacity_fd: File::open("/sys/class/power_supply/battery/capacity").ok(),
            temp_fd: File::open("/sys/class/power_supply/battery/temp").ok(),
            current_fd: File::open(current_path).ok(),
            status_fd: File::open("/sys/class/power_supply/battery/status").ok(),
            buf: [0; 32],
        }
    }

    fn read_fd_to_str<'a>(fd: &mut Option<File>, buf: &'a mut [u8], node_name: &'static str) -> Result<&'a str, ChargerError> {
        if let Some(f) = fd {
            let _ = f.seek(SeekFrom::Start(0));
            if let Ok(n) = f.read(buf) {
                if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                    return Ok(s.trim());
                }
            }
            Err(ChargerError::ParseError(node_name))
        } else {
            Err(ChargerError::SysfsRead { 
                path: std::path::PathBuf::from(node_name), 
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "FD not open") 
            })
        }
    }

    pub fn read_capacity(&mut self) -> Result<u8, ChargerError> {
        let s = Self::read_fd_to_str(&mut self.capacity_fd, &mut self.buf, "capacity")?;
        s.parse().map_err(|_| ChargerError::ParseError("capacity"))
    }

    pub fn read_temperature_dc(&mut self) -> Result<i32, ChargerError> {
        let s = Self::read_fd_to_str(&mut self.temp_fd, &mut self.buf, "temp")?;
        s.parse().map_err(|_| ChargerError::ParseError("temp"))
    }

    pub fn read_current_ma(&mut self) -> Result<f32, ChargerError> {
        let s = Self::read_fd_to_str(&mut self.current_fd, &mut self.buf, "current_now")?;
        if let Ok(val) = s.parse::<i64>() {
            if val != 0 {
                let mut ua = val as f32;
                if ua.abs() > 10_000.0 { ua /= 1000.0; }
                return Ok(ua);
            }
        }
        Ok(0.0)
    }

    pub fn is_plugged_in(&mut self) -> Result<bool, ChargerError> {
        let s = Self::read_fd_to_str(&mut self.status_fd, &mut self.buf, "status")?;
        let s_lower = s.to_lowercase();
        if s_lower.contains("discharging") {
            return Ok(false);
        }
        Ok(true) // Charging, Full, Not charging
    }
}
