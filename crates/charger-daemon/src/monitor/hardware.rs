use super::snapshot::{ChargingState, SensorSnapshot};
use charger_core::battery::control;
use std::time::{Duration, Instant};

const VERIFY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
const MAX_VERIFICATION_RETRIES: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareTarget {
    ChargingEnabled,
    ChargingDisabled,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Unknown,
    Pending,
    Synced,
    Failed,
}

struct Verification {
    generation: u64,
    target: HardwareTarget,
    deadline: Instant,
}

pub struct HardwareController {
    pub target: HardwareTarget,
    pub sync: SyncState,
    pub force_apply: bool,

    generation: u64,
    verification: Option<Verification>,
    verification_failures: u8,
}

impl HardwareController {
    pub fn new() -> Self {
        Self {
            target: HardwareTarget::Unmanaged,
            sync: SyncState::Unknown,
            force_apply: true,

            generation: 0,
            verification: None,
            verification_failures: 0,
        }
    }

    pub fn invalidate_verification(&mut self) {
        self.generation += 1;
        self.verification = None;
        self.verification_failures = 0;
        self.sync = SyncState::Unknown;
    }

    pub fn needs_apply(&self, new_target: HardwareTarget) -> bool {
        self.target != new_target || self.force_apply || self.sync == SyncState::Failed
    }

    pub fn apply_target(&mut self, target: HardwareTarget) {
        self.target = target;
        let success = match target {
            HardwareTarget::ChargingEnabled => {
                match control::set_charging(true) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!("Failed to enable charging: {}", e);
                        false
                    }
                }
            }
            HardwareTarget::ChargingDisabled => {
                match control::set_charging(false) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!("Failed to disable charging: {}", e);
                        false
                    }
                }
            }
            HardwareTarget::Unmanaged => {
                match control::set_charging(true) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!("Failed to restore charging: {}", e);
                        false
                    }
                }
            }
        };

        if success {
            self.force_apply = false;
            self.sync = SyncState::Pending;
            self.verification_failures = 0;
            self.generation += 1;
            
            self.verification = Some(Verification {
                generation: self.generation,
                target,
                deadline: Instant::now() + VERIFY_DELAYS[0],
            });
        } else {
            self.force_apply = true;
            self.sync = SyncState::Failed;
            self.invalidate_verification();
        }
    }

    pub fn verification_due(&self) -> bool {
        if let Some(v) = &self.verification {
            Instant::now() >= v.deadline
        } else {
            false
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.verification.as_ref().map(|v| v.deadline)
    }

    pub fn verify(&mut self, snapshot: &SensorSnapshot) {
        let Some(v) = &self.verification else { return };
        if v.generation != self.generation { return }

        let success = match v.target {
            HardwareTarget::ChargingEnabled => {
                snapshot.online == Some(true) && snapshot.charging_state() == ChargingState::Charging
            }
            HardwareTarget::ChargingDisabled => {
                snapshot.charging_state() != ChargingState::Charging
            }
            HardwareTarget::Unmanaged => {
                true
            }
        };

        if success {
            self.sync = SyncState::Synced;
            self.verification = None;
            self.verification_failures = 0;
        } else {
            tracing::warn!("Verification failed for target {:?}", self.target);
            self.verification_failed();
        }
    }

    fn verification_failed(&mut self) {
        self.verification_failures = self.verification_failures.saturating_add(1);

        if self.verification_failures > MAX_VERIFICATION_RETRIES {
            tracing::error!(
                "Hardware synchronization failed after {} retries",
                MAX_VERIFICATION_RETRIES
            );
            self.sync = SyncState::Failed;
            self.verification = None;
            self.force_apply = true; // Force re-apply on next tick
            return;
        }

        let index = (self.verification_failures as usize).min(VERIFY_DELAYS.len() - 1);
        self.verification = Some(Verification {
            generation: self.generation,
            target: self.target,
            deadline: Instant::now() + VERIFY_DELAYS[index],
        });
    }
}
