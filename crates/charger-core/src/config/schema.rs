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
    /// Validasi dan sanitasi nilai konfigurasi agar aman dari nilai tidak valid.
    pub fn validate(&mut self) {
        // Clamp charge_limit antara 50% hingga 100%
        self.charge_limit = self.charge_limit.clamp(50, 100);

        // Normalisasi resume_limit:
        // - resume_limit = 0 (tidak dikonfigurasi) → fallback ke charge_limit - 2
        // - resume_limit >= charge_limit (invalid) → clamp ke charge_limit - 1
        // - resume_limit < 1 → minimum 1
        //
        // Invariant setelah validate(): 0 < resume_limit < charge_limit
        if self.resume_limit == 0 || self.resume_limit >= self.charge_limit {
            self.resume_limit = self.charge_limit.saturating_sub(2).max(1);
        }

        // Maximum temperature dc (30.0°C hingga 60.0°C → 300 hingga 600)
        self.max_temp_dc = self.max_temp_dc.clamp(300, 600);

        // Polling interval antara 1 hingga 300 detik
        self.poll_interval_secs = self.poll_interval_secs.clamp(1, 300);
    }

    pub fn load(path: &PathBuf) -> Result<Self, crate::error::ChargerError> {
        if !path.exists() {
            let mut cfg = Self::default();
            cfg.validate();
            return Ok(cfg);
        }

        let raw =
            std::fs::read_to_string(path).map_err(|e| crate::error::ChargerError::ConfigRead {
                path: path.clone(),
                source: e,
            })?;

        let mut config: Self = toml::from_str(&raw)
            .map_err(|e| crate::error::ChargerError::ConfigParse(e.to_string()))?;
        config.validate();
        Ok(config)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation_clamping() {
        let mut cfg = Config {
            enabled: true,
            charge_limit: 150, // invalid > 100
            resume_limit: 98,
            thermal_cutoff: true,
            max_temp_dc: 800,      // invalid > 600
            poll_interval_secs: 0, // invalid < 1
            log_path: PathBuf::from("/tmp/test.log"),
        };

        cfg.validate();
        assert_eq!(cfg.charge_limit, 100);
        assert_eq!(cfg.resume_limit, 98); // 98 < 100 → valid, tidak diubah
        assert_eq!(cfg.max_temp_dc, 600);
        assert_eq!(cfg.poll_interval_secs, 1);

        // resume_limit >= charge_limit → dinormalisasi ke charge_limit - 2
        let mut invalid_resume = Config {
            charge_limit: 80,
            resume_limit: 85, // resume_limit > charge_limit
            ..Config::default()
        };
        invalid_resume.validate();
        assert_eq!(invalid_resume.charge_limit, 80);
        assert_eq!(invalid_resume.resume_limit, 78);

        // resume_limit = 0 (tidak dikonfigurasi) → fallback ke charge_limit - 2
        let mut zero_resume = Config {
            charge_limit: 80,
            resume_limit: 0,
            ..Config::default()
        };
        zero_resume.validate();
        assert_eq!(zero_resume.resume_limit, 78);

        // resume_limit == charge_limit → invalid, normalisasi ke charge_limit - 2
        let mut equal_resume = Config {
            charge_limit: 80,
            resume_limit: 80,
            ..Config::default()
        };
        equal_resume.validate();
        assert_eq!(equal_resume.resume_limit, 78);

        // resume_limit > charge_limit → sama seperti >=
        let mut above_resume = Config {
            charge_limit: 80,
            resume_limit: 105,
            ..Config::default()
        };
        above_resume.validate();
        assert_eq!(above_resume.resume_limit, 78);
    }
}
