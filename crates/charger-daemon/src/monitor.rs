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
struct EvaluationContext {
    now: Instant,
}

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
enum DeadlineSource {
    Event,
    Telemetry,
    Policy,
    Reconcile,
    Heartbeat,
    Attach,
}

#[derive(Debug, Clone, Copy)]
struct NextDeadline {
    at: Instant,
    #[allow(dead_code)]
    source: DeadlineSource,
}

#[derive(Debug)]
struct HardwareTrack {
    /// Last known physical hardware charging mode.
    mode: control::ActualHardwareMode,

    /// Set when hardware state has changed and reconciliation is due.
    event_pending: bool,
}

impl HardwareTrack {
    fn new() -> Self {
        Self {
            mode: control::ActualHardwareMode::Unknown,
            event_pending: false,
        }
    }

    fn mark_changed(&mut self) {
        self.event_pending = true;
    }

    fn mark_reconciled(&mut self) {
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
}

impl MonitorState {
    fn new() -> Self {
        Self {
            operating_mode: OperatingMode::Normal,
            policy: PolicyState::clear(),
            power_state: reader::PowerState::Unknown,
            attach_started: None,
            hw: HardwareTrack::new(),
        }
    }

    fn mark_hardware_changed(&mut self) {
        self.hw.mark_changed();
    }

    fn mark_reconciled(&mut self) {
        self.hw.mark_reconciled();
    }

    fn reset_charger_state(&mut self) {
        self.policy = PolicyState::clear();
        self.attach_started = None;
    }
}

#[derive(Debug)]
struct EventScheduler {
    next_deadline: Option<Instant>,
}

impl EventScheduler {
    fn new(now: Instant) -> Self {
        Self {
            next_deadline: Some(now),
        }
    }

    fn request_immediate(&mut self, now: Instant) {
        self.request_at(now);
    }

    fn request_at(&mut self, deadline: Instant) {
        self.next_deadline = Some(
            self.next_deadline
                .map(|current| current.min(deadline))
                .unwrap_or(deadline),
        );
    }

    fn consume(&mut self) {
        self.next_deadline = None;
    }
}

#[derive(Debug, Default)]
struct HeartbeatScheduler {
    next_deadline: Option<Instant>,
}

impl HeartbeatScheduler {
    fn new() -> Self {
        Self { next_deadline: None }
    }

    fn schedule(&mut self, now: Instant) {
        if self.next_deadline.is_none() {
            self.next_deadline = Some(now + FALLBACK_HEARTBEAT);
        }
    }

    fn clear(&mut self) {
        self.next_deadline = None;
    }

    fn consume(&mut self) {
        self.next_deadline = None;
    }
}

#[derive(Debug)]
struct TelemetryScheduler {
    configured_interval: Duration,
    limit: f32,
    thermal_cutoff_c: f32,
    history: VecDeque<Sample>,
    ema_capacity_rate: f32,
    ema_temperature_rate: f32,
    last_interval: Duration,
    next_deadline: Option<Instant>,
}

impl TelemetryScheduler {
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
            next_deadline: None,
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
        self.next_deadline = None;
    }

    fn schedule_next(&mut self, now: Instant, policy: PolicyState, operating_mode: OperatingMode) {
        let interval = self.next_interval(policy, operating_mode);
        self.next_deadline = Some(now + interval);
    }

