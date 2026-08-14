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

pub const THERMAL_STEP1_TEMP_DC: i32 = 380; // 38.0°C
pub const THERMAL_STEP1_CURRENT_UA: u32 = 2_500_000; // 2.5A

pub const THERMAL_STEP2_TEMP_DC: i32 = 410; // 41.0°C
pub const THERMAL_STEP2_CURRENT_UA: u32 = 1_500_000; // 1.5A

pub const THERMAL_STEP3_TEMP_DC: i32 = 430; // 43.0°C
pub const THERMAL_STEP3_CURRENT_UA: u32 = 800_000; // 800mA

pub const THERMAL_STEP_HOLD_WINDOW: Duration = Duration::from_secs(10);
pub const GRACE_CURRENT_CAP_UA: u32 = 1_000_000; // 1.0A

/// Step regulasi termal bertingkat (Stepped Thermal Throttling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThermalStep {
    #[default]
    Normal,
    Step1,
    Step2,
    Step3,
}

impl ThermalStep {
    pub const fn target_ua(self) -> Option<u32> {
        match self {
            Self::Normal => None,
            Self::Step1 => Some(THERMAL_STEP1_CURRENT_UA),
            Self::Step2 => Some(THERMAL_STEP2_CURRENT_UA),
            Self::Step3 => Some(THERMAL_STEP3_CURRENT_UA),
        }
    }

    pub const fn level(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Step1 => 1,
            Self::Step2 => 2,
            Self::Step3 => 3,
        }
    }
}

/// State machine eksplisit untuk charge-limit lifecycle.
///
/// ```text
/// NORMAL
///   │ SOC >= charge_limit
///   ▼
/// GRACE(started_at)
///   │ SOC < limit → NORMAL (timer reset)
///   │ elapsed >= 5m → SUSPENDED
///   ▼
/// SUSPENDED
///   │ SOC <= resume_limit → NORMAL
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChargeLimitState {
    /// SOC di bawah charge_limit; tidak ada timer aktif.
    #[default]
    Normal,

    /// SOC >= charge_limit; menunggu grace period sebelum suspend.
    Grace { started_at: Instant },

    /// Grace period habis; charging diblokir hingga SOC <= resume_limit.
    Suspended,
}

/// Temporal state terpisah dari PolicyResult (pure bitmask).
/// Menyimpan state machine charge-limit dan thermal stepping tanpa mencemari PolicyResult.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyRuntime {
    pub charge_limit_state: ChargeLimitState,
    pub thermal_step: ThermalStep,
    pub thermal_step_updated_at: Option<Instant>,
}

impl PolicyRuntime {
    pub fn clear(&mut self) {
        self.charge_limit_state = ChargeLimitState::Normal;
        self.thermal_step = ThermalStep::Normal;
        self.thermal_step_updated_at = None;
    }

    /// Deadline kapan charge-limit grace period berakhir.
    /// Hanya relevan saat state = Grace. Saat Suspended, grace timer sudah tidak aktif.
    /// Digunakan sebagai earliest wake-up target oleh scheduler.
    pub fn charge_limit_deadline(&self) -> Option<Instant> {
        match self.charge_limit_state {
            ChargeLimitState::Grace { started_at } => Some(started_at + CHARGE_LIMIT_SUSPEND_DELAY),
            // Suspended: tidak ada deadline timer aktif.
            // Normal: tidak relevan.
            _ => None,
        }
    }
}

