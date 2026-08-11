use std::time::Instant;

use charger_core::battery::control;

use super::{
    intent::{IntentMode, OperatingIntent},
    policy::PolicyResult,
    reality::{ConnectionState, ObservedState},
    scheduler::Urgency,
};

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

/// Alasan penundaan pengisian daya berdomain terstruktur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    Disconnected,
    AttachingSettleWindow,
    SensorUnavailable,
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
    /// 3. System Safety Policy Check (via strongest_block())
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
                    if cause == BlockCause::SensorStale {
                        return Self::Wait {
                            reason: WaitReason::SensorUnavailable,
                        };
                    }
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
                reason: WaitReason::Disconnected,
            }
            | Self::Wait {
                reason: WaitReason::AttachingSettleWindow,
            } => DesiredHardwareState::NoChange,
            Self::Allow => DesiredHardwareState::ChargingEnabled,
            Self::Bypass => DesiredHardwareState::Bypass,
            Self::Block { .. } | Self::Wait { .. } => DesiredHardwareState::ChargingDisabled,
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
            Self::Wait {
                reason: WaitReason::Disconnected,
            } => Urgency::Idle,
            Self::Wait {
                reason: WaitReason::AttachingSettleWindow,
            } => Urgency::Normal,
            Self::Wait {
                reason: WaitReason::SensorUnavailable,
            }
            | Self::Block {
                cause: BlockCause::SensorStale,
            } => Urgency::Recovery,
            Self::Allow
            | Self::Bypass
            | Self::Block {
                cause: BlockCause::UserDisabled,
            } => Urgency::Normal,
        }
    }
}
