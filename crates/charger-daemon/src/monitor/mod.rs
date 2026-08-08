pub mod snapshot {
use charger_core::battery::reader::BatteryStatus;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingState {
    Charging,
    NotCharging,
    Full,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temp_dc: Option<i32>,
    #[allow(dead_code)]
    pub current_ma: Option<i32>,
    pub status: Option<BatteryStatus>,
    pub online: Option<bool>,
    pub ts: Instant,
}

impl SensorSnapshot {
    pub fn charging_state(&self) -> ChargingState {
        match self.status {
            Some(BatteryStatus::Charging) => ChargingState::Charging,
            Some(BatteryStatus::NotCharging) => ChargingState::NotCharging,
            Some(BatteryStatus::Full) => ChargingState::Full,
            _ => ChargingState::Unknown,
        }
    }
}

}

pub mod hardware {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    NotOwned,
    Owned { original_charging: bool },
}

const STATE_FILE: &str = "/data/adb/charger-control/ownership.state";

pub fn load_persistent_ownership() -> Option<bool> {
    if let Ok(content) = std::fs::read_to_string(STATE_FILE) {
        let trimmed = content.trim();
        if trimmed == "1" {
            Some(true)
        } else if trimmed == "0" {
            Some(false)
        } else {
            None
        }
    } else {
        None
    }
}

pub fn save_persistent_ownership(original: bool) {
    if let Some(parent) = std::path::Path::new(STATE_FILE).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temp_file = format!("{}.tmp", STATE_FILE);
    let content = if original { "1" } else { "0" };
    
    if let Err(e) = std::fs::write(&temp_file, content) {
        tracing::error!("Failed to persist ownership (write tmp): {}", e);
        return;
    }
    if let Err(e) = std::fs::rename(&temp_file, STATE_FILE) {
        tracing::error!("Failed to persist ownership (rename tmp): {}", e);
    }
}

pub fn clear_persistent_ownership() {
    if let Err(e) = std::fs::remove_file(STATE_FILE) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::error!("Failed to clear persistent ownership: {}", e);
        }
    }
}

