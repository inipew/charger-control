use super::hardware::HardwareTarget;
use super::snapshot::SensorSnapshot;
use charger_core::config::schema::Config;
use std::fmt;

const FAULT_RECOVERY_READS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargePolicyState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    Fault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReason {
    DaemonDisabled,
    ChargerOffline,
    NormalCharging,
    ChargeLimitReached,
    WaitingForLimitResume,
    ThermalLimitReached,
    WaitingForThermalResume,
    SensorFault,
    FaultRecovering,
    CapacityUnavailable,
}

impl fmt::Display for DecisionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DecisionReason::DaemonDisabled => "daemon_disabled",
            DecisionReason::ChargerOffline => "charger_offline",
            DecisionReason::NormalCharging => "normal_charging",
            DecisionReason::ChargeLimitReached => "charge_limit_reached",
            DecisionReason::WaitingForLimitResume => "waiting_for_limit_resume",
            DecisionReason::ThermalLimitReached => "thermal_limit_reached",
            DecisionReason::WaitingForThermalResume => "waiting_for_thermal_resume",
            DecisionReason::SensorFault => "sensor_fault",
            DecisionReason::FaultRecovering => "fault_recovering",
            DecisionReason::CapacityUnavailable => "capacity_unavailable",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug)]
pub struct Decision {
    pub policy: ChargePolicyState,
    pub target: HardwareTarget,
    pub reason: DecisionReason,
}

pub struct DecisionEngine {
    pub policy: ChargePolicyState,
    fault_recovery_reads: u8,
}

impl DecisionEngine {
    pub fn new() -> Self {
        Self {
            policy: ChargePolicyState::Charging, // Start assuming charging, policy will adapt
            fault_recovery_reads: 0,
        }
    }

    pub fn evaluate(&mut self, snapshot: &SensorSnapshot, cfg: &Config) -> Decision {
        if !cfg.enabled {
            self.policy = ChargePolicyState::Disabled;
            return Decision {
                policy: self.policy,
                target: HardwareTarget::Unmanaged,
                reason: DecisionReason::DaemonDisabled,
            };
        }

        if snapshot.online == Some(false) {
            self.policy = ChargePolicyState::Offline;
            return Decision {
                policy: self.policy,
                target: HardwareTarget::Unmanaged,
                reason: DecisionReason::ChargerOffline,
            };
        }

        if snapshot.temp_dc.is_none() {
            self.fault_recovery_reads = 0;
            self.policy = ChargePolicyState::Fault;
            return Decision {
                policy: self.policy,
                target: HardwareTarget::ChargingDisabled, // Fail-safe
                reason: DecisionReason::SensorFault,
            };
        }

        if self.policy == ChargePolicyState::Fault {
            self.fault_recovery_reads += 1;
            if self.fault_recovery_reads >= FAULT_RECOVERY_READS {
                tracing::info!("Sensor recovered completely, exiting Fault state.");
                self.fault_recovery_reads = 0;
                self.policy = ChargePolicyState::Charging;
            } else {
                return Decision {
                    policy: self.policy,
                    target: HardwareTarget::ChargingDisabled,
                    reason: DecisionReason::FaultRecovering,
                };
            }
        }

        if snapshot.capacity_pct.is_none() {
            // Keep the previous policy and target, just indicate unavailable
            let target = self.policy_to_target(self.policy);
            return Decision {
                policy: self.policy,
                target,
                reason: DecisionReason::CapacityUnavailable,
            };
        }

        let cap = snapshot.capacity_pct.unwrap();
        let temp = snapshot.temp_dc.unwrap();

        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };
        let thermal_max = cfg.max_temp_dc;
        let safe_hysteresis = cfg
            .thermal_resume_hysteresis_dc
            .clamp(1, thermal_max.saturating_sub(1).max(1));
        let thermal_resume = thermal_max.saturating_sub(safe_hysteresis);

        match self.policy {
            ChargePolicyState::Disabled | ChargePolicyState::Offline | ChargePolicyState::Fault => {
                self.policy = ChargePolicyState::Charging;
                self.evaluate(snapshot, cfg) // Re-evaluate cleanly
            }
            ChargePolicyState::Charging => {
                if cfg.thermal_cutoff && temp >= thermal_max {
                    self.policy = ChargePolicyState::ThermalCutoff;
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingDisabled,
                        reason: DecisionReason::ThermalLimitReached,
                    }
                } else if cap >= limit {
                    self.policy = ChargePolicyState::LimitReached;
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingDisabled,
                        reason: DecisionReason::ChargeLimitReached,
                    }
                } else {
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingEnabled,
                        reason: DecisionReason::NormalCharging,
                    }
                }
            }
            ChargePolicyState::LimitReached => {
                if cap <= resume {
                    self.policy = ChargePolicyState::Charging;
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingEnabled,
                        reason: DecisionReason::NormalCharging,
                    }
                } else {
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingDisabled,
                        reason: DecisionReason::WaitingForLimitResume,
                    }
                }
            }
            ChargePolicyState::ThermalCutoff => {
                if temp <= thermal_resume || !cfg.thermal_cutoff {
                    self.policy = ChargePolicyState::Charging;
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingEnabled,
                        reason: DecisionReason::NormalCharging,
                    }
                } else {
                    Decision {
                        policy: self.policy,
                        target: HardwareTarget::ChargingDisabled,
                        reason: DecisionReason::WaitingForThermalResume,
                    }
                }
            }
        }
    }

    fn policy_to_target(&self, policy: ChargePolicyState) -> HardwareTarget {
        match policy {
            ChargePolicyState::Disabled => HardwareTarget::Unmanaged,
            ChargePolicyState::Offline => HardwareTarget::Unmanaged,
            ChargePolicyState::Charging => HardwareTarget::ChargingEnabled,
            ChargePolicyState::LimitReached => HardwareTarget::ChargingDisabled,
            ChargePolicyState::ThermalCutoff => HardwareTarget::ChargingDisabled,
            ChargePolicyState::Fault => HardwareTarget::ChargingDisabled,
        }
    }
}
