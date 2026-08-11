use std::time::Instant;

use charger_core::config::schema::Config;

use super::{
    decision::BlockCause,
    reality::{ObservedState, Sample},
};

pub const THERMAL_HYSTERESIS_DC: i32 = 20; // 0.2°C hysteresis standar
pub const THERMAL_EMERGENCY_OFFSET_DC: i32 = 30; // 3.0°C di atas max_temp_dc
pub const THERMAL_EMERGENCY_RELEASE_OFFSET_DC: i32 = 40; // 4.0°C di bawah max_temp_dc untuk release latch

pub const MASK_THERMAL_EMERGENCY: u16 = 1 << 0;
pub const MASK_SENSOR_STALE: u16 = 1 << 1;
pub const MASK_THERMAL: u16 = 1 << 2;
pub const MASK_CHARGE_LIMIT: u16 = 1 << 3;

/// Alasan pemblokiran pengisian daya berdasarkan bitmask u16 (ekspansibel hingga 16 kebijakan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyBlock {
    ThermalEmergency,
    SensorStale,
    Thermal,
    ChargeLimit,
}

impl PolicyBlock {
    pub const fn mask(self) -> u16 {
        match self {
            Self::ThermalEmergency => MASK_THERMAL_EMERGENCY,
            Self::SensorStale => MASK_SENSOR_STALE,
            Self::Thermal => MASK_THERMAL,
            Self::ChargeLimit => MASK_CHARGE_LIMIT,
        }
    }
}

/// Hasil evaluasi policy murni berbasis bitmask u16 (Zero Heap Allocation, Copy Trait).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PolicyResult {
    pub bits: u16,
}

impl PolicyResult {
    pub const fn clear() -> Self {
        Self { bits: 0 }
    }

    #[allow(dead_code)]
    pub const fn is_allowed(self) -> bool {
        self.bits == 0
    }

    pub const fn is_blocked_by(self, block: PolicyBlock) -> bool {
        (self.bits & block.mask()) != 0
    }

    pub fn add(&mut self, block: PolicyBlock) {
        self.bits |= block.mask();
    }

    pub fn remove(&mut self, block: PolicyBlock) {
        self.bits &= !block.mask();
    }

    /// Menentukan blokir berprioritas tertinggi untuk digunakan oleh Decision Resolver.
    /// Prioritas Keamanan: ThermalEmergency > SensorStale > Thermal > ChargeLimit
    pub fn strongest_block(self) -> Option<BlockCause> {
        if self.is_blocked_by(PolicyBlock::ThermalEmergency) {
            Some(BlockCause::ThermalEmergency)
        } else if self.is_blocked_by(PolicyBlock::SensorStale) {
            Some(BlockCause::SensorStale)
        } else if self.is_blocked_by(PolicyBlock::Thermal) {
            Some(BlockCause::Thermal)
        } else if self.is_blocked_by(PolicyBlock::ChargeLimit) {
            Some(BlockCause::ChargeLimit)
        } else {
            None
        }
    }
}

/// Menghitung evaluasi policy murni berdasarkan data pengamatan saat ini dan konfigurasi.
pub fn evaluate_policy(
    observed: &ObservedState,
    config: &Config,
    current_result: &PolicyResult,
    now: Instant,
) -> PolicyResult {
    let mut next_result = *current_result;

    // 1. Jika charger dicabut, bersihkan semua blokir policy
    if !observed.connection.is_connected() {
        return PolicyResult::clear();
    }

    // 2. Periksa kesegaran data sensor
    let sample = match observed.sample {
        Some(s) if !s.is_stale(now) => {
            next_result.remove(PolicyBlock::SensorStale);
            s
        }
        _ => {
            next_result.add(PolicyBlock::SensorStale);
            return next_result;
        }
    };

    // 3. Evaluasi Thermal Emergency Latch & Dedicated Recovery Hysteresis (Relatif terhadap config.max_temp_dc)
    let is_emergency_blocked = evaluate_thermal_emergency(
        sample,
        config,
        current_result.is_blocked_by(PolicyBlock::ThermalEmergency),
    );
    if is_emergency_blocked {
        next_result.add(PolicyBlock::ThermalEmergency);
    } else {
        next_result.remove(PolicyBlock::ThermalEmergency);
    }

    // 4. Evaluasi Thermal Policy Standar dengan Hysteresis
    let is_thermal_blocked = evaluate_thermal_block(
        sample,
        config,
        current_result.is_blocked_by(PolicyBlock::Thermal),
    );
    if is_thermal_blocked {
        next_result.add(PolicyBlock::Thermal);
    } else {
        next_result.remove(PolicyBlock::Thermal);
    }

    // 5. Evaluasi SOC Charge Limit Policy dengan Hysteresis Nyata (resume_limit atau limit - 2.0%)
    let is_limit_blocked = evaluate_limit_block(
        sample,
        config,
        current_result.is_blocked_by(PolicyBlock::ChargeLimit),
    );
    if is_limit_blocked {
        next_result.add(PolicyBlock::ChargeLimit);
    } else {
        next_result.remove(PolicyBlock::ChargeLimit);
    }

    next_result
}

fn evaluate_thermal_emergency(sample: Sample, config: &Config, currently_blocked: bool) -> bool {
    let temp_dc = (sample.temperature_c * 10.0).round() as i32;
    let emergency_dc = config.max_temp_dc + THERMAL_EMERGENCY_OFFSET_DC;
    let release_dc = config.max_temp_dc - THERMAL_EMERGENCY_RELEASE_OFFSET_DC;

    if currently_blocked {
        // Emergency Latch: Tetap terblokir sampai suhu dingin (<= max_temp_dc - 4.0°C)
        temp_dc > release_dc
    } else {
        temp_dc >= emergency_dc
    }
}

fn evaluate_thermal_block(sample: Sample, config: &Config, currently_blocked: bool) -> bool {
    let temp_dc = (sample.temperature_c * 10.0).round() as i32;
    let limit_dc = config.max_temp_dc;
    let resume_dc = limit_dc - THERMAL_HYSTERESIS_DC;

    if currently_blocked {
        temp_dc >= resume_dc
    } else {
        temp_dc >= limit_dc
    }
}

fn evaluate_limit_block(sample: Sample, config: &Config, currently_blocked: bool) -> bool {
    let limit_soc = config.charge_limit as f32;
    if limit_soc <= 0.0 {
        return false;
    }

    let current_soc = sample.capacity;

    let resume_soc = if config.resume_limit > 0 && (config.resume_limit as f32) < limit_soc {
        config.resume_limit as f32
    } else {
        (limit_soc - 2.0).max(0.0)
    };

    if currently_blocked {
        current_soc >= resume_soc
    } else {
        current_soc >= limit_soc
    }
}
