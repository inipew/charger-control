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

    #[allow(dead_code)]
    pub fn is_safety_fault(&self) -> bool {
        matches!(self, Self::PermissionDenied | Self::ReadbackMismatch)
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

/// Penjejak status domain binary charger actuator (ChargingEnabled / ChargingDisabled).
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
    Verified {
        applied_ua: Option<u32>,
    },
    Commanded {
        applied_ua: Option<u32>,
    },
    Fault {
        error: HardwareFault,
        failed_at: Instant,
        retry: FaultRetryPolicy,
    },
}

impl CurrentLimitStatus {
    #[allow(dead_code)]
    pub fn applied_ua(&self) -> Option<Option<u32>> {
        match self {
            Self::Verified { applied_ua } | Self::Commanded { applied_ua } => Some(*applied_ua),
            _ => None,
        }
    }
}

/// Penjejak status domain pembatas arus (Fast Charge Current Regulation).
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

    pub fn mark_applied(&mut self, target_ua: Option<u32>, verified: bool) {
        self.applied_limit_ua = target_ua;
        self.reconcile_needed = false;
        if verified {
            self.status = CurrentLimitStatus::Verified {
                applied_ua: target_ua,
            };
        } else {
            self.status = CurrentLimitStatus::Commanded {
                applied_ua: target_ua,
            };
        }
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
            CurrentLimitStatus::Verified { .. } | CurrentLimitStatus::Commanded { .. } => {
                ConvergenceState::Converged
            }
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

/// Status rekonsiliasi Fast Charge Bypass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastChargeStatus {
    Unknown,
    Applied { target_ua: u32 },
    Released,
    Fault {
        error: HardwareFault,
        failed_at: Instant,
        retry: FaultRetryPolicy,
    },
}

/// Penjejak status Fast Charge & USB-PD Bypass actuator.
#[derive(Debug)]
pub struct FastChargeTrack {
    pub status: FastChargeStatus,
    pub applied_target_ua: Option<u32>,
    pub reconcile_needed: bool,
}

impl FastChargeTrack {
    pub fn new() -> Self {
        Self {
            status: FastChargeStatus::Unknown,
            applied_target_ua: None,
            reconcile_needed: true,
        }
    }

    pub fn mark_applied(&mut self, target_ua: u32) {
        self.applied_target_ua = Some(target_ua);
        self.status = FastChargeStatus::Applied { target_ua };
        self.reconcile_needed = false;
    }

    pub fn mark_released(&mut self) {
        self.applied_target_ua = None;
        self.status = FastChargeStatus::Released;
        self.reconcile_needed = false;
    }

    pub fn mark_fault(&mut self, error: HardwareFault, now: Instant) {
        let retry = error.retry_policy();
        self.status = FastChargeStatus::Fault {
            error,
            failed_at: now,
            retry,
        };
        self.reconcile_needed = false;
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if let FastChargeStatus::Fault {
            failed_at,
            retry: FaultRetryPolicy::After(dur),
            ..
        } = self.status
        {
            Some(failed_at + dur)
        } else {
            None
        }
    }

    pub fn reset_on_disconnect(&mut self) {
        self.status = FastChargeStatus::Unknown;
        self.applied_target_ua = None;
        self.reconcile_needed = true;
    }

    pub fn convergence(&self) -> ConvergenceState {
        match self.status {
            FastChargeStatus::Fault { .. } => ConvergenceState::Fault,
            FastChargeStatus::Applied { .. } | FastChargeStatus::Released => {
                ConvergenceState::Converged
            }
            FastChargeStatus::Unknown => {
                if self.reconcile_needed {
                    ConvergenceState::Deferred
                } else {
                    ConvergenceState::Converged
                }
            }
        }
    }
}

/// Tracking status rekonsiliasi hardware gabungan yang mengisolasi domain charger, limit arus, dan fast-charge bypass.
#[derive(Debug)]
pub struct HardwareTrack {
    pub charger: ChargerActuatorTrack,
    pub current_limit: CurrentLimitTrack,
    pub fast_charge: FastChargeTrack,
}

impl HardwareTrack {
    pub fn new() -> Self {
        Self {
            charger: ChargerActuatorTrack::new(),
            current_limit: CurrentLimitTrack::new(),
            fast_charge: FastChargeTrack::new(),
        }
    }

    pub fn mark_verification_needed(&mut self) {
        self.charger.mark_verification_needed();
        self.current_limit.reconcile_needed = true;
        self.fast_charge.reconcile_needed = true;
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let mut min_d: Option<Instant> = None;
        for d in [
            self.charger.next_deadline(),
            self.current_limit.next_deadline(),
            self.fast_charge.next_deadline(),
        ]
        .into_iter()
        .flatten()
        {
            min_d = Some(min_d.map_or(d, |cur| cur.min(d)));
        }
        min_d
    }

    pub fn reset_on_disconnect(&mut self) {
        self.charger.reset_on_disconnect();
        self.current_limit.reset_on_disconnect();
        self.fast_charge.reset_on_disconnect();
    }

    pub fn overall_convergence(&self) -> ConvergenceState {
        self.charger
            .convergence()
            .combine(self.current_limit.convergence())
            .combine(self.fast_charge.convergence())
    }

