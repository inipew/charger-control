use std::sync::{Arc, RwLock};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use charger_core::{battery::{control, reader}, config::schema::Config};
use crate::ipc::DaemonMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason { None, LimitReached, ThermalCutoff }

pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: Receiver<DaemonMessage>) {
    let mut stop_reason = StopReason::None;

    tracing::info!("Monitor loop started (Sync Native)");

    let (initial_level, initial_limit, enabled) = {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner());
        (reader::read_capacity().unwrap_or(0), cfg.charge_limit, cfg.enabled)
    };

    if enabled && initial_level < initial_limit {
        let _ = control::set_charging(true);
        tracing::info!("Boot sync: Forcing charging ON because {}% < {}%", initial_level, initial_limit);
    }

    loop {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();

        if !cfg.enabled {
            // BUG FIX 2: Restore normal charging state if daemon is disabled
            if stop_reason != StopReason::None {
                if let Err(e) = control::set_charging(true) {
                    tracing::error!("failed to restore charging state on disable: {e}");
                }
                stop_reason = StopReason::None;
                tracing::info!("Daemon disabled. Restored charging state to true.");
            }

            match rx.recv_timeout(Duration::from_secs(cfg.poll_interval_secs)) {
                Ok(DaemonMessage::Shutdown) => {
                    tracing::info!("Monitor loop shutting down");
                    break;
                }
                Ok(DaemonMessage::Reload) => {
                    continue; // Re-evaluate loop with new config
                }
                Err(RecvTimeoutError::Timeout) => {
                    continue; // Stay asleep
                }
                Err(RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }

        let level = reader::read_capacity().unwrap_or(0);
        let temp_dc = reader::read_temperature_dc().unwrap_or(0);
        let limit = cfg.charge_limit;
        let max_temp_dc = cfg.max_temp_dc;

        if cfg.thermal_cutoff && temp_dc >= max_temp_dc {
            if stop_reason != StopReason::ThermalCutoff {
                if let Err(e) = control::set_charging(false) {
                    tracing::error!("thermal cutoff: set_charging(false) failed: {e}");
                }
                stop_reason = StopReason::ThermalCutoff;
                tracing::warn!(
                    "⚠ Charging stopped — Temp {:.1}°C (limit {:.1}°C)",
                    temp_dc as f32 / 10.0,
                    max_temp_dc as f32 / 10.0
                );
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
        } else if level <= limit.saturating_sub(2) && stop_reason == StopReason::LimitReached {
            if let Err(e) = control::set_charging(true) {
                tracing::error!("resume from limit: set_charging(true) failed: {e}");
            }
            stop_reason = StopReason::None;
            tracing::info!("⚡ Charging resumed — Level: {}% | Limit: {}%", level, limit);
        }

        // Wait for shutdown/reload or sleep (timeout)
        match rx.recv_timeout(Duration::from_secs(cfg.poll_interval_secs)) {
            Ok(DaemonMessage::Shutdown) => {
                tracing::info!("Monitor loop shutting down");
                break;
            }
            Ok(DaemonMessage::Reload) => {
                tracing::info!("Config reloaded, re-evaluating instantly");
                // Immediately loops again
            }
            Err(RecvTimeoutError::Timeout) => {
                // Timeout reached naturally, loop continues to check battery
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!("IPC channel disconnected, shutting down monitor");
                break;
            }
        }
    }
}
