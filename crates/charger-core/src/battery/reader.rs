use std::{
    fs,
    io::Read,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    battery::nodes::{
        BATTERY_CAPACITY_NODES, BATTERY_CAPACITY_RAW_NODES, BATTERY_CURRENT_NODES,
        BATTERY_REAL_SOC_NODES, BATTERY_SOC_DECIMAL_NODES, BATTERY_STATUS_NODE, BATTERY_TEMP_NODES,
        BATTERY_VOLTAGE_NODES, CHARGE_FULL_DESIGN_NODES, CYCLE_COUNT_NODES, INPUT_CURRENT_NODES,
        ONLINE_NODES, TECHNOLOGY_NODES, TYPEC_MODE_NODES,
    },
    error::ChargerError,
};

static CAPACITY_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static CAPACITY_RAW_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static SOC_DECIMAL_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static REAL_SOC_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static INPUT_CURRENT_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static BATTERY_CURRENT_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static VOLTAGE_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static TEMP_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static CHARGE_FULL_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static CYCLE_COUNT_CACHED_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Read and trim one sysfs value into a caller-provided buffer (Zero Heap Allocation).
pub fn read_sysfs_buf<'a>(path: &Path, buf: &'a mut [u8]) -> Result<&'a str, ChargerError> {
    let mut file = fs::File::open(path).map_err(|source| ChargerError::SysfsRead {
        path: path.to_owned(),
        source,
    })?;
    let bytes_read = file.read(buf).map_err(|source| ChargerError::SysfsRead {
        path: path.to_owned(),
        source,
    })?;
    let s = std::str::from_utf8(&buf[..bytes_read]).map_err(|_| ChargerError::SysfsRead {
        path: path.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf-8"),
    })?;
    Ok(s.trim())
}