    pub fn safety_fault(&self) -> Option<HardwareFault> {
        if let HardwareStatus::Fault { error, .. } = self.charger.status {
            if error.is_safety_fault() {
                return Some(error);
            }
        }
        None
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
#[allow(dead_code)]
pub enum CurrentReconcileResult {
    Skipped,
    Stable(Option<u32>),
    Changed(Option<u32>),
    Deferred,
    Failed(ChargerError),
}

/// Rekonsiliasi batas arus pengisian daya ke sysfs secara closed-loop, idempotent, terisolasi, dan patuh retry backoff.
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

    // 2. IDEMPOTENCY & VERIFICATION CHECK:
    // Jika status sudah Verified/Commanded dan applied_limit_ua sudah sama persis dengan desired_ua,
    // bersihkan flag reconcile_needed dan JANGAN tulis ulang ke sysfs kecuali bypass requested.
    if !opts.bypass_retry_delay
        && matches!(
            track.current_limit.status,
            CurrentLimitStatus::Verified { .. } | CurrentLimitStatus::Commanded { .. }
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
                // Closed-loop verification: read back if node exists
                let readback = control::read_fast_charge_current();
                let is_verified = match readback {
                    Some(actual_ua) if actual_ua == ua => true,
                    Some(actual_ua) => {
                        tracing::warn!(
                            target_ua = ua,
                            actual_ua,
                            "Current limit readback mismatch"
                        );
                        track
                            .current_limit
                            .mark_fault(HardwareFault::ReadbackMismatch, now);
                        return CurrentReconcileResult::Failed(ChargerError::HardwareError(
                            "Current limit readback mismatch",
                        ));
                    }
                    None => false, // Readback node not available on this kernel; status is Commanded
                };

                track.current_limit.mark_applied(Some(ua), is_verified);
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
        // HANYA tulis reset jika sebelumnya pernah dibatasi (is_some) atau status belum Verified/Commanded(None)
        if track.current_limit.applied_limit_ua.is_some()
            || !matches!(
                track.current_limit.status,
                CurrentLimitStatus::Verified { applied_ua: None }
                    | CurrentLimitStatus::Commanded { applied_ua: None }
            )
            || opts.bypass_retry_delay
        {
            match control::reset_fast_charge_current() {
                Ok(()) => {
                    let readback = control::read_fast_charge_current();
                    let is_verified = readback.is_some_and(|actual| actual >= 5_000_000);
                    track.current_limit.mark_applied(None, is_verified);
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

/// Rekonsiliasi Fast Charge & USB-PD Bypass ke sysfs secara idempotent, terisolasi, dan aman.
pub fn reconcile_fast_charge(
    desired_hw: DesiredHardwareState,
    policy: super::policy::FastChargePolicy,
    track: &mut HardwareTrack,
    opts: CurrentReconcileOptions,
    now: Instant,
) -> CurrentReconcileResult {
    // 0. GUARD: Jika Disconnected / NoChange, jangan lakukan mutasi fisik
    if desired_hw == DesiredHardwareState::NoChange {
        return CurrentReconcileResult::Skipped;
    }

    // 1. Deferral check
    let fault_retry_blocked = match &track.fast_charge.status {
        FastChargeStatus::Fault {
            failed_at,
            retry: FaultRetryPolicy::After(delay),
            ..
        } => !opts.bypass_retry_delay && now < *failed_at + *delay,

        FastChargeStatus::Fault {
            retry: FaultRetryPolicy::Never,
            ..
        } => true,

        _ => false,
    };

    if fault_retry_blocked {
        return CurrentReconcileResult::Deferred;
    }

    // 2. Evaluasi apakah fast charge bypass harus aktif
    let should_activate =
        desired_hw == DesiredHardwareState::ChargingEnabled && policy.is_active();
    let target_ua = policy.target_ua().unwrap_or(5_850_000);

    if should_activate {
        // IDEMPOTENCY: Jika sudah Applied dengan target yang sama persis, skip penulisan sysfs!
        if !opts.bypass_retry_delay
            && track.fast_charge.applied_target_ua == Some(target_ua)
            && matches!(track.fast_charge.status, FastChargeStatus::Applied { .. })
        {
            track.fast_charge.reconcile_needed = false;
            return CurrentReconcileResult::Stable(Some(target_ua));
        }

        match control::apply_fast_charge_bypass(true, target_ua) {
            Ok(()) => {
                let prev = track.fast_charge.applied_target_ua;
                track.fast_charge.mark_applied(target_ua);
                if prev != Some(target_ua) {
                    CurrentReconcileResult::Changed(Some(target_ua))
                } else {
                    CurrentReconcileResult::Stable(Some(target_ua))
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed applying fast charge bypass");
                track.fast_charge.mark_fault(HardwareFault::WriteFailed, now);
                CurrentReconcileResult::Failed(e)
            }
        }
    } else {
        // Should be Released / Inactive
        if track.fast_charge.applied_target_ua.is_some()
            || !matches!(track.fast_charge.status, FastChargeStatus::Released)
            || opts.bypass_retry_delay
        {
            match control::apply_fast_charge_bypass(false, 0) {
                Ok(()) => {
                    let prev = track.fast_charge.applied_target_ua;
                    track.fast_charge.mark_released();
                    if prev.is_some() {
                        CurrentReconcileResult::Changed(None)
                    } else {
                        CurrentReconcileResult::Stable(None)
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed releasing fast charge bypass");
                    track.fast_charge.mark_fault(HardwareFault::WriteFailed, now);
                    CurrentReconcileResult::Failed(e)
                }
            }
        } else {
            track.fast_charge.reconcile_needed = false;
            CurrentReconcileResult::Stable(None)
        }
    }
}

/// Global Safe Hardware State yang menjadi invariant keselamatan universal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SafeHardwareState {
    pub charge_path: control::ActualHardwareMode,
    pub current_limit_ua: Option<u32>,
    pub fast_charge: bool,
}

impl SafeHardwareState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            charge_path: control::ActualHardwareMode::ChargingDisabled,
            current_limit_ua: Some(500_000),
            fast_charge: false,
        }
    }
}

impl Default for SafeHardwareState {
    fn default() -> Self {
        Self::new()
    }
}
