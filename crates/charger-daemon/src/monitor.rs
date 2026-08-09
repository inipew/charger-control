use std::{
    collections::VecDeque,
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixDatagram,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use charger_core::{
    battery::{control, reader},
    config::schema::Config,
};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);

const FALLBACK_HEARTBEAT: Duration = Duration::from_secs(600);
const ATTACHED_SETTLE_INTERVAL: Duration = Duration::from_secs(3);

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

#[derive(Debug)]
struct MonitorState {
    operating_mode: OperatingMode,

    policy: PolicyState,

    hardware: control::ActualHardwareMode,

    power_state: reader::PowerState,

    last_evaluation: Instant,

    last_hardware_reconcile: Instant,

    attach_started: Option<Instant>,

    hardware_event_pending: bool,

    pending_netlink: bool,

    force_evaluation: bool,

    error_backoff: Duration,
}

impl MonitorState {
    fn new() -> Self {
        let now = Instant::now();

        Self {
            operating_mode: OperatingMode::Normal,

            policy: PolicyState::clear(),

            hardware: control::ActualHardwareMode::Unknown,

            power_state: reader::PowerState::Unknown,

            last_evaluation: now - Duration::from_secs(60),

            last_hardware_reconcile: now - HARDWARE_RECONCILE_INTERVAL,

            attach_started: None,

            hardware_event_pending: false,

            pending_netlink: false,

            force_evaluation: true,

            error_backoff: ERROR_BACKOFF_INITIAL,
        }
    }

    fn mark_hardware_changed(&mut self) {
        self.hardware_event_pending = true;
    }

    fn mark_force_evaluation(&mut self) {
        self.force_evaluation = true;
    }

    fn clear_evaluation_request(&mut self) {
        self.force_evaluation = false;
        self.pending_netlink = false;
    }

    fn mark_success(&mut self) {
        self.error_backoff = ERROR_BACKOFF_INITIAL;
    }

    fn mark_failure(&mut self) {
        self.error_backoff = (self.error_backoff * 2).min(ERROR_BACKOFF_MAX);
    }

    fn hardware_reconcile_due(&self) -> bool {
        self.hardware_event_pending
            || self.last_hardware_reconcile.elapsed() >= HARDWARE_RECONCILE_INTERVAL
    }

    fn mark_reconciled(&mut self) {
        self.last_hardware_reconcile = Instant::now();
        self.hardware_event_pending = false;
    }