/// Read and trim one sysfs value as a heap-allocated String.
pub fn read_sysfs(path: &Path) -> Result<String, ChargerError> {
    let mut buf = [0u8; 128];
    read_sysfs_buf(path, &mut buf).map(|s| s.to_owned())
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
    let mut buf = [0u8; 64];
    let idx = cached_idx.load(Ordering::Relaxed);
    if idx < paths.len() {
        if let Ok(raw) = read_sysfs_buf(Path::new(paths[idx]), &mut buf) {
            if let Some(value) = parse(raw) {
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
        let Ok(raw) = read_sysfs_buf(Path::new(path), &mut buf) else {
            continue;
        };

        if let Some(value) = parse(raw) {
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

pub fn read_soc_decimal() -> Option<f32> {
    read_first_cached(
        BATTERY_SOC_DECIMAL_NODES,
        &SOC_DECIMAL_CACHED_IDX,
        |raw| {
            let value = raw.parse::<f32>().ok()?;
            if value.is_finite() && (0.0..100.0).contains(&value) {
                Some(value / 100.0)
            } else {
                None
            }
        },
        "soc_decimal",
    )
    .ok()
}

/// Read battery SOC with fractional precision.
///
/// Supported representations:
///
/// - battery/capacity: 0..100 (+ optional bms/soc_decimal fractional part)
/// - bms/capacity_raw: either 0..100 or 0..10000
/// - battery/real_soc: 0..100
pub fn read_capacity_raw() -> Result<f32, ChargerError> {
    if let Ok(value) = read_first_cached(
        BATTERY_CAPACITY_NODES,
        &CAPACITY_CACHED_IDX,
        parse_percentage,
        "capacity",
    ) {
        if value < 100.0 {
            if let Some(dec) = read_soc_decimal() {
                return Ok((value + dec).min(100.0));
            }
        }
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
    read_first_cached(
        CHARGE_FULL_DESIGN_NODES,
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
    read_first_cached(
        CYCLE_COUNT_NODES,
        &CYCLE_COUNT_CACHED_IDX,
        |raw| raw.parse::<u32>().ok(),
        "cycle_count",
    )
}

pub fn read_technology() -> Result<String, ChargerError> {
    // Values that describe the power supply category, not battery chemistry.
    const NON_TECH_VALUES: &[&str] = &["Battery", "Mains", "USB", "BMS", "UPS", "Unknown"];

    for path in TECHNOLOGY_NODES {
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

/// Normalized metrics of battery current and power flow.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CurrentMetrics {
    /// Absolute current in mA.
    pub current_ma: f32,
    /// Signed current in mA (+ for charging, - for discharging).
    pub signed_ma: f32,
    /// Whether current is flowing into battery (charging).
    pub is_charging_flow: bool,
    /// Absolute power in Watts.
    pub wattage_w: f32,
}

/// Read normalized battery current, direction, and power draw.
pub fn get_battery_metrics() -> Result<CurrentMetrics, ChargerError> {
    let current_ua = read_battery_current_ua()?;
    let voltage_uv = read_voltage_uv().unwrap_or(3_800_000);
    let power_state = get_power_state().unwrap_or(PowerState::Unknown);

    let raw_ma = current_ua as f32 / 1000.0;

    // Standard Linux power supply class convention:
    // negative current_now = charging into battery (sink),
    // positive current_now = discharging from battery (source).
    let is_charging_flow = if raw_ma < 0.0 {
        true
    } else if raw_ma > 0.0 && power_state.is_plugged_in() {
        // Fallback for drivers that invert convention
        true
    } else if raw_ma > 0.0 {
        false
    } else {
        power_state.is_plugged_in()
    };

    let abs_ma = raw_ma.abs();
    let signed_ma = if is_charging_flow { abs_ma } else { -abs_ma };
    let wattage_w = calc_wattage_from_ua_w(voltage_uv, current_ua.abs());

    Ok(CurrentMetrics {
        current_ma: abs_ma,
        signed_ma,
        is_charging_flow,
        wattage_w,
    })
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
/// 1. AC / Charger / USB / Mains online
/// 2. USB Type-C external power source attached hint (Source attached / Sink role)
/// 3. disconnected
pub fn get_power_state() -> Result<PowerState, ChargerError> {
    let mut source_available = false;
    let mut buf = [0u8; 64];

    for path_str in ONLINE_NODES {
        if let Ok(online) = read_sysfs_buf(Path::new(path_str), &mut buf) {
            source_available = true;
            if online == "1" {
                let mut status_buf = [0u8; 64];
                let status = read_sysfs_buf(Path::new(BATTERY_STATUS_NODE), &mut status_buf)
                    .unwrap_or("Unknown");

                return Ok(if status.eq_ignore_ascii_case("Charging") {
                    PowerState::Charging
                } else {
                    PowerState::Connected
                });
            }
        }
    }

    for path_str in TYPEC_MODE_NODES {
        if let Ok(typec) = read_sysfs_buf(Path::new(path_str), &mut buf) {
            source_available = true;
            if is_typec_source_attached(typec) {
                return Ok(PowerState::Attached);
            }
        }
    }

    if source_available {
        Ok(PowerState::Disconnected)
    } else {
        Ok(PowerState::Unknown)
    }
}

/// Discriminate whether a Type-C connection represents an incoming power source (charger)
/// versus an outgoing power sink (OTG flash drive, accessory, or reverse charging).
///
/// Kernel Type-C mode conventions:
/// - Qualcomm `typec_mode`: "Source attached ..." indicates the partner device is a power Source (phone is charging).
///   "Sink attached ..." indicates the partner is a Sink (phone is powering OTG accessory).
/// - Linux standard `power_role`: "sink" (or "[sink] source") indicates the local port is a Sink (phone is charging).
///   "source" indicates the local port is a Source (phone is powering OTG accessory).
pub fn is_typec_source_attached(raw: &str) -> bool {
    let lower = raw.trim().to_lowercase();

    // Explicitly reject OTG, accessory, or idle states
    if lower.contains("sink attached")
        || lower.contains("nothing attached")
        || lower.contains("audio adapter")
        || lower.contains("debug accessory")
        || lower == "none"
        || lower == "source"
        || lower.starts_with("[source]")
    {
        return false;
    }

    // 1. Qualcomm/Android typec_mode: partner is a Source (supplying power to phone)
    if lower.contains("source attached") {
        return true;
    }

    // 2. Linux Type-C class power_role: phone is Sink (consuming power)
    if lower == "sink" || lower.starts_with("[sink]") {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_typec_source_attached() {
        // Chargers (Incoming power Source attached)
        assert!(is_typec_source_attached(
            "Source attached (default current)"
        ));
        assert!(is_typec_source_attached(
            "Source attached (medium current 1.5A)"
        ));
        assert!(is_typec_source_attached(
            "Source attached (high current 3.0A)"
        ));
        assert!(is_typec_source_attached(
            "Source attached (non-compliant charger)"
        ));
        assert!(is_typec_source_attached("[sink] source"));
        assert!(is_typec_source_attached("sink"));

        // OTG / Reverse charging / Accessories (Should NOT be detected as charger!)
        assert!(!is_typec_source_attached("Sink attached"));
        assert!(!is_typec_source_attached("Sink attached (powered cable)"));
        assert!(!is_typec_source_attached("Sink attached (debug accessory)"));
        assert!(!is_typec_source_attached(
            "Audio adapter accessory attached"
        ));
        assert!(!is_typec_source_attached("Nothing attached"));
        assert!(!is_typec_source_attached("none"));
        assert!(!is_typec_source_attached("[source] sink"));
        assert!(!is_typec_source_attached("source"));
    }

    #[test]
    fn test_wattage_calculation() {
        // 4.40V * 500mA = 2.2W
        let w = calc_wattage_w(4_400_000, 500.0);
        assert!((w - 2.2).abs() < 0.001);

        // 4.35V * 1000000uA (1A) = 4.35W
        let w2 = calc_wattage_from_ua_w(4_350_000, 1_000_000);
        assert!((w2 - 4.35).abs() < 0.001);
    }

    #[test]
    fn test_parse_percentage() {
        assert_eq!(parse_percentage("98"), Some(98.0));
        assert_eq!(parse_percentage("100"), Some(100.0));
        assert_eq!(parse_percentage("0"), Some(0.0));
        assert_eq!(parse_percentage("-5"), None);
        assert_eq!(parse_percentage("150"), None);
        assert_eq!(parse_percentage("abc"), None);
    }

    #[test]
    fn test_power_state_plugged_in() {
        assert!(PowerState::Connected.is_plugged_in());
        assert!(PowerState::Charging.is_plugged_in());
        assert!(PowerState::Attached.is_plugged_in());
        assert!(!PowerState::Disconnected.is_plugged_in());
        assert!(!PowerState::Unknown.is_plugged_in());
    }

    #[test]
    fn test_current_metrics_struct() {
        // Negative current = Charging flow (into battery)
        let charging = CurrentMetrics {
            current_ma: 785.5,
            signed_ma: 785.5,
            is_charging_flow: true,
            wattage_w: calc_wattage_w(4_400_000, 785.5),
        };
        assert!(charging.is_charging_flow);
        assert!((charging.wattage_w - 3.4562).abs() < 0.01);

        // Positive current = Discharging flow (out of battery)
        let discharging = CurrentMetrics {
            current_ma: 350.0,
            signed_ma: -350.0,
            is_charging_flow: false,
            wattage_w: calc_wattage_w(4_100_000, 350.0),
        };
        assert!(!discharging.is_charging_flow);
        assert_eq!(discharging.signed_ma, -350.0);
    }
}
