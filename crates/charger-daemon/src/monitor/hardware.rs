use std::time::{Duration, Instant};

use charger_core::{battery::control, error::ChargerError};

use super::decision::DesiredHardwareState;

/// Kebijakan retry pemulihan fault hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultRetryPolicy {
    Never,
    After(Duration),
}

/// Taksonomi kesalahan hardware terstruktur tanpa string heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareFault {
    NodeMissing,
    PermissionDenied,
    WriteFailed,
    ReadbackMismatch,
}

impl HardwareFault {
    pub fn retry_policy(&self) -> FaultRetryPolicy {
        match self {
            Self::PermissionDenied => FaultRetryPolicy::Never,
            Self::NodeMissing => FaultRetryPolicy::After(Duration::from_secs(30)),
            Self::WriteFailed => FaultRetryPolicy::After(Duration::from_secs(5)),
            Self::ReadbackMismatch => FaultRetryPolicy::After(Duration::from_secs(10)),
        }
    }

    #[allow(dead_code)]
    pub fn is_retryable(&self) -> bool {
        !matches!(self.retry_policy(), FaultRetryPolicy::Never)
    }
}

/// Status transisi eksplisit dari hardware actuator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardwareStatus {
    Unknown,
    Reconciling {
        target: DesiredHardwareState,
        started_at: Instant,
    },
    Stable {
        mode: control::ActualHardwareMode,
    },
    Fault {
        error: HardwareFault,
        failed_at: Instant,
        retry: FaultRetryPolicy,
    },
}

/// Observasi actuator hardware terkini.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareObservation {
    pub mode: control::ActualHardwareMode,
    pub verified_at: Option<Instant>,
}

impl HardwareObservation {
    pub fn new() -> Self {
        Self {
            mode: control::ActualHardwareMode::Unknown,
            verified_at: None,
        }
    }
}

/// Tracking status rekonsiliasi hardware dengan FSM eksplisit (Event-Driven Verification & Fault Recovery Policy).
#[derive(Debug)]
pub struct HardwareTrack {
    pub last_verified_obs: HardwareObservation,
    pub status: HardwareStatus,
    pub verification_needed: bool,
}

impl HardwareTrack {
    pub fn new() -> Self {
        Self {
            last_verified_obs: HardwareObservation::new(),
            status: HardwareStatus::Unknown,
            verification_needed: true,
        }
    }

    pub fn mark_verification_needed(&mut self) {
        self.verification_needed = true;
    }

    pub fn update_observation(&mut self, mode: control::ActualHardwareMode, now: Instant) {
        self.last_verified_obs = HardwareObservation {
            mode,
            verified_at: Some(now),
        };
        self.status = HardwareStatus::Stable { mode };
        self.verification_needed = false;
    }

    pub fn mark_fault(&mut self, error: HardwareFault, now: Instant) {
        let retry = error.retry_policy();
        self.status = HardwareStatus::Fault {
            error,
            failed_at: now,
            retry,
        };
        if error != HardwareFault::ReadbackMismatch {
            self.last_verified_obs = HardwareObservation::new();
        }
        self.verification_needed = false;
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if let HardwareStatus::Fault {
            failed_at,
            retry: FaultRetryPolicy::After(dur),
            ..
        } = self.status
        {
            return Some(failed_at + dur);
        }
        None
    }

    pub fn reset_on_disconnect(&mut self) {
        self.status = HardwareStatus::Unknown;
        self.last_verified_obs = HardwareObservation::new();
        self.verification_needed = true;
    }
}

/// Hasil rekonsiliasi hardware actuator.
#[derive(Debug)]
pub enum ReconcileResult {
    Skipped(control::ActualHardwareMode),
    Stable(control::ActualHardwareMode),
    Changed(control::ActualHardwareMode),
    Deferred,
    Failed(ChargerError),
}

/// Menjalankan rekonsiliasi hardware secara **idempotent & event-driven** (Verify HANYA jika event/write/fault retry memintanya).
pub struct ReconcileOptions {
    pub bypass_retry_delay: bool,
    pub force_verification: bool,
}

