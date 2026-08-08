use charger_core::{
    battery::{control, reader::BatteryStatus, reader::CachedReader},
    config::schema::Config,
};
use std::collections::VecDeque;
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600); // 10 minutes

const DANGER_TEMP_MARGIN: f32 = 3.0;
const DANGER_CAP_MARGIN: f32 = 2.0;
const EMA_ALPHA: f32 = 0.3;
const NETLINK_DEBOUNCE: Duration = Duration::from_millis(250);
const FAULT_RECOVERY_READS: u8 = 3;

const PREDICTION_SAFETY_FACTOR: f32 = 0.5;
const TEMP_RATE_DANGER: f32 = 0.15;
const EMA_HISTORY_LEN: usize = 5;
const VERIFY_DELAY: Duration = Duration::from_millis(500);
const MAX_VERIFICATION_FAILURES: u8 = 3;
const NETLINK_RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const NETLINK_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct SensorSnapshot {
    capacity_pct: Option<u8>,
    temp_dc: Option<i32>,
    #[allow(dead_code)]
    current_ma: Option<i32>,
    status: Option<BatteryStatus>,
    online: Option<bool>,
    ts: Instant,
}

impl SensorSnapshot {
    fn is_charging(&self) -> bool {
        matches!(self.status, Some(BatteryStatus::Charging))
    }
}

// Sensor criticality policy:
// - Temperature is safety-critical and drives Fault on read failure.
// - Capacity is policy-critical; if missing, the daemon holds state and takes no action.
// - Status/current are advisory for scheduling and hardware verification.
// - Online state is a routing/event signal for unplugged behavior.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChargeState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    Fault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChargeCommand {
    Enable,
    Disable,
    RestoreCharging,
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionReason {
    DaemonDisabled,
    FaultRecovering,
    ChargerOffline,
    NormalCharging,
    ChargeLimitReached,
    WaitingForLimitResume,
    ThermalLimitReached,
    WaitingForThermalResume,
    SensorFault,
}

impl fmt::Display for DecisionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DecisionReason::DaemonDisabled => "daemon_disabled",
            DecisionReason::FaultRecovering => "fault_recovering",
            DecisionReason::ChargerOffline => "charger_offline",
            DecisionReason::NormalCharging => "normal_charging",
            DecisionReason::ChargeLimitReached => "charge_limit_reached",
            DecisionReason::WaitingForLimitResume => "waiting_for_limit_resume",
            DecisionReason::ThermalLimitReached => "thermal_limit_reached",
            DecisionReason::WaitingForThermalResume => "waiting_for_thermal_resume",
            DecisionReason::SensorFault => "sensor_fault",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug)]
struct Decision {
    command: ChargeCommand,
    state: ChargeState,
    reason: DecisionReason,
}

struct AdaptiveScheduler {
    limit: f32,
    resume_limit: f32,
    thermal_cutoff: f32,
    history: VecDeque<SensorSnapshot>,
    ema_cap_rate: f32,
    ema_temp_rate: f32,
    last_interval: Duration,
}

impl AdaptiveScheduler {
    fn new(limit: u8, resume_limit: u8, thermal_cutoff: i32) -> Self {
        Self {
            limit: limit as f32,
            resume_limit: resume_limit as f32,
            thermal_cutoff: thermal_cutoff as f32 / 10.0,
            history: VecDeque::new(),
            ema_cap_rate: 0.0,
            ema_temp_rate: 0.0,
            last_interval: MIN_INTERVAL,
        }
    }

