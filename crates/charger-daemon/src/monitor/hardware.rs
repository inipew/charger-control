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
    CurrentLimitNodeMissing,
    CurrentLimitWriteFailed,
}

impl HardwareFault {
    pub fn retry_policy(&self) -> FaultRetryPolicy {
        match self {
            Self::PermissionDenied => FaultRetryPolicy::Never,
            Self::NodeMissing | Self::CurrentLimitNodeMissing => {
                FaultRetryPolicy::After(Duration::from_secs(30))
            }
            Self::WriteFailed | Self::CurrentLimitWriteFailed => {
                FaultRetryPolicy::After(Duration::from_secs(5))
            }
            Self::ReadbackMismatch => FaultRetryPolicy::After(Duration::from_secs(10)),
        }
    }

    #[allow(dead_code)]
    pub fn is_retryable(&self) -> bool {
        !matches!(self.retry_policy(), FaultRetryPolicy::Never)
    }
}

/// Status konvergensi antara desired state dan physical hardware truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergenceState {
    Converged,
    Reconciling,
    Deferred,
    Fault,
}

impl ConvergenceState {
    /// Mengombinasikan status konvergensi dari beberapa domain actuator.
    /// Prioritas: Fault > Reconciling > Deferred > Converged.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Fault, _) | (_, Self::Fault) => Self::Fault,
            (Self::Reconciling, _) | (_, Self::Reconciling) => Self::Reconciling,
            (Self::Deferred, _) | (_, Self::Deferred) => Self::Deferred,
            (Self::Converged, Self::Converged) => Self::Converged,
        }
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

/// Observasi actuator hardware binary terkini.
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

/// Penjejak status domain binary charger actuator (ChargingEnabled / ChargingDisabled / Bypass).
#[derive(Debug)]
pub struct ChargerActuatorTrack {
    pub observation: HardwareObservation,
    pub status: HardwareStatus,
    pub verification_needed: bool,
}

impl ChargerActuatorTrack {
    pub fn new() -> Self {
        Self {
            observation: HardwareObservation::new(),
            status: HardwareStatus::Unknown,
            verification_needed: true,
        }
    }

    pub fn mark_verification_needed(&mut self) {
        self.verification_needed = true;
    }

    pub fn update_observation(&mut self, mode: control::ActualHardwareMode, now: Instant) {
        self.observation = HardwareObservation {
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
            self.observation = HardwareObservation::new();
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
        self.observation = HardwareObservation::new();
        self.verification_needed = true;
    }

    pub fn convergence(&self) -> ConvergenceState {
        match self.status {
            HardwareStatus::Fault { .. } => ConvergenceState::Fault,
            HardwareStatus::Reconciling { .. } => ConvergenceState::Reconciling,
            HardwareStatus::Stable { .. } => ConvergenceState::Converged,
            HardwareStatus::Unknown => {
                if self.verification_needed {
                    ConvergenceState::Deferred
                } else {
                    ConvergenceState::Converged
                }
            }
        }
    }
}

/// Status transisi eksplisit dari domain pembatas arus (Fast Charge Current Regulation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentLimitStatus {
    Unknown,
    Reconciling {
        target_ua: Option<u32>,
        started_at: Instant,
    },
    Stable {
        applied_ua: Option<u32>,
    },
    Fault {
        error: HardwareFault,
        failed_at: Instant,
        retry: FaultRetryPolicy,
    },
}

/// Penjejak status domain pembatas arus (Fast Charge Current Regulation).
///
/// **Catatan**: `applied_limit_ua` mencatat *last successful write intent / actuation*,
/// bukan physical sensor readback observasional (kecuali driver kernel mendukung sysfs readback).
#[derive(Debug)]
pub struct CurrentLimitTrack {
    pub applied_limit_ua: Option<u32>,
    pub reconcile_needed: bool,
    pub status: CurrentLimitStatus,
}

impl CurrentLimitTrack {
    pub fn new() -> Self {
        Self {
            applied_limit_ua: None,
            reconcile_needed: true,
            status: CurrentLimitStatus::Unknown,
        }
    }

    pub fn mark_applied(&mut self, target_ua: Option<u32>) {
        self.applied_limit_ua = target_ua;
        self.reconcile_needed = false;
        self.status = CurrentLimitStatus::Stable {
            applied_ua: target_ua,
        };
    }

