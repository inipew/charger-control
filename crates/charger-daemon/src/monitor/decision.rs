use std::time::Instant;

use charger_core::{battery::control, config::schema::Config};

use super::{
    intent::{IntentMode, OperatingIntent},
    policy::{ChargeLimitState, PolicyResult, PolicyRuntime, ThermalStep, GRACE_CURRENT_CAP_UA},
    reality::{ConnectionState, ObservedState},
    scheduler::Urgency,
};

/// Domain keputusan regulasi batas arus pengisian daya.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentRegulation {
    Unconstrained,
    ConfigLimit { target_ua: u32 },
    ThermalThrottle { step: u8, target_ua: u32 },
    GraceCap { target_ua: u32 },
    Disabled,
}

impl CurrentRegulation {
    pub const fn target_ua(&self) -> Option<u32> {
        match self {
            Self::Unconstrained | Self::Disabled => None,
            Self::ConfigLimit { target_ua }
            | Self::ThermalThrottle { target_ua, .. }
            | Self::GraceCap { target_ua } => Some(*target_ua),
        }
    }
}

/// Menghitung batas arus pengisian daya berdasarkan hierarki otoritas murni:
/// 1. Binary Block / Disabled / Disconnected -> Disabled (None)
/// 2. Arbitrasi Multi-Constraint: Batas arus efektif adalah nilai MINIMUM dari seluruh constraint aktif:
///    - Stepped Thermal Throttling (Step 1: 2.5A, Step 2: 1.5A, Step 3: 0.8A)
///    - Grace Top-Off Cap (1.0A)
///    - User Config Limit (config.max_charge_current_ma)
/// 3. Jika tidak ada constraint yang aktif -> Unconstrained
pub fn resolve_current_regulation(
    config: &Config,
    policy_runtime: &PolicyRuntime,
    decision: &ChargingDecision,
) -> CurrentRegulation {
    // 1. Jika sakelar biner tidak mengizinkan pengisian daya (Block / Wait / Bypass), regulasi arus dinonaktifkan
    if !matches!(decision, ChargingDecision::Allow) {
        return CurrentRegulation::Disabled;
    }

    let user_limit_ua = if config.max_charge_current_ma > 0 {
        Some(config.max_charge_current_ma.max(500) * 1000)
    } else {
        None
    };

    let thermal_limit_ua = if config.thermal_throttling_enabled
        && policy_runtime.thermal_step != ThermalStep::Normal
    {
        policy_runtime.thermal_step.target_ua()
    } else {
        None
    };

    let is_grace = matches!(
        policy_runtime.charge_limit_state,
        ChargeLimitState::Grace { .. }
    );
    let grace_limit_ua = if is_grace {
        Some(GRACE_CURRENT_CAP_UA)
    } else {
        None
    };

    // Evaluasi seluruh constraint aktif untuk mencari batas arus paling ketat (minimum)
    let mut effective_limit_ua: Option<u32> = None;
    for &limit in &[user_limit_ua, thermal_limit_ua, grace_limit_ua] {
        if let Some(val) = limit {
            effective_limit_ua = Some(effective_limit_ua.map_or(val, |cur| cur.min(val)));
        }
    }

    let Some(target_ua) = effective_limit_ua else {
        return CurrentRegulation::Unconstrained;
    };

    // Klasifikasikan domain keputusan berdasarkan constraint utama yang membatasi:
    // A. Jika Thermal Throttling aktif dan membatasi lebih ketat atau sama dengan Grace & User
    if let Some(thermal_ua) = thermal_limit_ua {
        if thermal_ua <= target_ua || (grace_limit_ua.is_none() && user_limit_ua.is_none()) {
            return CurrentRegulation::ThermalThrottle {
                step: policy_runtime.thermal_step.level(),
                target_ua,
            };
        }
    }

    // B. Jika Grace Top-Off Cap aktif dan membatasi lebih ketat dari Thermal
    if is_grace {
        return CurrentRegulation::GraceCap { target_ua };
    }

    // C. Jika Thermal Throttling aktif (misal dengan User Limit lebih rendah dari Thermal Step)
    if policy_runtime.thermal_step != ThermalStep::Normal && config.thermal_throttling_enabled {
        return CurrentRegulation::ThermalThrottle {
            step: policy_runtime.thermal_step.level(),
            target_ua,
        };
    }

    // D. User Config Limit
    CurrentRegulation::ConfigLimit { target_ua }
}

