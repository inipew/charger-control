Ya. Saya akan jadikan struktur ini sebagai versi yang benar-benar konsisten: terutama memperbaiki **ownership recovery, verification, scheduler ETA, offline transition, `Unmanaged`, dan event loop**.

Ada beberapa bug penting di versi sekarang:

1. `load_persistent_ownership()` dipulihkan lalu langsung `clear`, tetapi `HardwareController` tidak mengetahui bahwa hardware sedang/sempat dimiliki.
2. `verification_failed()` menjadwalkan retry berdasarkan `applied_target`, tetapi `force_apply = false`; ini bisa membuat retry tidak benar-benar terjadi.
3. `Offline` mengembalikan `current_target`, sehingga ketika charger dicabut controller tetap mempertahankan `ChargingEnabled`. Ini sebenarnya bisa benar jika ingin membiarkan hardware apa adanya, tetapi perlu dibedakan dengan `Unmanaged`.
4. `eta_to()` salah untuk kasus rate negatif/positif karena `distance.signum() == rate.signum()` tidak selalu berarti bergerak menuju threshold.
5. Scheduler menggunakan `thermal_cutoff * 10.0` walaupun `thermal_cutoff` sudah dikonversi ke °C, sehingga cukup membingungkan dan rawan salah unit.
6. `current_ma` disimpan sebagai `i32`, padahal pembacaan aslinya `f32`; kehilangan presisi tidak terlalu fatal tetapi tidak perlu.
7. `is_plugged_in()` fallback `Ok(true)` adalah **unsafe** untuk state machine charging-control.
8. `CachedReader` hanya memilih **satu** current node, padahal node pertama bisa valid tetapi stale/unsupported.
9. `apply_target()` tidak membatalkan verification lama ketika target berubah di dalam fungsi.
10. Setelah `verify()` sukses, `failed_attempts` tidak di-reset.
11. Poll loop saat tidak ada event bisa melakukan evaluasi terlalu sering karena alur `should_evaluate` dan `continue` kurang jelas.
12. Netlink event hanya diperhatikan untuk `ACTION=change`, tetapi tidak memfilter device power-supply secara lebih spesifik—masih acceptable, tetapi kita bisa buat lebih robust.

Berikut versi yang saya sarankan.

