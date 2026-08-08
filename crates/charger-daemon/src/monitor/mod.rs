pub mod decision;
pub mod hardware;
pub mod netlink;
pub mod scheduler;
pub mod snapshot;

use charger_core::{battery::reader::CachedReader, config::schema::Config};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, RwLock};
use std::time::Instant;

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

        let limit = cfg.charge_limit;
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < limit {
            cfg.resume_limit
        } else {
            limit.saturating_sub(2)
        };

        let mut scheduler_changed = false;
        if (scheduler.limit - limit as f32).abs() > f32::EPSILON
            || (scheduler.resume_limit - resume as f32).abs() > f32::EPSILON
            || (scheduler.thermal_cutoff - cfg.max_temp_dc as f32 / 10.0).abs() > f32::EPSILON
        {
            scheduler_changed = true;
        }

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

        // 1. Observe snapshot in scheduler (ignores transitional samples implicitly by only reading pure state)
        // We only push to EMA if hardware is completely synced and we are not forcing apply.
        if hardware.sync == hardware::SyncState::Synced || hardware.sync == hardware::SyncState::Unknown {
            scheduler.observe(&snapshot);
        }

        if scheduler_changed {
            scheduler.reset_prediction();
        }

        // 2. Hardware verification step
        if hardware.verification_due() {
            hardware.verify(&snapshot);
        }

        // 3. Re-evaluate policy based on config and sensor readings
        let old_target = hardware.target;
        let decision = engine.evaluate(&snapshot, &cfg);

        if decision.target != old_target {
            tracing::info!(
                "Policy change triggered target update: {:?} -> {:?} (Reason: {}, Policy: {:?})",
                old_target,
                decision.target,
                decision.reason,
                decision.policy
            );
            hardware.invalidate_verification();
            hardware.force_apply = true;
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
        let timeout = scheduler.next_interval(&snapshot, netlink.is_connected());
        
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
                        should_evaluate = true;
                        break;
                    }
                }
                next_wake = next_wake.min(nd);
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
                    remaining.as_millis() as i32,
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