    fn reset_charger_state(&mut self) {
        self.policy = PolicyState::clear();
        self.attach_started = None;
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

                let temperature_delta = sample.temperature_c - previous.temperature_c;

                let temperature_rate = temperature_delta / seconds;

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

        match sample.power_state {
            reader::PowerState::Disconnected => {
                self.last_interval = FALLBACK_HEARTBEAT;
                return self.last_interval;
            }

            reader::PowerState::Attached => {
                self.last_interval = ATTACHED_SETTLE_INTERVAL;
                return self.last_interval;
            }

            _ => {}
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
    let mut buffer = [0u8; 8192];

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
    data.split(|byte| *byte == 0).any(|part| match name {
        b"ac" => part.starts_with(b"POWER_SUPPLY_ONLINE="),

        b"battery" => {
            part.starts_with(b"POWER_SUPPLY_STATUS=")
                || part.starts_with(b"POWER_SUPPLY_CAPACITY=")
                || part.starts_with(b"POWER_SUPPLY_TEMP=")
        }

        b"usb" => {
            part.starts_with(b"POWER_SUPPLY_TYPEC_MODE=")
                || part.starts_with(b"POWER_SUPPLY_ONLINE=")
                || part.starts_with(b"POWER_SUPPLY_PRESENT=")
        }

        _ => false,
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
            | b"bms"
            | b"mtk-charger"
            | b"mt_charger"
    )
}

fn duration_to_poll_ms(duration: Duration) -> i32 {
    duration.as_millis().min(i32::MAX as u128) as i32
}

#[derive(Debug, Clone, Copy)]
struct MonitorSnapshot {
    capacity: f32,
    temperature_dc: i32,
    power_state: reader::PowerState,
}

fn read_monitor_snapshot() -> Result<MonitorSnapshot, &'static str> {
    let capacity = reader::read_capacity_raw().map_err(|_| "battery_capacity_read_failed")?;

    if !capacity.is_finite() {
        return Err("battery_capacity_non_finite");
    }

    let capacity = capacity.clamp(0.0, 100.0);

    let temperature_dc =
        reader::read_temperature_dc().map_err(|_| "battery_temperature_read_failed")?;

    let power_state = reader::get_power_state().map_err(|_| "power_state_read_failed")?;

    if power_state == reader::PowerState::Unknown {
        return Err("power_state_unknown");
    }

    Ok(MonitorSnapshot {
        capacity,
        temperature_dc,
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
        evaluate_thermal_policy(snapshot.temperature_dc, previous.thermal_blocked, cfg);

    let limit_blocked = evaluate_limit_policy(snapshot.capacity, previous.limit_blocked, cfg);

    PolicyState {
        thermal_blocked,
        limit_blocked,
    }
}

fn evaluate_thermal_policy(temperature_dc: i32, previous_blocked: bool, cfg: &Config) -> bool {
    if !cfg.thermal_cutoff {
        return false;
    }

    if temperature_dc >= cfg.max_temp_dc {
        return true;
    }

    if previous_blocked {
        let resume = cfg.max_temp_dc.saturating_sub(THERMAL_HYSTERESIS_DC);

        return temperature_dc > resume;
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

/// Reconcile hardware against the desired state.
///
/// Important invariant:
///
/// A single evaluation performs at most one `get_actual_charging_state()`
/// operation. If a read discovers drift, application is deferred to the next
/// evaluation.
fn reconcile_hardware(desired: DesiredHardwareState, state: &mut MonitorState) -> ReconcileResult {
    let expected = desired.hardware_mode(control::has_distinct_bypass_node());

    let reconcile_due = state.hardware_reconcile_due();

    /*
     * Known-good state.
     */
    if state.hardware == expected {
        if !reconcile_due {
            return ReconcileResult::Stable;
        }

        let actual = control::get_actual_charging_state();

        state.mark_reconciled();

        if actual == expected {
            state.hardware = actual;
            return ReconcileResult::Stable;
        }

        tracing::warn!(
            ?expected,
            ?actual,
            "hardware drift detected; deferring apply"
        );

        state.hardware = control::ActualHardwareMode::Unknown;

        /*
         * A verification read already happened.
         * Force another evaluation where the write can happen.
         */
        return ReconcileResult::NeedsNextEvaluation;
    }

    /*
     * Unknown/inconsistent state.
     *
     * Probe first. Never write after a probe in the same evaluation.
     */
    if matches!(
        state.hardware,
        control::ActualHardwareMode::Unknown | control::ActualHardwareMode::Inconsistent
    ) {
        let actual = control::get_actual_charging_state();

        state.mark_reconciled();

        if actual == expected {
            state.hardware = actual;
            return ReconcileResult::Stable;
        }

        /*
         * We now have authoritative knowledge that hardware differs.
         */
        state.hardware = actual;

        return ReconcileResult::NeedsNextEvaluation;
    }

    /*
     * Hardware is known and differs from desired.
     *
     * No read is necessary. Apply directly.
     */
    let success = match desired {
        DesiredHardwareState::ChargingEnabled => apply_charging(true, &mut state.hardware),

        DesiredHardwareState::ChargingDisabled => apply_charging(false, &mut state.hardware),

        DesiredHardwareState::Bypass => apply_bypass(expected, &mut state.hardware),
    };

    state.mark_reconciled();

    if success {
        ReconcileResult::Applied
    } else {
        ReconcileResult::Failed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileResult {
    Stable,
    Applied,
    NeedsNextEvaluation,
    Failed,
}

fn restore_normal_charging(state: &mut MonitorState) -> bool {
    match state.hardware {
        control::ActualHardwareMode::ChargingEnabled => true,

        control::ActualHardwareMode::Bypass => {
            match control::exit_bypass_mode() {
                Ok(()) => {
                    /*
                     * exit_bypass_mode() does not verify hardware.
                     * Force a reconciliation on the next evaluation.
                     */
                    state.hardware = control::ActualHardwareMode::Unknown;

                    state.mark_hardware_changed();

                    true
                }

                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to exit bypass"
                    );

                    state.hardware = control::ActualHardwareMode::Unknown;

                    state.mark_hardware_changed();
                    state.mark_failure();

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
            state.hardware = control::ActualHardwareMode::Unknown;

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
    state.pending_netlink = false;
    state.force_evaluation = false;

    /*
     * Do NOT discard hardware_event_pending if hardware is still
     * unknown/inconsistent. It must survive until the monitor resumes.
     */
    if matches!(
        state.hardware,
        control::ActualHardwareMode::Unknown | control::ActualHardwareMode::Inconsistent
    ) {
        state.hardware_event_pending = true;
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
            state.force_evaluation = true;
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
) -> Duration {
    if state.force_evaluation {
        return Duration::ZERO;
    }

    let mut timeout = scheduler.next_interval(state.policy, state.operating_mode);

    if state.power_state.is_disconnected() && netlink_available && !state.pending_netlink {
        timeout = Duration::from_secs(u64::MAX / 2);
    }

    if state.pending_netlink {
        let elapsed = state.last_evaluation.elapsed();

        if elapsed >= NETLINK_COALESCE {
            return Duration::ZERO;
        }

        timeout = timeout.min(NETLINK_COALESCE - elapsed);
    }

    if let Some(attached_at) = state.attach_started {
        let elapsed = attached_at.elapsed();

        if elapsed < ATTACH_SETTLE_WINDOW {
            timeout = timeout.min(ATTACH_SETTLE_WINDOW - elapsed);
        }
    }

    if state.error_backoff > ERROR_BACKOFF_INITIAL {
        timeout = timeout.max(state.error_backoff);
    }

    timeout
}

fn handle_netlink(fd: RawFd, state: &mut MonitorState) {
    match drain_netlink(fd) {
        NetlinkEvent::Fast => {
            /*
             * A fast event means that an important power-supply
             * attribute may have changed.
             *
             * It does NOT necessarily mean that a charger was attached.
             * Attach timing is derived exclusively from PowerState
             * transitions in handle_power_transition().
             */
            state.pending_netlink = false;
            state.mark_hardware_changed();
            state.mark_force_evaluation();
        }

        NetlinkEvent::Coalesced => {
            state.pending_netlink = true;
            state.mark_hardware_changed();

            if state.last_evaluation.elapsed() >= NETLINK_COALESCE {
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

        scheduler.reset();
    }
}

fn log_policy_change(previous: PolicyState, current: PolicyState, snapshot: MonitorSnapshot) {
    if previous == current {
        return;
    }

    tracing::info!(
        soc = snapshot.capacity,
        temperature_c = snapshot.temperature_dc as f32 / 10.0,
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
        temperature_c = snapshot.temperature_dc as f32 / 10.0,
        limit_blocked = policy.limit_blocked,
        thermal_blocked = policy.thermal_blocked,
        "charging hardware state applied"
    );
}

fn evaluate_once(cfg: &Config, state: &mut MonitorState, scheduler: &mut AdaptiveScheduler) {
    let snapshot = match read_monitor_snapshot() {
        Ok(snapshot) => {
            state.mark_success();
            snapshot
        }

        Err(reason) => {
            tracing::error!(reason, "failed to read monitor snapshot");

            /*
             * We cannot safely evaluate normal policy without a
             * valid snapshot.
             *
             * Thermal protection therefore fails closed.
             */
            if cfg.thermal_cutoff {
                match control::set_charging(false) {
                    Ok(()) => {
                        let actual = control::get_actual_charging_state();

                        if actual == control::ActualHardwareMode::ChargingDisabled {
                            state.hardware = actual;

                            tracing::warn!("thermal fail-safe charging disable verified");
                        } else {
                            state.hardware = control::ActualHardwareMode::Unknown;

                            tracing::error!(
                                ?actual,
                                "thermal fail-safe charging disable could not be verified"
                            );
                        }
                    }

                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            "failed fail-safe charging disable"
                        );

                        state.hardware = control::ActualHardwareMode::Unknown;
                    }
                }
            } else {
                state.hardware = control::ActualHardwareMode::Unknown;
            }

            /*
             * Keep hardware reconciliation alive after a read failure.
             */
            state.mark_hardware_changed();
            state.mark_failure();

            state.last_evaluation = Instant::now();

            return;
        }
    };

    handle_power_transition(snapshot, state, scheduler);

    let sample = Sample {
        capacity: snapshot.capacity,
        temperature_c: snapshot.temperature_dc as f32 / 10.0,
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

                state.mark_success();
            }

            ReconcileResult::Failed => {
                state.mark_failure();
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

        state.last_evaluation = Instant::now();

        return;
    }

    /*
     * Disconnect:
     *
     * Clear policy, but do not blindly enable charging.
     */
    if snapshot.power_state.is_disconnected() {
        state.policy = PolicyState::clear();

        scheduler.reset();

        /*
         * If hardware is in bypass, exit it.
         * Otherwise leave charging state alone until a valid
         * attached snapshot establishes the next desired state.
         */
        if state.hardware == control::ActualHardwareMode::Bypass {
            if let Err(error) = control::exit_bypass_mode() {
                tracing::warn!(
                    error = %error,
                    "failed to exit bypass while disconnected"
                );

                state.hardware = control::ActualHardwareMode::Unknown;

                state.mark_failure();
            } else {
                state.hardware = control::ActualHardwareMode::Unknown;

                state.mark_hardware_changed();
            }
        }

        state.last_evaluation = Instant::now();

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

            state.mark_success();
        }

        ReconcileResult::Failed => {
            state.mark_failure();

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

    state.last_evaluation = Instant::now();
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
pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
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

        let timeout = calculate_timeout(&state, &mut scheduler, netlink_available);

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

        let timeout_ms = duration_to_poll_ms(timeout);

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

        if state.pending_netlink && state.last_evaluation.elapsed() >= NETLINK_COALESCE {
            state.mark_force_evaluation();
        }

        if result == 0 {
            state.mark_force_evaluation();
        }

        if !state.force_evaluation {
            continue;
        }

        state.clear_evaluation_request();

        evaluate_once(&cfg, &mut state, &mut scheduler);

        /*
         * If attach settle window has expired, remove it.
         */
        if let Some(attached_at) = state.attach_started {
            if attached_at.elapsed() >= ATTACH_SETTLE_WINDOW {
                state.attach_started = None;
            }
        }
    }
}
