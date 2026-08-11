use std::{
    collections::VecDeque,
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixDatagram,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use crate::ipc::DaemonDiagnostics;
use charger_core::{
    battery::{control, reader},
    config::schema::Config,
};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);

/// Fallback heartbeat saat Netlink tidak tersedia.
///
/// Digunakan agar daemon tetap mendeteksi perubahan status charger
/// pada perangkat yang tidak mendukung Netlink uevent.
const FALLBACK_HEARTBEAT: Duration = Duration::from_secs(600);

const NETLINK_COALESCE: Duration = Duration::from_millis(100);

const ATTACH_SETTLE_WINDOW: Duration = Duration::from_secs(5);

const HARDWARE_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

const ERROR_BACKOFF_INITIAL: Duration = Duration::from_secs(2);
const ERROR_BACKOFF_MAX: Duration = Duration::from_secs(60);

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

const THERMAL_HYSTERESIS_DC: i32 = 20;

const MAX_HISTORY: usize = 5;
const EMA_ALPHA: f32 = 0.30;

const BYPASS_INTERVAL: Duration = Duration::from_secs(30);
const THERMAL_BLOCKED_INTERVAL: Duration = Duration::from_secs(10);
const LIMIT_BLOCKED_INTERVAL: Duration = Duration::from_secs(15);

const SOC_DANGER_DISTANCE: f32 = 2.0;
const THERMAL_DANGER_DISTANCE: f32 = 3.0;
const THERMAL_RATE_DANGER: f32 = 0.15;

