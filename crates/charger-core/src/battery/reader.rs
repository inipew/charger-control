use std::{fs, path::Path};

use crate::{
    battery::nodes::{
        AC_ONLINE_NODE, BATTERY_CAPACITY_NODES, BATTERY_CAPACITY_RAW_NODES, BATTERY_CURRENT_NODES,
        BATTERY_REAL_SOC_NODES, BATTERY_STATUS_NODE, BATTERY_TEMP_NODES, INPUT_CURRENT_NODES,
        USB_ONLINE_NODE, USB_TYPEC_MODE_NODE,
    },
    error::ChargerError,
};

/// Read and trim one sysfs value.
pub fn read_sysfs(path: &Path) -> Result<String, ChargerError> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|source| ChargerError::SysfsRead {
            path: path.to_owned(),
            source,
        })
}

/// Read the first valid value from a list of sysfs nodes.
fn read_first<T, F>(paths: &[&str], parse: F, error_name: &'static str) -> Result<T, ChargerError>
where
    F: Fn(&str) -> Option<T>,
{
    for path in paths {
        let Ok(raw) = read_sysfs(Path::new(path)) else {
            continue;
        };

        if let Some(value) = parse(&raw) {
            return Ok(value);
        }
    }

    Err(ChargerError::ParseError(error_name))
}

/// Read battery SOC as an integer percentage.
pub fn read_capacity() -> Result<u8, ChargerError> {
    Ok(read_capacity_raw()?.round().clamp(0.0, 100.0) as u8)
}

/// Read battery SOC with fractional precision.
///
/// Supported representations:
///
/// - battery/capacity: 0..100
/// - bms/capacity_raw: either 0..100 or 0..10000
/// - battery/real_soc: 0..100
pub fn read_capacity_raw() -> Result<f32, ChargerError> {
    if let Ok(value) = read_first(BATTERY_CAPACITY_NODES, parse_percentage, "capacity") {
        return Ok(value);
    }

    if let Ok(value) = read_first(
        BATTERY_CAPACITY_RAW_NODES,
        |raw| {
            let value = raw.parse::<f32>().ok()?;

            if !value.is_finite() {
                return None;
            }

            let normalized = if value > 100.0 { value / 100.0 } else { value };

            if (0.0..=100.0).contains(&normalized) {
                Some(normalized)
            } else {
                None
            }
        },
        "capacity_raw",
    ) {
        return Ok(value);
    }

    if let Ok(value) = read_first(BATTERY_REAL_SOC_NODES, parse_percentage, "real_soc") {
        return Ok(value);
    }

    Err(ChargerError::ParseError("capacity"))
}

fn parse_percentage(raw: &str) -> Option<f32> {
    let value = raw.parse::<f32>().ok()?;

    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Some(value)
    } else {
        None
    }
}

pub fn read_input_current_ua() -> Result<i64, ChargerError> {
    read_first(
        INPUT_CURRENT_NODES,
        |raw| raw.parse::<i64>().ok(),
        "input_current_ua",
    )
}

pub fn read_battery_current_ua() -> Result<i64, ChargerError> {
    read_first(
        BATTERY_CURRENT_NODES,
        |raw| raw.parse::<i64>().ok(),
        "battery_current_ua",
    )
}

pub fn read_voltage_uv() -> Result<u32, ChargerError> {
    let raw = read_sysfs(Path::new("/sys/class/power_supply/battery/voltage_now"))?;

    raw.parse::<u32>()
        .map_err(|_| ChargerError::ParseError("voltage_now"))
}

pub fn read_temperature_dc() -> Result<i32, ChargerError> {
    read_first(
        BATTERY_TEMP_NODES,
        |raw| {
            let value = raw.parse::<i32>().ok()?;

            if (-400..=1200).contains(&value) {
                Some(value)
            } else {
                None
            }
        },
        "temp",
    )
}

pub fn read_charge_full_design() -> Result<u32, ChargerError> {
    const NODES: &[&str] = &[
        "/sys/class/power_supply/battery/charge_full_design",
        "/sys/class/power_supply/bms/charge_full_design",
        "/sys/class/power_supply/battery/capacity_design_uah",
    ];

    read_first(
        NODES,
        |raw| {
            let value = raw.parse::<u32>().ok()?;

            if value == 0 {
                return None;
            }

            let mah = if value > 100_000 { value / 1000 } else { value };

            (mah > 0).then_some(mah)
        },
        "charge_full_design",
    )
}

pub fn read_cycle_count() -> Result<u32, ChargerError> {
    const NODES: &[&str] = &[
        "/sys/class/power_supply/battery/cycle_count",
        "/sys/class/power_supply/bms/cycle_count",
        "/sys/class/power_supply/main/cycle_count",
    ];

    read_first(NODES, |raw| raw.parse::<u32>().ok(), "cycle_count")
}

pub fn read_technology() -> Result<String, ChargerError> {
    const NODES: &[&str] = &[
        "/sys/class/power_supply/battery/technology",
        "/sys/class/power_supply/battery/type",
        "/sys/class/power_supply/bms/battery_type",
    ];

    for path in NODES {
        if let Ok(value) = read_sysfs(Path::new(path)) {
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    Ok("Li-ion".to_owned())
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
    pub const fn is_plugged_in(self) -> bool {
        matches!(self, Self::Attached | Self::Connected | Self::Charging)
    }

    pub const fn is_charging(self) -> bool {
        matches!(self, Self::Charging)
    }

    pub const fn is_disconnected(self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

/// Determine current external power state.
///
/// Priority:
///
/// 1. AC online
/// 2. USB Type-C attached hint
/// 3. disconnected
pub fn get_power_state() -> Result<PowerState, ChargerError> {
    if let Ok(ac_online) = read_sysfs(Path::new(AC_ONLINE_NODE)) {
        if ac_online == "1" {
            let status =
                read_sysfs(Path::new(BATTERY_STATUS_NODE)).unwrap_or_else(|_| "Unknown".to_owned());

            return Ok(if status.eq_ignore_ascii_case("Charging") {
                PowerState::Charging
            } else {
                PowerState::Connected
            });
        }
    }

    if let Ok(usb_online) = read_sysfs(Path::new(USB_ONLINE_NODE)) {
        if usb_online == "1" {
            let status =
                read_sysfs(Path::new(BATTERY_STATUS_NODE)).unwrap_or_else(|_| "Unknown".to_owned());

            return Ok(if status.eq_ignore_ascii_case("Charging") {
                PowerState::Charging
            } else {
                PowerState::Connected
            });
        }
    }

    if let Ok(typec) = read_sysfs(Path::new(USB_TYPEC_MODE_NODE)) {
        if typec.contains("Source attached") {
            return Ok(PowerState::Attached);
        }
    }

    Ok(PowerState::Disconnected)
}
