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
    ConfigReload,
    TimerExpired,
}

/// Dispatcher Reducer untuk memproses event pada `MonitorContext`.
pub fn handle_event(ctx: &mut MonitorContext, event: MonitorEvent, now: Instant) {
    match event {
        MonitorEvent::ChargerAttached
        | MonitorEvent::AcChanged
        | MonitorEvent::UsbChanged
        | MonitorEvent::TypeCChanged => {
            ctx.hardware_track.mark_verification_needed();
            ctx.mark_force_evaluation();
        }
        MonitorEvent::ChargerDetached => {
            ctx.reset_charger_state();
            ctx.hardware_track.reset_on_disconnect();
            ctx.observed.clear_sample();
            ctx.mark_force_evaluation();
        }
        MonitorEvent::BatteryChanged | MonitorEvent::BmsChanged => {
            if ctx.observed.connection.is_connected() {
                ctx.mark_force_evaluation();
            }
        }
        MonitorEvent::IpcCommand(cmd) => match cmd {
            DaemonCommand::BypassOn => {
                ctx.intent = OperatingIntent::bypass(now, None);
                ctx.mark_force_hardware_verification();
                ctx.mark_force_evaluation();
            }
            DaemonCommand::BypassOff => {
                ctx.intent = OperatingIntent::normal();
                ctx.mark_force_hardware_verification();
                ctx.mark_force_evaluation();
            }
            DaemonCommand::DisableOn => {
                ctx.intent = OperatingIntent::disabled();
                ctx.mark_force_hardware_verification();
                ctx.mark_force_evaluation();
            }
            DaemonCommand::DisableOff => {
                ctx.intent = OperatingIntent::normal();
                ctx.mark_force_hardware_verification();
                ctx.mark_force_evaluation();
            }
            DaemonCommand::Reload => {
                ctx.adaptive_scheduler.reset_history();
                ctx.mark_force_hardware_verification();
                ctx.mark_force_evaluation();
            }
            _ => {}
        },
        MonitorEvent::ConfigReload => {
            ctx.adaptive_scheduler.reset_history();
            ctx.mark_force_hardware_verification();
            ctx.mark_force_evaluation();
        }
        MonitorEvent::TimerExpired => {
            ctx.mark_force_evaluation();
        }
    }
}