```rust
// monitor.rs

pub mod snapshot {
    use charger_core::battery::reader::BatteryStatus;
    use std::time::Instant;

    #[derive(Clone, Debug)]
    pub struct SensorSnapshot {
        pub capacity_pct: Option<u8>,
        pub temp_dc: Option<i32>,
        pub current_ma: Option<f32>,
        pub status: Option<BatteryStatus>,
        pub online: Option<bool>,
        pub ts: Instant,
    }
}

pub mod hardware {
    use super::snapshot::SensorSnapshot;
    use charger_core::battery::control;
    use std::path::Path;
    use std::time::{Duration, Instant};

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

    const STATE_FILE: &str = "/data/adb/charger-control/ownership.state";

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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Ownership {
        NotOwned,
        Owned { original_charging: bool },
    }

    struct Verification {
        generation: u64,
        target: HardwareTarget,
        deadline: Instant,
    }

    pub fn load_persistent_ownership() -> Option<bool> {
        let content = std::fs::read_to_string(STATE_FILE).ok()?;

        match content.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }

    pub fn save_persistent_ownership(original: bool) {
        let path = Path::new(STATE_FILE);

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::error!(
                    "Failed to create ownership state directory: {}",
                    e
                );
                return;
            }
        }

        let tmp = format!("{}.tmp", STATE_FILE);
        let value = if original { "1" } else { "0" };

        if let Err(e) = std::fs::write(&tmp, value) {
            tracing::error!(
                "Failed to write ownership state: {}",
                e
            );
            return;
        }

        if let Err(e) = std::fs::rename(&tmp, STATE_FILE) {
            tracing::error!(
                "Failed to commit ownership state: {}",
                e
            );

            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn clear_persistent_ownership() {
        match std::fs::remove_file(STATE_FILE) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::error!(
                    "Failed to clear ownership state: {}",
                    e
                );
            }
        }
    }

    pub fn recover_stale_ownership() {
        let Some(original) = load_persistent_ownership() else {
            return;
        };

        tracing::warn!(
            "Found stale hardware ownership state. \
             Restoring original charging state: {}",
            original
        );

        match control::set_charging(original) {
            Ok(()) => {
                tracing::info!(
                    "Stale ownership recovered successfully."
                );

                clear_persistent_ownership();
            }

            Err(e) => {
                tracing::error!(
                    "Failed to recover stale ownership: {}. \
                     Keeping state file for next restart.",
                    e
                );
            }
        }
    }

    pub struct HardwareController {
        pub desired_target: HardwareTarget,
        pub applied_target: HardwareTarget,
        pub sync: SyncState,
        pub force_apply: bool,
        pub ownership: Ownership,

        generation: u64,
        verification: Option<Verification>,
        verification_failures: u8,
        failed_attempts: u8,
        retry_at: Option<Instant>,
    }

    impl Default for HardwareController {
        fn default() -> Self {
            Self::new()
        }
    }

    impl HardwareController {
        pub fn new() -> Self {
            Self {
                desired_target: HardwareTarget::Unmanaged,
                applied_target: HardwareTarget::Unmanaged,
                sync: SyncState::Unknown,
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
            if self.desired_target != target {
                return true;
            }

            if self.force_apply {
                if self.sync == SyncState::Failed {
                    return self
                        .retry_at
                        .is_none_or(|deadline| now >= deadline);
                }

                return true;
            }

            self.applied_target != target
        }

        pub fn apply_target(&mut self, target: HardwareTarget) {
            self.desired_target = target;

            match target {
                HardwareTarget::ChargingEnabled => {
                    self.apply_charging(true, target);
                }

                HardwareTarget::ChargingDisabled => {
                    self.apply_charging(false, target);
                }

                HardwareTarget::Unmanaged => {
                    self.release_ownership();
                }
            }
        }

        fn apply_charging(
            &mut self,
            enable: bool,
            target: HardwareTarget,
        ) {
            /*
             * First write:
             * remember what the hardware looked like before
             * the daemon started controlling it.
             */
            if self.ownership == Ownership::NotOwned {
                match control::is_charging_enabled() {
                    Ok(original) => {
                        tracing::info!(
                            "Taking hardware ownership. \
                             Original charging state: {}",
                            original
                        );

                        save_persistent_ownership(original);

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
                        return;
                    }
                }
            }

            match control::set_charging(enable) {
                Ok(()) => {
                    tracing::info!(
                        "Hardware charging set to {}",
                        enable
                    );

                    self.mark_apply_success(target);
                }

                Err(e) => {
                    tracing::error!(
                        "Failed to set charging={} : {}",
                        enable,
                        e
                    );

                    self.mark_apply_failed();
                }
            }
        }

        fn release_ownership(&mut self) {
            self.invalidate_verification();

            let original = match self.ownership {
                Ownership::Owned {
                    original_charging,
                } => Some(original_charging),

                Ownership::NotOwned => None,
            };

            match original {
                Some(original_charging) => {
                    match control::set_charging(original_charging) {
                        Ok(()) => {
                            tracing::info!(
                                "Released ownership and restored \
                                 original charging state: {}",
                                original_charging
                            );

                            clear_persistent_ownership();

                            self.ownership = Ownership::NotOwned;
                            self.applied_target =
                                HardwareTarget::Unmanaged;
                            self.desired_target =
                                HardwareTarget::Unmanaged;

                            self.sync = SyncState::Synced;
                            self.force_apply = false;
                            self.failed_attempts = 0;
                        }

                        Err(e) => {
                            tracing::error!(
                                "Failed to restore original charging \
                                 state: {}",
                                e
                            );

                            /*
                             * Keep ownership + state file.
                             * We MUST NOT pretend that ownership was
                             * released if restoration failed.
                             */
                            self.sync = SyncState::Failed;
                            self.force_apply = true;
                            self.retry_at =
                                Some(Instant::now() + RETRY_BACKOFF[0]);
                        }
                    }
                }

                None => {
                    self.applied_target =
                        HardwareTarget::Unmanaged;

                    self.desired_target =
                        HardwareTarget::Unmanaged;

                    self.sync = SyncState::Synced;
                    self.force_apply = false;
                }
            }
        }

        fn mark_apply_success(
            &mut self,
            target: HardwareTarget,
        ) {
            self.applied_target = target;
            self.force_apply = false;
            self.sync = SyncState::Pending;

            self.verification_failures = 0;
            self.retry_at = None;

            self.generation =
                self.generation.wrapping_add(1);

            self.verification = Some(Verification {
                generation: self.generation,
                target,
                deadline: Instant::now() + VERIFY_DELAYS[0],
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
                Some(Instant::now() + RETRY_BACKOFF[0]);
        }

        pub fn verification_due(&self, now: Instant) -> bool {
            self.verification
                .as_ref()
                .is_some_and(|v| now >= v.deadline)
        }

        pub fn next_deadline(&self) -> Option<Instant> {
            self.verification.as_ref().map(|v| v.deadline)
        }

        pub fn retry_due(&self, now: Instant) -> bool {
            self.sync == SyncState::Failed
                && self
                    .retry_at
                    .is_some_and(|deadline| now >= deadline)
        }

        pub fn verify(
            &mut self,
            snapshot: &SensorSnapshot,
        ) {
            let Some(v) = self.verification.as_ref() else {
                return;
            };

            if v.generation != self.generation {
                self.verification = None;
                return;
            }

            let target = v.target;

            let success = match target {
                HardwareTarget::ChargingEnabled => {
                    match control::is_charging_enabled() {
                        Ok(true) => {
                            /*
                             * If online information exists, charger must
                             * actually be online. If it doesn't exist,
                             * don't reject solely because the sensor is
                             * unavailable.
                             */
                            snapshot.online != Some(false)
                        }

                        Ok(false) => false,

                        Err(e) => {
                            tracing::warn!(
                                "Unable to verify charging state: {}",
                                e
                            );

                            false
                        }
                    }
                }

                HardwareTarget::ChargingDisabled => {
                    let current_safe = snapshot
                        .current_ma
                        .is_none_or(|current| current <= 100.0);

                    match control::is_charging_enabled() {
                        Ok(false) => current_safe,

                        Ok(true) => false,

                        Err(e) => {
                            tracing::warn!(
                                "Unable to verify charging state: {}",
                                e
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
                self.verification = None;
                self.verification_failures = 0;
                self.failed_attempts = 0;
                self.retry_at = None;
                self.force_apply = false;
            } else {
                self.verification_failed();
            }
        }

        fn verification_failed(&mut self) {
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

                /*
                 * Keep force_apply=true.
                 * retry_due()/needs_apply() will permit the next attempt.
                 */
                self.force_apply = true;
                self.retry_at = Some(
                    Instant::now() + backoff
                );

                return;
            }

            let index =
                (self.verification_failures as usize)
                    .min(VERIFY_DELAYS.len() - 1);

            self.verification = Some(Verification {
                generation: self.generation,
                target: self.applied_target,
                deadline: Instant::now()
                    + VERIFY_DELAYS[index],
            });

            self.sync = SyncState::Pending;
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

            match control::set_charging(original_charging) {
                Ok(()) => {
                    tracing::info!(
                        "Shutdown: restored original charging state: {}",
                        original_charging
                    );

                    clear_persistent_ownership();

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

                Err(e) => {
                    /*
                     * IMPORTANT:
                     * Do not clear ownership state if restoration failed.
                     * The next daemon instance can recover it.
                     */
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
}

pub mod decision {
    use super::hardware::HardwareTarget;
    use super::snapshot::SensorSnapshot;
    use charger_core::battery::reader::BatteryStatus;
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
        fn fmt(
            &self,
            f: &mut fmt::Formatter<'_>,
        ) -> fmt::Result {
            let value = match self {
                Self::DaemonDisabled =>
                    "daemon_disabled",

                Self::ChargerOffline =>
                    "charger_offline",

                Self::NormalCharging =>
                    "normal_charging",

                Self::ChargeLimitReached =>
                    "charge_limit_reached",

                Self::WaitingForLimitResume =>
                    "waiting_for_limit_resume",

                Self::ThermalLimitReached =>
                    "thermal_limit_reached",

                Self::WaitingForThermalResume =>
                    "waiting_for_thermal_resume",

                Self::SensorFault =>
                    "sensor_fault",

                Self::FaultRecovering =>
                    "fault_recovering",

                Self::CapacityUnavailable =>
                    "capacity_unavailable",
            };

            write!(f, "{value}")
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct Decision {
        pub policy: ChargePolicyState,
        pub target: HardwareTarget,
        pub reason: DecisionReason,
    }

    pub struct DecisionEngine {
        pub policy: ChargePolicyState,
        fault_recovery_reads: u8,
    }

    impl Default for DecisionEngine {
        fn default() -> Self {
            Self::new()
        }
    }

    impl DecisionEngine {
        pub fn new() -> Self {
            Self {
                policy: ChargePolicyState::Charging,
                fault_recovery_reads: 0,
            }
        }

        pub fn evaluate(
            &mut self,
            snapshot: &SensorSnapshot,
            cfg: &Config,
        ) -> Decision {
            /*
             * 1. Daemon disabled
             */
            if !cfg.enabled {
                self.policy =
                    ChargePolicyState::Disabled;

                self.fault_recovery_reads = 0;

                return Self::decision(
                    self.policy,
                    HardwareTarget::Unmanaged,
                    DecisionReason::DaemonDisabled,
                );
            }

            /*
             * 2. Charger physically disconnected
             *
             * Do NOT touch hardware here.
             *
             * This is deliberately Unmanaged so the daemon doesn't
             * continuously write charging nodes while unplugged.
             */
            if snapshot.online == Some(false) {
                self.policy =
                    ChargePolicyState::Offline;

                self.fault_recovery_reads = 0;

                return Self::decision(
                    self.policy,
                    HardwareTarget::Unmanaged,
                    DecisionReason::ChargerOffline,
                );
            }

            /*
             * 3. Validate mandatory sensors.
             */
            let sensors_valid =
                snapshot.capacity_pct.is_some()
                    && snapshot.temp_dc.is_some()
                    && snapshot.online.is_some()
                    && snapshot.status.is_some();

            if !sensors_valid {
                self.policy =
                    ChargePolicyState::Fault;

                self.fault_recovery_reads = 0;

                let reason =
                    if snapshot.capacity_pct.is_none() {
                        DecisionReason::CapacityUnavailable
                    } else {
                        DecisionReason::SensorFault
                    };

                return Self::decision(
                    self.policy,
                    HardwareTarget::ChargingDisabled,
                    reason,
                );
            }

            /*
             * 4. Fault recovery.
             */
            if self.policy == ChargePolicyState::Fault {
                self.fault_recovery_reads =
                    self.fault_recovery_reads
                        .saturating_add(1);

                if self.fault_recovery_reads
                    < FAULT_RECOVERY_READS
                {
                    return Self::decision(
                        ChargePolicyState::Fault,
                        HardwareTarget::ChargingDisabled,
                        DecisionReason::FaultRecovering,
                    );
                }

                self.fault_recovery_reads = 0;

                tracing::info!(
                    "Battery sensors recovered."
                );
            }

            let capacity =
                snapshot.capacity_pct.unwrap();

            let temp_dc =
                snapshot.temp_dc.unwrap();

            /*
             * 5. Thermal hysteresis.
             */
            let thermal_max =
                cfg.max_temp_dc;

            let hysteresis =
                cfg.thermal_resume_hysteresis_dc
                    .clamp(
                        1,
                        thermal_max
                            .saturating_sub(1)
                            .max(1),
                    );

            let thermal_resume =
                thermal_max
                    .saturating_sub(hysteresis);

            let thermal_cutoff =
                if self.policy
                    == ChargePolicyState::ThermalCutoff
                {
                    temp_dc > thermal_resume
                } else {
                    temp_dc >= thermal_max
                };

            /*
             * 6. Charge-limit hysteresis.
             */
            let limit =
                cfg.charge_limit;

            let resume =
                if cfg.resume_limit > 0
                    && cfg.resume_limit < limit
                {
                    cfg.resume_limit
                } else {
                    limit.saturating_sub(2)
                };

            let limit_reached =
                if self.policy
                    == ChargePolicyState::LimitReached
                {
                    capacity > resume
                } else {
                    capacity >= limit
                };

            /*
             * 7. Priority:
             *
             * thermal > charge limit > normal charging
             */
            if thermal_cutoff {
                self.policy =
                    ChargePolicyState::ThermalCutoff;

                let reason =
                    if temp_dc >= thermal_max {
                        DecisionReason::ThermalLimitReached
                    } else {
                        DecisionReason::WaitingForThermalResume
                    };

                return Self::decision(
                    self.policy,
                    HardwareTarget::ChargingDisabled,
                    reason,
                );
            }

            if limit_reached {
                self.policy =
                    ChargePolicyState::LimitReached;

                let reason =
                    if capacity >= limit {
                        DecisionReason::ChargeLimitReached
                    } else {
                        DecisionReason::WaitingForLimitResume
                    };

                return Self::decision(
                    self.policy,
                    HardwareTarget::ChargingDisabled,
                    reason,
                );
            }

            self.policy =
                ChargePolicyState::Charging;

            Self::decision(
                self.policy,
                HardwareTarget::ChargingEnabled,
                DecisionReason::NormalCharging,
            )
        }

        fn decision(
            policy: ChargePolicyState,
            target: HardwareTarget,
            reason: DecisionReason,
        ) -> Decision {
            Decision {
                policy,
                target,
                reason,
            }
        }
    }
}

pub mod netlink {
    use std::os::fd::{
        AsRawFd,
        FromRawFd,
        OwnedFd,
    };
    use std::time::{
        Duration,
        Instant,
    };

    const INITIAL_BACKOFF: Duration =
        Duration::from_secs(1);

    const MAX_BACKOFF: Duration =
        Duration::from_secs(60);

    const DEBOUNCE: Duration =
        Duration::from_millis(250);

    pub struct NetlinkMonitor {
        socket: Option<OwnedFd>,
        reconnect_at: Option<Instant>,
        backoff: Duration,
        debounce_target: Option<Instant>,
    }

    impl Default for NetlinkMonitor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl NetlinkMonitor {
        pub fn new() -> Self {
            let mut monitor = Self {
                socket: None,
                reconnect_at: None,
                backoff: INITIAL_BACKOFF,
                debounce_target: None,
            };

            monitor.try_reconnect(Instant::now());

            monitor
        }

        pub fn is_connected(&self) -> bool {
            self.socket.is_some()
        }

        pub fn as_raw_fd(&self) -> Option<i32> {
            self.socket
                .as_ref()
                .map(|fd| fd.as_raw_fd())
        }

        pub fn disconnect(&mut self) {
            self.socket = None;
        }

        pub fn schedule_reconnect(
            &mut self,
            now: Instant,
        ) {
            /*
             * Don't keep pushing reconnect further into the future
             * if already scheduled.
             */
            if self.reconnect_at.is_none() {
                self.reconnect_at =
                    Some(now + self.backoff);
            }

            self.backoff =
                (self.backoff * 2)
                    .min(MAX_BACKOFF);
        }

        pub fn should_reconnect(
            &self,
            now: Instant,
        ) -> bool {
            if self.socket.is_some() {
                return false;
            }

            self.reconnect_at
                .map_or(true, |deadline| now >= deadline)
        }

        pub fn try_reconnect(
            &mut self,
            now: Instant,
        ) -> bool {
            match Self::create_netlink_socket() {
                Ok(socket) => {
                    tracing::info!(
                        "Netlink power-supply monitor connected."
                    );

                    self.socket = Some(socket);
                    self.reconnect_at = None;
                    self.backoff = INITIAL_BACKOFF;

                    true
                }

                Err(e) => {
                    tracing::warn!(
                        "Netlink connection failed: {}",
                        e
                    );

                    self.reconnect_at =
                        Some(now + self.backoff);

                    self.backoff =
                        (self.backoff * 2)
                            .min(MAX_BACKOFF);

                    false
                }
            }
        }

        fn create_netlink_socket()
            -> std::io::Result<OwnedFd>
        {
            let fd = unsafe {
                libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_RAW,
                    libc::NETLINK_KOBJECT_UEVENT,
                )
            };

            if fd < 0 {
                return Err(
                    std::io::Error::last_os_error()
                );
            }

            let mut addr:
                libc::sockaddr_nl =
                unsafe { std::mem::zeroed() };

            addr.nl_family =
                libc::AF_NETLINK
                    as libc::sa_family_t;

            addr.nl_pid = 0;
            addr.nl_groups = 1;

            let result = unsafe {
                libc::bind(
                    fd,
                    &addr as *const _
                        as *const libc::sockaddr,
                    std::mem::size_of::<
                        libc::sockaddr_nl
                    >() as u32,
                )
            };

            if result < 0 {
                let error =
                    std::io::Error::last_os_error();

                unsafe {
                    libc::close(fd);
                }

                return Err(error);
            }

            Ok(unsafe {
                OwnedFd::from_raw_fd(fd)
            })
        }

        pub fn handle_events(
            &mut self,
            now: Instant,
        ) {
            let Some(fd) = self.as_raw_fd()
            else {
                return;
            };

            let mut buffer = [0u8; 4096];
            let mut changed = false;

            loop {
                let n = unsafe {
                    libc::recv(
                        fd,
                        buffer.as_mut_ptr()
                            as *mut libc::c_void,
                        buffer.len(),
                        libc::MSG_DONTWAIT,
                    )
                };

                if n < 0 {
                    let error =
                        std::io::Error::last_os_error();

                    match error.kind() {
                        std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted => {
                            break;
                        }

                        _ => {
                            tracing::error!(
                                "Netlink recv failed: {}",
                                error
                            );

                            self.disconnect();

                            self.reconnect_at =
                                Some(now + self.backoff);

                            return;
                        }
                    }
                }

                if n == 0 {
                    break;
                }

                let packet =
                    &buffer[..n as usize];

                if Self::contains(
                    packet,
                    b"SUBSYSTEM=power_supply",
                ) && Self::contains(
                    packet,
                    b"ACTION=change",
                ) {
                    changed = true;
                }
            }

            if changed {
                self.debounce_target =
                    Some(now + DEBOUNCE);
            }
        }

        pub fn debounce_due(
            &mut self,
            now: Instant,
        ) -> bool {
            if self
                .debounce_target
                .is_some_and(|deadline| now >= deadline)
            {
                self.debounce_target = None;
                return true;
            }

            false
        }

        pub fn next_deadline(
            &self,
        ) -> Option<Instant> {
            match (
                self.debounce_target,
                self.reconnect_at,
            ) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }

        fn contains(
            haystack: &[u8],
            needle: &[u8],
        ) -> bool {
            haystack
                .windows(needle.len())
                .any(|window| window == needle)
        }
    }
}

pub mod scheduler {
    use super::snapshot::SensorSnapshot;
    use charger_core::config::schema::Config;
    use std::collections::VecDeque;
    use std::time::Duration;

    const MIN_INTERVAL: Duration =
        Duration::from_secs(2);

    const MAX_INTERVAL: Duration =
        Duration::from_secs(90);

    const UNPLUGGED_HEARTBEAT: Duration =
        Duration::from_secs(600);

    const UNPLUGGED_HEARTBEAT_NO_NETLINK: Duration =
        Duration::from_secs(30);

    const HISTORY_LEN: usize = 6;

    const EMA_ALPHA: f32 = 0.35;

    const CAPACITY_SAFETY_FACTOR: f32 = 0.25;

    const THERMAL_SAFETY_FACTOR: f32 = 0.15;

    pub struct AdaptiveScheduler {
        limit: f32,
        resume_limit: f32,
        thermal_cutoff_dc: f32,

        history: VecDeque<SensorSnapshot>,

        cap_rate_ema: Option<f32>,
        temp_rate_ema: Option<f32>,

        pub last_interval: Duration,
    }

    impl AdaptiveScheduler {
        pub fn new(
            limit: u8,
            resume_limit: u8,
            thermal_cutoff_dc: i32,
        ) -> Self {
            Self {
                limit: limit as f32,
                resume_limit: resume_limit as f32,
                thermal_cutoff_dc:
                    thermal_cutoff_dc as f32,

                history: VecDeque::new(),

                cap_rate_ema: None,
                temp_rate_ema: None,

                last_interval: MIN_INTERVAL,
            }
        }

        pub fn sync_config(
            &mut self,
            cfg: &Config,
        ) {
            let new_limit =
                cfg.charge_limit as f32;

            let new_resume =
                if cfg.resume_limit > 0
                    && cfg.resume_limit < cfg.charge_limit
                {
                    cfg.resume_limit as f32
                } else {
                    cfg.charge_limit
                        .saturating_sub(2)
                        as f32
                };

            let new_thermal =
                cfg.max_temp_dc as f32;

            if (self.limit - new_limit).abs()
                > f32::EPSILON
                || (self.resume_limit - new_resume).abs()
                    > f32::EPSILON
                || (self.thermal_cutoff_dc
                    - new_thermal)
                    .abs()
                    > f32::EPSILON
            {
                self.limit = new_limit;
                self.resume_limit = new_resume;
                self.thermal_cutoff_dc =
                    new_thermal;

                self.reset_prediction();
            }
        }

        pub fn observe(
            &mut self,
            snapshot: &SensorSnapshot,
        ) {
            if let Some(previous) =
                self.history.back()
            {
                let dt = snapshot
                    .ts
                    .saturating_duration_since(previous.ts)
                    .as_secs_f32();

                if dt >= 0.5 {
                    if let (
                        Some(cap),
                        Some(previous_cap),
                    ) = (
                        snapshot.capacity_pct,
                        previous.capacity_pct,
                    ) {
                        let rate =
                            (cap as f32
                                - previous_cap as f32)
                                / dt;

                        self.cap_rate_ema =
                            Some(ema(
                                self.cap_rate_ema,
                                rate,
                            ));
                    }

                    if let (
                        Some(temp),
                        Some(previous_temp),
                    ) = (
                        snapshot.temp_dc,
                        previous.temp_dc,
                    ) {
                        let rate =
                            (temp as f32
                                - previous_temp as f32)
                                / dt;

                        self.temp_rate_ema =
                            Some(ema(
                                self.temp_rate_ema,
                                rate,
                            ));
                    }
                }
            }

            self.history
                .push_back(snapshot.clone());

            while self.history.len() > HISTORY_LEN {
                self.history.pop_front();
            }
        }

        pub fn reset_prediction(&mut self) {
            self.history.clear();
            self.cap_rate_ema = None;
            self.temp_rate_ema = None;
            self.last_interval = MIN_INTERVAL;
        }

        pub fn next_interval(
            &mut self,
            snapshot: &SensorSnapshot,
            netlink_alive: bool,
        ) -> Duration {
            if snapshot.online == Some(false) {
                self.last_interval =
                    if netlink_alive {
                        UNPLUGGED_HEARTBEAT
                    } else {
                        UNPLUGGED_HEARTBEAT_NO_NETLINK
                    };

                return self.last_interval;
            }

            let target =
                match self.cap_rate_ema {
                    Some(rate) if rate < -0.01 =>
                        self.resume_limit,

                    _ => self.limit,
                };

            let cap_eta = Self::eta_to(
                snapshot
                    .capacity_pct
                    .map(|v| v as f32),
                target,
                self.cap_rate_ema,
                CAPACITY_SAFETY_FACTOR,
            );

            let temp_eta = Self::eta_to(
                snapshot
                    .temp_dc
                    .map(|v| v as f32),
                self.thermal_cutoff_dc,
                self.temp_rate_ema,
                THERMAL_SAFETY_FACTOR,
            );

            let mut interval =
                match (cap_eta, temp_eta) {
                    (Some(cap), Some(temp)) =>
                        cap.min(temp),

                    (Some(cap), None) => cap,

                    (None, Some(temp)) => temp,

                    (None, None) =>
                        self.fallback_interval(snapshot),
                };

            /*
             * Explicit thermal proximity guard.
             *
             * temp_dc uses deci-degree Celsius.
             */
            if let Some(temp) =
                snapshot.temp_dc
            {
                let margin =
                    self.thermal_cutoff_dc
                        - temp as f32;

                if margin <= 30.0 {
                    interval =
                        interval.min(
                            Duration::from_secs(5)
                        );
                } else if margin <= 50.0 {
                    interval =
                        interval.min(
                            Duration::from_secs(15)
                        );
                }
            }

            self.last_interval =
                interval.clamp(
                    MIN_INTERVAL,
                    MAX_INTERVAL,
                );

            self.last_interval
        }

        fn eta_to(
            current: Option<f32>,
            threshold: f32,
            rate: Option<f32>,
            safety: f32,
        ) -> Option<Duration> {
            let current = current?;
            let rate = rate?;

            if rate.abs() <= 0.01 {
                return None;
            }

            /*
             * Determine whether the current value is actually moving
             * toward the threshold.
             */
            let distance =
                threshold - current;

            if distance.abs() <= 0.01 {
                return Some(MIN_INTERVAL);
            }

            /*
             * Positive rate moves upward.
             * Negative rate moves downward.
             */
            let moving_toward =
                (distance > 0.0 && rate > 0.0)
                    || (distance < 0.0 && rate < 0.0);

            if !moving_toward {
                return None;
            }

            let seconds =
                (distance.abs() / rate.abs())
                    * safety;

            Some(
                Duration::from_secs_f32(
                    seconds.max(0.0)
                )
            )
        }

        fn fallback_interval(
            &self,
            snapshot: &SensorSnapshot,
        ) -> Duration {
            let capacity_fraction =
                snapshot.capacity_pct.map(|capacity| {
                    let capacity =
                        capacity as f32;

                    if capacity >= self.limit {
                        0.0
                    } else {
                        (self.limit - capacity)
                            / self.limit.max(1.0)
                    }
                    .clamp(0.0, 1.0)
                });

            let thermal_fraction =
                snapshot.temp_dc.map(|temp| {
                    let margin =
                        self.thermal_cutoff_dc
                            - temp as f32;

                    (margin
                        / self.thermal_cutoff_dc.max(1.0))
                        .clamp(0.0, 1.0)
                });

            let fraction =
                match (
                    capacity_fraction,
                    thermal_fraction,
                ) {
                    (Some(a), Some(b)) =>
                        a.min(b),

                    (Some(a), None) => a,

                    (None, Some(b)) => b,

                    (None, None) => 1.0,
                };

            MIN_INTERVAL
                + (MAX_INTERVAL - MIN_INTERVAL)
                    .mul_f32(fraction)
        }
    }

    fn ema(
        previous: Option<f32>,
        sample: f32,
    ) -> f32 {
        match previous {
            Some(previous) =>
                EMA_ALPHA * sample
                    + (1.0 - EMA_ALPHA) * previous,

            None => sample,
        }
    }
}

use charger_core::{
    battery::reader::CachedReader,
    config::schema::Config,
};

use decision::DecisionEngine;
use hardware::HardwareController;
use netlink::NetlinkMonitor;
use scheduler::AdaptiveScheduler;
use snapshot::SensorSnapshot;

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{
    Arc,
    RwLock,
};
use std::time::{
    Duration,
    Instant,
};

pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    rx: UnixDatagram,
) {
    tracing::info!(
        "Charging monitor started."
    );

    /*
     * Recover state BEFORE constructing normal controller state.
     *
     * If previous daemon died while owning the hardware,
     * restore the original state first.
     */
    hardware::recover_stale_ownership();

    let mut battery_reader =
        CachedReader::new();

    let mut netlink =
        NetlinkMonitor::new();

    let mut engine =
        DecisionEngine::new();

    let mut hardware =
        HardwareController::new();

    let initial_cfg =
        config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

    let effective_resume =
        if initial_cfg.resume_limit > 0
            && initial_cfg.resume_limit
                < initial_cfg.charge_limit
        {
            initial_cfg.resume_limit
        } else {
            initial_cfg
                .charge_limit
                .saturating_sub(2)
        };

    let mut scheduler =
        AdaptiveScheduler::new(
            initial_cfg.charge_limit,
            effective_resume,
            initial_cfg.max_temp_dc,
        );

    let mut pfds = [
        libc::pollfd {
            fd: rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        },
    ];

    loop {
        let now = Instant::now();

        let cfg =
            config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

        scheduler.sync_config(&cfg);

        /*
         * Snapshot.
         */
        let snapshot =
            SensorSnapshot {
                capacity_pct:
                    battery_reader
                        .read_capacity()
                        .ok(),

                temp_dc:
                    battery_reader
                        .read_temperature_dc()
                        .ok(),

                current_ma:
                    battery_reader
                        .read_current_ma()
                        .ok(),

                status:
                    battery_reader
                        .read_status()
                        .ok(),

                online:
                    battery_reader
                        .is_plugged_in()
                        .ok(),

                ts: Instant::now(),
            };

        /*
         * Hardware verification must happen BEFORE decision
         * only if verification is due.
         */
        if hardware.verification_due(now) {
            hardware.verify(&snapshot);
        }

        /*
         * Feed scheduler only when hardware state is stable.
         */
        if hardware.sync
            == hardware::SyncState::Synced
        {
            scheduler.observe(&snapshot);
        }

        /*
         * Policy decision.
         */
        let decision =
            engine.evaluate(
                &snapshot,
                &cfg,
            );

        tracing::debug!(
            "Decision: policy={:?}, target={:?}, reason={}",
            decision.policy,
            decision.target,
            decision.reason
        );

        /*
         * Target changed.
         */
        if decision.target
            != hardware.desired_target
        {
            tracing::info!(
                "Hardware target changed: {:?} -> {:?}",
                hardware.desired_target,
                decision.target
            );

            hardware.set_desired_target(
                decision.target
            );

            /*
             * Config/policy change should reset prediction.
             */
            scheduler.reset_prediction();
        }

        /*
         * Retry failed hardware synchronization.
         */
        if hardware.retry_due(now) {
            tracing::warn!(
                "Retrying failed hardware synchronization."
            );

            hardware.force_apply = true;
        }

        /*
         * Apply desired target.
         */
        if hardware.needs_apply(
            decision.target,
            now,
        ) {
            tracing::info!(
                "Applying hardware target: {:?} \
                 (sync={:?}, force={})",
                decision.target,
                hardware.sync,
                hardware.force_apply
            );

            hardware.apply_target(
                decision.target
            );
        }

        /*
         * Netlink reconnect.
         */
        if netlink.should_reconnect(now) {
            netlink.try_reconnect(now);
        }

        /*
         * Determine next normal scheduler wakeup.
         */
        let mut timeout =
            scheduler.next_interval(
                &snapshot,
                netlink.is_connected(),
            );

        /*
         * Failed hardware state needs to be retried
         * promptly when retry deadline arrives.
         */
        if let Some(retry_at) =
            hardware.next_deadline()
        {
            if hardware.sync
                == hardware::SyncState::Failed
            {
                let remaining =
                    retry_at.saturating_duration_since(
                        Instant::now()
                    );

                timeout =
                    timeout.min(remaining);
            }
        }

        let target_wake =
            Instant::now() + timeout;

        /*
         * Inner event loop.
         */
        loop {
            let loop_now =
                Instant::now();

            if loop_now >= target_wake {
                break;
            }

            let mut next_wake =
                target_wake;

            /*
             * Netlink deadline.
             */
            if let Some(deadline) =
                netlink.next_deadline()
            {
                if deadline <= loop_now {
                    if netlink
                        .debounce_due(loop_now)
                    {
                        break;
                    }

                    if netlink
                        .should_reconnect(loop_now)
                    {
                        netlink
                            .try_reconnect(
                                loop_now
                            );
                    }
                }

                if let Some(deadline) =
                    netlink.next_deadline()
                {
                    if deadline > loop_now {
                        next_wake =
                            next_wake.min(
                                deadline
                            );
                    }
                }
            }

            /*
             * Hardware verification deadline.
             */
            if let Some(deadline) =
                hardware.next_deadline()
            {
                if deadline <= loop_now {
                    break;
                }

                next_wake =
                    next_wake.min(deadline);
            }

            /*
             * Hardware retry deadline.
             *
             * next_deadline() above is verification only,
             * so explicitly check retry state via a short
             * poll interval.
             */
            if hardware.sync
                == hardware::SyncState::Failed
            {
                /*
                 * We intentionally cap failed-state polling.
                 * This avoids spinning while still making
                 * recovery responsive.
                 */
                next_wake =
                    next_wake.min(
                        loop_now
                            + Duration::from_secs(2)
                    );
            }

            let remaining =
                next_wake
                    .saturating_duration_since(
                        loop_now
                    );

            let timeout_ms =
                remaining
                    .as_millis()
                    .clamp(
                        1,
                        i32::MAX as u128,
                    ) as i32;

            /*
             * Prepare pollfds.
             */
            pfds[0].revents = 0;

            let mut nfds = 1;

            if let Some(fd) =
                netlink.as_raw_fd()
            {
                pfds[1].fd = fd;
                pfds[1].events =
                    libc::POLLIN;
                pfds[1].revents = 0;

                nfds = 2;
            } else {
                pfds[1].fd = -1;
                pfds[1].events = 0;
                pfds[1].revents = 0;
            }

            let ret = unsafe {
                libc::poll(
                    pfds.as_mut_ptr(),
                    nfds as libc::nfds_t,
                    timeout_ms,
                )
            };

            if ret < 0 {
                let error =
                    std::io::Error::last_os_error();

                if error.kind()
                    == std::io::ErrorKind::Interrupted
                {
                    continue;
                }

                tracing::error!(
                    "poll() failed: {}",
                    error
                );

                break;
            }

            if ret == 0 {
                break;
            }

            /*
             * IPC.
             */
            let ipc_events =
                pfds[0].revents;

            if ipc_events
                & (libc::POLLERR
                    | libc::POLLHUP
                    | libc::POLLNVAL)
                != 0
            {
                tracing::error!(
                    "IPC channel closed. \
                     Restoring hardware before exit."
                );

                hardware.shutdown_restore();

                return;
            }

            if ipc_events
                & libc::POLLIN
                != 0
            {
                let mut command =
                    [0u8; 1];

                match rx.recv(&mut command) {
                    Ok(1) => {
                        match command[0] {
                            /*
                             * Reload config.
                             */
                            1 => {
                                tracing::info!(
                                    "Configuration reload requested."
                                );

                                hardware
                                    .invalidate_verification();

                                hardware.force_apply =
                                    true;

                                scheduler
                                    .reset_prediction();

                                break;
                            }

                            /*
                             * Shutdown.
                             */
                            2 => {
                                tracing::info!(
                                    "Shutdown requested."
                                );

                                hardware
                                    .shutdown_restore();

                                return;
                            }

                            _ => {
                                tracing::debug!(
                                    "Unknown IPC command: {}",
                                    command[0]
                                );
                            }
                        }
                    }

                    Ok(_) => {}

                    Err(e) => {
                        tracing::warn!(
                            "Failed to read IPC command: {}",
                            e
                        );
                    }
                }
            }

            /*
             * Netlink.
             */
            if nfds > 1 {
                let events =
                    pfds[1].revents;

                if events
                    & (libc::POLLERR
                        | libc::POLLHUP
                        | libc::POLLNVAL)
                    != 0
                {
                    tracing::warn!(
                        "Netlink socket error; reconnecting."
                    );

                    netlink.disconnect();

                    netlink.schedule_reconnect(
                        Instant::now()
                    );
                } else if events
                    & libc::POLLIN
                    != 0
                {
                    let event_now =
                        Instant::now();

                    netlink.handle_events(
                        event_now
                    );

                    if netlink
                        .debounce_due(
                            event_now
                        )
                    {
                        break;
                    }
                }
            }
        }
    }
}
```

