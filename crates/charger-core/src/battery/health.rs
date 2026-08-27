use crate::{battery::reader::read_sysfs, error::ChargerError};
use std::{fmt, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryHealth {
    Good,
    Overheat,
    Dead,
    OverVoltage,
    Unspecified,
    Cold,
    Unknown,
}

impl BatteryHealth {
    pub fn from_sysfs(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "good" => Self::Good,
            "overheat" => Self::Overheat,
            "dead" => Self::Dead,
            "over voltage" | "overvoltage" => Self::OverVoltage,
            "cold" => Self::Cold,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for BatteryHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Good => write!(f, "Good"),
            Self::Overheat => write!(f, "Overheat ⚠"),
            Self::Dead => write!(f, "Dead ☠"),
            Self::OverVoltage => write!(f, "Over Voltage ⚡"),
            Self::Cold => write!(f, "Cold 🧊"),
            Self::Unknown | Self::Unspecified => write!(f, "Unknown"),
        }
    }
}

pub const BATTERY_HEALTH_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/health",
    "/sys/class/power_supply/bms/health",
];

pub fn read_health() -> Result<BatteryHealth, ChargerError> {
    for &path_str in BATTERY_HEALTH_NODES {
        let path = Path::new(path_str);
        if let Ok(raw) = read_sysfs(path) {
            return Ok(BatteryHealth::from_sysfs(&raw));
        }
    }
    Ok(BatteryHealth::Unknown)
}