struct Verification {
    generation: u64,
    target: HardwareTarget,
    deadline: Instant,
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
        }
    }

    pub fn invalidate_verification(&mut self) {
        self.generation += 1;
        self.verification = None;
        self.verification_failures = 0;
        self.sync = SyncState::Unknown;
    }

    pub fn needs_apply(&self, new_target: HardwareTarget) -> bool {
        self.applied_target != new_target || self.force_apply || self.sync == SyncState::Failed
    }

    pub fn apply_target(&mut self, target: HardwareTarget) {
        self.desired_target = target;

        match target {
            HardwareTarget::ChargingEnabled | HardwareTarget::ChargingDisabled => {
                let enable = target == HardwareTarget::ChargingEnabled;

                if self.ownership == Ownership::NotOwned {
                    match control::is_charging_enabled() {
                        Ok(original) => {
                            save_persistent_ownership(original);
                            self.ownership = Ownership::Owned { original_charging: original };
                        }
                        Err(e) => {
                            tracing::error!("Failed to read original charging state: {}", e);
                            self.mark_apply_failed();
                            return;
                        }
                    }
                }

                match control::set_charging(enable) {
                    Ok(()) => self.mark_apply_success(target),
                    Err(e) => {
                        tracing::error!("Failed to {} charging: {}", if enable { "enable" } else { "disable" }, e);
                        self.mark_apply_failed();
                    }
                }
            }

            HardwareTarget::Unmanaged => {
                tracing::debug!("Entering Unmanaged state; relinquishing hardware ownership");

                if let Ownership::Owned { original_charging } = self.ownership {
                    match control::set_charging(original_charging) {
                        Ok(()) => {
                            tracing::info!("Restored original charging state ({}) before going Unmanaged", original_charging);
                            clear_persistent_ownership();
                            self.ownership = Ownership::NotOwned;
                        }
                        Err(e) => {
                            tracing::error!("Failed to restore original charging state: {}", e);
                            self.mark_apply_failed();
                            return;
                        }
                    }
                }

                self.force_apply = false;
                self.sync = SyncState::Synced;
                self.applied_target = target;
                self.verification = None;
                self.verification_failures = 0;
            }
        }
    }

    fn mark_apply_success(&mut self, target: HardwareTarget) {
        self.applied_target = target;
        self.force_apply = false;
        self.sync = SyncState::Pending;
        self.verification_failures = 0;

        self.generation += 1;

        self.verification = Some(Verification {
            generation: self.generation,
            target,
            deadline: Instant::now() + VERIFY_DELAYS[0],
        });
    }

    fn mark_apply_failed(&mut self) {
        self.invalidate_verification();
        self.force_apply = true;
        self.sync = SyncState::Failed;
    }

    pub fn verification_due(&self) -> bool {
        self.verification.as_ref().is_some_and(|v| Instant::now() >= v.deadline)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.verification.as_ref().map(|v| v.deadline)
    }

    pub fn verify(&mut self, snapshot: &SensorSnapshot) {
        let Some(v) = &self.verification else {
            return;
        };

        if v.generation != self.generation {
            return;
        }

        let success = match v.target {
            HardwareTarget::ChargingEnabled => {
                snapshot.online == Some(true)
                    && control::is_charging_enabled().unwrap_or(false) == true
            }

            HardwareTarget::ChargingDisabled => {
                let control_disabled = control::is_charging_enabled().unwrap_or(true) == false;
                let battery_safe = matches!(snapshot.charging_state(), ChargingState::NotCharging | ChargingState::Full);
                control_disabled && battery_safe
            }

            // Normally this is never present because Unmanaged
            // has no verification operation.
            HardwareTarget::Unmanaged => true,
        };

        if success {
            self.sync = SyncState::Synced;
            self.verification = None;
            self.verification_failures = 0;
        } else {
            tracing::warn!("Verification failed for target {:?}", self.applied_target);
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
            target: self.applied_target,
            deadline: Instant::now() + VERIFY_DELAYS[index],
        });
    }

    pub fn shutdown_restore(&mut self) {
        if let Ownership::Owned { original_charging } = self.ownership {
            tracing::info!("Daemon shutting down; restoring original charging state ({})", original_charging);

            match control::set_charging(original_charging) {
                Ok(()) => {
                    tracing::info!("Charging control restored; daemon relinquishing hardware ownership");
                    self.desired_target = HardwareTarget::Unmanaged;
                    self.applied_target = HardwareTarget::Unmanaged;
                    self.sync = SyncState::Synced;
                    self.force_apply = false;
                    self.verification = None;
                    self.verification_failures = 0;
                    
                    clear_persistent_ownership();
                    self.ownership = Ownership::NotOwned;
                }

                Err(e) => {
                    tracing::error!(
                        "Failed to restore original charging state during shutdown: {}",
                        e
                    );

                    self.sync = SyncState::Failed;
                    self.force_apply = true;
                }
            }
        } else {
            tracing::info!("Daemon shutting down without hardware ownership; leaving charging untouched");
        }
    }
}

}

pub mod decision {
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

