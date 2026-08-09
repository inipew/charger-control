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
                self.ema_cap_rate = ALPHA * ((s.capacity - prev.capacity) / dt) + (1.0 - ALPHA) * self.ema_cap_rate;
                self.ema_temp_rate = ALPHA * ((s.temp - prev.temp) / dt) + (1.0 - ALPHA) * self.ema_temp_rate;
            }
        }
        self.history.push_back(s);
        if self.history.len() > 5 { self.history.pop_front(); }
    }

    fn next_interval(&mut self, stop_reason: StopReason) -> Duration {
        let s = self.history.back().expect("At least 1 sample needed");

        if s.power_state == reader::PowerState::Disconnected {
            self.last_interval = UNPLUGGED_HEARTBEAT;
            return self.last_interval;
        }

        let dist_to_limit = (self.limit - s.capacity).max(0.0);
        let dist_to_thermal = (self.thermal_cutoff - s.temp).max(0.0);

        // High-risk state: use aggressive fallback polling
        let danger = (dist_to_limit < 2.0
            || dist_to_thermal < 3.0
            || self.ema_temp_rate > 0.15)
            && stop_reason == StopReason::None;
            
        if danger {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        if stop_reason == StopReason::ThermalCutoff {
            self.last_interval = Duration::from_secs(10);
            return self.last_interval;
        } else if stop_reason != StopReason::None {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason { None, LimitReached, ThermalCutoff, Bypass }

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
    let mut stop_reason = StopReason::None;
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
    let mut is_charging_enabled = true; // Track actual hardware state

    loop {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();

        if !cfg.enabled {
            if stop_reason != StopReason::None {
                if let Err(e) = control::set_charging(true) {
                    tracing::error!("failed to restore charging state on disable: {e}");
                }
                stop_reason = StopReason::None;
                tracing::info!("Daemon disabled. Restored charging state to true.");
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

        let mut timeout = scheduler.next_interval(stop_reason);
        
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
                        stop_reason = StopReason::Bypass;
                        needs_evaluation = true;
                    }
                    if buf[0] == 4 {
                        tracing::info!("Bypass mode disabled via IPC");
                        stop_reason = StopReason::None;
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
        if stop_reason == StopReason::Bypass {
            last_eval_time = Instant::now();
            continue;
        }

        // Gate 2: Unified Policy Engine (Desired vs Actual)
        let mut new_stop_reason = stop_reason;
        
        if power_state == reader::PowerState::Disconnected {
            // Unplug resets the policy state
            new_stop_reason = StopReason::None;
        } else if power_state.is_plugged_in() {
            let thermal_resume_dc = max_temp_dc.saturating_sub(20);
            
            if cfg.thermal_cutoff && temp_dc >= max_temp_dc {
                new_stop_reason = StopReason::ThermalCutoff;
            } else if stop_reason == StopReason::ThermalCutoff && temp_dc <= thermal_resume_dc && level < limit {
                new_stop_reason = StopReason::None;
            } else if level >= limit {
                new_stop_reason = StopReason::LimitReached;
            } else if level <= effective_resume && stop_reason == StopReason::LimitReached {
                new_stop_reason = StopReason::None;
            }
        }
        
        let desired_charging = new_stop_reason == StopReason::None;
        
        if desired_charging != is_charging_enabled {
            if let Ok(()) = control::set_charging(desired_charging) {
                is_charging_enabled = desired_charging;
                stop_reason = new_stop_reason;
                
                if power_state == reader::PowerState::Disconnected {
                    tracing::info!("🔌 Charger disconnected. Restored charging state and reset logic.");
                } else if desired_charging {
                    tracing::info!("✅ Charging resumed (Reason cleared: {:?})", stop_reason);
                } else {
                    tracing::warn!("⚠ Charging stopped (Reason: {:?})", stop_reason);
                }
            } else {
                tracing::error!("Failed to apply charging state");
            }
        } else if new_stop_reason != stop_reason {
            // Semantic update only, no hardware IO needed
            stop_reason = new_stop_reason;
        }
        
        last_eval_time = Instant::now();
    }
}
