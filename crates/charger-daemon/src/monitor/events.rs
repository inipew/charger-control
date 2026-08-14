use std::time::Instant;

use crate::ipc::DaemonCommand;

use super::{intent::OperatingIntent, MonitorContext};

/// Klasifikasi jenis uevent kernel yang relevan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UeventKind {
    Ac,
    Usb,
    TypeC,
    Battery,
    Bms,
    Other,
}

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
        MonitorEvent::ChargerAttached
        | MonitorEvent::AcChanged
        | MonitorEvent::UsbChanged
        | MonitorEvent::TypeCChanged => {
            ctx.hardware_track.mark_verification_needed();
            ctx.mark_evaluation_requested();
        }
        MonitorEvent::ChargerDetached => {
            ctx.reset_on_detach();
            ctx.hardware_track.reset_on_disconnect();
            ctx.observed.clear_sample();
            ctx.mark_evaluation_requested();
        }
        MonitorEvent::BatteryChanged | MonitorEvent::BmsChanged => {
            ctx.mark_evaluation_requested();
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
