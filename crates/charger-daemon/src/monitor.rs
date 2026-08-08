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
    online: bool,
    ts: Instant,
}

struct AdaptiveScheduler {
    limit: f32,
    resume_limit: f32,
    thermal_cutoff: f32,
    history: VecDeque<Sample>,
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

    fn push_sample(&mut self, s: Sample) {
        if let Some(prev) = self.history.back() {
            let dt = (s.ts - prev.ts).as_secs_f32().max(0.5);
            const ALPHA: f32 = 0.3; // Smoothing factor
            self.ema_cap_rate = ALPHA * ((s.capacity - prev.capacity) / dt) + (1.0 - ALPHA) * self.ema_cap_rate;
            self.ema_temp_rate = ALPHA * ((s.temp - prev.temp) / dt) + (1.0 - ALPHA) * self.ema_temp_rate;
        }
        self.history.push_back(s);
        if self.history.len() > 5 { self.history.pop_front(); }
    }

    fn next_interval(&mut self) -> Duration {
        let s = self.history.back().expect("At least 1 sample needed");

        if !s.online {
            self.last_interval = UNPLUGGED_HEARTBEAT;
            return self.last_interval;
        }

        let dist_to_limit = (self.limit - s.capacity).max(0.0);
        let dist_to_thermal = (self.thermal_cutoff - s.temp).max(0.0);

        // Danger -> immediate reaction
        let danger = dist_to_limit < 2.0
            || dist_to_thermal < 3.0
            || self.ema_temp_rate > 0.15;
            
        if danger {
            self.last_interval = MIN_INTERVAL;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason { None, LimitReached, ThermalCutoff }

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

pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    let mut stop_reason = StopReason::None;
    tracing::info!("Monitor loop started (Adaptive Netlink Event-Driven)");

    let (initial_level, initial_limit, enabled) = {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner());
        (reader::read_capacity().unwrap_or(0), cfg.charge_limit, cfg.enabled)
    };

    if enabled && initial_level < initial_limit {
        let _ = control::set_charging(true);
        tracing::info!("Boot sync: Forcing charging ON because {}% < {}%", initial_level, initial_limit);
    }

    let nl_fd = create_netlink_socket().unwrap_or(-1);
    if nl_fd >= 0 {
        tracing::info!("Successfully bound to NETLINK_KOBJECT_UEVENT");
    } else {
        tracing::warn!("Failed to bind Netlink socket, falling back to pure adaptive timer");
    }

    let mut scheduler = AdaptiveScheduler::new(initial_limit, 95, 420);

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

        let level = reader::read_capacity().unwrap_or(0);
        let temp_dc = reader::read_temperature_dc().unwrap_or(0);
        let current = reader::read_current_ma().unwrap_or(0.0);
        let online = reader::is_plugged_in().unwrap_or(true);
        
        let limit = cfg.charge_limit;
        let max_temp_dc = cfg.max_temp_dc;
        let effective_resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };

        scheduler.limit = limit as f32;
        scheduler.resume_limit = effective_resume as f32;
        scheduler.thermal_cutoff = max_temp_dc as f32 / 10.0;
        
        scheduler.push_sample(Sample {
            capacity: level as f32,
            temp: temp_dc as f32 / 10.0,
            _current_ma: current,
            online,
            ts: Instant::now(),
        });

        if cfg.thermal_cutoff && temp_dc >= max_temp_dc {
            if stop_reason != StopReason::ThermalCutoff {
                if let Err(e) = control::set_charging(false) {
                    tracing::error!("thermal cutoff: set_charging(false) failed: {e}");
                }
                stop_reason = StopReason::ThermalCutoff;
                tracing::warn!("⚠ Charging stopped — Temp {:.1}°C (limit {:.1}°C)", temp_dc as f32 / 10.0, max_temp_dc as f32 / 10.0);
            }
        } else if stop_reason == StopReason::ThermalCutoff && level < limit {
            if let Err(e) = control::set_charging(true) {
                tracing::error!("resume from thermal: set_charging(true) failed: {e}");
            }
            stop_reason = StopReason::None;
            tracing::info!("✅ Temperature normal — Charging resumed at {}%", level);
        } else if level >= limit {
            if stop_reason != StopReason::LimitReached {
                if let Err(e) = control::set_charging(false) {
                    tracing::error!("limit reached: set_charging(false) failed: {e}");
                }
                stop_reason = StopReason::LimitReached;
                tracing::info!("🔋 Limit reached — Charging stopped at {}%", limit);
            }
        } else if level <= effective_resume && stop_reason == StopReason::LimitReached {
            if let Err(e) = control::set_charging(true) {
                tracing::error!("resume from limit: set_charging(true) failed: {e}");
            }
            stop_reason = StopReason::None;
            tracing::info!("⚡ Charging resumed — Level: {}% | Threshold: {}%", level, effective_resume);
        }

        let timeout = scheduler.next_interval();
        
        let mut pfds = vec![
            libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        if nl_fd >= 0 {
            pfds.push(libc::pollfd { fd: nl_fd, events: libc::POLLIN, revents: 0 });
        }

        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout.as_millis() as i32) };
        
        if ret > 0 {
            // IPC Command received
            if pfds[0].revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 1];
                if rx.recv(&mut buf).is_ok() {
                    if buf[0] == 2 {
                        tracing::info!("Monitor loop shutting down via IPC");
                        break; 
                    }
                    if buf[0] == 1 {
                        tracing::info!("Config reloaded, re-evaluating instantly");
                        continue;
                    }
                }
            }
            
            // Netlink uevent received
            if pfds.len() > 1 && (pfds[1].revents & libc::POLLIN) != 0 {
                // Drain socket buffer to clear POLLIN
                let mut buf = [0u8; 4096];
                unsafe { libc::recv(nl_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT) };
                // We just continue loop to sample and re-evaluate immediately
                tracing::debug!("Woken up by Netlink event");
            }
        }
    }
}