pub fn reconcile(
    desired: DesiredHardwareState,
    track: &mut HardwareTrack,
    has_distinct_bypass: bool,
    opts: ReconcileOptions,
    now: Instant,
) -> ReconcileResult {
    // 0. Jika status desired adalah NoChange (misal saat Disconnected), jangan ubah hardware.
    // NoChange secara sengaja bersifat non-reconciling:
    // decision layer tidak mengatur hardware state transitions
    // saat charger terputus (disconnected) atau selama settling.
    if desired == DesiredHardwareState::NoChange {
        return ReconcileResult::Skipped(track.last_verified_obs.mode);
    }

    let retry_due = match track.status {
        HardwareStatus::Fault {
            failed_at,
            retry: FaultRetryPolicy::After(delay),
            ..
        } => now >= failed_at + delay,
        _ => false,
    };

    // 1. Deferral check: Jika status saat ini adalah Fault dan belum waktunya retry (atau retry policy Never), tunda rekonsiliasi
    // Bypassing delay hanya diizinkan melalui emergency_override (misal: ThermalEmergency).
    if let HardwareStatus::Fault {
        error: _,
        failed_at,
        retry,
    } = &track.status
    {
        match retry {
            FaultRetryPolicy::After(delay) => {
                if now < *failed_at + *delay && !opts.bypass_retry_delay {
                    return ReconcileResult::Deferred;
                }
            }
            FaultRetryPolicy::Never => {
                return ReconcileResult::Deferred;
            }
        }
    }

    let expected_actual = desired.hardware_mode(has_distinct_bypass);

    // 2. Verification Phase: HANYA baca sysfs jika diperlukan.
    let must_verify = track.verification_needed || retry_due || opts.force_verification;
    let current_actual = if must_verify {
        control::get_actual_charging_state()
    } else {
        track.last_verified_obs.mode
    };

    // 3. Mutation Phase: Jika status hardware saat ini SUDAH SAMA, skip penulisan sysfs! (Idempotency)
    if current_actual == expected_actual {
        track.update_observation(current_actual, now);
        return ReconcileResult::Stable(current_actual);
    }

    track.status = HardwareStatus::Reconciling {
        target: desired,
        started_at: now,
    };

    // 4. Mutation Phase: Jalankan penulisan fisik sysfs
    let write_res = match expected_actual {
        control::ActualHardwareMode::Unknown | control::ActualHardwareMode::Inconsistent => {
            Err(ChargerError::HardwareError(
                "Cannot write Unknown or Inconsistent hardware target state",
            ))
        }
        control::ActualHardwareMode::ChargingEnabled => control::set_charging(true),
        control::ActualHardwareMode::ChargingDisabled => control::set_charging(false),
        control::ActualHardwareMode::Bypass => control::enter_bypass_mode(),
    };

    match write_res {
        Ok(()) => {}
        Err(ChargerError::NoChargingNodeFound) => {
            control::reset_node_caches();
            track.mark_fault(HardwareFault::NodeMissing, now);
            return ReconcileResult::Failed(ChargerError::NoChargingNodeFound);
        }
        Err(ChargerError::SysfsWrite { ref source, .. })
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            track.mark_fault(HardwareFault::PermissionDenied, now);
            return ReconcileResult::Failed(ChargerError::HardwareError(
                "Permission denied writing sysfs node",
            ));
        }
        Err(err) => {
            track.mark_fault(HardwareFault::WriteFailed, now);
            tracing::error!(?err, "Hardware sysfs write failed");
            return ReconcileResult::Failed(err);
        }
    }

    // 5. Readback Verification Pasca Penulisan & Mismatch Detection
    let actual_after = control::get_actual_charging_state();

    if actual_after != expected_actual {
        track.update_observation(actual_after, now);
        track.mark_fault(HardwareFault::ReadbackMismatch, now);
        tracing::error!(
            ?desired,
            ?expected_actual,
            ?actual_after,
            "Hardware readback mismatch detected! Actuator write failed to reach target state."
        );
        return ReconcileResult::Failed(ChargerError::HardwareError("Hardware readback mismatch"));
    }

    let is_changed = track.last_verified_obs.mode != actual_after;
    track.update_observation(actual_after, now);

    if is_changed {
        ReconcileResult::Changed(actual_after)
    } else {
        ReconcileResult::Stable(actual_after)
    }
}
