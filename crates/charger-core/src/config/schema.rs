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

    /// Batas resume pengisian (dalam persentase SOC).
    ///
    /// Nilai `0` = mode otomatis / auto hysteresis (`charge_limit - 2`).
    ///
    /// Contoh:
    /// charge_limit = 100
    /// resume_limit = 0  -> auto resume di 98%
    ///
    /// Jika dikonfigurasi eksplisit (misal 95):
    /// 100% -> OFF
    /// 99%  -> tetap OFF
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

    /// Batas arus maksimum pengisian daya (dalam mA).
    /// 0 = Tidak dibatasi (kecepatan bawaan charger/hardware).
    pub max_charge_current_ma: u32,

    /// Aktifkan regulasi termal bertingkat adaptif (Stepped Thermal Throttling)
    pub thermal_throttling_enabled: bool,

    /// Paksa aktifkan fast charging / USB-PD bypass pada charger non-OEM
    pub fast_charge: bool,

    /// Batas maksimum persentase baterai untuk aktivasi fast charge bypass (default: 90%)
    pub fast_charge_max_soc: u8,

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
            max_charge_current_ma: 0,
            thermal_throttling_enabled: true,
            fast_charge: true,
            fast_charge_max_soc: 90,

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

        // Batas arus: 0 = unconstrained, jika > 0 maka di-clamp antara 500 mA s/d 10000 mA (10A)
        if self.max_charge_current_ma > 0 {
            self.max_charge_current_ma = self.max_charge_current_ma.clamp(500, 10000);
        }

        // Clamp fast_charge_max_soc antara 50% hingga 100%
        self.fast_charge_max_soc = self.fast_charge_max_soc.clamp(50, 100);
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
            ..Config::default()
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

        // max_charge_current_ma: 0 remains 0, < 500 clamped to 500, > 10000 clamped to 10000
        let mut current_cfg = Config {
            max_charge_current_ma: 200, // < 500
            ..Config::default()
        };
        current_cfg.validate();
        assert_eq!(current_cfg.max_charge_current_ma, 500);

        let mut current_high = Config {
            max_charge_current_ma: 25000, // > 10000
            ..Config::default()
        };
        current_high.validate();
        assert_eq!(current_high.max_charge_current_ma, 10000);

        let mut current_zero = Config {
            max_charge_current_ma: 0,
            ..Config::default()
        };
        current_zero.validate();
        assert_eq!(current_zero.max_charge_current_ma, 0);
    }

    #[test]
    fn test_config_toml_roundtrip() {
        let original = Config {
            enabled: true,
            charge_limit: 85,
            resume_limit: 80,
            thermal_cutoff: true,
            max_temp_dc: 450,
            poll_interval_secs: 5,
            max_charge_current_ma: 1800,
            thermal_throttling_enabled: true,
            fast_charge: true,
            fast_charge_max_soc: 90,
            log_path: PathBuf::from("/data/adb/test.log"),
        };

        let toml_str = toml::to_string(&original).expect("Serialization failed");
        let mut parsed: Config = toml::from_str(&toml_str).expect("Deserialization failed");
        parsed.validate();

        assert_eq!(parsed.charge_limit, 85);
        assert_eq!(parsed.resume_limit, 80);
        assert_eq!(parsed.max_charge_current_ma, 1800);
        assert!(parsed.thermal_throttling_enabled);
        assert!(parsed.thermal_cutoff);
        assert!(parsed.fast_charge);
        assert_eq!(parsed.fast_charge_max_soc, 90);
    }

    #[test]
    fn test_config_backwards_compatibility_missing_fields() {
        let legacy_toml = r#"
            enabled = true
            charge_limit = 90
            resume_limit = 85
            thermal_cutoff = false
            max_temp_dc = 400
            poll_interval_secs = 10
            log_path = "/data/adb/test.log"
        "#;

        let mut parsed: Config = toml::from_str(legacy_toml).expect("Legacy TOML parsing failed");
        parsed.validate();

        assert_eq!(parsed.charge_limit, 90);
        assert_eq!(parsed.resume_limit, 85);
        // Default values for new fields must be used
        assert_eq!(parsed.max_charge_current_ma, 0); // Unconstrained
        assert!(parsed.thermal_throttling_enabled); // Default true
    }

    #[test]
    fn test_config_extreme_boundary_clamping() {
        let mut lower_boundary = Config {
            charge_limit: 10,         // < 50 -> clamp to 50
            resume_limit: 5,          // < 50 -> clamp to 50 - 2 = 48
            max_temp_dc: 100,         // < 300 -> clamp to 300 (30.0 C)
            poll_interval_secs: 0,    // < 1 -> clamp to 1
            max_charge_current_ma: 1, // 1 < 500 -> clamp to 500
            ..Config::default()
        };
        lower_boundary.validate();
        assert_eq!(lower_boundary.charge_limit, 50);
        assert_eq!(lower_boundary.resume_limit, 5); // 5 < 50 is valid!
        assert_eq!(lower_boundary.max_temp_dc, 300);
        assert_eq!(lower_boundary.poll_interval_secs, 1);
        assert_eq!(lower_boundary.max_charge_current_ma, 500);

        let mut upper_boundary = Config {
            charge_limit: 250,             // > 100 -> clamp to 100
            resume_limit: 240,             // > 100 -> clamp to 100 - 2 = 98
            max_temp_dc: 9999,             // > 600 -> clamp to 600 (60.0 C)
            poll_interval_secs: 999,       // > 300 -> clamp to 300
            max_charge_current_ma: 999999, // > 10000 -> clamp to 10000
            ..Config::default()
        };
        upper_boundary.validate();
        assert_eq!(upper_boundary.charge_limit, 100);
        assert_eq!(upper_boundary.resume_limit, 98);
        assert_eq!(upper_boundary.max_temp_dc, 600);
        assert_eq!(upper_boundary.poll_interval_secs, 300);
        assert_eq!(upper_boundary.max_charge_current_ma, 10000);
    }
}