    pub fn mark_fault(&mut self, error: HardwareFault, now: Instant) {
        let retry = error.retry_policy();
        self.status = CurrentLimitStatus::Fault {
            error,
            failed_at: now,
            retry,
        };
        self.reconcile_needed = false;
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if let CurrentLimitStatus::Fault {
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
        self.applied_limit_ua = None;
        self.reconcile_needed = true;
        self.status = CurrentLimitStatus::Unknown;
    }

    pub fn convergence(&self) -> ConvergenceState {
        match self.status {
            CurrentLimitStatus::Fault { .. } => ConvergenceState::Fault,
            CurrentLimitStatus::Reconciling { .. } => ConvergenceState::Reconciling,
            CurrentLimitStatus::Stable { .. } => ConvergenceState::Converged,
            CurrentLimitStatus::Unknown => {
                if self.reconcile_needed {
                    ConvergenceState::Deferred
                } else {
                    ConvergenceState::Converged
                }
            }
        }
    }
}

/// Tracking status rekonsiliasi hardware gabungan yang mengisolasi domain charger dan domain limit arus.
#[derive(Debug)]
pub struct HardwareTrack {
    pub charger: ChargerActuatorTrack,
    pub current_limit: CurrentLimitTrack,
}

impl HardwareTrack {
    pub fn new() -> Self {
        Self {
            charger: ChargerActuatorTrack::new(),
            current_limit: CurrentLimitTrack::new(),
        }
    }

    pub fn mark_verification_needed(&mut self) {
        self.charger.mark_verification_needed();
        self.current_limit.reconcile_needed = true;
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        match (
            self.charger.next_deadline(),
            self.current_limit.next_deadline(),
        ) {
            (Some(d1), Some(d2)) => Some(d1.min(d2)),
            (Some(d1), None) => Some(d1),
            (None, Some(d2)) => Some(d2),
            (None, None) => None,
        }
    }

    pub fn reset_on_disconnect(&mut self) {
        self.charger.reset_on_disconnect();
        self.current_limit.reset_on_disconnect();
    }

    pub fn overall_convergence(&self) -> ConvergenceState {
        self.charger
            .convergence()
            .combine(self.current_limit.convergence())
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

/// Menjalankan rekonsiliasi hardware binary secara **idempotent & event-driven** (Verify HANYA jika event/write/fault retry memintanya).
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
        return ReconcileResult::Skipped(track.charger.observation.mode);
    }

    // 1. Deferral check: Jika status charger saat ini adalah Fault dan belum waktunya retry (atau retry policy Never), tunda rekonsiliasi
    // Bypassing delay hanya diizinkan melalui emergency_override (misal: ThermalEmergency).
    let fault_retry_blocked = match &track.charger.status {
        HardwareStatus::Fault {
            failed_at,
            retry: FaultRetryPolicy::After(delay),
            ..
        } => !opts.bypass_retry_delay && now < *failed_at + *delay,
        HardwareStatus::Fault {
            retry: FaultRetryPolicy::Never,
            ..
        } => true,
        _ => false,
    };

    if fault_retry_blocked {
        return ReconcileResult::Deferred;
    }

    let expected_actual = desired.hardware_mode(has_distinct_bypass);

    // 2. Verification Phase: HANYA baca sysfs jika diperlukan.
    // Fault permanent (Never retry) tidak memicu spam sysfs read.
    let must_verify = track.charger.verification_needed
        || opts.force_verification
        || matches!(
            track.charger.status,
            HardwareStatus::Fault {
                retry: FaultRetryPolicy::After(_),
                ..
            }
        );
    let current_actual = if must_verify {
        control::get_actual_charging_state()
    } else {
        track.charger.observation.mode
    };

    // 3. Mutation Phase: Jika status hardware saat ini SUDAH SAMA, skip penulisan sysfs! (Idempotency)
    if current_actual == expected_actual {
        track.charger.update_observation(current_actual, now);
        return ReconcileResult::Stable(current_actual);
    }

    track.charger.status = HardwareStatus::Reconciling {
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
            track.charger.mark_fault(HardwareFault::NodeMissing, now);
            return ReconcileResult::Failed(ChargerError::NoChargingNodeFound);
        }
        Err(ChargerError::SysfsWrite { ref source, .. })
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            track
                .charger
                .mark_fault(HardwareFault::PermissionDenied, now);
            return ReconcileResult::Failed(ChargerError::HardwareError(
                "Permission denied writing sysfs node",
            ));
        }
        Err(err) => {
            track.charger.mark_fault(HardwareFault::WriteFailed, now);
            tracing::error!(?err, "Hardware sysfs write failed");
            return ReconcileResult::Failed(err);
        }
    }

    // 5. Readback Verification Pasca Penulisan & Mismatch Detection
    let actual_after = control::get_actual_charging_state();

    if actual_after != expected_actual {
        track.charger.update_observation(actual_after, now);
        track
            .charger
            .mark_fault(HardwareFault::ReadbackMismatch, now);
        tracing::error!(
            ?desired,
            ?expected_actual,
            ?actual_after,
            "Hardware readback mismatch detected! Actuator write failed to reach target state."
        );
        return ReconcileResult::Failed(ChargerError::HardwareError("Hardware readback mismatch"));
    }

    let is_changed = track.charger.observation.mode != actual_after;
    track.charger.update_observation(actual_after, now);

    if is_changed {
        ReconcileResult::Changed(actual_after)
    } else {
        ReconcileResult::Stable(actual_after)
    }
}

