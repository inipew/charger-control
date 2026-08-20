use std::time::Instant;

use crate::ipc::DaemonCommand;

use super::{intent::OperatingIntent, MonitorContext};

pub use charger_core::battery::uevent::UeventKind;

/// Kejadian transien (Event) yang memicu pembaruan state machine monitor.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    ChargerAttached,
    ChargerDetached,
    AcChanged,
    UsbChanged,
    TypeCChanged,
    BatteryChanged,
    BmsChanged,
    IpcCommand(DaemonCommand),
}

/// Dispatcher Reducer untuk memproses event pada `MonitorContext`.
pub fn handle_event(ctx: &mut MonitorContext, event: MonitorEvent, now: Instant) {
    match event {
        MonitorEvent::ChargerAttached => {
            ctx.hardware_track.mark_verification_needed();
            ctx.mark_evaluation_requested();
        }
        MonitorEvent::AcChanged | MonitorEvent::UsbChanged | MonitorEvent::TypeCChanged => {
            // Jika kabel belum terhubung stabil (misal saat disconnect atau settling), verifikasi hardware
            if !matches!(
                ctx.observed.connection,
                super::reality::ConnectionState::Attached
            ) {
                ctx.hardware_track.mark_verification_needed();
            }
            ctx.mark_evaluation_requested();
        }
        MonitorEvent::ChargerDetached => {
            ctx.reset_on_detach();
            ctx.hardware_track.reset_on_disconnect();

            // Pulihkan status fisik hardware ke kondisi default pabrik (1x eksekusi saat detach)
            // agar PMIC tidak tertinggal dalam status input_suspend saat berjalan dengan baterai
            let _ = charger_core::battery::control::set_charging(true);
            let _ = charger_core::battery::control::reset_fast_charge_current();

            ctx.observed.clear_sample();
            ctx.mark_evaluation_requested();
        }
        MonitorEvent::BatteryChanged | MonitorEvent::BmsChanged => {
            // Throttle 1.5s berlaku umum (baik saat connected maupun disconnected)
            // kecuali jika sedang terjadi Thermal Emergency.
            let is_emergency = ctx.policy_result.strongest_block()
                == Some(super::decision::BlockCause::ThermalEmergency);

            let should_throttle = !is_emergency
                && ctx.diag.last_battery_event_eval.is_some_and(|last| {
                    now.duration_since(last) < std::time::Duration::from_millis(1500)
                });

            if !should_throttle {
                ctx.diag.last_battery_event_eval = Some(now);
                ctx.mark_evaluation_requested();
            }
        }
        MonitorEvent::IpcCommand(cmd) => match cmd {
            DaemonCommand::BypassOn => {
                ctx.intent = OperatingIntent::bypass(now, None);
                ctx.mark_force_hardware_verification();
                ctx.mark_evaluation_requested();
            }
            DaemonCommand::BypassOff => {
                ctx.intent = OperatingIntent::normal();
                ctx.mark_force_hardware_verification();
                ctx.mark_evaluation_requested();
            }
            DaemonCommand::DisableOn => {
                ctx.intent = OperatingIntent::disabled();
                ctx.mark_force_hardware_verification();
                ctx.mark_evaluation_requested();
            }
            DaemonCommand::DisableOff => {
                ctx.intent = OperatingIntent::normal();
                ctx.mark_force_hardware_verification();
                ctx.mark_evaluation_requested();
            }
            DaemonCommand::Reload => {
                // Tidak clear policy_runtime di sini.
                // Main loop (mod.rs) sudah mengecek diff config dan clear
                // ChargeLimitState hanya jika charge_limit/resume_limit/max_temp_dc benar-benar berubah.
                // Clearing tanpa cek diff akan me-reset state Suspended meski config tidak berubah,
                // menyebabkan charging resume saat SOC masih di atas resume_limit.
                ctx.adaptive_scheduler.reset_history();
                ctx.mark_force_hardware_verification();
                ctx.mark_evaluation_requested();
            }
            _ => {}
        },
    }
}