    fn clear_deadline(&mut self) {
        self.next_deadline = None;
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

            let temperature_delta = sample.temperature_c - previous.temperature_c;
            let temperature_rate = temperature_delta / seconds;

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

#[derive(Debug, Default)]
struct PolicyScheduler {
    next_deadline: Option<Instant>,
}

impl PolicyScheduler {
    fn schedule(&mut self, now: Instant, policy: PolicyState, operating_mode: OperatingMode) {
        let interval = if operating_mode == OperatingMode::Bypass {
            BYPASS_INTERVAL
        } else if policy.thermal_blocked {
            THERMAL_BLOCKED_INTERVAL
        } else if policy.limit_blocked {
            LIMIT_BLOCKED_INTERVAL
        } else {
            self.next_deadline = None;
            return;
        };

        self.next_deadline = Some(now + interval);
    }

    fn clear(&mut self) {
        self.next_deadline = None;
    }
}

#[derive(Debug)]
struct ReconcileScheduler {
    snapshot_backoff: Duration,
    hardware_backoff: Duration,
    next_snapshot_retry_at: Option<Instant>,
    next_hardware_retry_at: Option<Instant>,
    next_periodic_reconcile_at: Option<Instant>,
}

impl ReconcileScheduler {
    fn new() -> Self {
        Self {
            snapshot_backoff: ERROR_BACKOFF_INITIAL,
            hardware_backoff: ERROR_BACKOFF_INITIAL,
            next_snapshot_retry_at: None,
            next_hardware_retry_at: None,
            next_periodic_reconcile_at: None,
        }
    }

    fn mark_snapshot_success(&mut self) {
        self.snapshot_backoff = ERROR_BACKOFF_INITIAL;
        self.next_snapshot_retry_at = None;
    }

    fn mark_hardware_success(&mut self) {
        self.hardware_backoff = ERROR_BACKOFF_INITIAL;
        self.next_hardware_retry_at = None;
    }

    fn mark_snapshot_failure(&mut self, now: Instant) {
        self.next_snapshot_retry_at = Some(now + self.snapshot_backoff);
        self.snapshot_backoff = (self.snapshot_backoff * 2).min(ERROR_BACKOFF_MAX);
    }

    fn mark_hardware_failure(&mut self, now: Instant) {
        self.next_hardware_retry_at = Some(now + self.hardware_backoff);
        self.hardware_backoff = (self.hardware_backoff * 2).min(ERROR_BACKOFF_MAX);
    }

    fn schedule_periodic(&mut self, now: Instant) {
        self.next_periodic_reconcile_at = Some(now + HARDWARE_RECONCILE_INTERVAL);
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut deadline = None;
        Self::merge_deadline(&mut deadline, self.next_snapshot_retry_at);
        Self::merge_deadline(&mut deadline, self.next_hardware_retry_at);
        Self::merge_deadline(&mut deadline, self.next_periodic_reconcile_at);
        deadline
    }

    fn merge_deadline(current: &mut Option<Instant>, candidate: Option<Instant>) {
        if let Some(candidate) = candidate {
            *current = Some(
                current
                    .map(|existing| existing.min(candidate))
                    .unwrap_or(candidate),
            );
        }
    }

    fn clear(&mut self) {
        self.next_snapshot_retry_at = None;
        self.next_hardware_retry_at = None;
        self.next_periodic_reconcile_at = None;
        self.snapshot_backoff = ERROR_BACKOFF_INITIAL;
        self.hardware_backoff = ERROR_BACKOFF_INITIAL;
    }

    fn consume_deadline(&mut self, now: Instant) {
        if self.next_snapshot_retry_at.is_some_and(|d| d <= now) {
            self.next_snapshot_retry_at = None;
        }
        if self.next_hardware_retry_at.is_some_and(|d| d <= now) {
            self.next_hardware_retry_at = None;
        }
        if self.next_periodic_reconcile_at.is_some_and(|d| d <= now) {
            self.next_periodic_reconcile_at = None;
        }
    }

    fn snapshot_retry_eligible(&self, now: Instant) -> bool {
        self.next_snapshot_retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    fn hardware_retry_eligible(&self, now: Instant) -> bool {
        self.next_hardware_retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    fn max_backoff(&self) -> Duration {
        self.snapshot_backoff.max(self.hardware_backoff)
    }
}

#[derive(Debug)]
struct MultiScheduler {
    event: EventScheduler,
    telemetry: TelemetryScheduler,
    policy: PolicyScheduler,
    reconcile: ReconcileScheduler,
    heartbeat: HeartbeatScheduler,
}

impl MultiScheduler {
    fn new(cfg: &Config, now: Instant) -> Self {
        Self {
            event: EventScheduler::new(now),
            telemetry: TelemetryScheduler::new(cfg),
            policy: PolicyScheduler::default(),
            reconcile: ReconcileScheduler::new(),
            heartbeat: HeartbeatScheduler::new(),
        }
    }

    fn update_config(&mut self, cfg: &Config) {
        self.telemetry.update_config(cfg);
    }

    fn reset(&mut self, now: Instant) {
        self.telemetry.reset();
        self.policy.clear();
        self.reconcile.clear();
        self.heartbeat.clear();
        self.event.request_immediate(now);
    }

    fn sync_environment(&mut self, now: Instant, state: &MonitorState, netlink_available: bool) {
        if state.power_state.is_disconnected() && !netlink_available {
            self.heartbeat.schedule(now);
        } else {
            self.heartbeat.clear();
        }
    }

    fn consume_expired(&mut self, now: Instant) {
        if self.event.next_deadline.is_some_and(|d| d <= now) {
            self.event.consume();
        }
        if self.telemetry.next_deadline.is_some_and(|d| d <= now) {
            self.telemetry.clear_deadline();
        }
        if self.policy.next_deadline.is_some_and(|d| d <= now) {
            self.policy.clear();
        }
        if self.reconcile.next_deadline().is_some_and(|d| d <= now) {
            self.reconcile.consume_deadline(now);
        }
        if self.heartbeat.next_deadline.is_some_and(|d| d <= now) {
            self.heartbeat.consume();
        }
    }

    fn next_deadline(&self, state: &MonitorState, now: Instant) -> Option<NextDeadline> {
        let mut result = None;

        if state.hw.event_pending {
            Self::merge_deadline(&mut result, Some(now), DeadlineSource::Event);
        }

        Self::merge_deadline(&mut result, self.event.next_deadline, DeadlineSource::Event);
        Self::merge_deadline(&mut result, self.telemetry.next_deadline, DeadlineSource::Telemetry);
        Self::merge_deadline(&mut result, self.policy.next_deadline, DeadlineSource::Policy);
        Self::merge_deadline(&mut result, self.reconcile.next_deadline(), DeadlineSource::Reconcile);
        Self::merge_deadline(&mut result, self.heartbeat.next_deadline, DeadlineSource::Heartbeat);

        if let Some(attached_at) = state.attach_started {
            Self::merge_deadline(
                &mut result,
                Some(attached_at + ATTACH_SETTLE_WINDOW),
                DeadlineSource::Attach,
            );
        }

        result
    }

    fn merge_deadline(
        current: &mut Option<NextDeadline>,
        candidate: Option<Instant>,
        source: DeadlineSource,
    ) {
        let Some(candidate) = candidate else {
            return;
        };

        let candidate = NextDeadline {
            at: candidate,
            source,
        };

        *current = Some(match *current {
            Some(current) if current.at <= candidate.at => current,
            _ => candidate,
        });
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
    data.split(|byte| *byte == 0).any(|part| {
        part.starts_with(b"POWER_SUPPLY_ONLINE=")
            || part.starts_with(b"POWER_SUPPLY_PRESENT=")
            || part.starts_with(b"POWER_SUPPLY_STATUS=")
            || part.starts_with(b"POWER_SUPPLY_CHARGING_ENABLED=")
            || part.starts_with(b"POWER_SUPPLY_INPUT_SUSPEND=")
            || match name {
                b"usb" | b"charger" | b"typec" => part.starts_with(b"POWER_SUPPLY_TYPEC_MODE="),
                _ => false,
            }
    })
}

fn is_relevant_power_supply(name: &[u8]) -> bool {
    matches!(
        name,
        b"usb"
            | b"battery"
            | b"main"
            | b"ac"
            | b"wireless"
            | b"wls"
            | b"bms"
            | b"mtk-charger"
            | b"mt_charger"
            | b"charger"
            | b"sec-charger"
            | b"pd-charger"
            | b"typec"
            | b"tcpc"
            | b"dc"
    )
}

fn deadline_to_timeout(now: Instant, deadline: Option<NextDeadline>) -> Option<Duration> {
    deadline.map(|deadline| deadline.at.saturating_duration_since(now))
}

fn update_diagnostics(
    diagnostics: &DaemonDiagnostics,
    state: &MonitorState,
    scheduler: &MultiScheduler,
    timeout: Option<Duration>,
) {
    diagnostics.is_idle.store(
        state.power_state.is_disconnected(),
        std::sync::atomic::Ordering::Relaxed,
    );

    diagnostics.error_backoff_ms.store(
        scheduler.reconcile.max_backoff().as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    diagnostics.poll_interval_ms.store(
        timeout.map(|duration| duration.as_millis() as u64).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
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

    if !temperature_c.is_finite() {
        return Err("battery_temperature_non_finite");
    }

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

    if capacity >= limit {
        return true;
    }

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

    state.hw.mode = actual;

    if matches!(
        actual,
        control::ActualHardwareMode::Unknown | control::ActualHardwareMode::Inconsistent
    ) {
        tracing::warn!(
            ?actual,
            ?expected,
            "hardware probe returned Unknown or Inconsistent; applying error backoff"
        );
        ReconcileResult::Failed
    } else {
        ReconcileResult::NeedsNextEvaluation
    }
}

fn reconcile_apply(
    desired: DesiredHardwareState,
    expected: control::ActualHardwareMode,
    state: &mut MonitorState,
) -> ReconcileResult {
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

fn reconcile_hardware(
    desired: DesiredHardwareState,
    state: &mut MonitorState,
    scheduler: &mut MultiScheduler,
    now: Instant,
) -> ReconcileResult {
    if !scheduler.reconcile.hardware_retry_eligible(now) {
        tracing::debug!("hardware reconciliation suppressed by backoff window");
        return ReconcileResult::Stable;
    }

    let expected = desired.hardware_mode(control::has_distinct_bypass_node());

    if state.hw.mode == control::ActualHardwareMode::Bypass
        && desired != DesiredHardwareState::Bypass
    {
        tracing::info!("transitioning out of Bypass mode — executing exit_bypass_mode");
        if let Err(error) = control::exit_bypass_mode() {
            tracing::error!(error = %error, "failed to exit bypass mode");
            state.hw.mode = control::ActualHardwareMode::Unknown;
            return ReconcileResult::Failed;
        }

        state.hw.mode = control::ActualHardwareMode::Unknown;
        state.mark_hardware_changed();
    }

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

fn restore_normal_charging(state: &mut MonitorState, scheduler: &mut MultiScheduler, now: Instant) -> bool {
    match state.hw.mode {
        control::ActualHardwareMode::ChargingEnabled => true,
        control::ActualHardwareMode::Bypass => {
            match control::exit_bypass_mode() {
                Ok(()) => {
                    state.hw.mode = control::ActualHardwareMode::Unknown;
                    state.mark_hardware_changed();
                    true
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to exit bypass"
                    );
                    state.hw.mode = control::ActualHardwareMode::Unknown;
                    state.mark_hardware_changed();
                    scheduler.reconcile.mark_hardware_failure(now);
                    false
                }
            }
        }
        control::ActualHardwareMode::ChargingDisabled
        | control::ActualHardwareMode::Inconsistent
        | control::ActualHardwareMode::Unknown => {
            state.hw.mode = control::ActualHardwareMode::Unknown;
            state.mark_hardware_changed();
            scheduler.event.request_immediate(now);
            false
        }
    }
}

fn handle_disabled(rx: &UnixDatagram, state: &mut MonitorState, scheduler: &mut MultiScheduler) -> bool {
    let now = Instant::now();
    let _ = restore_normal_charging(state, scheduler, now);

    state.operating_mode = OperatingMode::Normal;
    state.policy = PolicyState::clear();
    state.attach_started = None;
    scheduler.event.consume();

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

        let revents = pollfd.revents;
        if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            tracing::error!(revents, "IPC socket error while disabled");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        if revents & libc::POLLIN == 0 {
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
            MonitorCommand::EnableBypass => {}
            MonitorCommand::DisableBypass => {}
        });

        if shutdown {
            tracing::info!("monitor loop shutting down");
            return true;
        }

        if reload {
            scheduler.event.request_immediate(Instant::now());
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

fn process_ipc(
    rx: &UnixDatagram,
    state: &mut MonitorState,
    scheduler: &mut MultiScheduler,
    now: Instant,
) -> bool {
    let mut shutdown = false;

    drain_ipc(rx, |command| match command {
        MonitorCommand::Shutdown => {
            shutdown = true;
        }
        MonitorCommand::Reload => {
            scheduler.event.request_immediate(now);
            tracing::debug!("configuration reload requested");
        }
        MonitorCommand::EnableBypass => {
            state.operating_mode = OperatingMode::Bypass;
            state.mark_hardware_changed();
            scheduler.event.request_immediate(now);
            tracing::info!("bypass mode enabled");
        }
        MonitorCommand::DisableBypass => {
            state.operating_mode = OperatingMode::Normal;
            state.mark_hardware_changed();
            scheduler.event.request_immediate(now);
            tracing::info!("bypass mode disabled");
        }
    });

    shutdown
}

fn handle_netlink(
    fd: RawFd,
    state: &mut MonitorState,
    scheduler: &mut MultiScheduler,
    now: Instant,
) {
    match drain_netlink(fd) {
        NetlinkEvent::Fast => {
            state.mark_hardware_changed();
            scheduler.event.request_immediate(now);
        }
        NetlinkEvent::Coalesced => {
            state.mark_hardware_changed();
            scheduler.event.request_at(now + NETLINK_COALESCE);
        }
        NetlinkEvent::None => {}
    }
}

fn handle_power_transition(
    ctx: EvaluationContext,
    snapshot: MonitorSnapshot,
    state: &mut MonitorState,
    scheduler: &mut MultiScheduler,
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
        state.hw.mode = control::ActualHardwareMode::Unknown;
        state.hw.event_pending = false;
        scheduler.reset(ctx.now);
        scheduler.event.consume();
    }

    if snapshot.power_state.is_plugged_in() && previous.is_disconnected() {
        state.attach_started = Some(ctx.now);
        state.mark_hardware_changed();
        scheduler.reconcile.mark_snapshot_success();
        scheduler.reconcile.mark_hardware_success();
        scheduler.reset(ctx.now);
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

fn apply_telemetry_failsafe(state: &mut MonitorState) {
    match control::set_charging(false) {
        Ok(()) => {
            let actual = control::get_actual_charging_state();

            if actual == control::ActualHardwareMode::ChargingDisabled {
                state.hw.mode = actual;
                tracing::warn!("telemetry fail-safe charging disable verified");
            } else {
                state.hw.mode = control::ActualHardwareMode::Unknown;
                tracing::error!(
                    ?actual,
                    "telemetry fail-safe charging disable could not be verified"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "failed telemetry fail-safe charging disable"
            );
            state.hw.mode = control::ActualHardwareMode::Unknown;
        }
    }
}

fn evaluate_once(
    ctx: EvaluationContext,
    cfg: &Config,
    state: &mut MonitorState,
    scheduler: &mut MultiScheduler,
) {
    let snapshot = match read_monitor_snapshot() {
        Ok(snapshot) => {
            scheduler.reconcile.mark_snapshot_success();
            snapshot
        }
        Err(reason) => {
            if !scheduler.reconcile.snapshot_retry_eligible(ctx.now) {
                tracing::debug!("snapshot retry suppressed by backoff window");
                return;
            }

            tracing::error!(reason, "failed to read monitor snapshot");

            if state.hw.mode.is_charging_enabled() {
                tracing::warn!("snapshot unreadable while charging — attempting fail-safe disable");
                apply_telemetry_failsafe(state);
            } else {
                state.hw.mode = control::ActualHardwareMode::Unknown;
            }

            scheduler.reconcile.mark_snapshot_failure(ctx.now);
            return;
        }
    };

    handle_power_transition(ctx, snapshot, state, scheduler);

    let sample = Sample {
        capacity: snapshot.capacity,
        temperature_c: snapshot.temperature_c,
        power_state: snapshot.power_state,
        timestamp: ctx.now,
    };

    scheduler.telemetry.push(sample);

    if state.operating_mode == OperatingMode::Bypass {
        state.policy = evaluate_policy(snapshot, state.policy, cfg);
        scheduler.policy.schedule(ctx.now, state.policy, state.operating_mode);

        let desired = desired_hardware_state(state.operating_mode, state.policy);
        let result = reconcile_hardware(desired, state, scheduler, ctx.now);

        match result {
            ReconcileResult::Applied => {
                log_apply_result(desired, snapshot, state.policy);
                scheduler.reconcile.mark_hardware_success();
                scheduler.reconcile.schedule_periodic(ctx.now);
            }
            ReconcileResult::Failed => {
                scheduler.reconcile.mark_hardware_failure(ctx.now);
            }
            ReconcileResult::NeedsNextEvaluation => {
                scheduler.event.request_at(ctx.now + Duration::from_millis(250));
            }
            ReconcileResult::Stable => {
                scheduler.reconcile.schedule_periodic(ctx.now);
            }
        }

        scheduler.telemetry.schedule_next(ctx.now, state.policy, state.operating_mode);
        return;
    }

    if snapshot.power_state.is_disconnected() {
        state.policy = PolicyState::clear();
        scheduler.reset(ctx.now);
        scheduler.event.consume();
        scheduler.telemetry.clear_deadline();

        if state.hw.mode == control::ActualHardwareMode::Bypass {
            if let Err(error) = control::exit_bypass_mode() {
                tracing::warn!(
                    error = %error,
                    "failed to exit bypass mode while disabled"
                );
                state.hw.mode = control::ActualHardwareMode::Unknown;
                scheduler.reconcile.mark_hardware_failure(ctx.now);
            } else {
                state.hw.mode = control::ActualHardwareMode::Unknown;
                state.mark_hardware_changed();
            }
        }

        tracing::debug!("charger disconnected — entering ultra-low-power idle");
        return;
    }

    let previous_policy = state.policy;
    state.policy = evaluate_policy(snapshot, state.policy, cfg);
    scheduler.policy.schedule(ctx.now, state.policy, state.operating_mode);

    log_policy_change(previous_policy, state.policy, snapshot);

    let desired = desired_hardware_state(state.operating_mode, state.policy);
    let result = reconcile_hardware(desired, state, scheduler, ctx.now);

    match result {
        ReconcileResult::Applied => {
            log_apply_result(desired, snapshot, state.policy);
            scheduler.reconcile.mark_hardware_success();
            scheduler.reconcile.schedule_periodic(ctx.now);
        }
        ReconcileResult::Failed => {
            scheduler.reconcile.mark_hardware_failure(ctx.now);
        }
        ReconcileResult::NeedsNextEvaluation => {
            scheduler.event.request_at(ctx.now + Duration::from_millis(250));
        }
        ReconcileResult::Stable => {
            scheduler.reconcile.schedule_periodic(ctx.now);
        }
    }

    scheduler.telemetry.schedule_next(ctx.now, state.policy, state.operating_mode);
}

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
    let mut netlink_available = nl_fd >= 0;

    diagnostics
        .netlink_available
        .store(netlink_available, std::sync::atomic::Ordering::Relaxed);

    if netlink_available {
        tracing::info!("netlink power-supply events enabled");
    } else {
        tracing::warn!("netlink unavailable; using fallback heartbeat");
    }

    let mut state = MonitorState::new();
    let startup_now = Instant::now();
    let mut scheduler = MultiScheduler::new(&initial_config, startup_now);

    loop {
        let cfg = config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        scheduler.update_config(&cfg);

        if !cfg.enabled {
            if handle_disabled(&rx, &mut state, &mut scheduler) {
                return;
            }
            continue;
        }

        let now = Instant::now();
        scheduler.sync_environment(now, &state, netlink_available);

        let deadline = scheduler.next_deadline(&state, now);
        let timeout = deadline_to_timeout(now, deadline);
        update_diagnostics(&diagnostics, &state, &scheduler, timeout);

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

        let wake_now = Instant::now();

        if result > 0 {
            let ipc_revents = pollfds[0].revents;
            if ipc_revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                tracing::error!(revents = ipc_revents, "IPC socket error detected");
            } else if ipc_revents & libc::POLLIN != 0 && process_ipc(&rx, &mut state, &mut scheduler, wake_now) {
                return;
            }

            if netlink_available {
                let nl_revents = pollfds[1].revents;
                if nl_revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    tracing::warn!(revents = nl_revents, "Netlink socket error detected; disabling netlink and switching to fallback heartbeat");
                    netlink_available = false;
                    diagnostics
                        .netlink_available
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                } else if nl_revents & libc::POLLIN != 0 {
                    handle_netlink(nl_fd, &mut state, &mut scheduler, wake_now);
                }
            }
        }

        let expired = scheduler
            .next_deadline(&state, wake_now)
            .is_some_and(|deadline| wake_now >= deadline.at);

        if expired {
            scheduler.consume_expired(wake_now);

            let ctx = EvaluationContext { now: wake_now };
            evaluate_once(ctx, &cfg, &mut state, &mut scheduler);

            if let Some(attached_at) = state.attach_started {
                if wake_now.saturating_duration_since(attached_at) >= ATTACH_SETTLE_WINDOW {
                    state.attach_started = None;
                }
            }
        }
    }
}
