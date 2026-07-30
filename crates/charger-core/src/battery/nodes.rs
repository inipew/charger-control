use std::path::Path;

pub const CHARGING_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/charging_enabled",
    "/sys/class/power_supply/main/charging_enabled",
    "/sys/class/power_supply/battery/battery_charging_enabled",
];

pub const SUSPEND_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/input_suspend",
    "/sys/class/power_supply/usb/input_suspend",
];

pub const CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/current_now",
    "/sys/class/power_supply/bms/current_now",
    "/sys/class/power_supply/main/current_now",
    "/sys/class/power_supply/battery/batt_current_now",
    "/sys/class/power_supply/usb/current_now",
];

/// Finds the first available sysfs path from a slice of candidates.
pub fn detect_node(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|&p| Path::new(p).exists())
}
