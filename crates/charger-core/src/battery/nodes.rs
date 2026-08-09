use std::path::Path;

pub const CHARGING_NODES: &[&str] = &["/sys/class/power_supply/battery/charging_enabled"];

pub const SUSPEND_NODES: &[&str] = &["/sys/class/power_supply/battery/input_suspend"];

pub const BATTERY_CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/current_now",
    "/sys/class/power_supply/battery/batt_current_now",
    "/sys/class/power_supply/bms/current_now",
];

pub const INPUT_CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/main/current_now",
    "/sys/class/power_supply/main/input_current_now",
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

pub const AC_ONLINE_NODE: &str = "/sys/class/power_supply/ac/online";

pub const BATTERY_STATUS_NODE: &str = "/sys/class/power_supply/battery/status";

pub const USB_TYPEC_MODE_NODE: &str = "/sys/class/power_supply/usb/typec_mode";

pub const USB_ONLINE_NODE: &str = "/sys/class/power_supply/usb/online";

pub const MAIN_CHARGING_NODE: &str = "/sys/class/power_supply/main/charging_enabled";

/// Finds the first available sysfs path.
pub fn detect_node(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
}
