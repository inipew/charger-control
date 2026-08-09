use std::sync::{Arc, RwLock};
use std::os::unix::net::UnixDatagram;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use charger_core::{battery::{control, reader}, config::schema::Config};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600); // 10 minutes

#[derive(Clone, Debug)]
struct Sample {
    capacity: f32,
    temp: f32,
    _current_ma: f32,
    power_state: reader::PowerState,
    ts: Instant,
}

struct AdaptiveScheduler {
    limit: f32,
    thermal_cutoff: f32,
    history: VecDeque<Sample>,
    ema_cap_rate: f32,
    ema_temp_rate: f32,
    last_interval: Duration,
}

impl AdaptiveScheduler {
    fn new(limit: u8, thermal_cutoff: i32) -> Self {
        Self {
            limit: limit as f32,
            thermal_cutoff: thermal_cutoff as f32 / 10.0,
            history: VecDeque::new(),
            ema_cap_rate: 0.0,
            ema_temp_rate: 0.0,
            last_interval: MIN_INTERVAL,
        }
    }

    fn push_sample(&mut self, s: Sample) {
        if let Some(prev) = self.history.back() {
            let dt = (s.ts - prev.ts).as_secs_f32();
            let capacity_rate = (s.capacity - prev.capacity).abs() / dt.max(0.1);
            
            if dt < 0.5 || capacity_rate > 1.0 {
                // Event evaluation or transient anomaly: update state but skip EMA measurement
                if let Some(last) = self.history.back_mut() {
                    *last = s;
                }
                return;
            }
            // Deep sleep recovery (Doze mode)
            if dt > 300.0 {
                self.ema_cap_rate = 0.0;
                self.ema_temp_rate = 0.0;
                self.history.clear();
            } else {
                const ALPHA: f32 = 0.3; // Smoothing factor
                let new_cap_rate = ALPHA * ((s.capacity - prev.capacity) / dt) + (1.0 - ALPHA) * self.ema_cap_rate;
                let new_temp_rate = ALPHA * ((s.temp - prev.temp) / dt) + (1.0 - ALPHA) * self.ema_temp_rate;
                if new_cap_rate.is_finite() {
                    self.ema_cap_rate = new_cap_rate;
                }
                if new_temp_rate.is_finite() {
                    self.ema_temp_rate = new_temp_rate;
                }
            }
        }
        self.history.push_back(s);
        if self.history.len() > 5 { self.history.pop_front(); }
    }

    fn next_interval(&mut self, limit_blocked: bool, thermal_blocked: bool, operating_mode: OperatingMode) -> Duration {
        let s = match self.history.back() {
            Some(sample) => sample,
            None => return Duration::ZERO,
        };

        if s.power_state == reader::PowerState::Disconnected {
            self.last_interval = UNPLUGGED_HEARTBEAT;
            return self.last_interval;
        } else if s.power_state == reader::PowerState::Attached {
            self.last_interval = Duration::from_secs(2);
            return self.last_interval;
        }

        let dist_to_limit = (self.limit - s.capacity).max(0.0);
        let dist_to_thermal = (self.thermal_cutoff - s.temp).max(0.0);

        // High-risk state: use aggressive fallback polling
        let danger = (dist_to_limit < 2.0
            || dist_to_thermal < 3.0
            || self.ema_temp_rate > 0.15)
            && !limit_blocked && !thermal_blocked && operating_mode == OperatingMode::Normal;
            
        if danger {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        if thermal_blocked {
            self.last_interval = Duration::from_secs(10);
            return self.last_interval;
        } else if limit_blocked || operating_mode == OperatingMode::Bypass {
            self.last_interval = Duration::from_secs(60);
            return self.last_interval;
        }

        // Predictive scheduling
        let predicted = if self.ema_cap_rate > 0.01 {
            Duration::from_secs_f32((dist_to_limit / self.ema_cap_rate * 0.5).max(0.0))
        } else {
            MAX_INTERVAL
        };
        let target = predicted.clamp(MIN_INTERVAL, MAX_INTERVAL);

        // Asymmetric adjustment: snap down instantly, ramp up slowly
        self.last_interval = if target < self.last_interval {
            target
        } else {
            self.last_interval.mul_f32(1.5).min(MAX_INTERVAL).min(target.max(self.last_interval))
        };
        self.last_interval
    }
}

struct NetlinkFd(std::os::unix::io::RawFd);
impl Drop for NetlinkFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0); }
        }
    }
}