    pub fn evaluate(&mut self, snapshot: &SensorSnapshot, cfg: &Config, current_target: HardwareTarget) -> Decision {
        // 1. Unconditional Overrides
        if !cfg.enabled {
            self.policy = ChargePolicyState::Disabled;
            return self.build_decision(DecisionReason::DaemonDisabled, HardwareTarget::Unmanaged);
        }

        if snapshot.online == Some(false) {
            self.policy = ChargePolicyState::Offline;
            return self.build_decision(DecisionReason::ChargerOffline, current_target);
        }

        if snapshot.temp_dc.is_none() || snapshot.capacity_pct.is_none() || snapshot.online.is_none() || snapshot.status.is_none() {
            self.fault_recovery_reads = 0;
            self.policy = ChargePolicyState::Fault;
            let reason = if snapshot.temp_dc.is_none() {
                DecisionReason::SensorFault
            } else if snapshot.capacity_pct.is_none() {
                DecisionReason::CapacityUnavailable
            } else {
                DecisionReason::SensorFault
            };
            return self.build_decision(reason, self.policy_to_target(self.policy));
        }

        if self.policy == ChargePolicyState::Fault {
            self.fault_recovery_reads += 1;
            if self.fault_recovery_reads < FAULT_RECOVERY_READS {
                return Decision {
                    policy: self.policy,
                    target: HardwareTarget::ChargingDisabled,
                    reason: DecisionReason::FaultRecovering,
                };
            }
            // Recovered completely from Fault
            self.fault_recovery_reads = 0;
            tracing::info!("Sensors recovered completely, exiting Fault state.");
            // Fall-through to normal priority routing
        }

        let cap = snapshot.capacity_pct.unwrap();
        let temp = snapshot.temp_dc.unwrap();

        // 2. Evaluate physical constraints with hysteresis independently
        let thermal_max = cfg.max_temp_dc;
        let safe_hysteresis = cfg
            .thermal_resume_hysteresis_dc
            .clamp(1, thermal_max.saturating_sub(1).max(1));
        let thermal_resume = thermal_max.saturating_sub(safe_hysteresis);

        let is_thermal = cfg.thermal_cutoff && if self.policy == ChargePolicyState::ThermalCutoff {
            temp > thermal_resume
        } else {
            temp >= thermal_max
        };

        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };

        let is_limit = if self.policy == ChargePolicyState::LimitReached {
            cap > resume
        } else {
            cap >= limit
        };

        // 3. Priority Routing
        if is_thermal {
            self.policy = ChargePolicyState::ThermalCutoff;
            let reason = if temp >= thermal_max {
                DecisionReason::ThermalLimitReached
            } else {
                DecisionReason::WaitingForThermalResume
            };
            self.build_decision(reason, self.policy_to_target(self.policy))
        } else if is_limit {
            self.policy = ChargePolicyState::LimitReached;
            let reason = if cap >= limit {
                DecisionReason::ChargeLimitReached
            } else {
                DecisionReason::WaitingForLimitResume
            };
            self.build_decision(reason, self.policy_to_target(self.policy))
        } else {
            self.policy = ChargePolicyState::Charging;
            self.build_decision(DecisionReason::NormalCharging, self.policy_to_target(self.policy))
        }
    }

    fn build_decision(&self, reason: DecisionReason, target: HardwareTarget) -> Decision {
        Decision {
            policy: self.policy,
            target,
            reason,
        }
    }

    fn policy_to_target(&self, policy: ChargePolicyState) -> HardwareTarget {
        match policy {
            // Daemon relinquishes hardware ownership.
            // HardwareController MUST NOT call set_charging() for this target.
            ChargePolicyState::Disabled => HardwareTarget::Unmanaged,
            ChargePolicyState::Offline => unreachable!("Offline handled separately"),
            
            ChargePolicyState::Charging => HardwareTarget::ChargingEnabled,
            
            ChargePolicyState::LimitReached
            | ChargePolicyState::ThermalCutoff
            | ChargePolicyState::Fault => HardwareTarget::ChargingDisabled,
        }
    }
}

}

pub mod netlink {
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const NETLINK_RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const NETLINK_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(60);
const NETLINK_DEBOUNCE: Duration = Duration::from_millis(250);

pub struct NetlinkMonitor {
    socket: Option<OwnedFd>,
    reconnect_at: Option<Instant>,
    backoff: Duration,
    debounce_target: Option<Instant>,
}

impl NetlinkMonitor {
    pub fn new() -> Self {
        let mut monitor = Self {
            socket: None,
            reconnect_at: None,
            backoff: NETLINK_RECONNECT_INITIAL_BACKOFF,
            debounce_target: None,
        };
        monitor.try_reconnect(Instant::now());
        monitor
    }

    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }

    pub fn as_raw_fd(&self) -> Option<i32> {
        self.socket.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn disconnect(&mut self) {
        self.socket = None;
    }

    pub fn schedule_reconnect(&mut self, now: Instant) {
        self.reconnect_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(NETLINK_RECONNECT_MAX_BACKOFF);
    }

    pub fn should_reconnect(&self, now: Instant) -> bool {
        if self.socket.is_some() {
            return false;
        }
        if let Some(target) = self.reconnect_at {
            now >= target
        } else {
            true // If no socket and no reconnect target, it should reconnect immediately
        }
    }

    pub fn try_reconnect(&mut self, now: Instant) -> bool {
        match Self::create_netlink_socket() {
            Ok(sock) => {
                tracing::info!("Netlink socket connected successfully");
                self.socket = Some(sock);
                self.reconnect_at = None;
                self.backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;
                true
            }
            Err(e) => {
                tracing::warn!("Netlink reconnect failed ({}).", e);
                self.schedule_reconnect(now);
                false
            }
        }
    }

    fn create_netlink_socket() -> std::io::Result<OwnedFd> {
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_KOBJECT_UEVENT) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0; // Let kernel assign PID
        addr.nl_groups = 1;
        let ret = unsafe {
            libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn handle_events(&mut self, now: Instant) {
        let Some(raw_fd) = self.as_raw_fd() else {
            return;
        };

        let mut buf = [0u8; 4096];
        let mut found = false;
        loop {
            let n = unsafe {
                libc::recv(
                    raw_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                match err.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => break,
                    _ => {
                        tracing::error!("Netlink recv failed: {}", err);
                        self.disconnect();
                        self.schedule_reconnect(now);
                        return;
                    }
                }
            }
            if n == 0 {
                break;
            }
            let buf_slice = &buf[..n as usize];

            if Self::contains_subslice(buf_slice, b"SUBSYSTEM=power_supply")
                && Self::contains_subslice(buf_slice, b"ACTION=change")
            {
                found = true;
            }
        }

        if found {
            self.debounce_target = Some(now + NETLINK_DEBOUNCE);
        }
    }

    pub fn debounce_due(&mut self, now: Instant) -> bool {
        if let Some(target) = self.debounce_target {
            if now >= target {
                self.debounce_target = None;
                return true;
            }
        }
        false
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        match (self.debounce_target, self.reconnect_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}

}

pub mod scheduler {
use super::snapshot::SensorSnapshot;
use charger_core::config::schema::Config;
use std::collections::VecDeque;
use std::time::Duration;

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600);
const UNPLUGGED_HEARTBEAT_NO_NETLINK: Duration = Duration::from_secs(30);

const HISTORY_LEN: usize = 6;
const EMA_ALPHA: f32 = 0.35;
const SAFETY_FACTOR: f32 = 0.25;         // poll di 1/4 ETA ke charge limit
const THERMAL_SAFETY_FACTOR: f32 = 0.15; // margin lebih ketat — suhu bisa naik lebih cepat dari capacity

pub struct AdaptiveScheduler {
    pub limit: f32,
    pub resume_limit: f32,
    pub thermal_cutoff: f32,
    history: VecDeque<SensorSnapshot>,
    pub last_interval: Duration,
    cap_rate_ema: Option<f32>,  // %/detik, smoothed
    temp_rate_ema: Option<f32>, // deci-°C/detik, smoothed
}

impl AdaptiveScheduler {
    pub fn new(limit: u8, resume_limit: u8, thermal_cutoff: i32) -> Self {
        Self {
            limit: limit as f32,
            resume_limit: resume_limit as f32,
            thermal_cutoff: thermal_cutoff as f32 / 10.0,
            history: VecDeque::new(),
            last_interval: MIN_INTERVAL,
            cap_rate_ema: None,
            temp_rate_ema: None,
        }
    }

    /// Panggil tiap kali baca config baru (termasuk saat reload), supaya
    /// threshold yang dipakai prediksi nggak nyangkut di nilai lama.
    pub fn sync_config(&mut self, cfg: &Config) {
        let new_limit = cfg.charge_limit as f32;
        let new_resume = cfg.resume_limit as f32;
        let new_thermal = cfg.max_temp_dc as f32 / 10.0;

        if (self.limit - new_limit).abs() > f32::EPSILON
            || (self.resume_limit - new_resume).abs() > f32::EPSILON
            || (self.thermal_cutoff - new_thermal).abs() > f32::EPSILON
        {
            self.limit = new_limit;
            self.resume_limit = new_resume;
            self.thermal_cutoff = new_thermal;
            self.reset_prediction();
        }
    }

    pub fn observe(&mut self, s: &SensorSnapshot) {
        if let Some(prev) = self.history.back() {
            let dt = s.ts.saturating_duration_since(prev.ts).as_secs_f32();
            // Sampel yang terlalu rapat bikin rate noisy (nyaris dibagi nol) — skip.
            if dt >= 0.5 {
                if let (Some(cap), Some(pcap)) = (s.capacity_pct, prev.capacity_pct) {
                    let rate = (cap as f32 - pcap as f32) / dt;
                    self.cap_rate_ema = Some(ema(self.cap_rate_ema, rate));
                }
                if let (Some(temp), Some(ptemp)) = (s.temp_dc, prev.temp_dc) {
                    let rate = (temp as f32 - ptemp as f32) / dt;
                    self.temp_rate_ema = Some(ema(self.temp_rate_ema, rate));
                }
            }
        }

        self.history.push_back(s.clone());
        while self.history.len() > HISTORY_LEN {
            self.history.pop_front();
        }
    }

    pub fn reset_prediction(&mut self) {
        self.last_interval = MIN_INTERVAL;
        self.cap_rate_ema = None;
        self.temp_rate_ema = None;
        self.history.clear();
    }

    pub fn next_interval(&mut self, s: &SensorSnapshot, netlink_alive: bool) -> Duration {
        if s.online == Some(false) {
            self.last_interval = if netlink_alive { UNPLUGGED_HEARTBEAT } else { UNPLUGGED_HEARTBEAT_NO_NETLINK };
            return self.last_interval;
        }

        let cap_target = if let Some(rate) = self.cap_rate_ema {
            if rate < -0.01 { self.resume_limit } else { self.limit }
        } else {
            self.limit
        };

        let cap_eta = self.eta_to(s.capacity_pct.map(|c| c as f32), cap_target, self.cap_rate_ema, SAFETY_FACTOR);
        let temp_eta = self.eta_to(s.temp_dc.map(|t| t as f32), self.thermal_cutoff * 10.0, self.temp_rate_ema, THERMAL_SAFETY_FACTOR);

        let mut interval = match (cap_eta, temp_eta) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => self.fallback_interval(s),
        };

        if let Some(t) = s.temp_dc {
            let temp_margin = self.thermal_cutoff * 10.0 - (t as f32);
            if temp_margin <= 30.0 {
                interval = interval.min(Duration::from_secs(5));
            } else if temp_margin <= 50.0 {
                interval = interval.min(Duration::from_secs(15));
            }
        }

        self.last_interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
        self.last_interval
    }

    /// Estimasi waktu sampai `current` nyampe `threshold` pada `rate` yang sudah
    /// di-smooth, dipotong `safety` supaya kita bangun jauh sebelum benar-benar
    /// nyampe. `None` kalau rate belum ada atau nilainya nggak lagi mendekat.
    fn eta_to(&self, current: Option<f32>, threshold: f32, rate: Option<f32>, safety: f32) -> Option<Duration> {
        let (current, rate) = (current?, rate?);
        let distance = threshold - current;

        // Only calculate ETA if we are moving towards the threshold
        if distance.signum() == rate.signum() && rate.abs() > 0.01 {
            let seconds = (distance.abs() / rate.abs()) * safety;
            Some(Duration::from_secs_f32(seconds.max(0.0)))
        } else {
            None
        }
    }

    /// Tier kasar berbasis jarak, dipakai cuma sebelum EMA rate cukup terpercaya
    /// (atau saat nilainya memang lagi flat).
    fn fallback_interval(&self, s: &SensorSnapshot) -> Duration {
        let cap_frac = s.capacity_pct.map(|c| {
            let c = c as f32;
            if let Some(rate) = self.cap_rate_ema {
                if rate < -0.01 && c >= self.resume_limit {
                    // discharging towards resume_limit
                    ((c - self.resume_limit) / (100.0 - self.resume_limit)).clamp(0.0, 1.0)
                } else {
                    // charging towards limit
                    ((self.limit - c) / self.limit).clamp(0.0, 1.0)
                }
            } else {
                ((self.limit - c) / self.limit).clamp(0.0, 1.0)
            }
        });
        let temp_frac = s.temp_dc.map(|t| {
            let thermal_max_dc = self.thermal_cutoff * 10.0;
            ((thermal_max_dc - t as f32) / thermal_max_dc).clamp(0.0, 1.0)
        });

        let frac = match (cap_frac, temp_frac) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => 1.0, // nggak ada data — poll rapat, aman dulu
        };

        MIN_INTERVAL + Duration::from_secs_f32((MAX_INTERVAL - MIN_INTERVAL).as_secs_f32() * frac)
    }
}

fn ema(prev: Option<f32>, sample: f32) -> f32 {
    match prev {
        Some(p) => EMA_ALPHA * sample + (1.0 - EMA_ALPHA) * p,
        None => sample,
    }
}

}


