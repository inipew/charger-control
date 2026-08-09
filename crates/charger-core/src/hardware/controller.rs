use crate::battery::control;

use crate::persistence::ownership::{clear_persistent_ownership, Ownership};
use std::time::{Duration, Instant};
use std::sync::Arc;
use crate::hardware::io::HardwareIo;
use crate::persistence::io::PersistenceIo;
use crate::time::Clock;

const VERIFY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

const MAX_VERIFICATION_RETRIES: u8 = 3;

const RETRY_BACKOFF: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareTarget {
    ChargingEnabled,
    ChargingDisabled,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareEffect {
    None,
    ChargingEnabled,
    ChargingDisabled,
    Unknown,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SyncState {
    Unknown,
    Pending,
    Synced,
    Failed,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ControllerEvent {
    ApplySuccess(HardwareTarget),
    ApplyFailed,
    VerificationFailed(u8),
    VerificationSuccess,
    ExternalModificationDetected,
}

struct Verification {
    generation: u64,
    target: HardwareTarget,
    deadline: Instant,
}

pub struct HardwareController {
    pub profile: Arc<crate::hardware::profile::HardwareProfile>,
    pub hw_io: Arc<dyn HardwareIo>,
    pub pers_io: Arc<dyn PersistenceIo>,
    pub clock: Arc<dyn Clock>,
    pub desired_target: HardwareTarget,
    pub applied_target: HardwareTarget,
    pub effect: HardwareEffect,
    pub sync: SyncState,
    pub force_apply: bool,
    pub ownership: Ownership,

    generation: u64,
    verification: Option<Verification>,
    verification_failures: u8,
    failed_attempts: u8,
    retry_at: Option<Instant>,
}

impl HardwareController {
    pub fn new(profile: Arc<crate::hardware::profile::HardwareProfile>, hw_io: Arc<dyn HardwareIo>, pers_io: Arc<dyn PersistenceIo>, clock: Arc<dyn Clock>) -> Self {
        Self::with_initial_sync(SyncState::Unknown, profile, hw_io, pers_io, clock)
    }

    pub fn with_initial_sync(sync: SyncState, profile: Arc<crate::hardware::profile::HardwareProfile>, hw_io: Arc<dyn HardwareIo>, pers_io: Arc<dyn PersistenceIo>, clock: Arc<dyn Clock>) -> Self {
        Self {
            profile,
            hw_io,
            pers_io,
            clock,
            desired_target: HardwareTarget::Unmanaged,
            applied_target: HardwareTarget::Unmanaged,
            effect: HardwareEffect::None,
            sync,
            force_apply: true,
            ownership: Ownership::NotOwned,

            generation: 0,
            verification: None,
            verification_failures: 0,
            failed_attempts: 0,
            retry_at: None,
        }
    }

    pub fn set_desired_target(&mut self, target: HardwareTarget) {
        if self.desired_target != target {
            tracing::debug!(
                "Hardware desired target: {:?} -> {:?}",
                self.desired_target,
                target
            );

            self.desired_target = target;
            self.invalidate_verification();
            self.force_apply = true;
        }
    }

    pub fn invalidate_verification(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.verification = None;
        self.verification_failures = 0;
        self.retry_at = None;

        if self.sync == SyncState::Pending {
            self.sync = SyncState::Unknown;
        }
    }

    pub fn needs_apply(
        &self,
        target: HardwareTarget,
        now: Instant,
    ) -> bool {
        if self.sync == SyncState::Failed {
            if let Some(deadline) = self.retry_at {
                if now < deadline {
                    return false;
                }
            }
        }

        if self.desired_target != target {
            return true;
        }

        if self.force_apply {
            return true;
        }

        self.applied_target != target
    }

    pub fn is_owned(&self) -> bool {
        matches!(self.ownership, Ownership::Owned { .. })
    }

    pub fn apply_target(&mut self, target: HardwareTarget) -> Vec<ControllerEvent> {
        self.desired_target = target;

        match target {
            HardwareTarget::ChargingEnabled => {
                self.apply_charging(true, target)
            }

            HardwareTarget::ChargingDisabled => {
                self.apply_charging(false, target)
            }

            HardwareTarget::Unmanaged => {
                self.release_ownership()
            }
        }
    }

    fn apply_charging(
        &mut self,
        enable: bool,
        target: HardwareTarget,
    ) -> Vec<ControllerEvent> {
        let mut events = Vec::new();
        if self.ownership == Ownership::NotOwned {
            match control::is_charging_enabled(&self.profile, &*self.hw_io) {
                Ok(original) => {
                    tracing::info!(
                        "Taking hardware ownership. \
                         Original charging state: {}",
                        original
                    );

                    let record = crate::persistence::ownership::OwnershipRecord {
                        version: 1,
                        generation: self.generation,
                        original_charging: original,
                        target_charging: enable,
                        phase: crate::persistence::ownership::OwnershipPhase::Acquiring,
                    };

                    if let Err(e) = crate::persistence::ownership::save_persistent_ownership(&record, &*self.pers_io) {
                        tracing::error!(
                            "Cannot persist hardware ownership phase Acquiring: {}",
                            e
                        );

                        self.mark_apply_failed();
                        events.push(ControllerEvent::ApplyFailed);
                        return events;
                    }

                    self.ownership = Ownership::Owned {
                        original_charging: original,
                    };
                }

                Err(e) => {
                    tracing::error!(
                        "Cannot acquire hardware ownership: {}",
                        e
                    );

                    self.mark_apply_failed();
                    events.push(ControllerEvent::ApplyFailed);
                    return events;
                }
            }
        }

        match control::set_charging(enable, &self.profile, &*self.hw_io) {
            Ok(res) if res.all_succeeded() => {
                tracing::info!(
                    "Hardware charging set to {}: {}/{} nodes succeeded",
                    enable, res.succeeded, res.attempted
                );

                if let Ownership::Owned { original_charging } = self.ownership {
                    let record = crate::persistence::ownership::OwnershipRecord {
                        version: 1,
                        generation: self.generation,
                        original_charging,
                        target_charging: enable,
                        phase: crate::persistence::ownership::OwnershipPhase::Owned,
                    };
                    if let Err(e) = crate::persistence::ownership::save_persistent_ownership(&record, &*self.pers_io) {
                        tracing::error!("Failed to persist hardware ownership phase Owned: {}", e);
                    }
                }

                self.mark_apply_success(target, false);
                events.push(ControllerEvent::ApplySuccess(target));
            }

            Ok(res) if res.partial_failure() => {
                tracing::error!(
                    "Hardware charging partially applied: {}/{} succeeded, {} failed",
                    res.succeeded, res.attempted, res.failed
                );

                self.mark_apply_failed();
                events.push(ControllerEvent::ApplyFailed);
            }

            Ok(res) => {
                tracing::error!(
                    "Charging control completely failed: {}/{} succeeded, {} failed",
                    res.succeeded, res.attempted, res.failed
                );

                self.mark_apply_failed();
                events.push(ControllerEvent::ApplyFailed);
            }

            Err(e) => {
                tracing::error!(
                    "Failed to set charging={} : {}",
                    enable,
                    e
                );

                self.mark_apply_failed();
                events.push(ControllerEvent::ApplyFailed);
            }
        }
        
        events
    }

    fn release_ownership(&mut self) -> Vec<ControllerEvent> {
        let mut events = Vec::new();
        self.invalidate_verification();

        let original = match self.ownership {
            Ownership::Owned {
                original_charging,
            } => Some(original_charging),

            Ownership::NotOwned => None,
        };

        match original {
            Some(original_charging) => {
                let record = crate::persistence::ownership::OwnershipRecord {
                    version: 1,
                    generation: self.generation,
                    original_charging,
                    target_charging: original_charging,
                    phase: crate::persistence::ownership::OwnershipPhase::Releasing,
                };
                if let Err(e) = crate::persistence::ownership::save_persistent_ownership(&record, &*self.pers_io) {
                    tracing::error!("Failed to persist hardware ownership phase Releasing: {}", e);
                }

                match control::set_charging(original_charging, &self.profile, &*self.hw_io) {
                    Ok(res) if res.all_succeeded() => {
                        tracing::info!(
                            "Released ownership and restored \
                             original charging state: {} (succeeded: {}/{})",
                            original_charging, res.succeeded, res.attempted
                        );

                        clear_persistent_ownership(&*self.pers_io);

                        self.ownership = Ownership::NotOwned;
                        self.applied_target =
                            HardwareTarget::Unmanaged;
                        self.desired_target =
                            HardwareTarget::Unmanaged;

                        self.sync = SyncState::Synced;
                        self.effect = HardwareEffect::None;
                        self.force_apply = false;
                        self.failed_attempts = 0;
                        events.push(ControllerEvent::ApplySuccess(HardwareTarget::Unmanaged));
                    }

                    Ok(res) if res.partial_failure() => {
                        tracing::error!(
                            "Partial failure restoring original charging \
                             state: {}/{} succeeded, {} failed",
                            res.succeeded, res.attempted, res.failed
                        );

                        self.sync = SyncState::Failed;
                        self.force_apply = true;
                        self.retry_at =
                            Some(self.clock.now() + RETRY_BACKOFF[0]);
                        events.push(ControllerEvent::ApplyFailed);
                    }

                    Ok(res) => {
                        tracing::error!(
                            "Complete failure restoring original charging \
                             state: {}/{} succeeded, {} failed",
                            res.succeeded, res.attempted, res.failed
                        );

                        self.sync = SyncState::Failed;
                        self.force_apply = true;
                        self.retry_at =
                            Some(self.clock.now() + RETRY_BACKOFF[0]);
                        events.push(ControllerEvent::ApplyFailed);
                    }

                    Err(e) => {
                        tracing::error!(
                            "Failed to restore original charging \
                             state: {}",
                            e
                        );

                        self.sync = SyncState::Failed;
                        self.force_apply = true;
                        self.retry_at =
                            Some(self.clock.now() + RETRY_BACKOFF[0]);
                        events.push(ControllerEvent::ApplyFailed);
                    }
                }
            }

            None => {
                self.applied_target =
                    HardwareTarget::Unmanaged;

                self.desired_target =
                    HardwareTarget::Unmanaged;

                self.sync = SyncState::Synced;
                self.effect = HardwareEffect::None;
                self.force_apply = false;
                events.push(ControllerEvent::ApplySuccess(HardwareTarget::Unmanaged));
            }
        }
        
        events
    }

    fn mark_apply_success(
        &mut self,
        target: HardwareTarget,
        partial: bool,
    ) {
        self.applied_target = target;
        
        self.effect = if partial {
            HardwareEffect::Unknown
        } else {
            match target {
                HardwareTarget::ChargingEnabled => HardwareEffect::ChargingEnabled,
                HardwareTarget::ChargingDisabled => HardwareEffect::ChargingDisabled,
                HardwareTarget::Unmanaged => HardwareEffect::None,
            }
        };

        self.force_apply = false;
        self.sync = SyncState::Pending;

        self.verification_failures = 0;
        self.retry_at = None;

        self.generation =
            self.generation.wrapping_add(1);

        self.verification = Some(Verification {
            generation: self.generation,
            target,
            deadline: self.clock.now() + VERIFY_DELAYS[0],
        });
    }

    fn mark_apply_failed(&mut self) {
        self.generation =
            self.generation.wrapping_add(1);

        self.verification = None;
        self.verification_failures = 0;
        self.sync = SyncState::Failed;
        self.force_apply = true;

        self.retry_at =
            Some(self.clock.now() + RETRY_BACKOFF[0]);
    }

    pub fn verification_due(&self, now: Instant) -> bool {
        self.verification
            .as_ref()
            .is_some_and(|v| now >= v.deadline)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        match (
            self.verification.as_ref().map(|v| v.deadline),
            if self.sync == SyncState::Failed { self.retry_at } else { None },
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    pub fn retry_due(&self, now: Instant) -> bool {
        self.sync == SyncState::Failed
            && self
                .retry_at
                .is_some_and(|deadline| now >= deadline)
    }

    pub fn verify(
        &mut self,
        snapshot: &crate::battery::snapshot::SensorSnapshot,
    ) -> Vec<ControllerEvent> {
        let mut events = Vec::new();
        let Some(v) = self.verification.as_ref() else {
            return events;
        };

        if v.generation != self.generation {
            self.verification = None;
            return events;
        }

        let target = v.target;

        let success = match target {
            HardwareTarget::ChargingEnabled => {
                match control::read_charging_state(&self.profile, &*self.hw_io) {
                    Ok(control::ChargingState::Enabled) => true,

                    Ok(control::ChargingState::Disabled) => false,

                    Ok(control::ChargingState::Mixed) => {
                        tracing::warn!(
                            "Charging nodes are in a mixed state; verification failed"
                        );
                        false
                    }

                    Ok(control::ChargingState::Unknown) | Err(_) => {
                        tracing::warn!(
                            "Unable to verify charging state"
                        );
                        false
                    }
                }
            }

            HardwareTarget::ChargingDisabled => {
                let current_safe = snapshot.battery_current_ma
                    .is_some_and(|current| current <= 100);

                match control::read_charging_state(&self.profile, &*self.hw_io) {
                    Ok(control::ChargingState::Disabled) => current_safe,

                    Ok(control::ChargingState::Enabled) => false,

                    Ok(control::ChargingState::Mixed) => {
                        tracing::warn!(
                            "Charging nodes are in a mixed state; verification failed"
                        );
                        false
                    }

                    Ok(control::ChargingState::Unknown) | Err(_) => {
                        tracing::warn!(
                            "Unable to verify charging state"
                        );
                        false
                    }
                }
            }

            HardwareTarget::Unmanaged => true,
        };

        if success {
            tracing::debug!(
                "Hardware verification succeeded: {:?}",
                target
            );

            self.sync = SyncState::Synced;
            self.effect = match target {
                HardwareTarget::ChargingEnabled => HardwareEffect::ChargingEnabled,
                HardwareTarget::ChargingDisabled => HardwareEffect::ChargingDisabled,
                HardwareTarget::Unmanaged => HardwareEffect::None,
            };
            self.verification = None;
            self.verification_failures = 0;
            self.failed_attempts = 0;
            self.retry_at = None;
            self.force_apply = false;
            events.push(ControllerEvent::VerificationSuccess);
        } else {
            if let Some(event) = self.verification_failed(target) {
                events.push(event);
            }
        }
        
        events
    }

    fn verification_failed(&mut self, target: HardwareTarget) -> Option<ControllerEvent> {
        self.verification_failures =
            self.verification_failures.saturating_add(1);

        if self.verification_failures >= MAX_VERIFICATION_RETRIES {
            self.failed_attempts =
                self.failed_attempts.saturating_add(1);

            let index =
                (self.failed_attempts as usize)
                    .saturating_sub(1)
                    .min(RETRY_BACKOFF.len() - 1);

            let backoff = RETRY_BACKOFF[index];

            tracing::error!(
                "Hardware verification failed after {} attempts. \
                 Retrying in {:?}.",
                MAX_VERIFICATION_RETRIES,
                backoff
            );

            self.sync = SyncState::Failed;
            self.verification = None;
            self.force_apply = true;
            self.retry_at = Some(
                self.clock.now() + backoff
            );

            return Some(ControllerEvent::VerificationFailed(MAX_VERIFICATION_RETRIES));
        }

        let index =
            (self.verification_failures as usize)
                .min(VERIFY_DELAYS.len() - 1);

        self.verification = Some(Verification {
            generation: self.generation,
            target,
            deadline: self.clock.now()
                + VERIFY_DELAYS[index],
        });

        self.sync = SyncState::Pending;
        Some(ControllerEvent::VerificationFailed(self.verification_failures))
    }

    pub fn reconcile(&mut self) -> Vec<ControllerEvent> {
        let mut events = Vec::new();
        if self.sync != SyncState::Synced {
            return events;
        }

        match self.applied_target {
            HardwareTarget::ChargingEnabled => {
                match control::read_charging_state(&self.profile, &*self.hw_io) {
                    Ok(control::ChargingState::Disabled) => {
                        tracing::warn!("External hardware modification detected (charging disabled). Re-syncing.");
                        self.sync = SyncState::Unknown;
                        self.force_apply = true;
                        events.push(ControllerEvent::ExternalModificationDetected);
                    }
                    Ok(control::ChargingState::Mixed) => {
                        tracing::warn!("Hardware state is Mixed (uncertain). Waiting for next verification.");
                        self.sync = SyncState::Unknown;
                    }
                    _ => {}
                }
            }
            HardwareTarget::ChargingDisabled => {
                match control::read_charging_state(&self.profile, &*self.hw_io) {
                    Ok(control::ChargingState::Enabled) => {
                        tracing::warn!("External hardware modification detected (charging enabled). Re-syncing.");
                        self.sync = SyncState::Unknown;
                        self.force_apply = true;
                        events.push(ControllerEvent::ExternalModificationDetected);
                    }
                    Ok(control::ChargingState::Mixed) => {
                        tracing::warn!("Hardware state is Mixed (uncertain). Waiting for next verification.");
                        self.sync = SyncState::Unknown;
                    }
                    _ => {}
                }
            }
            HardwareTarget::Unmanaged => {}
        }
        
        events
    }

    pub fn shutdown_restore(&mut self) {
        let Ownership::Owned {
            original_charging,
        } = self.ownership
        else {
            tracing::info!(
                "Daemon shutting down without hardware ownership."
            );

            return;
        };

        let record = crate::persistence::ownership::OwnershipRecord {
            version: 1,
            generation: self.generation,
            original_charging,
            target_charging: original_charging,
            phase: crate::persistence::ownership::OwnershipPhase::Releasing,
        };
        if let Err(e) = crate::persistence::ownership::save_persistent_ownership(&record, &*self.pers_io) {
            tracing::error!("Failed to persist hardware ownership phase Releasing during shutdown: {}", e);
        }

        match control::set_charging(original_charging, &self.profile, &*self.hw_io) {
            Ok(res) if res.all_succeeded() => {
                tracing::info!(
                    "Shutdown: restored original charging state: {} (succeeded: {}/{})",
                    original_charging, res.succeeded, res.attempted
                );

                clear_persistent_ownership(&*self.pers_io);

                self.ownership = Ownership::NotOwned;
                self.desired_target =
                    HardwareTarget::Unmanaged;
                self.applied_target =
                    HardwareTarget::Unmanaged;
                self.sync = SyncState::Synced;
                self.force_apply = false;
                self.verification = None;
                self.retry_at = None;
            }

            Ok(res) if res.partial_failure() => {
                tracing::error!(
                    "Partial failure restoring charging state during shutdown: \
                     {}/{} succeeded, {} failed",
                    res.succeeded, res.attempted, res.failed
                );

                self.sync = SyncState::Failed;
                self.force_apply = true;
            }

            Ok(res) => {
                tracing::error!(
                    "Complete failure restoring charging state during shutdown: \
                     {}/{} succeeded, {} failed",
                    res.succeeded, res.attempted, res.failed
                );

                self.sync = SyncState::Failed;
                self.force_apply = true;
            }

            Err(e) => {
                tracing::error!(
                    "Failed to restore charging state during shutdown: {}",
                    e
                );

                self.sync = SyncState::Failed;
                self.force_apply = true;
            }
        }
    }
}