### Ada satu bagian yang **wajib** saya ubah juga: `is_plugged_in()`

Versi lama Anda punya:

```rust
Ok(true)
```

sebagai fallback.

Ini saya anggap **bug safety-critical** untuk charger daemon. Kalau semua `online` node gagal dibaca, daemon tidak boleh menganggap charger **terpasang**.

Saya sarankan menjadi:

```rust
pub fn is_plugged_in() -> Result<bool, ChargerError> {
    let mut found_online_node = false;

    if let Ok(entries) =
        fs::read_dir("/sys/class/power_supply")
    {
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };

            let lower = name.to_ascii_lowercase();

            if lower.contains("battery")
                || lower.contains("bms")
            {
                continue;
            }

            let online_path =
                entry.path().join("online");

            if !online_path.exists() {
                continue;
            }

            found_online_node = true;

            if let Ok(value) =
                read_sysfs(&online_path)
            {
                if value == "1" {
                    return Ok(true);
                }
            }
        }
    }

    if found_online_node {
        Ok(false)
    } else {
        Err(ChargerError::NoChargingNodeFound)
    }
}
```

Dan **jangan** melakukan:

```rust
online: battery_reader.is_plugged_in().ok(),
```

kalau `CachedReader::is_plugged_in()` masih fallback `Ok(false)` secara diam-diam. Lebih baik error benar-benar menjadi `None`, sehingga `DecisionEngine` masuk `Fault`.