use charger_core::{battery::reader::CachedReader, config::schema::Config};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use decision::DecisionEngine;
use hardware::HardwareController;
use netlink::NetlinkMonitor;
use scheduler::AdaptiveScheduler;
use snapshot::SensorSnapshot;

pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    tracing::info!("Monitor loop started (Rewrite Final: State Machine Segregation)");

    let mut battery_reader = CachedReader::new();
    let mut netlink = NetlinkMonitor::new();
    let mut engine = DecisionEngine::new();
    let mut hardware = HardwareController::new();

    // Recover persistent ownership state
    if let Some(original) = hardware::load_persistent_ownership() {
        tracing::warn!("Found stale ownership state! Recovering hardware original state ({})...", original);
        if let Err(e) = charger_core::battery::control::set_charging(original) {
            tracing::error!("Failed to restore stale original charging state: {}", e);
        } else {
            tracing::info!("Recovered stale hardware ownership successfully.");
        }
        hardware::clear_persistent_ownership();
    }

    let initial_cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let effective_resume =
        if initial_cfg.resume_limit > 0 && initial_cfg.resume_limit < initial_cfg.charge_limit {
            initial_cfg.resume_limit
        } else {
            initial_cfg.charge_limit.saturating_sub(2)
        };
    let mut scheduler = AdaptiveScheduler::new(
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
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();

        scheduler.sync_config(&cfg);

        let snapshot = SensorSnapshot {
            capacity_pct: battery_reader.read_capacity().ok(),
            temp_dc: battery_reader.read_temperature_dc().ok(),
            current_ma: battery_reader.read_current_ma().map(|c| c as i32).ok(),
            status: battery_reader.read_status().ok(),
            online: battery_reader.is_plugged_in().ok(),
            ts: Instant::now(),
        };

        // 1. Observe snapshot in scheduler (ignores transitional samples implicitly by only reading pure state)
        // We only push to EMA if hardware is completely synced and we are not forcing apply.
        if hardware.sync == hardware::SyncState::Synced {
            scheduler.observe(&snapshot);
        }



        // 2. Hardware verification step
        if hardware.verification_due() {
            hardware.verify(&snapshot);
        }

        // 3. Re-evaluate policy based on config and sensor readings
        let decision = engine.evaluate(&snapshot, &cfg, hardware.applied_target);

        if decision.target != hardware.desired_target {
            tracing::info!(
                "Policy change triggered target update: {:?} -> {:?} (Reason: {}, Policy: {:?})",
                hardware.desired_target,
                decision.target,
                decision.reason,
                decision.policy
            );
            hardware.invalidate_verification();
            hardware.force_apply = true;
            hardware.desired_target = decision.target;
        }

        // 4. Apply hardware target if necessary
        if hardware.needs_apply(decision.target) {
            tracing::info!(
                "Applying hardware target: {:?} (Force: {}, SyncState: {:?})",
                decision.target,
                hardware.force_apply,
                hardware.sync
            );
            hardware.apply_target(decision.target);
        }

        // 5. Check if netlink reconnect is due
        if netlink.should_reconnect(now) {
            netlink.try_reconnect(now);
        }

        // 6. Calculate sleep timeout from scheduler
        let mut timeout = scheduler.next_interval(&snapshot, netlink.is_connected());
        
        if hardware.sync == hardware::SyncState::Failed {
            timeout = timeout.min(Duration::from_secs(2));
        }

        let mut should_evaluate = false;
        let mut loop_now = Instant::now();
        let target_wake = loop_now + timeout;

        while loop_now < target_wake {
            let mut next_wake = target_wake;

            if let Some(nd) = netlink.next_deadline() {
                if loop_now >= nd {
                    if netlink.debounce_due(loop_now) {
                        should_evaluate = true;
                        break;
                    }
                    if netlink.should_reconnect(loop_now) {
                        netlink.try_reconnect(loop_now);
                    }
                }
                
                // Update next_wake considering possible changes to next_deadline
                if let Some(new_nd) = netlink.next_deadline() {
                    if new_nd > loop_now {
                        next_wake = next_wake.min(new_nd);
                    }
                }
            }

            if let Some(vd) = hardware.next_deadline() {
                if loop_now >= vd {
                    should_evaluate = true;
                    break;
                }
                next_wake = next_wake.min(vd);
            }

            let remaining = next_wake.saturating_duration_since(loop_now);

            pfds[0].revents = 0;
            let mut num_fds = 1;
            
            if let Some(nl_fd) = netlink.as_raw_fd() {
                pfds[1].fd = nl_fd;
                pfds[1].events = libc::POLLIN;
                pfds[1].revents = 0;
                num_fds = 2;
            } else {
                pfds[1].fd = -1;
                pfds[1].events = 0;
            }

            let ret = unsafe {
                libc::poll(
                    pfds.as_mut_ptr(),
                    num_fds as libc::nfds_t,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            loop_now = Instant::now();

            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::error!("poll() failed: {}", err);
                should_evaluate = true;
                break;
            } else if ret == 0 {
                should_evaluate = true;
                break;
            }

            let ipc_events = pfds[0].revents;
            if ipc_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                tracing::error!("IPC socket error/hangup. Restoring charging before exit.");
                hardware.shutdown_restore();
                return;
            }

            if ipc_events & libc::POLLIN != 0 {
                let mut buf = [0u8; 1];
                if rx.recv(&mut buf).is_ok() {
                    if buf[0] == 2 {
                        tracing::info!("Monitor loop shutting down");
                        hardware.shutdown_restore();
                        return;
                    }
                    if buf[0] == 1 {
                        tracing::info!("Config reloaded");
                        should_evaluate = true;
                        // For config reload, we force an engine re-evaluation
                        // and re-sync hardware
                        hardware.invalidate_verification();
                        hardware.force_apply = true;
                        scheduler.reset_prediction();
                        break;
                    }
                }
            }

            if num_fds > 1 {
                let nl_events = pfds[1].revents;
                if nl_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    tracing::error!("Netlink socket error. Disconnecting and scheduling reconnect.");
                    netlink.disconnect();
                    netlink.schedule_reconnect(loop_now);
                } else if nl_events & libc::POLLIN != 0 {
                    netlink.handle_events(loop_now);
                }
                
            }
        }

        if !should_evaluate {
            continue;
        }
    }
}

