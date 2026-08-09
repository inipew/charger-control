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

#[derive(Debug, Clone, Copy)]
pub struct OnlineNodeConfig {
    pub path: &'static str,
    pub priority: u8,
}

pub struct HardwareCapabilities {
    pub charging_control: bool,
    pub input_suspend: bool,
    pub current_measurement: bool,
    pub temperature: bool,
}

pub struct HardwareProfile {
    pub name: &'static str,
    pub capabilities: HardwareCapabilities,

    pub charging_nodes: &'static [&'static str],
    pub suspend_nodes: &'static [&'static str],
    pub current_nodes: &'static [CurrentNodeConfig],
    pub online_nodes: &'static [OnlineNodeConfig],

    pub capacity_path: &'static str,
    pub temperature_path: &'static str,
    pub status_path: &'static str,
}

pub const GENERIC_PROFILE: HardwareProfile = HardwareProfile {
    name: "generic",
    capabilities: HardwareCapabilities {
        charging_control: true,
        input_suspend: true,
        current_measurement: true,
        temperature: true,
    },
    charging_nodes: &[
        "/sys/class/power_supply/battery/charging_enabled",
        "/sys/class/power_supply/main/charging_enabled",
        "/sys/class/power_supply/battery/battery_charging_enabled",
    ],
    suspend_nodes: &[
        "/sys/class/power_supply/battery/input_suspend",
        "/sys/class/power_supply/usb/input_suspend",
    ],
    current_nodes: &[
        CurrentNodeConfig { path: "/sys/class/power_supply/battery/current_now", unit: CurrentUnit::MicroAmp, priority: 100 },
        CurrentNodeConfig { path: "/sys/class/power_supply/bms/current_now", unit: CurrentUnit::MicroAmp, priority: 90 },
        CurrentNodeConfig { path: "/sys/class/power_supply/main/current_now", unit: CurrentUnit::MicroAmp, priority: 80 },
        CurrentNodeConfig { path: "/sys/class/power_supply/battery/batt_current_now", unit: CurrentUnit::MicroAmp, priority: 70 },
        CurrentNodeConfig { path: "/sys/class/power_supply/usb/current_now", unit: CurrentUnit::MicroAmp, priority: 60 },
    ],
    online_nodes: &[
        OnlineNodeConfig { path: "/sys/class/power_supply/usb/online", priority: 100 },
        OnlineNodeConfig { path: "/sys/class/power_supply/ac/online", priority: 90 },
        OnlineNodeConfig { path: "/sys/class/power_supply/wireless/online", priority: 80 },
        OnlineNodeConfig { path: "/sys/class/power_supply/dc/online", priority: 70 },
    ],
    capacity_path: "/sys/class/power_supply/battery/capacity",
    temperature_path: "/sys/class/power_supply/battery/temp",
    status_path: "/sys/class/power_supply/battery/status",
};