---

### Ada satu masalah lagi di `CachedReader`

Saat ini:

```rust
let current_path = CURRENT_NODES
    .iter()
    .find(|&&p| Path::new(p).exists())
    .copied()
```

hanya memilih **node pertama yang exist**.

Pada Android vendor kernel, ini tidak selalu ideal. Lebih robust kalau reader mencoba beberapa node ketika membaca current.

Selain itu, untuk:

```rust
current_now
```

vendor Android bisa memberikan:

* µA
* mA
* nilai negatif saat discharge
* nilai positif saat charge

Jadi normalisasi unit sebaiknya dilakukan secara eksplisit berdasarkan node, bukan hanya:

```rust
if ua.abs() > 10_000.0
```

Heuristik `> 10_000` masih bisa dipakai sebagai fallback, tetapi idealnya `CURRENT_NODES` mendefinisikan unit masing-masing.

---

### Perubahan arsitektur yang paling penting

Dengan versi di atas, lifecycle-nya menjadi:

```text
                  ┌─────────────────┐
                  │    Unmanaged    │
                  └────────┬────────┘
                           │
                    policy needs charge
                           │
                           ▼
                  ┌─────────────────┐
                  │ acquire ownership│
                  └────────┬────────┘
                           │
                           ▼
                  ┌─────────────────┐
                  │ ChargingEnabled │
                  └────────┬────────┘
                           │
                 capacity / temperature
                           │
              ┌────────────┴────────────┐
              ▼                         ▼
       LimitReached              ThermalCutoff
              │                         │
              └────────────┬────────────┘
                           ▼
                  ┌──────────────────┐
                  │ChargingDisabled  │
                  └────────┬─────────┘
                           │
                    hysteresis resume
                           │
                           ▼
                  ChargingEnabled
```

Sedangkan **charger dicabut**:

```text
ChargingEnabled
      │
      │ online == false
      ▼
 Unmanaged
      │
      └── restore original hardware state
```

Ini penting karena `Offline` tidak boleh terus-menerus menjadi state yang memegang hardware.

### Satu catatan penting

Kode di atas mengasumsikan bahwa `Config::max_temp_dc` memang **deci-degree Celsius**, misalnya:

```text
450 = 45.0°C
500 = 50.0°C
```

dan `SensorSnapshot.temp_dc` juga:

```text
450 = 45.0°C
```

Itu konsisten dengan `battery/temp` Android yang umumnya menggunakan deci°C.

Untuk `current_ma`, saya sengaja mengubah snapshot dari:

```rust
Option<i32>
```

menjadi:

```rust
Option<f32>
```

karena reader Anda memang menghasilkan `f32`, sehingga tidak perlu kehilangan presisi.

**Namun**, sebelum dianggap 100% final, saya masih akan memperbaiki `CachedReader` agar **tidak mengunci satu current node**, dan memperbaiki `control.rs` supaya `set_charging()` membedakan **partial failure** vs **semua node gagal**. Dua bagian itu justru cukup penting untuk device Android dengan vendor power-supply node yang berbeda-beda.
