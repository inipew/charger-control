use std::sync::Arc;
use tokio::{sync::RwLock, time};
use charger_core::{battery::{control, reader}, config::schema::Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason { None, LimitReached, ThermalCutoff }

pub async fn run_monitor_loop(config: Arc<RwLock<Config>>, mut shutdown_rx: tokio::sync::mpsc::Receiver<()>) {
    let mut stop_reason = StopReason::None;

    tracing::info!("Monitor loop started");

    // Fix Point 2: Sinkronisasi awal saat daemon pertama kali menyala.
    // Jika baterai di bawah limit, paksa nyalakan agar tidak nyangkut.
    let initial_level = reader::read_capacity().unwrap_or(0);
    let initial_limit = config.read().await.charge_limit;
    if config.read().await.enabled && initial_level < initial_limit {
        let _ = control::set_charging(true);
        tracing::info!("Boot sync: Forcing charging ON because {}% < {}%", initial_level, initial_limit);
    }

    loop {
        let cfg = config.read().await.clone();

        if !cfg.enabled {
            // Wait silently if disabled
            continue;
        }

        // Read battery values
        let level = reader::read_capacity().unwrap_or(0);
        let temp_dc = reader::read_temperature_dc().unwrap_or(0);
        let limit = cfg.charge_limit;
        let max_temp_dc = cfg.max_temp_dc;

        // --- Thermal cutoff logic ---
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
            continue;
        }

        // Resume from thermal cutoff if cooled down
        if stop_reason == StopReason::ThermalCutoff && level < limit {
            if let Err(e) = control::set_charging(true) {
                tracing::error!("resume from thermal: set_charging(true) failed: {e}");
            }
            stop_reason = StopReason::None;
            tracing::info!("✅ Temperature normal — Charging resumed at {}%", level);
        }

        // --- Charge limit logic ---
        if level >= limit {
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

        // Sleep di akhir loop agar eksekusi pertama langsung instan tanpa menunggu
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!("Monitor loop shutting down");
                break;
            }
            _ = time::sleep(time::Duration::from_secs(cfg.poll_interval_secs)) => {}
        }
    }
}
