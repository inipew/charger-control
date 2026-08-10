use std::path::Path;

/// Charging control nodes.
///
/// The order is intentional and must be preserved by callers.
pub const CHARGING_NODES: &[&str] = &["/sys/class/power_supply/battery/charging_enabled"];

/// Input suspend control nodes.
///
/// The order is intentional and must be preserved by callers.
pub const SUSPEND_NODES: &[&str] = &["/sys/class/power_supply/battery/input_suspend"];

pub const BATTERY_CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/current_now",
    "/sys/class/power_supply/battery/batt_current_now",
    "/sys/class/power_supply/bms/current_now",
];

pub const INPUT_CURRENT_NODES: &[&str] = &[
    // main/current_now: actual current flowing from charger IC to system (0 when discharging).
    // main/input_current_now is intentionally excluded: it is a driver-configured
    // input limit (e.g. 2000 mA), not the actual current being drawn.
    "/sys/class/power_supply/main/current_now",
    "/sys/class/power_supply/usb/input_current_now",
    "/sys/class/power_supply/usb/current_now",
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

pub const BATTERY_REAL_SOC_NODES: &[&str] = &["/sys/class/power_supply/battery/real_soc"];

pub const CHARGE_FULL_DESIGN_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/charge_full_design",
    "/sys/class/power_supply/bms/charge_full_design",
    "/sys/class/power_supply/battery/capacity_design_uah",
];

pub const CYCLE_COUNT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/cycle_count",
    "/sys/class/power_supply/bms/cycle_count",
    "/sys/class/power_supply/main/cycle_count",
];

pub const TECHNOLOGY_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/technology",
    "/sys/class/power_supply/battery/type",
    "/sys/class/power_supply/bms/battery_type",
];

pub const AC_ONLINE_NODE: &str = "/sys/class/power_supply/ac/online";

pub const USB_ONLINE_NODE: &str = "/sys/class/power_supply/usb/online";

pub const USB_TYPEC_MODE_NODE: &str = "/sys/class/power_supply/usb/typec_mode";

pub const BATTERY_STATUS_NODE: &str = "/sys/class/power_supply/battery/status";

/// Finds the first available sysfs path.
pub fn detect_node(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
}