    fn push_sample(&mut self, s: SensorSnapshot) {
        if let Some(prev) = self.history.back() {
            if prev
                .charging_state()
                .zip(s.charging_state())
                .is_some_and(|(prev, current)| prev != current)
            {
                self.ema_cap_rate = 0.0;
            }

            let dt = (s.ts - prev.ts).as_secs_f32().max(0.5);

            if let (Some(cap), Some(prev_cap)) = (s.capacity_pct, prev.capacity_pct) {
                self.ema_cap_rate = EMA_ALPHA * ((cap as f32 - prev_cap as f32) / dt)
                    + (1.0 - EMA_ALPHA) * self.ema_cap_rate;
            }
            if let (Some(temp), Some(prev_temp)) = (s.temp_dc, prev.temp_dc) {
                self.ema_temp_rate = EMA_ALPHA
                    * ((temp as f32 / 10.0 - prev_temp as f32 / 10.0) / dt)
                    + (1.0 - EMA_ALPHA) * self.ema_temp_rate;
            }
        }
        self.history.push_back(s);
        if self.history.len() > EMA_HISTORY_LEN {
            self.history.pop_front();
        }
    }

    fn next_interval(&mut self, s: &SensorSnapshot) -> Duration {
        if s.online == Some(false) {
            self.last_interval = UNPLUGGED_HEARTBEAT;
            return self.last_interval;
        }

        let (Some(cap), Some(temp_dc)) = (s.capacity_pct, s.temp_dc) else {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        };
        let Some(is_charging) = s.charging_state() else {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        };

        let cap = cap as f32;
        let temp = temp_dc as f32 / 10.0;

        if !is_charging && cap <= self.resume_limit {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        let dist_to_limit = (self.limit - cap).max(0.0);
        let dist_to_thermal = (self.thermal_cutoff - temp).max(0.0);
        let dist_to_resume = (cap - self.resume_limit).max(0.0);

        let danger_high = dist_to_limit < DANGER_CAP_MARGIN
            || dist_to_thermal < DANGER_TEMP_MARGIN
            || self.ema_temp_rate > TEMP_RATE_DANGER;
        let danger_low = !is_charging && dist_to_resume < DANGER_CAP_MARGIN;

        if danger_high || danger_low {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        let predicted = if is_charging && self.ema_cap_rate > 0.01 {
            Duration::from_secs_f32(
                (dist_to_limit / self.ema_cap_rate * PREDICTION_SAFETY_FACTOR).max(0.0),
            )
        } else if !is_charging && self.ema_cap_rate < -0.01 {
            Duration::from_secs_f32(
                (dist_to_resume / (-self.ema_cap_rate) * PREDICTION_SAFETY_FACTOR).max(0.0),
            )
        } else {
            MAX_INTERVAL
        };
        let target = predicted.clamp(MIN_INTERVAL, MAX_INTERVAL);

        self.last_interval = if target < self.last_interval {
            target
        } else {
            self.last_interval
                .mul_f32(1.5)
                .min(MAX_INTERVAL)
                .min(target.max(self.last_interval))
        };
        self.last_interval
    }
}

fn create_netlink_socket() -> std::io::Result<OwnedFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
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

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

struct DecisionEngine {
    state: ChargeState,
    fault_recovery_reads: u8,
}

impl DecisionEngine {
    fn new() -> Self {
        Self {
            state: ChargeState::Charging,
        }
    }

    fn reconfigure(&mut self, cfg: &Config, snapshot: Option<&SensorSnapshot>) {
        match self.state {
            ChargeState::ThermalCutoff if !cfg.thermal_cutoff => {
                tracing::info!(
                    "Reconfigure: Thermal cutoff disabled, recovering to Charging state."
                );
                self.state = ChargeState::Charging;
            }
            ChargeState::LimitReached => {
                if cfg.charge_limit >= 100
                    || snapshot
                        .and_then(|s| s.capacity_pct)
                        .is_some_and(|cap| cap < cfg.charge_limit)
                {
                    tracing::info!(
                        "Reconfigure: Charge limit now permits charging, recovering to Charging state."
                    );
                    self.state = ChargeState::Charging;
                }
            }
            _ => {}
        }
    }

    fn evaluate(&mut self, snapshot: &SensorSnapshot, cfg: &Config) -> Decision {
        if !cfg.enabled {
            self.fault_recovery_reads = 0;
            self.state = ChargeState::Disabled;
            return Decision {
                command: ChargeCommand::ReleaseControl,
                state: ChargeState::Disabled,
                reason: DecisionReason::DaemonDisabled,
            };
        }

        if snapshot.online == Some(false) {
            self.fault_recovery_reads = 0;
            self.state = ChargeState::Offline;
            return Decision {
                command: ChargeCommand::Noop,
                state: ChargeState::Offline,
                reason: DecisionReason::ChargerOffline,
            };
        }

        if snapshot.temp_dc.is_none() {
            self.state = ChargeState::Fault {
                retry_count: FAULT_RECOVERY_READS,
            };
            return Decision {
                command: ChargeCommand::Disable,
                state: self.state,
                reason: DecisionReason::SensorFault,
            };
        }

        if let ChargeState::Fault { retry_count } = self.state {
            if retry_count > 0 {
                self.state = ChargeState::Fault {
                    retry_count: retry_count - 1,
                };
                return Decision {
                    command: ChargeCommand::Disable,
                    state: self.state,
                    reason: DecisionReason::SensorFault,
                };
            } else {
                tracing::info!("Sensor recovered completely, exiting Fault state.");
                self.state = ChargeState::Charging;
            }

            tracing::info!(
                "Sensor recovered for {} consecutive reads, exiting Fault state.",
                self.fault_recovery_reads
            );
            self.fault_recovery_reads = 0;
            self.state = ChargeState::Charging;
        }

        if snapshot.capacity_pct.is_none() {
            // Missing capacity is non-critical, we hold current state and take no action.
            return Decision {
                command: ChargeCommand::Noop,
                state: self.state,
                reason: DecisionReason::SensorFault,
            };
        }

        let cap = snapshot.capacity_pct.unwrap();
        let temp = snapshot.temp_dc.unwrap();

        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };
        let thermal_max = cfg.max_temp_dc;
        let safe_hysteresis = cfg
            .thermal_resume_hysteresis_dc
            .clamp(1, thermal_max.saturating_sub(1).max(1));
        let thermal_resume = thermal_max.saturating_sub(safe_hysteresis);

        match self.state {
            ChargeState::Disabled | ChargeState::Offline => {
                self.state = ChargeState::Charging;
                self.evaluate(snapshot, cfg)
            }
            ChargeState::Fault { .. } => Decision {
                command: ChargeCommand::Noop,
                state: self.state,
                reason: DecisionReason::SensorFault,
            },
            ChargeState::Charging => {
                if cfg.thermal_cutoff && temp >= thermal_max {
                    self.state = ChargeState::ThermalCutoff;
                    Decision {
                        command: ChargeCommand::Disable,
                        state: self.state,
                        reason: DecisionReason::ThermalLimitReached,
                    }
                } else if cap >= limit {
                    self.state = ChargeState::LimitReached;
                    Decision {
                        command: ChargeCommand::Disable,
                        state: self.state,
                        reason: DecisionReason::ChargeLimitReached,
                    }
                } else {
                    Decision {
                        command: ChargeCommand::Enable,
                        state: self.state,
                        reason: DecisionReason::NormalCharging,
                    }
                }
            }
            ChargeState::LimitReached => {
                if cap <= resume {
                    self.state = ChargeState::Charging;
                    Decision {
                        command: ChargeCommand::Enable,
                        state: self.state,
                        reason: DecisionReason::NormalCharging,
                    }
                } else {
                    Decision {
                        command: ChargeCommand::Disable,
                        state: self.state,
                        reason: DecisionReason::WaitingForLimitResume,
                    }
                }
            }
            ChargeState::ThermalCutoff => {
                if temp <= thermal_resume {
                    self.state = ChargeState::Charging;
                    Decision {
                        command: ChargeCommand::Enable,
                        state: self.state,
                        reason: DecisionReason::NormalCharging,
                    }
                } else {
                    Decision {
                        command: ChargeCommand::Disable,
                        state: self.state,
                        reason: DecisionReason::WaitingForThermalResume,
                    }
                }
            }
        }
    }
}

pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    tracing::info!("Monitor loop started (Analisa_2.md Final Fixes)");

