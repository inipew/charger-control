use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    battery::nodes::{
        AC_ONLINE_NODE, BATTERY_CAPACITY_NODES, BATTERY_CAPACITY_RAW_NODES, BATTERY_CURRENT_NODES,
        BATTERY_REAL_SOC_NODES, BATTERY_STATUS_NODE, BATTERY_TEMP_NODES, BATTERY_VOLTAGE_NODES,
        INPUT_CURRENT_NODES, USB_ONLINE_NODE, USB_TYPEC_MODE_NODE,
    },
    error::ChargerError,
};

static CAPACITY_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static CAPACITY_RAW_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static REAL_SOC_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static INPUT_CURRENT_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static BATTERY_CURRENT_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static VOLTAGE_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static TEMP_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static CHARGE_FULL_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static CYCLE_COUNT_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Read and trim one sysfs value.
pub fn read_sysfs(path: &Path) -> Result<String, ChargerError> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|source| ChargerError::SysfsRead {
            path: path.to_owned(),
            source,
        })
}

/// Read the first valid value from a list of sysfs nodes with atomic index caching and resolution logging.
fn read_first_cached<T, F>(
    paths: &[&str],
    cached_idx: &AtomicUsize,
    parse: F,
    error_name: &'static str,
) -> Result<T, ChargerError>
where
    F: Fn(&str) -> Option<T>,
{
    let idx = cached_idx.load(Ordering::Relaxed);
    if idx < paths.len() {
        if let Ok(raw) = read_sysfs(Path::new(paths[idx])) {
            if let Some(value) = parse(&raw) {
                return Ok(value);
            }
        }
        // Cached node read or parse failed — invalidate stale cache index
        // so candidate scanning can resolve a new working sysfs node.
        cached_idx.store(usize::MAX, Ordering::Relaxed);
    }

    for (i, path) in paths.iter().enumerate() {
        if i == idx {
            continue;
        }
        let Ok(raw) = read_sysfs(Path::new(path)) else {
            continue;
        };

        if let Some(value) = parse(&raw) {
            let prev = cached_idx.swap(i, Ordering::Relaxed);
            if prev != i {
                tracing::info!(
                    category = error_name,
                    path = paths[i],
                    "active sysfs node resolved"
                );
            }
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
    if let Ok(value) = read_first_cached(
        BATTERY_CAPACITY_NODES,
        &CAPACITY_CACHED_IDX,
        parse_percentage,
        "capacity",
    ) {
        return Ok(value);
    }

    if let Ok(value) = read_first_cached(
        BATTERY_CAPACITY_RAW_NODES,
        &CAPACITY_RAW_CACHED_IDX,
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

    if let Ok(value) = read_first_cached(
        BATTERY_REAL_SOC_NODES,
        &REAL_SOC_CACHED_IDX,
        parse_percentage,
        "real_soc",
    ) {
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
    read_first_cached(
        INPUT_CURRENT_NODES,
        &INPUT_CURRENT_CACHED_IDX,
        |raw| {
            let value = raw.parse::<i64>().ok()?;
            // Guard: skip 0 to avoid returning driver-configured limits
            // when charger is not actively supplying current.
            if value == 0 {
                None
            } else {
                Some(value)
            }
        },
        "input_current_ua",
    )
}

pub fn read_battery_current_ua() -> Result<i64, ChargerError> {
    read_first_cached(
        BATTERY_CURRENT_NODES,
        &BATTERY_CURRENT_CACHED_IDX,
        |raw| raw.parse::<i64>().ok(),
        "battery_current_ua",
    )
}

/// Read battery voltage in microvolts (µV).
///
/// Node unit differences across power supply drivers:
///
/// - `battery/voltage_now`: µV  (e.g. 4471000 = 4.471 V)
/// - `bms/voltage_now`:      mV  (e.g. 4471    = 4.471 V)
///
/// Auto-normalizes: values < 10_000 are assumed to be in mV and
/// are multiplied by 1000 to produce µV.
pub fn read_voltage_uv() -> Result<u32, ChargerError> {
    read_first_cached(
        BATTERY_VOLTAGE_NODES,
        &VOLTAGE_CACHED_IDX,
        |raw| {
            let value = raw.parse::<u32>().ok()?;
            if value == 0 {
                return None;
            }
            // bms/voltage_now reports mV (~4471); battery/voltage_now reports µV (~4471000).
            // A real battery voltage in µV is always >> 10_000 (Li-ion is 3000000–4500000 µV).
            Some(if value < 10_000 { value * 1000 } else { value })
        },
        "voltage_now",
    )
}

pub fn read_temperature_dc() -> Result<i32, ChargerError> {
    read_first_cached(
        BATTERY_TEMP_NODES,
        &TEMP_CACHED_IDX,
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

/// Read battery temperature in degrees Celsius (°C).
pub fn read_temperature_c() -> Result<f32, ChargerError> {
    read_temperature_dc().map(|dc| dc as f32 / 10.0)
}

pub fn read_charge_full_design() -> Result<u32, ChargerError> {
    const NODES: &[&str] = &[
        "/sys/class/power_supply/battery/charge_full_design",
        "/sys/class/power_supply/bms/charge_full_design",
        "/sys/class/power_supply/battery/capacity_design_uah",
    ];

    read_first_cached(
        NODES,
        &CHARGE_FULL_CACHED_IDX,
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

    read_first_cached(
        NODES,
        &CYCLE_COUNT_CACHED_IDX,
        |raw| raw.parse::<u32>().ok(),
        "cycle_count",
    )
}

pub fn read_technology() -> Result<String, ChargerError> {
    const NODES: &[&str] = &[
        "/sys/class/power_supply/battery/technology",
        // battery/type = "Battery" is the power supply type, not chemistry — skip.
        // bms/battery_type may be a numeric ID on some platforms — validated below.
        "/sys/class/power_supply/bms/battery_type",
    ];

    // Values that describe the power supply category, not battery chemistry.
    const NON_TECH_VALUES: &[&str] = &["Battery", "Mains", "USB", "BMS", "UPS", "Unknown"];

    for path in NODES {
        if let Ok(value) = read_sysfs(Path::new(path)) {
            if value.is_empty() {
                continue;
            }
            // Skip values that are power supply category labels, not chemistry strings.
            if NON_TECH_VALUES
                .iter()
                .any(|&skip| value.eq_ignore_ascii_case(skip))
            {
                continue;
            }
            // Skip bare numeric values (some drivers export a numeric battery-type ID).
            if value.parse::<i64>().is_ok() {
                continue;
            }
            return Ok(value);
        }
    }

    Ok("Li-ion".to_owned())
}

pub fn calc_wattage_w(voltage_uv: u32, current_ma: f32) -> f32 {
    (voltage_uv as f32 / 1_000_000.0) * (current_ma / 1000.0)
}

/// Directly calculate wattage in Watts from voltage in microvolts (uV) and current in microamperes (uA).
pub fn calc_wattage_from_ua_w(voltage_uv: u32, current_ua: i64) -> f32 {
    (voltage_uv as f64 / 1_000_000.0 * current_ua as f64 / 1_000_000.0) as f32
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
    let mut source_available = false;

    if let Ok(ac_online) = read_sysfs(Path::new(AC_ONLINE_NODE)) {
        source_available = true;
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
        source_available = true;
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
        source_available = true;
        if typec.contains("Source attached") {
            return Ok(PowerState::Attached);
        }
    }

    if source_available {
        Ok(PowerState::Disconnected)
    } else {
        Ok(PowerState::Unknown)
    }
}
