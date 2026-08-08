use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_CONFIG_PATH: &str = "/data/adb/charger-control/config.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global toggle — apakah daemon aktif
    pub enabled: bool,

    /// Batas pengisian (50–100%)
    pub charge_limit: u8,

    /// Batas resume pengisian (misal: 75% untuk resume saat limit 80%)
    pub resume_limit: u8,

    /// Aktifkan thermal cutoff
    pub thermal_cutoff: bool,

    /// Suhu maksimum sebelum charging dihentikan (°C × 10 = decidegree)
    pub max_temp_dc: i32,

    /// Hysteresis suhu untuk resume charging (default: 30 = 3°C di bawah max)
    pub thermal_resume_hysteresis_dc: i32,

    /// Path log file
    pub log_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            charge_limit: 100,
            resume_limit: 95,
            thermal_cutoff: false,
            max_temp_dc: 420,                 // 42.0°C
            thermal_resume_hysteresis_dc: 30, // 3.0°C
            log_path: PathBuf::from("/data/adb/charger-control/charger-control.log"),
        }
    }
}

impl Config {
    pub fn load(path: &PathBuf) -> Result<Self, crate::error::ChargerError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).map_err(|e| crate::error::ChargerError::ConfigRead {
                path: path.clone(),
                source: e,
            })?;
        toml::from_str(&raw).map_err(|e| crate::error::ChargerError::ConfigParse(e.to_string()))
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), crate::error::ChargerError> {
        let raw = toml::to_string_pretty(self)
            .map_err(|e| crate::error::ChargerError::ConfigSerialize(e.to_string()))?;
        std::fs::write(path, raw).map_err(|e| crate::error::ChargerError::ConfigRead {
            path: path.clone(),
            source: e,
        })
    }
}