/// Evaluasi Stepped Thermal Throttling dengan Hold Hysteresis (10 detik).
pub fn evaluate_thermal_stepping(
    temp_dc: i32,
    config: &Config,
    runtime: &mut PolicyRuntime,
    now: Instant,
) -> ThermalStep {
    if !config.thermal_throttling_enabled {
        runtime.thermal_step = ThermalStep::Normal;
        return ThermalStep::Normal;
    }

    let desired_step = if temp_dc >= THERMAL_STEP3_TEMP_DC {
        ThermalStep::Step3
    } else if temp_dc >= THERMAL_STEP2_TEMP_DC {
        ThermalStep::Step2
    } else if temp_dc >= THERMAL_STEP1_TEMP_DC {
        ThermalStep::Step1
    } else {
        ThermalStep::Normal
    };

    if desired_step != runtime.thermal_step {
        // Step up (more severe throttling) occurs immediately for safety.
        // Step down (recovering to higher current) requires step hold window to avoid oscillating.
        let is_step_up = desired_step.level() > runtime.thermal_step.level();
        let hold_expired = runtime
            .thermal_step_updated_at
            .is_none_or(|t| now.duration_since(t) >= THERMAL_STEP_HOLD_WINDOW);

        if is_step_up || hold_expired {
            runtime.thermal_step = desired_step;
            runtime.thermal_step_updated_at = Some(now);
        }
    }

    runtime.thermal_step
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

    // 1. Jika charger dicabut, bersihkan semua blokir policy.
    // - Jika dalam Grace: batalkan grace timer ke Normal (karena charging terputus fisik).
    // - Jika dalam Suspended: PERTAHANKAN state agar persist melewati siklus detach/attach
    //   sehingga re-attach dengan SOC masih di atas resume_limit tetap diblokir.
    if !observed.connection.is_connected() {
        if matches!(runtime.charge_limit_state, ChargeLimitState::Grace { .. }) {
            runtime.charge_limit_state = ChargeLimitState::Normal;
        }
        return PolicyResult::clear();
    }

    // 2. Periksa kesegaran data sensor
    let sample = match observed.sample {
        Some(s) if !s.is_stale(now) => {
            next_result.remove(PolicyBlock::SensorStale);
            s
        }
        _ => {
            // Sensor stale: reset charge-limit state karena daemon kehilangan
            // observability — tidak boleh mengklaim "SOC >= limit selama 5 menit"
            // tanpa data segar.
            runtime.clear();
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
    let is_limit_blocked = evaluate_limit_block(sample, config, runtime, now);
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

/// Evaluasi SOC Charge Limit dengan state machine eksplisit.
///
/// Semantik:
/// - `Normal`:    SOC < limit → Normal. SOC >= limit → Grace(now).
/// - `Grace`:     SOC < limit → Normal (reset). elapsed >= 5m → Suspended. Else → Grace.
/// - `Suspended`: SOC > resume_soc → Suspended. SOC <= resume_soc → Normal.
///
/// Boundary resume: `SOC <= resume_soc` → unblock (inklusif di resume_limit).
fn evaluate_limit_block(
    sample: Sample,
    config: &Config,
    runtime: &mut PolicyRuntime,
    now: Instant,
) -> bool {
    let limit_soc = config.charge_limit as f32;
    if limit_soc <= 0.0 {
        runtime.charge_limit_state = ChargeLimitState::Normal;
        return false;
    }

    let current_soc = sample.capacity;

    let resume_soc = if config.resume_limit > 0 && (config.resume_limit as f32) < limit_soc {
        config.resume_limit as f32
    } else {
        (limit_soc - 2.0).max(0.0)
    };

    match runtime.charge_limit_state {
        ChargeLimitState::Suspended => {
            if current_soc > resume_soc {
                // SOC masih di atas resume_limit → tetap Suspended
                true
            } else {
                // SOC <= resume_limit: Suspended → Normal, charging kembali diizinkan
                runtime.charge_limit_state = ChargeLimitState::Normal;
                false
            }
        }

        ChargeLimitState::Grace { started_at } => {
            if current_soc < limit_soc {
                // SOC turun di bawah limit → Grace → Normal, timer reset
                runtime.charge_limit_state = ChargeLimitState::Normal;
                false
            } else if now.duration_since(started_at) >= CHARGE_LIMIT_SUSPEND_DELAY {
                // Grace period habis → Grace → Suspended
                runtime.charge_limit_state = ChargeLimitState::Suspended;
                true
            } else {
                // Masih dalam grace period → tetap Grace, belum block
                false
            }
        }

        ChargeLimitState::Normal => {
            if current_soc >= limit_soc {
                // Baru mencapai limit → Normal → Grace
                runtime.charge_limit_state = ChargeLimitState::Grace { started_at: now };
            }
            // State Normal tidak pernah menghasilkan block
            false
        }
    }
}