const MAX_VALID_SOC_RATE: f32 = 1.0;
const MIN_RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const MAX_RATE_SAMPLE_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy)]
struct Sample {
    capacity: f32,
    temperature_c: f32,
    power_state: reader::PowerState,
    timestamp: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatingMode {
    Normal,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PolicyState {
    thermal_blocked: bool,
    limit_blocked: bool,
}

impl PolicyState {
    const fn clear() -> Self {
        Self {
            thermal_blocked: false,
            limit_blocked: false,
        }
    }

    const fn charging_allowed(self) -> bool {
        !self.thermal_blocked && !self.limit_blocked
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesiredHardwareState {
    ChargingEnabled,
    ChargingDisabled,
    Bypass,
}

impl DesiredHardwareState {
    fn hardware_mode(self, has_distinct_bypass: bool) -> control::ActualHardwareMode {
        match self {
            Self::ChargingEnabled => control::ActualHardwareMode::ChargingEnabled,

            Self::ChargingDisabled => control::ActualHardwareMode::ChargingDisabled,

            Self::Bypass if has_distinct_bypass => control::ActualHardwareMode::Bypass,

            Self::Bypass => control::ActualHardwareMode::ChargingDisabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    Snapshot,
    Hardware,
}

#[derive(Debug)]
struct SchedulingState {
    /// Instant of the last completed evaluation.
    last_evaluation: Instant,

    /// Whether a Netlink coalesced event is pending coalesce window.
    pending_netlink: bool,

    /// Whether an evaluation must run on the next loop iteration.
    force_evaluation: bool,

    snapshot_backoff: Duration,
    next_snapshot_retry_at: Option<Instant>,

    hardware_backoff: Duration,
    next_hardware_retry_at: Option<Instant>,
}

impl SchedulingState {
    fn new() -> Self {
        Self {
            last_evaluation: Instant::now() - Duration::from_secs(60),
            pending_netlink: false,
            force_evaluation: true,
            snapshot_backoff: ERROR_BACKOFF_INITIAL,
            next_snapshot_retry_at: None,
            hardware_backoff: ERROR_BACKOFF_INITIAL,
            next_hardware_retry_at: None,
        }
    }

    fn mark_force_evaluation(&mut self) {
        self.force_evaluation = true;
    }

    fn clear_evaluation_request(&mut self) {
        self.force_evaluation = false;
        self.pending_netlink = false;
    }

    fn mark_success(&mut self, kind: FailureKind) {
        match kind {
            FailureKind::Snapshot => {
                self.snapshot_backoff = ERROR_BACKOFF_INITIAL;
                self.next_snapshot_retry_at = None;
            }
            FailureKind::Hardware => {
                self.hardware_backoff = ERROR_BACKOFF_INITIAL;
                self.next_hardware_retry_at = None;
            }
        }
    }

    fn mark_success_all(&mut self) {
        self.mark_success(FailureKind::Snapshot);
        self.mark_success(FailureKind::Hardware);
    }

    fn mark_failure(&mut self, kind: FailureKind) {
        match kind {
            FailureKind::Snapshot => {
                self.next_snapshot_retry_at = Some(Instant::now() + self.snapshot_backoff);

                self.snapshot_backoff = (self.snapshot_backoff * 2).min(ERROR_BACKOFF_MAX);
            }

            FailureKind::Hardware => {
                self.next_hardware_retry_at = Some(Instant::now() + self.hardware_backoff);

                self.hardware_backoff = (self.hardware_backoff * 2).min(ERROR_BACKOFF_MAX);
            }
        }
    }
}

#[derive(Debug)]
struct HardwareTrack {
    /// Last known physical hardware charging mode.
    mode: control::ActualHardwareMode,

    /// Set when hardware state has changed and reconciliation is due.
    event_pending: bool,

    /// Instant of the last hardware reconciliation read.
    last_reconcile: Instant,
}

impl HardwareTrack {
    fn new() -> Self {
        Self {
            mode: control::ActualHardwareMode::Unknown,
            event_pending: false,
            last_reconcile: Instant::now() - HARDWARE_RECONCILE_INTERVAL,
        }
    }

    fn reconcile_due(&self) -> bool {
        self.event_pending || self.last_reconcile.elapsed() >= HARDWARE_RECONCILE_INTERVAL
    }

    fn mark_changed(&mut self) {
        self.event_pending = true;
    }

    fn mark_reconciled(&mut self) {
        self.last_reconcile = Instant::now();
        self.event_pending = false;
    }
}

#[derive(Debug)]
struct MonitorState {
    operating_mode: OperatingMode,

    policy: PolicyState,

    power_state: reader::PowerState,

    attach_started: Option<Instant>,

    /// Hardware mode tracking, reconciliation, and event flags.
    hw: HardwareTrack,

    /// Scheduling, retry windows, and netlink coalescing.
    sched: SchedulingState,
}

impl MonitorState {
    fn new() -> Self {
        Self {
            operating_mode: OperatingMode::Normal,

            policy: PolicyState::clear(),

            power_state: reader::PowerState::Unknown,

            attach_started: None,

            hw: HardwareTrack::new(),

            sched: SchedulingState::new(),
        }
    }

    fn mark_hardware_changed(&mut self) {
        self.hw.mark_changed();
    }

    fn mark_force_evaluation(&mut self) {
        self.sched.mark_force_evaluation();
    }

    fn clear_evaluation_request(&mut self) {
        self.sched.clear_evaluation_request();
    }

    fn mark_success(&mut self, kind: FailureKind) {
        self.sched.mark_success(kind);
    }

    fn mark_success_all(&mut self) {
        self.sched.mark_success_all();
    }

    fn mark_failure(&mut self, kind: FailureKind) {
        self.sched.mark_failure(kind);
    }

    fn hardware_reconcile_due(&self) -> bool {
        self.hw.reconcile_due()
    }

    fn mark_reconciled(&mut self) {
        self.hw.mark_reconciled();
    }

    fn reset_charger_state(&mut self) {
        self.policy = PolicyState::clear();
        self.attach_started = None;
        self.sched.mark_success_all();
    }
}

#[derive(Debug)]
struct AdaptiveScheduler {
    configured_interval: Duration,

    limit: f32,

    thermal_cutoff_c: f32,

    history: VecDeque<Sample>,

    ema_capacity_rate: f32,

    ema_temperature_rate: f32,

    last_interval: Duration,
}

impl AdaptiveScheduler {
    fn new(cfg: &Config) -> Self {
        let interval = normalize_poll_interval(cfg.poll_interval_secs);

        Self {
            configured_interval: interval,

            limit: cfg.charge_limit.min(100) as f32,

            thermal_cutoff_c: cfg.max_temp_dc as f32 / 10.0,

            history: VecDeque::with_capacity(MAX_HISTORY),

            ema_capacity_rate: 0.0,

            ema_temperature_rate: 0.0,

            last_interval: interval,
        }
    }

    fn update_config(&mut self, cfg: &Config) {
        self.configured_interval = normalize_poll_interval(cfg.poll_interval_secs);

        self.limit = cfg.charge_limit.min(100) as f32;

        self.thermal_cutoff_c = cfg.max_temp_dc as f32 / 10.0;

        self.last_interval = self.last_interval.min(self.configured_interval);
    }

    fn reset(&mut self) {
        self.history.clear();

        self.ema_capacity_rate = 0.0;
        self.ema_temperature_rate = 0.0;

        self.last_interval = self.configured_interval;
    }

    fn push(&mut self, sample: Sample) {
        if let Some(previous) = self.history.back().copied() {
            let dt = sample
                .timestamp
                .saturating_duration_since(previous.timestamp);

            if dt < MIN_RATE_SAMPLE_INTERVAL {
                self.push_history(sample);
                return;
            }

            if dt > MAX_RATE_SAMPLE_INTERVAL {
                self.reset();
                self.push_history(sample);
                return;
            }

            let seconds = dt.as_secs_f32();

            let capacity_delta = sample.capacity - previous.capacity;

            let absolute_rate = capacity_delta.abs() / seconds.max(0.1);

            if absolute_rate <= MAX_VALID_SOC_RATE {
                let rate = capacity_delta / seconds;

                self.ema_capacity_rate = ema(self.ema_capacity_rate, rate);
            }

            /*
             * Temperature rate is independent of SOC validity.
             *
             * A sudden SOC correction (e.g. BMS re-calibration) should not
             * cause a valid temperature reading to be discarded.
             */
            let temperature_delta = sample.temperature_c - previous.temperature_c;
            let temperature_rate = temperature_delta / seconds;

            // Clamp to a physically plausible range: Li-ion cells rarely
            // change faster than 2 °C/s even under extreme abuse.
            const MAX_VALID_TEMP_RATE_C_PER_S: f32 = 2.0;
            if temperature_rate.abs() <= MAX_VALID_TEMP_RATE_C_PER_S {
                self.ema_temperature_rate = ema(self.ema_temperature_rate, temperature_rate);
            }
        }

        self.push_history(sample);
    }

    fn push_history(&mut self, sample: Sample) {
        self.history.push_back(sample);

        if self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    fn next_interval(&mut self, policy: PolicyState, operating_mode: OperatingMode) -> Duration {
        let Some(sample) = self.history.back().copied() else {
            return self.configured_interval;
        };

        if sample.power_state == reader::PowerState::Disconnected {
            self.last_interval = FALLBACK_HEARTBEAT;
            return self.last_interval;
        }

        if operating_mode == OperatingMode::Bypass {
            self.last_interval = BYPASS_INTERVAL;
            return self.last_interval;
        }

        if policy.thermal_blocked {
            self.last_interval = THERMAL_BLOCKED_INTERVAL;
            return self.last_interval;
        }

        if policy.limit_blocked {
            self.last_interval = LIMIT_BLOCKED_INTERVAL;
            return self.last_interval;
        }

        let distance_to_limit = (self.limit - sample.capacity).max(0.0);

        let distance_to_thermal = (self.thermal_cutoff_c - sample.temperature_c).max(0.0);

        let thermal_danger = distance_to_thermal < THERMAL_DANGER_DISTANCE
            || self.ema_temperature_rate > THERMAL_RATE_DANGER;

        let limit_danger = distance_to_limit < SOC_DANGER_DISTANCE;

        if limit_danger || thermal_danger {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        let target = self.predict_interval(sample, distance_to_limit);

        self.last_interval = smooth_interval(self.last_interval, target);

        self.last_interval
    }

    fn predict_interval(&self, sample: Sample, distance_to_limit: f32) -> Duration {
        if sample.power_state.is_charging()
            && self.ema_capacity_rate > 0.01
            && distance_to_limit > 0.0
        {
            let seconds = distance_to_limit / self.ema_capacity_rate * 0.5;

            Duration::from_secs_f32(seconds.max(0.0))
                .max(self.configured_interval)
                .clamp(MIN_INTERVAL, MAX_INTERVAL)
        } else {
            self.configured_interval
        }
    }
}

fn ema(previous: f32, current: f32) -> f32 {
    let value = EMA_ALPHA * current + (1.0 - EMA_ALPHA) * previous;

    if value.is_finite() {
        value
    } else {
        previous
    }
}

fn normalize_poll_interval(seconds: u64) -> Duration {
    let seconds = if seconds == 0 {
        DEFAULT_POLL_INTERVAL.as_secs()
    } else {
        seconds
    };

    Duration::from_secs(seconds).clamp(MIN_INTERVAL, MAX_INTERVAL)
}

fn smooth_interval(previous: Duration, target: Duration) -> Duration {
    if target < previous {
        return target;
    }

    previous
        .mul_f32(1.5)
        .max(MIN_INTERVAL)
        .min(target)
        .min(MAX_INTERVAL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetlinkEvent {
    None,
    Fast,
    Coalesced,
}

struct NetlinkFd(RawFd);

impl Drop for NetlinkFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

fn create_netlink_socket() -> Option<RawFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };

    if fd < 0 {
        return None;
    }

    let rcvbuf: libc::c_int = 256 * 1024;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as u32,
        );
    }

    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };

    address.nl_family = libc::AF_NETLINK as libc::sa_family_t;

    address.nl_pid = 0;
    address.nl_groups = 1;

    let result = unsafe {
        libc::bind(
            fd,
            &address as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if result < 0 {
        unsafe {
            libc::close(fd);
        }

        return None;
    }

    Some(fd)
}

fn drain_netlink(fd: RawFd) -> NetlinkEvent {
    let mut buffer = [0u8; 16384];

    let mut result = NetlinkEvent::None;

    loop {
        let received = unsafe {
            libc::recv(
                fd,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };

        if received <= 0 {
            break;
        }

        let data = &buffer[..received as usize];

        let mut subsystem_power_supply = false;
        let mut name: &[u8] = b"";

        for part in data.split(|byte| *byte == 0) {
            if part == b"SUBSYSTEM=power_supply" {
                subsystem_power_supply = true;
            } else if let Some(value) = part.strip_prefix(b"POWER_SUPPLY_NAME=") {
                name = value;
            }
        }

        if !subsystem_power_supply {
            continue;
        }

        if is_fast_power_supply_event(name, data) {
            result = NetlinkEvent::Fast;
            continue;
        }

        if result == NetlinkEvent::None && is_relevant_power_supply(name) {
            result = NetlinkEvent::Coalesced;
        }
    }

    result
}

fn is_fast_power_supply_event(name: &[u8], data: &[u8]) -> bool {
    match name {
        // Physical AC charger connection transition.
        b"ac" => data
            .split(|byte| *byte == 0)
            .any(|part| part.starts_with(b"POWER_SUPPLY_ONLINE=")),

        // USB/Type-C attach/detach/role transition.
        b"usb" | b"charger" | b"typec" => data.split(|byte| *byte == 0).any(|part| {
            part.starts_with(b"POWER_SUPPLY_TYPEC_MODE=")
                || part.starts_with(b"POWER_SUPPLY_ONLINE=")
                || part.starts_with(b"POWER_SUPPLY_PRESENT=")
        }),

        // Battery and BMS telemetry/status must NOT wake the monitor via Fast events.
        b"battery" | b"bms" => false,

        _ => false,
    }
}

fn is_relevant_power_supply(name: &[u8]) -> bool {
    let s = std::str::from_utf8(name).unwrap_or("");
    s.contains("usb")
        || s.contains("batt")
        || s.contains("main")
        || s.contains("ac")
        || s.contains("wls")
        || s.contains("bms")
        || s.contains("charger")
        || s.contains("typec")
        || s.contains("tcpc")
        || s.contains("dc")
}

fn timeout_to_poll_ms(timeout: Option<Duration>) -> i32 {
    match timeout {
        None => -1,
        Some(duration) => duration.as_millis().min(i32::MAX as u128) as i32,
    }
}

#[derive(Debug, Clone, Copy)]
struct MonitorSnapshot {
    capacity: f32,
    temperature_c: f32,
    power_state: reader::PowerState,
}

fn read_monitor_snapshot() -> Result<MonitorSnapshot, &'static str> {
    let capacity = reader::read_capacity_raw().map_err(|_| "battery_capacity_read_failed")?;

    if !capacity.is_finite() {
        return Err("battery_capacity_non_finite");
    }

    let capacity = capacity.clamp(0.0, 100.0);

    let temperature_c =
        reader::read_temperature_c().map_err(|_| "battery_temperature_read_failed")?;

    let power_state = reader::get_power_state().map_err(|_| "power_state_read_failed")?;

    if power_state == reader::PowerState::Unknown {
        return Err("power_state_unknown");
    }

    Ok(MonitorSnapshot {
        capacity,
        temperature_c,
        power_state,
    })
}

/// Evaluate the charging policy.
///
/// This function contains policy only. It does not read hardware and does
/// not perform writes.
fn evaluate_policy(snapshot: MonitorSnapshot, previous: PolicyState, cfg: &Config) -> PolicyState {
    if snapshot.power_state.is_disconnected() {
        return PolicyState::clear();
    }

    let thermal_blocked =
        evaluate_thermal_policy(snapshot.temperature_c, previous.thermal_blocked, cfg);

    let limit_blocked = evaluate_limit_policy(snapshot.capacity, previous.limit_blocked, cfg);

    PolicyState {
        thermal_blocked,
        limit_blocked,
    }
}

fn evaluate_thermal_policy(temperature_c: f32, previous_blocked: bool, cfg: &Config) -> bool {
    if !cfg.thermal_cutoff {
        return false;
    }

    let max_temp_c = cfg.max_temp_dc as f32 / 10.0;

    if temperature_c >= max_temp_c {
        return true;
    }

    if previous_blocked {
        let resume_c = (cfg.max_temp_dc.saturating_sub(THERMAL_HYSTERESIS_DC)) as f32 / 10.0;

        return temperature_c > resume_c;
    }

    false
}

fn evaluate_limit_policy(capacity: f32, previous_blocked: bool, cfg: &Config) -> bool {
    let limit = cfg.charge_limit.min(100) as f32;

    /*
     * Reaching the configured limit is immediately blocking.
     */
    if capacity >= limit {
        return true;
    }

    /*
     * Once blocked, remain blocked until the configured resume
     * threshold is reached.
     */
    if previous_blocked {
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < cfg.charge_limit {
            cfg.resume_limit as f32
        } else {
            cfg.charge_limit.saturating_sub(1) as f32
        };

        return capacity > resume;
    }

    false
}

fn desired_hardware_state(mode: OperatingMode, policy: PolicyState) -> DesiredHardwareState {
    /*
     * Thermal protection is a hard safety interlock.
     *
     * Bypass must never override thermal protection.
     */
    if policy.thermal_blocked {
        return DesiredHardwareState::ChargingDisabled;
    }

    match mode {
        OperatingMode::Bypass => DesiredHardwareState::Bypass,

        OperatingMode::Normal => {
            if policy.charging_allowed() {
                DesiredHardwareState::ChargingEnabled
            } else {
                DesiredHardwareState::ChargingDisabled
            }
        }
    }
}

/// Apply normal charging state and perform exactly one verification read.
///
/// On success, `hardware` becomes the verified state.
///
/// On failure, `hardware` becomes Unknown.
fn apply_charging(enable: bool, hardware: &mut control::ActualHardwareMode) -> bool {
    let expected = if enable {
        control::ActualHardwareMode::ChargingEnabled
    } else {
        control::ActualHardwareMode::ChargingDisabled
    };

    match control::set_charging(enable) {
        Ok(()) => {
            let actual = control::get_actual_charging_state();

            if actual == expected {
                *hardware = actual;
                true
            } else {
                tracing::warn!(
                    ?expected,
                    ?actual,
                    "charging hardware verification mismatch"
                );

                *hardware = control::ActualHardwareMode::Unknown;
                false
            }
        }

        Err(error) => {
            tracing::error!(
                enable,
                error = %error,
                "failed to apply charging state"
            );

            // If no charging node was found at all, the cached paths are
            // stale. Invalidate them so the next evaluation re-discovers.
            if matches!(
                error,
                charger_core::error::ChargerError::NoChargingNodeFound
            ) {
                control::reset_node_caches();
            }

            *hardware = control::ActualHardwareMode::Unknown;
            false
        }
    }
}

/// Apply bypass and perform exactly one verification read.
fn apply_bypass(
    expected: control::ActualHardwareMode,
    hardware: &mut control::ActualHardwareMode,
) -> bool {
    match control::enter_bypass_mode() {
        Ok(()) => {
            let actual = control::get_actual_charging_state();

            if actual == expected {
                *hardware = actual;
                true
            } else {
                tracing::warn!(?expected, ?actual, "bypass hardware verification mismatch");

                *hardware = control::ActualHardwareMode::Unknown;
                false
            }
        }

        Err(error) => {
            tracing::error!(
                error = %error,
                "failed to enter bypass"
            );

            *hardware = control::ActualHardwareMode::Unknown;
            false
        }
    }
}

fn reconcile_known_stable(
    expected: control::ActualHardwareMode,
    state: &mut MonitorState,
) -> ReconcileResult {
    if !state.hardware_reconcile_due() {
        return ReconcileResult::Stable;
    }

    let actual = control::get_actual_charging_state();
    state.mark_reconciled();

    if actual == expected {
        state.hw.mode = actual;
        return ReconcileResult::Stable;
    }

    tracing::warn!(
        ?expected,
        ?actual,
        "hardware drift detected; deferring apply"
    );

    state.hw.mode = control::ActualHardwareMode::Unknown;

    /*
     * A verification read already happened.
     * Force another evaluation where the write can happen.
     */
    ReconcileResult::NeedsNextEvaluation
}

fn reconcile_unknown_or_inconsistent(
    expected: control::ActualHardwareMode,
    state: &mut MonitorState,
) -> ReconcileResult {
    let actual = control::get_actual_charging_state();
    state.mark_reconciled();

    if actual == expected {
        state.hw.mode = actual;
        return ReconcileResult::Stable;
    }

    /*
     * We now have authoritative knowledge that hardware differs.
     */
    state.hw.mode = actual;
    ReconcileResult::NeedsNextEvaluation
}

fn reconcile_apply(
    desired: DesiredHardwareState,
    expected: control::ActualHardwareMode,
    state: &mut MonitorState,
) -> ReconcileResult {
    /*
     * Hardware is known and differs from desired.
     *
     * No read is necessary. Apply directly.
     */
    let success = match desired {
        DesiredHardwareState::ChargingEnabled => apply_charging(true, &mut state.hw.mode),
        DesiredHardwareState::ChargingDisabled => apply_charging(false, &mut state.hw.mode),
        DesiredHardwareState::Bypass => apply_bypass(expected, &mut state.hw.mode),
    };

    state.mark_reconciled();

    if success {
        ReconcileResult::Applied
    } else {
        ReconcileResult::Failed
    }
}

/// Reconcile hardware against the desired state.
///
/// Important invariant:
///
/// A single evaluation performs at most one `get_actual_charging_state()`
/// operation. If a read discovers drift, application is deferred to the next
/// evaluation.
fn reconcile_hardware(desired: DesiredHardwareState, state: &mut MonitorState) -> ReconcileResult {
    let expected = desired.hardware_mode(control::has_distinct_bypass_node());

    if state.hw.mode == expected {
        return reconcile_known_stable(expected, state);
    }

    if matches!(
        state.hw.mode,
        control::ActualHardwareMode::Unknown | control::ActualHardwareMode::Inconsistent
    ) {
        return reconcile_unknown_or_inconsistent(expected, state);
    }

    reconcile_apply(desired, expected, state)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileResult {
    Stable,
    Applied,
    NeedsNextEvaluation,
    Failed,
}

fn restore_normal_charging(state: &mut MonitorState) -> bool {
    match state.hw.mode {
        control::ActualHardwareMode::ChargingEnabled => true,

        control::ActualHardwareMode::Bypass => {
            match control::exit_bypass_mode() {
                Ok(()) => {
                    /*
                     * exit_bypass_mode() does not verify hardware.
                     * Force a reconciliation on the next evaluation.
                     */
                    state.hw.mode = control::ActualHardwareMode::Unknown;

                    state.mark_hardware_changed();
                    state.mark_force_evaluation();

                    true
                }

                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to exit bypass"
                    );

                    state.hw.mode = control::ActualHardwareMode::Unknown;

                    state.mark_hardware_changed();
                    state.mark_failure(FailureKind::Hardware);

                    false
                }
            }
        }

        control::ActualHardwareMode::ChargingDisabled
        | control::ActualHardwareMode::Inconsistent
        | control::ActualHardwareMode::Unknown => {
            /*
             * Do not blindly enable charging here.
             *
             * The next normal evaluation must obtain a fresh snapshot
             * and evaluate thermal/limit policy first.
             */
            state.hw.mode = control::ActualHardwareMode::Unknown;

            state.mark_hardware_changed();
            state.mark_force_evaluation();

            false
        }
    }
}

fn handle_disabled(rx: &UnixDatagram, state: &mut MonitorState) -> bool {
    /*
     * Disabled mode must not blindly enable charging.
     *
     * If we are in bypass, exit bypass so that hardware is returned
     * to normal semantics. Unknown/disabled hardware is left alone
     * until the monitor is enabled again and policy is evaluated.
     */
    let _ = restore_normal_charging(state);

    state.operating_mode = OperatingMode::Normal;
    state.policy = PolicyState::clear();
    state.attach_started = None;
    state.sched.pending_netlink = false;
    state.sched.force_evaluation = false;

    /*
     * Do NOT discard hardware_event_pending if hardware is still
     * unknown/inconsistent. It must survive until the monitor resumes.
     */
    if matches!(
        state.hw.mode,
        control::ActualHardwareMode::Unknown | control::ActualHardwareMode::Inconsistent
    ) {
        state.hw.event_pending = true;
    }

    let mut pollfd = libc::pollfd {
        fd: rx.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, -1) };

        if result < 0 {
            let error = std::io::Error::last_os_error();

            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }

            tracing::error!(
                error = %error,
                "poll failed while disabled"
            );

            std::thread::sleep(Duration::from_secs(1));
            return false;
        }

        if pollfd.revents & libc::POLLIN == 0 {
            continue;
        }

        let mut shutdown = false;
        let mut reload = false;

        drain_ipc(rx, |command| match command {
            MonitorCommand::Shutdown => {
                shutdown = true;
            }

            MonitorCommand::Reload => {
                reload = true;
            }

            /*
             * These commands are intentionally ignored while disabled.
             *
             * A later reload/enable cycle will establish the correct
             * hardware state from a fresh snapshot.
             */
            MonitorCommand::EnableBypass => {}

            MonitorCommand::DisableBypass => {}
        });

        if shutdown {
            tracing::info!("monitor loop shutting down");
            return true;
        }

        if reload {
            state.sched.force_evaluation = true;
        }

        return false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorCommand {
    Shutdown,
    Reload,
    EnableBypass,
    DisableBypass,
}

fn decode_command(value: u8) -> Option<MonitorCommand> {
    match value {
        1 => Some(MonitorCommand::Reload),
        2 => Some(MonitorCommand::Shutdown),
        3 => Some(MonitorCommand::EnableBypass),
        4 => Some(MonitorCommand::DisableBypass),
        _ => None,
    }
}

fn drain_ipc<F>(rx: &UnixDatagram, mut handle: F)
where
    F: FnMut(MonitorCommand),
{
    loop {
        let mut buffer = [0u8; 1];

        match rx.recv(&mut buffer) {
            Ok(0) => break,

            Ok(_) => {
                if let Some(command) = decode_command(buffer[0]) {
                    handle(command);
                }
            }

            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                break;
            }

            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Failed reading internal IPC"
                );
                break;
            }
        }
    }
}

fn process_ipc(rx: &UnixDatagram, state: &mut MonitorState) -> bool {
    let mut shutdown = false;

    drain_ipc(rx, |command| match command {
        MonitorCommand::Shutdown => {
            shutdown = true;
        }

        MonitorCommand::Reload => {
            state.mark_force_evaluation();

            tracing::debug!("configuration reload requested");
        }

        MonitorCommand::EnableBypass => {
            /*
             * Policy is evaluated by evaluate_once().
             *
             * Bypass is only a desired operating mode here.
             */
            state.operating_mode = OperatingMode::Bypass;

            state.mark_hardware_changed();
            state.mark_force_evaluation();

            tracing::info!("bypass mode enabled");
        }

        MonitorCommand::DisableBypass => {
            /*
             * IMPORTANT:
             *
             * Do not call exit_bypass_mode() here.
             *
             * The current hardware state may need to become:
             *
             *   ChargingDisabled
             *
             * because charge_limit or thermal policy may currently
             * block charging.
             *
             * A fresh snapshot must determine the desired state first.
             */
            state.operating_mode = OperatingMode::Normal;

            state.mark_hardware_changed();
            state.mark_force_evaluation();

            tracing::info!("bypass mode disabled");
        }
    });

    shutdown
}

fn calculate_timeout(
    state: &MonitorState,
    scheduler: &mut AdaptiveScheduler,
    netlink_available: bool,
    diagnostics: &DaemonDiagnostics,
) -> Option<Duration> {
    diagnostics.is_idle.store(
        state.power_state.is_disconnected(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let max_backoff = state
        .sched
        .snapshot_backoff
        .max(state.sched.hardware_backoff);
    diagnostics.error_backoff_ms.store(
        max_backoff.as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    if state.sched.force_evaluation {
        /*
         * Respect active retry windows for snapshot and hardware failures.
         * If both are active, take the earliest retry time so we resume as
         * soon as the first retry window expires.
         */
        let now = Instant::now();
        let retry_at = match (
            state.sched.next_snapshot_retry_at,
            state.sched.next_hardware_retry_at,
        ) {
            (Some(s), Some(h)) => Some(s.min(h)),
            (Some(s), None) => Some(s),
            (None, Some(h)) => Some(h),
            (None, None) => None,
        };

        if let Some(retry_at) = retry_at {
            if now < retry_at {
                let wait = retry_at - now;
                diagnostics.poll_interval_ms.store(
                    wait.as_millis() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                return Some(wait);
            }
        }

        diagnostics
            .poll_interval_ms
            .store(0, std::sync::atomic::Ordering::Relaxed);
        return Some(Duration::ZERO);
    }

    /*
     * Ultra-Low-Power Idle saat charger terlepas.
     *
     * Dua jalur:
     *
     * a) Netlink tersedia → timeout INFINITE (None / poll -1).
     *    Kernel membangunkan poll hanya saat ada uevent.
     *    CPU usage = 0.00%.
     *
     * b) Netlink tidak tersedia → FALLBACK_HEARTBEAT.
     *    Pemeriksaan berkala ringan sebagai safeguard.
     *
     * pending_netlink hanya mungkin true di sini jika uevent
     * baru saja diterima; dalam kasus itu kita tidak idle.
     */
    if state.power_state.is_disconnected() && !state.sched.pending_netlink {
        if netlink_available {
            diagnostics
                .poll_interval_ms
                .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
            return None;
        } else {
            diagnostics.poll_interval_ms.store(
                FALLBACK_HEARTBEAT.as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            return Some(FALLBACK_HEARTBEAT);
        }
    }

    let mut timeout = scheduler.next_interval(state.policy, state.operating_mode);

    if state.sched.pending_netlink {
        let elapsed = state.sched.last_evaluation.elapsed();

        if elapsed >= NETLINK_COALESCE {
            diagnostics
                .poll_interval_ms
                .store(0, std::sync::atomic::Ordering::Relaxed);
            return Some(Duration::ZERO);
        }

        timeout = timeout.min(NETLINK_COALESCE - elapsed);
    }

    if let Some(attached_at) = state.attach_started {
        let elapsed = attached_at.elapsed();

        if elapsed < ATTACH_SETTLE_WINDOW {
            timeout = timeout.min(ATTACH_SETTLE_WINDOW - elapsed);
        }
    }

    diagnostics.poll_interval_ms.store(
        timeout.as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    Some(timeout)
}

fn handle_netlink(fd: RawFd, state: &mut MonitorState) {
    match drain_netlink(fd) {
        NetlinkEvent::Fast => {
            state.sched.pending_netlink = false;
            state.mark_hardware_changed();
            state.mark_force_evaluation();
        }

        NetlinkEvent::Coalesced => {
            // When disconnected, telemetry / coalesced events should be ignored
            // so the daemon remains sleeping in poll(-1).
            if state.power_state.is_disconnected() {
                state.sched.pending_netlink = false;
                return;
            }

            state.sched.pending_netlink = true;
            state.mark_hardware_changed();

            if state.sched.last_evaluation.elapsed() >= NETLINK_COALESCE {
                state.mark_force_evaluation();
            }
        }

        NetlinkEvent::None => {}
    }
}

fn handle_power_transition(
    snapshot: MonitorSnapshot,
    state: &mut MonitorState,
    scheduler: &mut AdaptiveScheduler,
) {
    if snapshot.power_state == state.power_state {
        return;
    }

    tracing::debug!(
        previous = ?state.power_state,
        current = ?snapshot.power_state,
        "power state changed"
    );

    let previous = state.power_state;

    state.power_state = snapshot.power_state;

    if snapshot.power_state.is_disconnected() {
        state.reset_charger_state();
        scheduler.reset();
    }

    if snapshot.power_state.is_plugged_in() && previous.is_disconnected() {
        state.attach_started = Some(Instant::now());

        state.mark_hardware_changed();

        state.mark_success_all();

        scheduler.reset();
    }
}

fn log_policy_change(previous: PolicyState, current: PolicyState, snapshot: MonitorSnapshot) {
    if previous == current {
        return;
    }

    tracing::info!(
        soc = snapshot.capacity,
        temperature_c = snapshot.temperature_c,
        limit_blocked_previous = previous.limit_blocked,
        limit_blocked = current.limit_blocked,
        thermal_blocked_previous = previous.thermal_blocked,
        thermal_blocked = current.thermal_blocked,
        "charging policy changed"
    );
}

fn log_apply_result(desired: DesiredHardwareState, snapshot: MonitorSnapshot, policy: PolicyState) {
    tracing::info!(
        ?desired,
        soc = snapshot.capacity,
        temperature_c = snapshot.temperature_c,
        limit_blocked = policy.limit_blocked,
        thermal_blocked = policy.thermal_blocked,
        "charging hardware state applied"
    );
}

/// Fail-safe helper invoked when a monitor snapshot cannot be read.
/// If charging is currently enabled, attempts to disable charging for safety.
fn apply_thermal_failsafe(state: &mut MonitorState, cfg: &Config) {
    if !cfg.thermal_cutoff {
        state.hw.mode = control::ActualHardwareMode::Unknown;
        return;
    }

    match control::emergency_disable_charging() {
        Ok(()) => {
            let actual = control::get_actual_charging_state();

            if actual == control::ActualHardwareMode::ChargingDisabled {
                state.hw.mode = actual;

                tracing::warn!("thermal fail-safe emergency charging disable verified");
            } else {
                state.hw.mode = actual;

                tracing::warn!(
                    ?actual,
                    "thermal fail-safe emergency disable applied; hardware mode updated"
                );
            }
        }

        Err(error) => {
            tracing::error!(
                error = %error,
                "failed fail-safe emergency charging disable"
            );

            state.hw.mode = control::ActualHardwareMode::Unknown;
        }
    }
}

fn should_trigger_failsafe(
    hw_mode: control::ActualHardwareMode,
    power_state: reader::PowerState,
    thermal_cutoff: bool,
) -> bool {
    thermal_cutoff && !power_state.is_disconnected() && !hw_mode.is_charging_disabled()
}

fn evaluate_once(cfg: &Config, state: &mut MonitorState, scheduler: &mut AdaptiveScheduler) {
    let snapshot = match read_monitor_snapshot() {
        Ok(snapshot) => {
            state.mark_success(FailureKind::Snapshot);
            snapshot
        }

        Err(reason) => {
            tracing::error!(reason, "failed to read monitor snapshot");

            /*
             * We cannot safely evaluate normal policy without a
             * valid snapshot.
             *
             * Thermal protection fails closed by attempting to disable charging
             * if charging is active or hardware state is unverified while plugged in.
             */
            if should_trigger_failsafe(state.hw.mode, state.power_state, cfg.thermal_cutoff) {
                tracing::warn!(
                    hw_mode = ?state.hw.mode,
                    power_state = ?state.power_state,
                    "snapshot unreadable while potential charging active — attempting fail-safe thermal disable"
                );

                apply_thermal_failsafe(state, cfg);
            } else {
                if !state.hw.mode.is_charging_disabled() {
                    state.hw.mode = control::ActualHardwareMode::Unknown;
                }
            }

            /*
             * Keep hardware reconciliation alive after a read failure.
             */
            state.mark_hardware_changed();
            state.mark_failure(FailureKind::Snapshot);

            state.sched.last_evaluation = Instant::now();

            return;
        }
    };

    handle_power_transition(snapshot, state, scheduler);

    let sample = Sample {
        capacity: snapshot.capacity,
        temperature_c: snapshot.temperature_c,
        power_state: snapshot.power_state,
        timestamp: Instant::now(),
    };

    scheduler.push(sample);

    /*
     * Bypass has priority over charge-limit policy,
     * but never over thermal safety.
     */
    if state.operating_mode == OperatingMode::Bypass {
        state.policy = evaluate_policy(snapshot, state.policy, cfg);

        let desired = desired_hardware_state(state.operating_mode, state.policy);

        let result = reconcile_hardware(desired, state);

        match result {
            ReconcileResult::Applied => {
                log_apply_result(desired, snapshot, state.policy);

                state.mark_success(FailureKind::Hardware);
            }

            ReconcileResult::Failed => {
                state.mark_failure(FailureKind::Hardware);
                state.mark_force_evaluation();
            }

            ReconcileResult::NeedsNextEvaluation => {
                /*
                 * Kept for API/state-machine compatibility.
                 * Current reconciliation normally resolves the
                 * discrepancy immediately.
                 */
                state.mark_force_evaluation();
            }

            ReconcileResult::Stable => {}
        }

        state.sched.last_evaluation = Instant::now();

        return;
    }

    /*
     * Disconnect:
     *
     * Clear policy, tetapi JANGAN langsung mengaktifkan charging.
     *
     * Reset pending_netlink agar calculate_timeout bisa masuk
     * ke mode idle infinite pada iterasi loop berikutnya.
     */
    if snapshot.power_state.is_disconnected() {
        state.policy = PolicyState::clear();
        state.sched.pending_netlink = false;

        scheduler.reset();

        if state.hw.mode == control::ActualHardwareMode::Bypass {
            if let Err(error) = control::exit_bypass_mode() {
                tracing::warn!(
                    error = %error,
                    "failed to exit bypass mode while disabled"
                );

                state.hw.mode = control::ActualHardwareMode::Unknown;

                state.mark_failure(FailureKind::Hardware);
            } else {
                state.hw.mode = control::ActualHardwareMode::Unknown;

                state.mark_hardware_changed();
            }
        }

        state.sched.last_evaluation = Instant::now();

        tracing::debug!("charger disconnected — entering ultra-low-power idle");

        return;
    }

    /*
     * Normal policy evaluation.
     */
    let previous_policy = state.policy;

    state.policy = evaluate_policy(snapshot, state.policy, cfg);

    log_policy_change(previous_policy, state.policy, snapshot);

    let desired = desired_hardware_state(state.operating_mode, state.policy);

    let result = reconcile_hardware(desired, state);

    match result {
        ReconcileResult::Applied => {
            log_apply_result(desired, snapshot, state.policy);

            state.mark_success(FailureKind::Hardware);
        }

        ReconcileResult::Failed => {
            state.mark_failure(FailureKind::Hardware);

            /*
             * Retry even when there is no new netlink event.
             */
            state.mark_force_evaluation();
        }

        ReconcileResult::NeedsNextEvaluation => {
            state.mark_force_evaluation();
        }

        ReconcileResult::Stable => {}
    }

    state.sched.last_evaluation = Instant::now();
}

/// Main charger monitor loop.
///
/// Architecture:
///
///     event -> wake -> snapshot -> policy -> desired state
///          -> reconciliation -> apply -> schedule
///
/// The monitor owns logical state. `reader` only reads hardware inputs and
/// `control` only manipulates hardware outputs.
pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    rx: UnixDatagram,
    diagnostics: Arc<DaemonDiagnostics>,
) {
    tracing::info!("charger monitor started");

    let initial_config = {
        config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    };

    let nl_fd = create_netlink_socket().unwrap_or(-1);

    let _netlink_guard = NetlinkFd(nl_fd);

    let netlink_available = nl_fd >= 0;

    diagnostics
        .netlink_available
        .store(netlink_available, std::sync::atomic::Ordering::Relaxed);

    if netlink_available {
        tracing::info!("netlink power-supply events enabled");
    } else {
        tracing::warn!("netlink unavailable; using fallback heartbeat");
    }

    let mut state = MonitorState::new();

    let mut scheduler = AdaptiveScheduler::new(&initial_config);

    loop {
        let cfg = config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        scheduler.update_config(&cfg);

        /*
         * Disabled mode is a genuine idle state.
         */
        if !cfg.enabled {
            if handle_disabled(&rx, &mut state) {
                return;
            }

            continue;
        }

        let timeout = calculate_timeout(&state, &mut scheduler, netlink_available, &diagnostics);

        let mut pollfds = [
            libc::pollfd {
                fd: rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: nl_fd,
                events: if netlink_available { libc::POLLIN } else { 0 },
                revents: 0,
            },
        ];

        let nfds = if netlink_available { 2 } else { 1 };

        let timeout_ms = timeout_to_poll_ms(timeout);

        let result = unsafe { libc::poll(pollfds.as_mut_ptr(), nfds, timeout_ms) };

        if result < 0 {
            let error = std::io::Error::last_os_error();

            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }

            tracing::error!(
                error = %error,
                "poll failed"
            );

            std::thread::sleep(Duration::from_secs(1));

            continue;
        }

        if result > 0 && pollfds[0].revents & libc::POLLIN != 0 && process_ipc(&rx, &mut state) {
            return;
        }

        if netlink_available && result > 0 && pollfds[1].revents & libc::POLLIN != 0 {
            handle_netlink(nl_fd, &mut state);
        }

        if state.sched.pending_netlink && state.sched.last_evaluation.elapsed() >= NETLINK_COALESCE
        {
            state.mark_force_evaluation();
        }

        /*
         * result == 0 berarti poll timeout habis (tidak ada event).
         *
         * Saat Disconnected + Netlink tersedia, timeout = infinite (-1)
         * sehingga result == 0 hanya terjadi jika Netlink tidak tersedia
         * dan FALLBACK_HEARTBEAT habis. Dalam kasus itu kita perlu evaluasi
         * untuk mendeteksi apakah charger sudah terpasang kembali.
         *
         * Saat charger terpasang (Connected), result == 0 berarti interval
         * normal habis dan evaluasi pengisian perlu dilakukan.
         */
        if result == 0 {
            state.mark_force_evaluation();
        }

        if !state.sched.force_evaluation {
            continue;
        }

        state.clear_evaluation_request();

        evaluate_once(&cfg, &mut state, &mut scheduler);

        if let Some(attached_at) = state.attach_started {
            if attached_at.elapsed() >= ATTACH_SETTLE_WINDOW {
                state.attach_started = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_trigger_failsafe() {
        // Charging enabled + plugged in + thermal cutoff on -> failsafe triggers
        assert!(should_trigger_failsafe(
            control::ActualHardwareMode::ChargingEnabled,
            reader::PowerState::Charging,
            true
        ));

        // Hardware mode Unknown + plugged in + thermal cutoff on -> failsafe triggers for safety
        assert!(should_trigger_failsafe(
            control::ActualHardwareMode::Unknown,
            reader::PowerState::Charging,
            true
        ));

        // Hardware mode Inconsistent + plugged in + thermal cutoff on -> failsafe triggers
        assert!(should_trigger_failsafe(
            control::ActualHardwareMode::Inconsistent,
            reader::PowerState::Connected,
            true
        ));

        // Hardware verified ChargingDisabled -> no need to trigger failsafe
        assert!(!should_trigger_failsafe(
            control::ActualHardwareMode::ChargingDisabled,
            reader::PowerState::Charging,
            true
        ));

        // Disconnected -> no failsafe needed
        assert!(!should_trigger_failsafe(
            control::ActualHardwareMode::ChargingEnabled,
            reader::PowerState::Disconnected,
            true
        ));

        // Thermal cutoff disabled -> failsafe does not trigger
        assert!(!should_trigger_failsafe(
            control::ActualHardwareMode::ChargingEnabled,
            reader::PowerState::Charging,
            false
        ));
    }
}

