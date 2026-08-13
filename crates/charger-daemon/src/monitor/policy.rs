use std::time::{Duration, Instant};

use charger_core::config::schema::Config;

use super::{
    decision::BlockCause,
    reality::{ObservedState, Sample},
};

pub const THERMAL_HYSTERESIS_DC: i32 = 20; // 2.0°C hysteresis standar
pub const THERMAL_EMERGENCY_OFFSET_DC: i32 = 30; // 3.0°C di atas max_temp_dc
pub const THERMAL_EMERGENCY_RELEASE_OFFSET_DC: i32 = 40; // 4.0°C di bawah max_temp_dc untuk release latch

pub const CHARGE_LIMIT_SUSPEND_DELAY: Duration = Duration::from_secs(5 * 60);

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

/// Temporal state terpisah dari PolicyResult (pure bitmask).
/// Menyimpan informasi waktu yang diperlukan policy tanpa mencemari PolicyResult.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyRuntime {
    /// Start of the current continuous interval where SOC >= charge_limit.
    pub charge_limit_grace_started_at: Option<Instant>,
}

impl PolicyRuntime {
    pub fn clear(&mut self) {
        self.charge_limit_grace_started_at = None;
    }

    /// Deadline kapan charge-limit grace period berakhir.
    /// Digunakan sebagai earliest wake-up target; actual evaluation
    /// may occur slightly after the deadline due to polling granularity.
    pub fn charge_limit_deadline(&self) -> Option<Instant> {
        self.charge_limit_grace_started_at
            .map(|t| t + CHARGE_LIMIT_SUSPEND_DELAY)
    }
}

/// Menghitung evaluasi policy murni berdasarkan data pengamatan saat ini dan konfigurasi.
pub fn evaluate_policy(
    observed: &ObservedState,
    config: &Config,
    current_result: &PolicyResult,
    runtime: &mut PolicyRuntime,
    now: Instant,
) -> PolicyResult {
    let mut next_result = *current_result;

    // 1. Jika charger dicabut, bersihkan semua blokir policy
    if !observed.connection.is_connected() {
        runtime.clear();
        return PolicyResult::clear();
    }

    // 2. Periksa kesegaran data sensor
    let sample = match observed.sample {
        Some(s) if !s.is_stale(now) => {
            next_result.remove(PolicyBlock::SensorStale);
            s
        }
        _ => {
            // Sensor stale: timer charge-limit harus reset karena
            // daemon kehilangan observability — tidak boleh mengklaim
            // "SOC >= limit selama 5 menit" tanpa data segar.
            runtime.charge_limit_grace_started_at = None;
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

    // 5. Evaluasi SOC Charge Limit Policy dengan Grace Period + Hysteresis
    let (is_limit_blocked, new_reached_at) = evaluate_limit_block(
        sample,
        config,
        current_result.is_blocked_by(PolicyBlock::ChargeLimit),
        runtime.charge_limit_grace_started_at,
        now,
    );
    runtime.charge_limit_grace_started_at = new_reached_at;
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

/// Evaluasi SOC Charge Limit dengan Grace Period temporal.
///
/// Semantik:
/// - Sudah blocked → pertahankan selama SOC >= resume_soc (hysteresis).
/// - Belum blocked, SOC < limit → timer reset.
/// - Belum blocked, SOC >= limit → catat reached_at, block hanya setelah grace period (5 menit).
fn evaluate_limit_block(
    sample: Sample,
    config: &Config,
    currently_blocked: bool,
    reached_at: Option<Instant>,
    now: Instant,
) -> (bool, Option<Instant>) {
    let limit_soc = config.charge_limit as f32;
    if limit_soc <= 0.0 {
        return (false, None);
    }

    let current_soc = sample.capacity;

    let resume_soc = if config.resume_limit > 0 && (config.resume_limit as f32) < limit_soc {
        config.resume_limit as f32
    } else {
        (limit_soc - 2.0).max(0.0)
    };

    // Sudah blocked: pertahankan selama SOC >= resume_soc (hysteresis standar)
    if currently_blocked {
        if current_soc >= resume_soc {
            return (true, reached_at);
        }
        // SOC turun di bawah resume → unblock dan reset timer
        return (false, None);
    }

    // Belum blocked: SOC belum mencapai limit
    if current_soc < limit_soc {
        return (false, None);
    }

    // SOC >= limit: mulai/lanjutkan timer grace period
    let first_reached = reached_at.unwrap_or(now);
    let elapsed = now.duration_since(first_reached);

    if elapsed >= CHARGE_LIMIT_SUSPEND_DELAY {
        // Grace period selesai → aktifkan ChargeLimit block
        (true, Some(first_reached))
    } else {
        // Masih dalam grace period → belum block, timer tetap berjalan
        (false, Some(first_reached))
    }
}