    let mut battery_reader = CachedReader::new();
    let mut _nl_sock = match create_netlink_socket() {
        Ok(sock) => {
            tracing::info!("Successfully bound to NETLINK_KOBJECT_UEVENT");
            Some(sock)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to bind Netlink socket ({}). Falling back to pure adaptive timer",
                e
            );
            None
        }
    };

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
    let mut engine = DecisionEngine::new();
    engine.reconfigure(&initial_cfg, None);

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
    let mut num_fds = 1;
    if let Some(ref nl) = _nl_sock {
        pfds[1].fd = nl.as_raw_fd();
        pfds[1].events = libc::POLLIN;
        num_fds = 2;
    }

    let mut verification_deadline: Option<Instant> = None;
    let mut pending_verification_state: Option<ChargeState> = None;
    let mut verification_failures = 0u8;
    let mut next_netlink_reconnect = if _nl_sock.is_some() {
        None
    } else {
        Some(Instant::now() + NETLINK_RECONNECT_INITIAL_BACKOFF)
    };
    let mut netlink_reconnect_backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;

    loop {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();

        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };
        scheduler.limit = limit as f32;
        scheduler.resume_limit = resume as f32;
        scheduler.thermal_cutoff = cfg.max_temp_dc as f32 / 10.0;

        let snapshot = SensorSnapshot {
            capacity_pct: battery_reader.read_capacity().ok(),
            temp_dc: battery_reader.read_temperature_dc().ok(),
            current_ma: battery_reader.read_current_ma().map(|c| c as i32).ok(),
            status: battery_reader.read_status().ok(),
            online: battery_reader.is_plugged_in().ok(),
            ts: Instant::now(),
        };

        scheduler.push_sample(snapshot.clone());

        // Perform asynchronous verification if deadline reached
        if let Some(deadline) = verification_deadline {
            if Instant::now() >= deadline {
                verification_deadline = None;
                if let Some(state) = pending_verification_state {
                    match state {
                        ChargeState::Charging => {
                            if !snapshot.is_charging() && snapshot.online == Some(true) {
                                tracing::warn!(
                                    "Verification: Hardware is NOT charging, but state is Charging"
                                );
                            }
                        }
                        ChargeState::Disabled
                        | ChargeState::LimitReached
                        | ChargeState::ThermalCutoff => {
                            if snapshot.is_charging() {
                                tracing::warn!(
                                    "Verification: Hardware is STILL charging, but state is {:?}",
                                    state
                                );
                            }
                        }
                        verification_deadline = Some(Instant::now() + VERIFY_DELAY);
                    }
                } else {
                    verification_failures = 0;
                    pending_verification_state = None;
                }
            }
        } else {
            scheduler.push_sample(snapshot.clone());
        }

        engine.reconfigure(&cfg, Some(&snapshot));

        let prev_state = engine.state;
        let decision = engine.evaluate(&snapshot, &cfg);

        if prev_state != decision.state {
            tracing::info!(
                "State transition: {:?} -> {:?} (Reason: {})",
                prev_state,
                decision.state,
                decision.reason
            );
        }

        match decision.command {
            ChargeCommand::Enable => {
                if prev_state != decision.state {
                    if let Err(e) = control::set_charging(true) {
                        tracing::error!("Failed to enable charging: {}", e);
                    }
                    verification_deadline = Some(Instant::now() + VERIFY_DELAY);
                    pending_verification_state = Some(decision.state);
                }
            }
            ChargeCommand::RestoreCharging => {
                if prev_state != decision.state {
                    if let Err(e) = control::set_charging(true) {
                        tracing::error!("Failed to restore charging: {}", e);
                    }
                }
            }
            ChargeCommand::Disable => {
                if prev_state != decision.state {
                    if let Err(e) = control::set_charging(false) {
                        tracing::error!("Failed to disable charging: {}", e);
                    }
                    verification_deadline = Some(Instant::now() + VERIFY_DELAY);
                    pending_verification_state = Some(decision.state);
                }
            }
            ChargeCommand::Noop => {}
        }

        let timeout = scheduler.next_interval(&snapshot);
        let mut should_evaluate = false;
        let mut now = Instant::now();
        let target_wake = now + timeout;
        let mut debounce_target: Option<Instant> = None;

        while now < target_wake {
            // Determine shortest wait time (debounce, verification, or target wake)
            let mut next_wake = target_wake;

            if let Some(debounce) = debounce_target {
                if now >= debounce {
                    should_evaluate = true;
                    break;
                }
                next_wake = next_wake.min(debounce);
            }

            if let Some(vd) = verification_deadline {
                if now >= vd {
                    should_evaluate = true;
                    break;
                }
                next_wake = next_wake.min(vd);
            }

            if let Some(reconnect_at) = next_netlink_reconnect {
                if now >= reconnect_at {
                    match create_netlink_socket() {
                        Ok(new_sock) => {
                            tracing::info!("Netlink socket reconnected successfully");
                            pfds[1].fd = new_sock.as_raw_fd();
                            pfds[1].events = libc::POLLIN;
                            num_fds = 2;
                            _nl_sock = Some(new_sock);
                            next_netlink_reconnect = None;
                            netlink_reconnect_backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Netlink reconnect failed ({}); retrying in {:?}.",
                                e,
                                netlink_reconnect_backoff
                            );
                            next_netlink_reconnect = Some(now + netlink_reconnect_backoff);
                            netlink_reconnect_backoff =
                                (netlink_reconnect_backoff * 2).min(NETLINK_RECONNECT_MAX_BACKOFF);
                        }
                    }
                }
                if let Some(reconnect_at) = next_netlink_reconnect {
                    next_wake = next_wake.min(reconnect_at);
                }
            }

            let remaining = next_wake.saturating_duration_since(now);

            pfds[0].revents = 0;
            if num_fds > 1 {
                pfds[1].revents = 0;
            }

            let ret = unsafe {
                libc::poll(
                    pfds.as_mut_ptr(),
                    num_fds as libc::nfds_t,
                    remaining.as_millis() as i32,
                )
            };
            now = Instant::now();

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
                tracing::error!("IPC socket error/hangup. Exiting monitor loop.");
                return;
            }

            if ipc_events & libc::POLLIN != 0 {
                let mut buf = [0u8; 1];
                if rx.recv(&mut buf).is_ok() {
                    if buf[0] == 2 {
                        tracing::info!("Monitor loop shutting down via IPC");
                        return;
                    }
                    if buf[0] == 1 {
                        tracing::info!("Config reloaded");
                        let latest_cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
                        engine.reconfigure(&latest_cfg, None);
                        should_evaluate = true;
                        break;
                    }
                }
            }

            if num_fds > 1 {
                let nl_events = pfds[1].revents;
                if nl_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    tracing::error!("Netlink socket error. Reconnecting...");
                    _nl_sock = None;
                    num_fds = 1; // Drop netlink from poll
                    pfds[1].fd = -1;

                    // Attempt reconnect
                    match create_netlink_socket() {
                        Ok(new_sock) => {
                            tracing::info!("Netlink socket reconnected successfully");
                            pfds[1].fd = new_sock.as_raw_fd();
                            pfds[1].events = libc::POLLIN;
                            num_fds = 2;
                            _nl_sock = Some(new_sock);
                        }
                        Err(e) => {
                            tracing::warn!("Netlink reconnect failed ({}).", e);
                        }
                    }
                } else if nl_events & libc::POLLIN != 0 {
                    let mut buf = [0u8; 4096];
                    let mut found = false;
                    loop {
                        let raw_fd = pfds[1].fd;
                        let n = unsafe {
                            libc::recv(
                                raw_fd,
                                buf.as_mut_ptr() as *mut libc::c_void,
                                buf.len(),
                                libc::MSG_DONTWAIT,
                            )
                        };
                        if n <= 0 {
                            break;
                        }
                        let buf_slice = &buf[..n as usize];

                        if contains_subslice(buf_slice, b"SUBSYSTEM=power_supply")
                            && contains_subslice(buf_slice, b"ACTION=change")
                        {
                            found = true;
                        }
                    }
                    if found && debounce_target.is_none() {
                        debounce_target = Some(now + NETLINK_DEBOUNCE);
                    }
                }
            }
        }

        if !should_evaluate {
            continue;
        }
    }
}
