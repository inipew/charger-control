use std::{fs, path::Path};

use crate::{
    battery::nodes::*,
    error::ChargerError,
};

/// Read a sysfs node and trim surrounding whitespace.
pub fn read_sysfs(
    path: &Path,
) -> Result<String, ChargerError> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|e| ChargerError::SysfsRead {
            path: path.to_owned(),
            source: e,
        })
}

/// Read the first available sysfs node from a list.
///
/// This helper is useful for devices where the same information can exist
/// under different power_supply nodes.
fn read_first_available(
    paths: &[&str],
) -> Result<String, ChargerError> {
    let mut last_error: Option<ChargerError> = None;

    for path in paths {
        match read_sysfs(Path::new(path)) {
            Ok(value) if !value.is_empty() => {
                return Ok(value);
            }

            Ok(_) => {
                last_error =
                    Some(ChargerError::ParseError("empty_sysfs_value"));
            }

            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or(
        ChargerError::ParseError("sysfs"),
    ))
}

/// Read battery SOC as an integer percentage.
pub fn read_capacity() -> Result<u8, ChargerError> {
    let value = read_capacity_raw()?;

    Ok(value
        .round()
        .clamp(0.0, 100.0) as u8)
}

/// Read battery SOC with fractional precision.
///
/// Priority:
/// 1. battery/capacity
/// 2. bms/capacity_raw
/// 3. battery/real_soc
///
/// Examples:
///     98      -> 98.0
///     9848    -> 98.48
pub fn read_capacity_raw() -> Result<f32, ChargerError> {
    // Standard Linux power_supply SOC.
    if let Ok(raw) = read_sysfs(
        Path::new("/sys/class/power_supply/battery/capacity"),
    ) {
        if let Ok(value) = raw.parse::<f32>() {
            if value.is_finite()
                && (0.0..=100.0).contains(&value)
            {
                return Ok(value);
            }
        }
    }

    // Some BMS implementations expose a higher-resolution SOC.
    if let Ok(raw) = read_sysfs(
        Path::new("/sys/class/power_supply/bms/capacity_raw"),
    ) {
        if let Ok(value) = raw.parse::<f32>() {
            if value.is_finite() {
                let normalized = if value > 100.0 {
                    value / 100.0
                } else {
                    value
                };

                if (0.0..=100.0).contains(&normalized) {
                    return Ok(normalized);
                }
            }
        }
    }

    // Android/MTK devices sometimes expose real_soc.
    if let Ok(raw) = read_sysfs(
        Path::new("/sys/class/power_supply/battery/real_soc"),
    ) {
        if let Ok(value) = raw.parse::<f32>() {
            if value.is_finite()
                && (0.0..=100.0).contains(&value)
            {
                return Ok(value);
            }
        }
    }

    Err(ChargerError::ParseError("capacity"))
}

/// Read charger/input current in microamps.
pub fn read_input_current_ua() -> Result<i64, ChargerError> {
    let mut last_parse_error = false;

    for path in INPUT_CURRENT_NODES {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            match raw.parse::<i64>() {
                Ok(value) => {
                    return Ok(value);
                }

                Err(_) => {
                    last_parse_error = true;
                }
            }
        }
    }

    if last_parse_error {
        Err(ChargerError::ParseError(
            "input_current_ua",
        ))
    } else {
        Err(ChargerError::ParseError(
            "input_current_ua",
        ))
    }
}

/// Read battery current in microamps.
///
/// Do not infer charging/discharging solely from the sign because some
/// Android/MTK drivers expose opposite conventions.
pub fn read_battery_current_ua() -> Result<i64, ChargerError> {
    for path in BATTERY_CURRENT_NODES {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if let Ok(value) = raw.parse::<i64>() {
                return Ok(value);
            }
        }
    }

    Err(ChargerError::ParseError(
        "battery_current_ua",
    ))
}