/// Status hardware yang diinginkan oleh Decision Resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredHardwareState {
    NoChange,
    ChargingEnabled,
    ChargingDisabled,
    Bypass,
}

impl DesiredHardwareState {
    pub fn hardware_mode(self, has_distinct_bypass: bool) -> control::ActualHardwareMode {
        match self {
            Self::NoChange => control::ActualHardwareMode::Unknown,
            Self::ChargingEnabled => control::ActualHardwareMode::ChargingEnabled,
            Self::ChargingDisabled => control::ActualHardwareMode::ChargingDisabled,
            Self::Bypass if has_distinct_bypass => control::ActualHardwareMode::Bypass,
            Self::Bypass => control::ActualHardwareMode::ChargingDisabled,
        }
    }
}

/// Alasan pemblokiran pengisian daya berdomain terstruktur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCause {
    ThermalEmergency,
    Thermal,
    ChargeLimit,
    SensorStale,
    UserDisabled,
}

/// Alasan penundaan pengisian daya berdomain terstruktur (hanya transisi koneksi fisik).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    Disconnected,
    AttachingSettleWindow,
}

/// Decision Resolver terstruktur tanpa alokasi heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargingDecision {
    Allow,
    Block { cause: BlockCause },
    Bypass,
    Wait { reason: WaitReason },
}

impl ChargingDecision {
    /// Menyelesaikan keputusan pengisian daya berdasarkan hirarki otoritas murni:
    /// 1. Connection check (Disconnect -> Wait(Disconnected))
    /// 2. Attaching window check (Attaching -> Wait(AttachingSettleWindow))
    /// 3. System Safety Policy Check (via strongest_block()) -> Block(Cause)
    /// 4. User/Operating Intent check (Disabled / Bypass / Normal)
    ///
    /// Hardware fault/recovery sengaja tidak termasuk dalam decision domain.
    /// Hardware actuator direkonsiliasi secara terpisah oleh `hardware::reconcile`.
    pub fn resolve(
        observed: &ObservedState,
        intent: &OperatingIntent,
        policy: &PolicyResult,
        now: Instant,
    ) -> Self {
        match observed.connection {
            ConnectionState::Disconnected => Self::Wait {
                reason: WaitReason::Disconnected,
            },
            ConnectionState::Attaching { .. } => Self::Wait {
                reason: WaitReason::AttachingSettleWindow,
            },
            ConnectionState::Attached => {
                // 1. Safety SELALU menang di atas Hardware Fault dan User Intent
                if let Some(cause) = policy.strongest_block() {
                    return Self::Block { cause };
                }

                // 2. Evaluasi Operating Intent (Disabled vs Bypass vs Normal)
                match intent.current_mode(now) {
                    IntentMode::Disabled => Self::Block {
                        cause: BlockCause::UserDisabled,
                    },
                    IntentMode::Bypass => Self::Bypass,
                    IntentMode::Normal => Self::Allow,
                }
            }
        }
    }

    /// Mengonversi keputusan domain ke status fisik hardware yang diinginkan.
    pub fn to_desired_hardware(&self) -> DesiredHardwareState {
        match self {
            Self::Wait {
                reason: WaitReason::Disconnected | WaitReason::AttachingSettleWindow,
            } => DesiredHardwareState::NoChange,
            Self::Allow => DesiredHardwareState::ChargingEnabled,
            Self::Bypass => DesiredHardwareState::Bypass,
            Self::Block { .. } => DesiredHardwareState::ChargingDisabled,
        }
    }

    /// Mengonversi keputusan domain langsung ke tingkat Urgensi polling scheduler.
    pub fn to_urgency(&self) -> Urgency {
        match self {
            Self::Block {
                cause: BlockCause::ThermalEmergency | BlockCause::Thermal,
            } => Urgency::Safety,
            Self::Block {
                cause: BlockCause::ChargeLimit,
            } => Urgency::Monitoring,
            Self::Block {
                cause: BlockCause::SensorStale,
            } => Urgency::Recovery,
            Self::Wait {
                reason: WaitReason::Disconnected,
            } => Urgency::Idle,
            Self::Wait {
                reason: WaitReason::AttachingSettleWindow,
            } => Urgency::Normal,
            Self::Allow
            | Self::Bypass
            | Self::Block {
                cause: BlockCause::UserDisabled,
            } => Urgency::Normal,
        }
    }
}
