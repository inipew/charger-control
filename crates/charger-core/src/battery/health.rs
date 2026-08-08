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

pub fn read_health() -> Result<BatteryHealth, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/health");
    if let Ok(raw) = read_sysfs(path) {
        Ok(BatteryHealth::from_sysfs(&raw))
    } else {
        Ok(BatteryHealth::Unknown)
    }
}