/// Read battery voltage in microvolts.
pub fn read_voltage_uv() -> Result<u32, ChargerError> {
    let raw = read_sysfs(
        Path::new(
            "/sys/class/power_supply/battery/voltage_now",
        ),
    )?;

    raw.parse::<u32>()
        .map_err(|_| {
            ChargerError::ParseError("voltage_now")
        })
}

/// Read battery temperature in deci-Celsius.
///
/// Uses BATTERY_TEMP_NODES so the function can support devices exposing
/// different temperature nodes.
pub fn read_temperature_dc() -> Result<i32, ChargerError> {
    for path in BATTERY_TEMP_NODES {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if let Ok(value) = raw.parse::<i32>() {
                // Reject obviously invalid temperatures.
                //
                // -40.0C = -400 dc
                // 120.0C = 1200 dc
                if (-400..=1200).contains(&value) {
                    return Ok(value);
                }
            }
        }
    }

    Err(ChargerError::ParseError("temp"))
}

/// Read design/full battery capacity and normalize to mAh.
pub fn read_charge_full_design() -> Result<u32, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/charge_full_design",
        "/sys/class/power_supply/bms/charge_full_design",
        "/sys/class/power_supply/battery/capacity_design_uah",
    ];

    for path in paths {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if let Ok(value) = raw.parse::<u32>() {
                if value == 0 {
                    continue;
                }

                // Most power_supply nodes expose uAh.
                //
                // Smaller values are assumed to already be mAh.
                let mah = if value > 100_000 {
                    value / 1000
                } else {
                    value
                };

                if mah > 0 {
                    return Ok(mah);
                }
            }
        }
    }

    Err(ChargerError::ParseError(
        "charge_full_design",
    ))
}

/// Read battery cycle count.
pub fn read_cycle_count() -> Result<u32, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/cycle_count",
        "/sys/class/power_supply/bms/cycle_count",
        "/sys/class/power_supply/main/cycle_count",
    ];

    for path in paths {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if let Ok(value) = raw.parse::<u32>() {
                return Ok(value);
            }
        }
    }

    Err(ChargerError::ParseError(
        "cycle_count",
    ))
}

/// Read battery chemistry / technology.
pub fn read_technology() -> Result<String, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/technology",
        "/sys/class/power_supply/battery/type",
        "/sys/class/power_supply/bms/battery_type",
    ];

    for path in paths {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if !raw.is_empty() {
                return Ok(raw);
            }
        }
    }

    Ok("Li-ion".to_string())
}

/// Calculate electrical power.
///
/// voltage_uv:
///     microvolts
///
/// current_ma:
///     milliamps
///
/// result:
///     watts
pub fn calc_wattage_w(
    voltage_uv: u32,
    current_ma: f32,
) -> f32 {
    (voltage_uv as f32 / 1_000_000.0)
        * (current_ma / 1000.0)
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
        matches!(
            self,
            Self::Attached
                | Self::Connected
                | Self::Charging
        )
    }

    pub fn is_charging(&self) -> bool {
        matches!(self, Self::Charging)
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

/// Determine current charger/power state.
///
/// Priority:
/// 1. AC online
/// 2. USB Type-C attached hint
/// 3. disconnected
///
/// AC is still treated as the primary source of truth because USB Type-C
/// attach can happen before the charger is actually online.
pub fn get_power_state() -> Result<PowerState, ChargerError> {
    let ac_online = read_sysfs(
        Path::new(AC_ONLINE_NODE),
    )?;

    if ac_online == "1" {
        let status = read_sysfs(
            Path::new(BATTERY_STATUS_NODE),
        )
        .unwrap_or_else(|_| "Unknown".to_string());

        if status.eq_ignore_ascii_case("Charging") {
            return Ok(PowerState::Charging);
        }

        return Ok(PowerState::Connected);
    }

    // Early USB Type-C attach hint.
    if let Ok(typec) = read_sysfs(
        Path::new(USB_TYPEC_MODE_NODE),
    ) {
        if typec.contains("Source attached") {
            return Ok(PowerState::Attached);
        }
    }

    Ok(PowerState::Disconnected)
}