/// Opsi rekonsiliasi limit arus hardware.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurrentReconcileOptions {
    pub bypass_retry_delay: bool,
}

/// Hasil rekonsiliasi limit arus hardware actuator.
#[derive(Debug)]
pub enum CurrentReconcileResult {
    Skipped,
    Stable(Option<u32>),
    Changed(Option<u32>),
    Deferred,
    Failed(ChargerError),
}

/// Rekonsiliasi batas arus pengisian daya ke sysfs secara idempotent, terisolasi, dan patuh retry backoff.
pub fn reconcile_current(
    desired_hw: DesiredHardwareState,
    target: super::decision::CurrentRegulation,
    track: &mut HardwareTrack,
    opts: CurrentReconcileOptions,
    now: Instant,
) -> CurrentReconcileResult {
    // 0. GUARD: Jika tidak ada perubahan hardware (misal Disconnected atau Attaching settle),
    // jangan lakukan mutasi fisik ke sysfs!
    if desired_hw == DesiredHardwareState::NoChange {
        return CurrentReconcileResult::Skipped;
    }

    let desired_ua = target.target_ua();

    // 1. Deferral check: Jika status saat ini adalah Fault dan belum waktunya retry (atau retry policy Never), tunda rekonsiliasi
    let fault_retry_blocked = match &track.current_limit.status {
        CurrentLimitStatus::Fault {
            failed_at,
            retry: FaultRetryPolicy::After(delay),
            ..
        } => !opts.bypass_retry_delay && now < *failed_at + *delay,

        CurrentLimitStatus::Fault {
            retry: FaultRetryPolicy::Never,
            ..
        } => true,

        _ => false,
    };

    if fault_retry_blocked {
        return CurrentReconcileResult::Deferred;
    }

    // 2. IDEMPOTENCY FIX:
    // Jika status sudah Stable dan applied_limit_ua sudah sama persis dengan desired_ua,
    // bersihkan flag reconcile_needed dan JANGAN tulis ulang ke sysfs!
    if !opts.bypass_retry_delay
        && matches!(
            track.current_limit.status,
            CurrentLimitStatus::Stable { .. }
        )
        && track.current_limit.applied_limit_ua == desired_ua
    {
        track.current_limit.reconcile_needed = false;
        return CurrentReconcileResult::Stable(track.current_limit.applied_limit_ua);
    }

    track.current_limit.status = CurrentLimitStatus::Reconciling {
        target_ua: desired_ua,
        started_at: now,
    };

    let prev_applied = track.current_limit.applied_limit_ua;

    if let Some(ua) = desired_ua {
        match control::set_fast_charge_current(ua) {
            Ok(()) => {
                track.current_limit.mark_applied(Some(ua));
                if prev_applied != Some(ua) {
                    CurrentReconcileResult::Changed(Some(ua))
                } else {
                    CurrentReconcileResult::Stable(Some(ua))
                }
            }
            Err(ChargerError::NoChargingNodeFound) => {
                control::reset_node_caches();
                track
                    .current_limit
                    .mark_fault(HardwareFault::CurrentLimitNodeMissing, now);
                CurrentReconcileResult::Failed(ChargerError::NoChargingNodeFound)
            }
            Err(e) => {
                tracing::warn!(error = %e, target_ua = ua, "Failed setting fast charge current limit");
                track
                    .current_limit
                    .mark_fault(HardwareFault::CurrentLimitWriteFailed, now);
                CurrentReconcileResult::Failed(e)
            }
        }
    } else {
        // Desired is Unconstrained (None / Max Hardware)
        // HANYA tulis reset jika sebelumnya pernah dibatasi (is_some) atau status belum Stable(None)
        if track.current_limit.applied_limit_ua.is_some()
            || !matches!(
                track.current_limit.status,
                CurrentLimitStatus::Stable { applied_ua: None }
            )
            || opts.bypass_retry_delay
        {
            match control::reset_fast_charge_current() {
                Ok(()) => {
                    track.current_limit.mark_applied(None);
                    if prev_applied.is_some() {
                        CurrentReconcileResult::Changed(None)
                    } else {
                        CurrentReconcileResult::Stable(None)
                    }
                }
                Err(ChargerError::NoChargingNodeFound) => {
                    control::reset_node_caches();
                    track
                        .current_limit
                        .mark_fault(HardwareFault::CurrentLimitNodeMissing, now);
                    CurrentReconcileResult::Failed(ChargerError::NoChargingNodeFound)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed resetting fast charge current limit");
                    track
                        .current_limit
                        .mark_fault(HardwareFault::CurrentLimitWriteFailed, now);
                    CurrentReconcileResult::Failed(e)
                }
            }
        } else {
            track.current_limit.reconcile_needed = false;
            CurrentReconcileResult::Stable(None)
        }
    }
}