const THERMAL_HYSTERESIS_DC: i32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatingMode { Normal, Bypass }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppliedChargingState { Unknown, Enabled, Disabled }

fn create_netlink_socket() -> Option<std::os::unix::io::RawFd> {
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_KOBJECT_UEVENT) };
    if fd < 0 {
        return None;
    }

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = std::process::id() as u32;
    addr.nl_groups = 1; // Listen to kernel broadcast groups (uevent)

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if ret < 0 {
        return None;
    }
    Some(fd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetlinkEvent {
    None,
    FastPath, // ac online, battery status, usb attached
    Coalesce, // other relevant events
}

/// Zero-allocation netlink parser. 
/// Drains socket and returns the most urgent event found.
fn drain_and_parse_netlink(fd: std::os::unix::io::RawFd) -> NetlinkEvent {
    let mut buf = [0u8; 8192];
    let mut urgent_event = NetlinkEvent::None;
    
    loop {
        let res = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT) };
        if res <= 0 { break; }
        
        let data = &buf[..res as usize];
        let mut is_power_supply = false;
        let mut name = b"".as_slice();
        
        for part in data.split(|&b| b == 0) {
            if part == b"SUBSYSTEM=power_supply" {
                is_power_supply = true;
            } else if part.starts_with(b"POWER_SUPPLY_NAME=") {
                name = &part[18..];
            }
        }
        
        if is_power_supply {
            let mut fast_path = false;
            for part in data.split(|&b| b == 0) {
                if name == b"ac" && part.starts_with(b"POWER_SUPPLY_ONLINE=") {
                    fast_path = true;
                } else if name == b"battery" && part.starts_with(b"POWER_SUPPLY_STATUS=") {
                    fast_path = true;
                } else if name == b"usb" && part.starts_with(b"POWER_SUPPLY_TYPEC_MODE=") {
                    fast_path = true; // Any TYPEC_MODE change is an early attach/detach hint
                }
            }
            
            if fast_path {
                urgent_event = NetlinkEvent::FastPath;
            } else if urgent_event == NetlinkEvent::None && matches!(name, b"usb" | b"battery" | b"main" | b"ac" | b"wireless" | b"bms" | b"mtk-charger" | b"mt_charger") {
                urgent_event = NetlinkEvent::Coalesce;
            }
        }
    }
    
    urgent_event
}

pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    tracing::info!("Monitor loop started (Bulletproof Event-Driven Architecture)");

    let (_initial_level, initial_limit, _enabled) = {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner());
        (reader::read_capacity().unwrap_or(0), cfg.charge_limit, cfg.enabled)
    };

    let nl_fd_raw = create_netlink_socket().unwrap_or(-1);
    let _nl_fd_guard = NetlinkFd(nl_fd_raw);
    let nl_fd = nl_fd_raw;
    if nl_fd >= 0 {
        tracing::info!("Successfully bound to NETLINK_KOBJECT_UEVENT");
    } else {
        tracing::warn!("Failed to bind Netlink socket, falling back to pure adaptive timer");
    }

    let mut scheduler = AdaptiveScheduler::new(initial_limit, 420);
    let mut last_eval_time = Instant::now() - Duration::from_secs(60); 

    let mut force_next_eval = true;
    let mut pending_netlink_eval = false;
    let mut applied_state = AppliedChargingState::Unknown;
    let mut operating_mode = OperatingMode::Normal;
    let mut thermal_blocked = false;
    let mut limit_blocked = false;

    loop {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();

        if !cfg.enabled {
            if applied_state != AppliedChargingState::Enabled {
                match control::set_charging(true) {
                    Ok(()) => {
                        applied_state = AppliedChargingState::Enabled;
                        tracing::info!("Daemon disabled. Restored hardware charging state to ON.");
                    }
                    Err(_) => {
                        applied_state = AppliedChargingState::Unknown;
                        tracing::error!("Daemon disabled: failed to fully restore hardware charging state");
                    }
                }
            }
            
            // Wait for events indefinitely when disabled
            let mut pfds = [
                libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            
            unsafe { libc::poll(pfds.as_mut_ptr(), 1, -1) };
            
            let mut buf = [0u8; 1];
            if rx.recv(&mut buf).is_ok() {
                if buf[0] == 2 { break; }
                if buf[0] == 1 { continue; }
            }
            continue;
        }

        let mut timeout = scheduler.next_interval(limit_blocked, thermal_blocked, operating_mode);
        
        // Deferred evaluation handler
        if force_next_eval {
            timeout = Duration::ZERO;
        } else if pending_netlink_eval {
            let elapsed = last_eval_time.elapsed();
            if elapsed >= Duration::from_millis(100) {
                timeout = Duration::ZERO;
            } else {
                let remain = Duration::from_millis(100) - elapsed;
                if remain < timeout {
                    timeout = remain;
                }
            }
        }
        
        let mut pfds = [
            libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: nl_fd, events: if nl_fd >= 0 { libc::POLLIN } else { 0 }, revents: 0 },
        ];
        let nfds = if nl_fd >= 0 { 2 } else { 1 };

        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, timeout.as_millis() as i32) };
        
        if ret < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.kind() != std::io::ErrorKind::Interrupted {
                tracing::error!("poll() failed: {}", errno);
                std::thread::sleep(Duration::from_secs(1));
            }
            continue;
        }
        
        let mut needs_evaluation = false;

        if ret == 0 {
            // Timeout expired
            needs_evaluation = true;
        } else if ret > 0 {
            // IPC Command received (Highest Priority)
            if pfds[0].revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 1];
                if rx.recv(&mut buf).is_ok() {
                    if buf[0] == 2 {
                        tracing::info!("Monitor loop shutting down via IPC");
                        break; 
                    }
                    if buf[0] == 1 {
                        tracing::info!("Config reloaded");
                        needs_evaluation = true;
                    }
                    if buf[0] == 3 {
                        tracing::info!("Bypass mode enabled via IPC");
                        operating_mode = OperatingMode::Bypass;
                        needs_evaluation = true;
                    }
                    if buf[0] == 4 {
                        tracing::info!("Bypass mode disabled via IPC");
                        operating_mode = OperatingMode::Normal;
                        needs_evaluation = true;
                    }
                }
            }
            
            // Netlink uevent received
            if pfds.len() > 1 && (pfds[1].revents & libc::POLLIN) != 0 {
                let event = drain_and_parse_netlink(nl_fd);
                if event == NetlinkEvent::FastPath {
                    needs_evaluation = true;
                    pending_netlink_eval = false;
                    tracing::debug!("Netlink Fast-Path triggered instant evaluation");
                } else if event == NetlinkEvent::Coalesce {
                    pending_netlink_eval = true;
                    let elapsed = last_eval_time.elapsed();
                    if elapsed >= Duration::from_millis(100) {
                        needs_evaluation = true;
                        tracing::debug!("Valid netlink event triggered evaluation (coalesce)");
                    } else {
                        tracing::debug!("Valid netlink event deferred (coalesce)");
                    }
                }
            }
        }

        // Deferred evaluation triggers timeout early
        if pending_netlink_eval && last_eval_time.elapsed() >= Duration::from_millis(100) {
            needs_evaluation = true;
        }

        // --- DECOUPLED LOOP LOGIC ---
        // Do not perform heavy sysfs IO unless evaluation is required
        if !needs_evaluation {
            continue;
        }

        force_next_eval = false;
        pending_netlink_eval = false;

        // --- SENSOR EVALUATION GATE ---
        let level = match reader::read_capacity() {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to read battery capacity: {}", e);
                last_eval_time = Instant::now();
                continue;
            }
        };
        let temp_dc = match reader::read_temperature_dc() {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to read battery temperature: {}", e);
                last_eval_time = Instant::now();
                continue;
            }
        };
        let current = reader::read_input_current_ua().unwrap_or(0) as f32 / 1000.0;
        let power_state = reader::get_power_state().unwrap_or(reader::PowerState::Disconnected);
        
        let limit = cfg.charge_limit;
        let max_temp_dc = cfg.max_temp_dc;
        let effective_resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };

        scheduler.limit = limit as f32;
        scheduler.thermal_cutoff = max_temp_dc as f32 / 10.0;
        
        scheduler.push_sample(Sample {
            capacity: level as f32,
            temp: temp_dc as f32 / 10.0,
            _current_ma: current,
            power_state,
            ts: Instant::now(),
        });

        // Gate 1: Protect Bypass Mode
        if operating_mode == OperatingMode::Bypass {
            if applied_state != AppliedChargingState::Disabled {
                match control::enter_bypass_mode() {
                    Ok(()) => {
                        applied_state = AppliedChargingState::Disabled;
                        tracing::info!("Hardware is now in BYPASS mode");
                    }
                    Err(_) => {
                        applied_state = AppliedChargingState::Unknown;
                        tracing::error!("Failed to fully apply BYPASS mode");
                    }
                }
            }
            last_eval_time = Instant::now();
            continue;
        }

        // Gate 2: Unified Policy Engine (Desired vs Actual)
        let mut new_thermal_blocked = thermal_blocked;
        let mut new_limit_blocked = limit_blocked;
        
        if power_state == reader::PowerState::Disconnected {
            // Unplug resets the policy state
            new_thermal_blocked = false;
            new_limit_blocked = false;
        } else if power_state.is_plugged_in() {
            let thermal_resume_dc = max_temp_dc.saturating_sub(THERMAL_HYSTERESIS_DC);
            
            if cfg.thermal_cutoff && temp_dc >= max_temp_dc {
                new_thermal_blocked = true;
            } else if thermal_blocked && temp_dc <= thermal_resume_dc {
                new_thermal_blocked = false;
            }
            
            if level >= limit {
                new_limit_blocked = true;
            } else if limit_blocked && level <= effective_resume {
                new_limit_blocked = false;
            }
        }
        
        let desired_charging = !new_thermal_blocked && !new_limit_blocked;
        let desired_applied = if desired_charging { AppliedChargingState::Enabled } else { AppliedChargingState::Disabled };
        
        if applied_state != desired_applied {
            match control::set_charging(desired_charging) {
                Ok(()) => {
                    applied_state = desired_applied;
                    thermal_blocked = new_thermal_blocked;
                    limit_blocked = new_limit_blocked;
                    
                    if power_state == reader::PowerState::Disconnected {
                        tracing::info!("🔌 Charger disconnected. Restored charging state and reset logic.");
                    } else if desired_charging {
                        tracing::info!("✅ Charging resumed (Thermal Blocked: {}, Limit Blocked: {})", thermal_blocked, limit_blocked);
                    } else {
                        tracing::warn!("⚠ Charging stopped (Thermal Blocked: {}, Limit Blocked: {})", thermal_blocked, limit_blocked);
                    }
                }
                Err(_) => {
                    applied_state = AppliedChargingState::Unknown;
                    tracing::error!("Failed to fully apply charging state");
                }
            }
        } else if new_thermal_blocked != thermal_blocked || new_limit_blocked != limit_blocked {
            // Semantic update only, no hardware IO needed
            thermal_blocked = new_thermal_blocked;
            limit_blocked = new_limit_blocked;
        }
        
        last_eval_time = Instant::now();
    }
}
