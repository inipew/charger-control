use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_CONFIG_PATH: &str = "/data/adb/charger-control/config.toml";
pub const DEFAULT_PID_PATH: &str = "/data/adb/charger-control/daemon.pid";
pub const DEFAULT_LOCK_PATH: &str = "/data/adb/charger-control/daemon.lock";
pub const DEFAULT_SOCKET_PATH: &str = "/data/adb/charger-control/daemon.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global toggle — apakah daemon aktif
    pub enabled: bool,

    /// Batas maksimum pengisian (50–100%)
    pub charge_limit: u8,

    /// Batas resume pengisian.
    ///
    /// 0 = tidak menggunakan resume limit.
    ///
    /// Contoh:
    /// charge_limit = 100
    /// resume_limit = 95
    ///
    /// Charging:
    /// 100% -> OFF
    /// 99%  -> tetap OFF
    /// 98%  -> tetap OFF
    /// ...
    /// 95%  -> ON kembali
    pub resume_limit: u8,

    /// Aktifkan thermal cutoff
    pub thermal_cutoff: bool,

    /// Suhu maksimum sebelum charging dihentikan
    /// 420 = 42.0°C
    pub max_temp_dc: i32,

    /// Polling interval monitor loop (detik)
    pub poll_interval_secs: u64,

    /// Path log file
    pub log_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            charge_limit: 100,

            // 0 = tidak ada resume hysteresis.
            resume_limit: 0,

            thermal_cutoff: false,
            max_temp_dc: 420,
            poll_interval_secs: 10,

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

        std::fs::write(path, raw).map_err(|e| crate::error::ChargerError::ConfigWrite {
            path: path.clone(),
            source: e,
        })
    }
}
