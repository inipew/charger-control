use charger_core::{
    battery::{control, reader::CachedReader},
    config::schema::Config,
};
use std::collections::VecDeque;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600); // 10 minutes

#[derive(Clone, Debug)]
struct SensorSnapshot {
    capacity_pct: u8,
    temp_dc: i32,
    _current_ma: i32,
    online: bool,
    _charging: bool, // derived from current_ma > 50
    ts: Instant,
}

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
    Noop,
}

#[derive(Debug)]
struct Decision {
    command: ChargeCommand,
    state: ChargeState,
    reason: &'static str,
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
            let dt = (s.ts - prev.ts).as_secs_f32().max(0.5);
            const ALPHA: f32 = 0.3; // Smoothing factor
            self.ema_cap_rate =
                ALPHA * ((s.capacity_pct as f32 - prev.capacity_pct as f32) / dt) + (1.0 - ALPHA) * self.ema_cap_rate;
            self.ema_temp_rate =
                ALPHA * ((s.temp_dc as f32 / 10.0 - prev.temp_dc as f32 / 10.0) / dt) + (1.0 - ALPHA) * self.ema_temp_rate;
        }
        self.history.push_back(s);
        if self.history.len() > 5 {
            self.history.pop_front();
        }
    }

    fn next_interval(&mut self, is_charging: bool) -> Duration {
        let s = self.history.back().expect("At least 1 sample needed");

        if !s.online {
            self.last_interval = UNPLUGGED_HEARTBEAT;
            return self.last_interval;
        }

        let cap = s.capacity_pct as f32;
        let temp = s.temp_dc as f32 / 10.0;
        let dist_to_limit = (self.limit - cap).max(0.0);
        let dist_to_thermal = (self.thermal_cutoff - temp).max(0.0);
        let dist_to_resume = (cap - self.resume_limit).max(0.0);

        let danger_high = dist_to_limit < 2.0 || dist_to_thermal < 3.0 || self.ema_temp_rate > 0.15;
        let danger_low = !is_charging && dist_to_resume < 2.0;

        if danger_high || danger_low {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        let predicted = if is_charging && self.ema_cap_rate > 0.01 {
            Duration::from_secs_f32((dist_to_limit / self.ema_cap_rate * 0.5).max(0.0))
        } else if !is_charging && self.ema_cap_rate < -0.01 {
            Duration::from_secs_f32((dist_to_resume / (-self.ema_cap_rate) * 0.5).max(0.0))
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

fn create_netlink_socket() -> Option<std::os::unix::io::RawFd> {
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_KOBJECT_UEVENT) };
    if fd < 0 { return None; }
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = 0; // Let kernel assign PID
    addr.nl_groups = 1;
    let ret = unsafe { libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as u32) };
    if ret < 0 {
        unsafe { libc::close(fd); }
        return None;
    }
    Some(fd)
}

struct DecisionEngine {
    state: ChargeState,
}

impl DecisionEngine {
    fn new() -> Self {
        Self { state: ChargeState::Charging }
    }

    fn evaluate(&mut self, snapshot: &SensorSnapshot, cfg: &Config) -> Decision {
        if !cfg.enabled {
            self.state = ChargeState::Disabled;
            return Decision { command: ChargeCommand::Enable, state: ChargeState::Disabled, reason: "Daemon Disabled" };
        }

        if !snapshot.online {
            self.state = ChargeState::Offline;
            return Decision { command: ChargeCommand::Noop, state: ChargeState::Offline, reason: "Charger Unplugged" };
        }

        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit { cfg.resume_limit } else { limit.saturating_sub(2) };
        let thermal_max = cfg.max_temp_dc;
        let thermal_resume = thermal_max.saturating_sub(cfg.thermal_resume_hysteresis_dc);

        // State Machine Transitions
        match self.state {
            ChargeState::Disabled | ChargeState::Offline | ChargeState::Fault => {
                // Re-evaluate from scratch
                self.state = ChargeState::Charging; // Assume charging initially to evaluate
                self.evaluate(snapshot, cfg)
            }
            ChargeState::Charging => {
                if cfg.thermal_cutoff && snapshot.temp_dc >= thermal_max {
                    self.state = ChargeState::ThermalCutoff;
                    Decision { command: ChargeCommand::Disable, state: self.state, reason: "Thermal Max Reached" }
                } else if snapshot.capacity_pct >= limit {
                    self.state = ChargeState::LimitReached;
                    Decision { command: ChargeCommand::Disable, state: self.state, reason: "Charge Limit Reached" }
                } else {
                    Decision { command: ChargeCommand::Enable, state: self.state, reason: "Normal Charging" }
                }
            }
            ChargeState::LimitReached => {
                // Hysteresis for limit
                if snapshot.capacity_pct <= resume {
                    self.state = ChargeState::Charging;
                    self.evaluate(snapshot, cfg)
                } else {
                    // Re-assert disable just in case
                    Decision { command: ChargeCommand::Disable, state: self.state, reason: "Waiting for Limit Resume" }
                }
            }
            ChargeState::ThermalCutoff => {
                // Hysteresis for thermal
                if snapshot.temp_dc <= thermal_resume {
                    self.state = ChargeState::Charging;
                    self.evaluate(snapshot, cfg)
                } else {
                    Decision { command: ChargeCommand::Disable, state: self.state, reason: "Waiting for Thermal Resume" }
                }
            }
        }
    }
}

pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    tracing::info!("Monitor loop started (Production-Grade State Machine)");

    let mut battery_reader = CachedReader::new();
    let nl_fd = create_netlink_socket().unwrap_or(-1);
    if nl_fd >= 0 {
        tracing::info!("Successfully bound to NETLINK_KOBJECT_UEVENT");
    } else {
        tracing::warn!("Failed to bind Netlink socket, falling back to pure adaptive timer");
    }

    let initial_cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let mut scheduler = AdaptiveScheduler::new(initial_cfg.charge_limit, 95, 420);
    let mut engine = DecisionEngine::new();

    loop {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();
        
        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit { cfg.resume_limit } else { limit.saturating_sub(2) };
        scheduler.limit = limit as f32;
        scheduler.resume_limit = resume as f32;
        scheduler.thermal_cutoff = cfg.max_temp_dc as f32 / 10.0;

        // 1. Snapshot Sensors
        let capacity_pct = battery_reader.read_capacity();
        let temp_dc = battery_reader.read_temperature_dc();
        let current_ma = battery_reader.read_current_ma();
        let online = battery_reader.is_plugged_in();

        // 2. Validate Snapshot (Fault State handling)
        if capacity_pct.is_err() || temp_dc.is_err() {
            tracing::error!("Sensor failure: cap={:?} temp={:?}", capacity_pct, temp_dc);
            engine.state = ChargeState::Fault;
            let _ = control::set_charging(false); // Fail-safe
            
            // Sleep and retry later
            let mut pfds = [libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            unsafe { libc::poll(pfds.as_mut_ptr(), 1, 5000) };
            continue;
        }

        let current = current_ma.unwrap_or(0.0) as i32;
        let snapshot = SensorSnapshot {
            capacity_pct: capacity_pct.unwrap(),
            temp_dc: temp_dc.unwrap(),
            _current_ma: current,
            online: online.unwrap_or(true),
            _charging: current > 50, // Deadband 50mA
            ts: Instant::now(),
        };

        scheduler.push_sample(snapshot.clone());

        // 3. Evaluate Decision
        let prev_state = engine.state;
        let decision = engine.evaluate(&snapshot, &cfg);
        
        if prev_state != decision.state {
            tracing::info!("State transition: {:?} -> {:?} (Reason: {})", prev_state, decision.state, decision.reason);
        }

        // 4. Apply & Verify
        match decision.command {
            ChargeCommand::Enable => {
                if prev_state != decision.state {
                    if let Err(e) = control::set_charging(true) {
                        tracing::error!("Failed to enable charging: {}", e);
                    }
                }
            }
            ChargeCommand::Disable => {
                if prev_state != decision.state {
                    if let Err(e) = control::set_charging(false) {
                        tracing::error!("Failed to disable charging: {}", e);
                    }
                    // Verify
                    std::thread::sleep(Duration::from_millis(300));
                    if let Ok(c) = battery_reader.read_current_ma() {
                        if c > 50.0 {
                            tracing::warn!("Verified after Disable: Hardware still charging! (Current: {} mA)", c);
                        }
                    }
                }
            }
            ChargeCommand::Noop => {}
        }

        // 5. Adaptive Sleep
        let timeout = scheduler.next_interval(decision.state == ChargeState::Charging);
        let mut should_evaluate = false;
        let mut now = Instant::now();
        let target_wake = now + timeout;

        while now < target_wake {
            let remaining = target_wake.saturating_duration_since(now);

            let mut pfds = vec![libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            if nl_fd >= 0 {
                pfds.push(libc::pollfd { fd: nl_fd, events: libc::POLLIN, revents: 0 });
            }

            let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, remaining.as_millis() as i32) };

            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    now = Instant::now();
                    continue; // EINTR is safe
                }
                tracing::error!("poll() failed: {}", err);
                should_evaluate = true;
                break;
            } else if ret == 0 {
                // Timeout
                should_evaluate = true;
                break;
            }

            // IPC Socket
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
                        if nl_fd >= 0 { unsafe { libc::close(nl_fd); } }
                        return;
                    }
                    if buf[0] == 1 {
                        tracing::info!("Config reloaded");
                        // We must re-evaluate immediately to reconcile state
                        if engine.state == ChargeState::LimitReached || engine.state == ChargeState::ThermalCutoff {
                             engine.state = ChargeState::Charging; // Reset state machine to force re-eval
                        }
                        should_evaluate = true;
                        break;
                    }
                }
            }

            // Netlink Uevent
            if pfds.len() > 1 {
                let nl_events = pfds[1].revents;
                if nl_events & libc::POLLIN != 0 {
                    let mut buf = [0u8; 4096];
                    let mut found = false;
                    loop {
                        let n = unsafe { libc::recv(nl_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT) };
                        if n <= 0 { break; }
                        let s = String::from_utf8_lossy(&buf[..n as usize]);
                        if s.contains("SUBSYSTEM=power_supply") && s.contains("ACTION=change") {
                            found = true;
                        }
                    }
                    if found {
                        should_evaluate = true;
                        break;
                    }
                }
            }
            
            now = Instant::now();
        }

        if !should_evaluate {
            continue;
        }
    }
}
