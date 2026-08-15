use std::path::Path;

/// Charging control nodes.
///
/// The order is intentional and must be preserved by callers.
pub const CHARGING_NODES: &[&str] = &["/sys/class/power_supply/battery/charging_enabled"];

/// Input suspend control nodes.
///
/// The order is intentional and must be preserved by callers.
pub const SUSPEND_NODES: &[&str] = &["/sys/class/power_supply/battery/input_suspend"];

pub const FAST_CHARGE_CURRENT_NODES: &[&str] =
    &["/sys/class/power_supply/battery/fast_charge_current"];

pub const THERMAL_INPUT_CURRENT_NODES: &[&str] =
    &["/sys/class/power_supply/battery/thermal_input_current"];

pub const BATTERY_CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/current_now",
    "/sys/class/power_supply/bms/current_now",
];

pub const INPUT_CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/usb/input_current_now",
    "/sys/class/power_supply/main/current_now",
];

pub const BATTERY_CAPACITY_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/capacity",
    "/sys/class/power_supply/bms/capacity",
];

pub const BATTERY_TEMP_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/temp",
    "/sys/class/power_supply/bms/temp",
];

pub const BATTERY_VOLTAGE_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/voltage_now",
    "/sys/class/power_supply/bms/voltage_now",
];

pub const BATTERY_CAPACITY_RAW_NODES: &[&str] = &["/sys/class/power_supply/bms/capacity_raw"];

pub const BATTERY_SOC_DECIMAL_NODES: &[&str] = &[
    "/sys/class/power_supply/bms/soc_decimal",
    "/sys/class/power_supply/battery/soc_decimal",
];

pub const BATTERY_REAL_SOC_NODES: &[&str] = &["/sys/class/power_supply/battery/real_soc"];

pub const CHARGE_FULL_DESIGN_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/charge_full_design",
    "/sys/class/power_supply/bms/charge_full_design",
];

pub const CYCLE_COUNT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/cycle_count",
    "/sys/class/power_supply/bms/cycle_count",
    "/sys/class/power_supply/batt_verify/maxim_batt_cycle_count",
];

pub const TECHNOLOGY_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/technology",
    "/sys/class/power_supply/bms/battery_type",
];

pub const ONLINE_NODES: &[&str] = &[
    "/sys/class/power_supply/ac/online",
    "/sys/class/power_supply/charger/online",
    "/sys/class/power_supply/usb/online",
    "/sys/class/power_supply/main/online",
    "/sys/class/power_supply/mains/online",
    "/sys/class/power_supply/wireless/online",
];

pub const TYPEC_MODE_NODES: &[&str] = &[
    "/sys/class/power_supply/usb/typec_mode",
    "/sys/class/power_supply/battery/typec_mode",
    "/sys/class/power_supply/usb/typec_power_role",
    "/sys/class/typec/port0/power_role",
];

pub const BATTERY_STATUS_NODE: &str = "/sys/class/power_supply/battery/status";

/// Finds the first available sysfs path.
pub fn detect_node(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
}
