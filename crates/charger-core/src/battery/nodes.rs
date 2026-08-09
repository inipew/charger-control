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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentUnit {
    MicroAmp,
    MilliAmp,
}

#[derive(Debug, Clone, Copy)]
pub struct CurrentNodeConfig {
    pub path: &'static str,
    pub unit: CurrentUnit,
    pub priority: u8,
}

pub const CURRENT_NODES: &[CurrentNodeConfig] = &[
    CurrentNodeConfig { path: "/sys/class/power_supply/battery/current_now", unit: CurrentUnit::MicroAmp, priority: 100 },
    CurrentNodeConfig { path: "/sys/class/power_supply/bms/current_now", unit: CurrentUnit::MicroAmp, priority: 90 },
    CurrentNodeConfig { path: "/sys/class/power_supply/main/current_now", unit: CurrentUnit::MicroAmp, priority: 80 },
    CurrentNodeConfig { path: "/sys/class/power_supply/battery/batt_current_now", unit: CurrentUnit::MicroAmp, priority: 70 },
    CurrentNodeConfig { path: "/sys/class/power_supply/usb/current_now", unit: CurrentUnit::MicroAmp, priority: 60 },
];

#[derive(Debug, Clone, Copy)]
pub struct OnlineNodeConfig {
    pub path: &'static str,
    pub priority: u8,
}

pub const ONLINE_NODES: &[OnlineNodeConfig] = &[
    OnlineNodeConfig { path: "/sys/class/power_supply/usb/online", priority: 100 },
    OnlineNodeConfig { path: "/sys/class/power_supply/ac/online", priority: 90 },
    OnlineNodeConfig { path: "/sys/class/power_supply/wireless/online", priority: 80 },
    OnlineNodeConfig { path: "/sys/class/power_supply/dc/online", priority: 70 },
];

/// Finds the first available sysfs path from a slice of candidates.
pub fn detect_node(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|&p| Path::new(p).exists())
}
