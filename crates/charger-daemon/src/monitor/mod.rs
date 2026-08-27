pub mod decision;
pub mod events;
pub mod hardware;
pub mod intent;
pub mod policy;
pub mod reality;
pub mod scheduler;
pub mod tests;

#[cfg(unix)]
use mio::unix::SourceFd;
#[cfg(unix)]
use mio::{Events, Interest, Poll, Token};
#[cfg(unix)]
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::net::UnixDatagram,
};

#[cfg(not(unix))]
pub struct OwnedFd;

#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

#[cfg(unix)]
use crate::ipc::DaemonCommand;
use crate::ipc::DaemonDiagnostics;
#[cfg(unix)]
use charger_core::battery::reader;
use charger_core::{battery::control, config::schema::Config};

#[cfg(unix)]
use decision::BlockCause;
use decision::ChargingDecision;
#[cfg(unix)]
use events::{handle_event, MonitorEvent, UeventKind};
use hardware::{HardwareFault, HardwareTrack};
use intent::OperatingIntent;
use policy::{PolicyResult, PolicyRuntime};
use reality::ObservedState;
#[cfg(unix)]
use reality::Sample;
use scheduler::{AdaptiveScheduler, SchedulingState, Urgency};

/// Penjejak perubahan status diagnostik untuk rate-limiting log & error fingerprinting.
#[derive(Debug, Default)]
pub struct DiagnosticsState {
    pub last_decision: Option<ChargingDecision>,
    pub last_computed_decision: Option<ChargingDecision>,
    pub last_hw_mode: Option<control::ActualHardwareMode>,
    pub last_heartbeat_log: Option<Instant>,
    pub last_error: Option<HardwareFault>,
    pub last_error_log: Option<Instant>,
    pub last_battery_event_eval: Option<Instant>,
    pub last_fsm_str: Option<String>,
    pub last_decision_str: Option<String>,
    pub last_current_reg_str: Option<String>,
    pub last_power_state_str: Option<String>,
    pub last_conv_str: Option<String>,
}

impl DiagnosticsState {
    pub fn should_log_error(&mut self, error: HardwareFault, now: Instant) -> bool {
        if self.last_error != Some(error) {
            self.last_error = Some(error);
            self.last_error_log = Some(now);
            return true;
        }

        if self
            .last_error_log
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(300))
        {
            self.last_error_log = Some(now);
            return true;
        }

        false
    }
}

/// Dynamic batching Uevent pendorong observabilitas & coalescing.
#[derive(Debug, Clone, Copy, Default)]
pub struct UeventBatch {
    pub ac: bool,
    pub usb: bool,
    pub typec: bool,
    pub battery: bool,
    pub bms: bool,
    pub overflow: bool,
    pub netlink_broken: bool,
}

impl UeventBatch {
    #[allow(dead_code)]
    pub fn has_any(&self) -> bool {
        self.ac
            || self.usb
            || self.typec
            || self.battery
            || self.bms
            || self.overflow
            || self.netlink_broken
    }
}

/// Context Terpadu (MonitorContext) yang menggabungkan 5 Layer State Machine.
pub struct MonitorContext {
    pub config: Config,
    pub observed: ObservedState,
    pub intent: OperatingIntent,
    pub policy_result: PolicyResult,
    pub hardware_track: HardwareTrack,
    pub sched: SchedulingState,
    pub adaptive_scheduler: AdaptiveScheduler,
    pub diag: DiagnosticsState,
    pub has_distinct_bypass: bool,
    pub policy_runtime: PolicyRuntime,
}

impl MonitorContext {
    pub fn new(config: &Config) -> Self {
        let has_distinct_bypass = control::has_distinct_bypass_node();
        let intent = if config.enabled {
            OperatingIntent::normal()
        } else {
            OperatingIntent::disabled()
        };
        Self {
            config: config.clone(),
            observed: ObservedState::new(),
            intent,
            policy_result: PolicyResult::clear(),
            hardware_track: HardwareTrack::new(),
            sched: SchedulingState::new(),
            adaptive_scheduler: AdaptiveScheduler::new(
                Duration::from_secs(config.poll_interval_secs),
                config.charge_limit as f32,
                config.resume_limit as f32,
                config.max_temp_dc as f32 / 10.0,
            ),
            diag: DiagnosticsState::default(),
            has_distinct_bypass,
            policy_runtime: PolicyRuntime::default(),
        }
    }

    #[allow(dead_code)]
    pub fn mark_hardware_changed(&mut self) {
        self.hardware_track.mark_verification_needed();
    }

    pub fn mark_evaluation_requested(&mut self) {
        self.sched.mark_evaluation_requested();
    }

    pub fn mark_force_hardware_verification(&mut self) {
        self.sched.mark_force_hardware_verification();
    }

    pub fn clear_evaluation_request(&mut self) {
        self.sched.clear_evaluation_request();
    }

    /// Reset penuh: digunakan saat config berubah atau explicit Reload.
    /// Menghapus policy_runtime (ChargeLimitState kembali ke Normal).
    #[allow(dead_code)]
    pub fn reset_charger_state(&mut self) {
        self.policy_result = PolicyResult::clear();
        self.policy_runtime.clear();
        self.sched.mark_snapshot_success();
        self.adaptive_scheduler.reset_history();
    }

    /// Reset partial saat charger dicabut (detach).
    ///
    /// **Tidak** menghapus `policy_runtime` agar `ChargeLimitState::Suspended`
    /// tetap bertahan melewati siklus detach/attach.
    ///
    /// Alasan: glitch koneksi fisik (bounce) atau race condition pembacaan
    /// `get_power_state()` bisa sesaat menyebabkan transisi `Attached → Disconnected`
    /// meskipun charger masih terpasang. Tanpa ini, state Suspended hilang dan
    /// SOC 99% yang seharusnya tetap diblokir menjadi Allow.
    pub fn reset_on_detach(&mut self) {
        self.policy_result = PolicyResult::clear();
        // policy_runtime sengaja TIDAK di-clear
        self.sched.mark_snapshot_success();
        self.adaptive_scheduler.reset_history();
    }
}

fn has_pending_recovery_deadline(ctx: &MonitorContext, now: Instant) -> bool {
    [
        ctx.hardware_track.next_deadline(),
        ctx.observed.next_sensor_retry(),
    ]
    .iter()
    .copied()
    .flatten()
    .any(|deadline| deadline > now)
}

fn can_sleep_forever(ctx: &MonitorContext) -> bool {
    matches!(
        ctx.observed.connection,
        reality::ConnectionState::Disconnected
    ) && !ctx.sched.evaluation_requested
        && ctx.intent.next_deadline().is_none()
        // Saat disconnected, retry hardware actuator di-pause sampai charger terpasang kembali
        && ctx.observed.next_sensor_retry().is_none()
}

/// Loop Pemantauan Daemon Utama (`run_monitor_loop`).
#[cfg(unix)]
pub fn run_monitor_loop(
    shared_config: Arc<RwLock<Config>>,
    rx: UnixDatagram,
    diagnostics: Arc<DaemonDiagnostics>,
) {
    let initial_config = match shared_config.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };

    let mut ctx = MonitorContext::new(&initial_config);

    if let Err(error) = control::grant_node_permissions() {
        tracing::warn!(error = %error, "Failed setting sysfs node permissions");
    }

    let mut poll = Poll::new().expect("Failed to create mio::Poll");
    let mut events = Events::with_capacity(64);
    const IPC: Token = Token(0);
    const NETLINK: Token = Token(1);

    let mut mio_rx = mio::net::UnixDatagram::from_std(rx);
    poll.registry()
        .register(&mut mio_rx, IPC, Interest::READABLE)
        .expect("Failed to register IPC datagram to mio::Poll");

    let mut netlink_fd: Option<OwnedFd> = setup_netlink_socket();
    if let Some(ref fd) = netlink_fd {
        let raw_fd = fd.as_raw_fd();
        let mut source = SourceFd(&raw_fd);
        if let Err(e) = poll
            .registry()
            .register(&mut source, NETLINK, Interest::READABLE)
        {
            tracing::warn!(error = %e, "Failed to register Netlink socket");
            netlink_fd = None;
        }
    }
    diagnostics
        .netlink_available
        .store(netlink_fd.is_some(), Ordering::Relaxed);

    let mut msg_buf = [0u8; 64];

    tracing::info!(
        netlink = netlink_fd.is_some(),
        distinct_bypass = ctx.has_distinct_bypass,
        "Monitor loop starting"
    );

    loop {
        if netlink_fd.is_none() {
            netlink_fd = setup_netlink_socket();
            if let Some(ref fd) = netlink_fd {
                let raw_fd = fd.as_raw_fd();
                let mut source = SourceFd(&raw_fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, NETLINK, Interest::READABLE)
                {
                    tracing::warn!(error = %e, "Failed to register new Netlink socket");
                    netlink_fd = None;
                }
            }
            diagnostics
                .netlink_available
                .store(netlink_fd.is_some(), Ordering::Relaxed);
        }

        let now_eval = Instant::now();

        // Normalize intent jika masa tenggang bypass telah habis
        ctx.intent.normalize(now_eval);

        // 1. Evaluasi Ulang Konfigurasi & Sensor & Policy & Reconcile jika ada permintaan evaluasi
        if ctx.sched.evaluation_requested {
            if let Ok(guard) = shared_config.read() {
                let new_config = guard.clone();

                // Jika parameter policy berubah, reset temporal state agar
                // grace timer dan policy lama tidak bocor ke sesi config baru.
                if new_config.charge_limit != ctx.config.charge_limit
                    || new_config.resume_limit != ctx.config.resume_limit
                    || new_config.max_temp_dc != ctx.config.max_temp_dc
                    || new_config.max_charge_current_ma != ctx.config.max_charge_current_ma
                    || new_config.thermal_throttling_enabled
                        != ctx.config.thermal_throttling_enabled
                    || new_config.thermal_cutoff != ctx.config.thermal_cutoff
                    || new_config.enabled != ctx.config.enabled
                {
                    if new_config.enabled != ctx.config.enabled {
                        ctx.intent = if new_config.enabled {
                            OperatingIntent::normal()
                        } else {
                            OperatingIntent::disabled()
                        };
                    }
                    ctx.policy_runtime.clear();
                    ctx.policy_result = PolicyResult::clear();
                    ctx.mark_force_hardware_verification();
                }

                ctx.config = new_config;
            }

            ctx.adaptive_scheduler.update_config(
                Duration::from_secs(ctx.config.poll_interval_secs),
                ctx.config.charge_limit as f32,
                ctx.config.resume_limit as f32,
                ctx.config.max_temp_dc as f32 / 10.0,
            );

            // 2. Baca Sensor Observation (Reality)
            let power_state = reader::get_power_state().unwrap_or(reader::PowerState::Unknown);

            let prev_connected = ctx.observed.connection.is_connected();
            ctx.observed.update_connection(power_state, now_eval);
            let now_connected = ctx.observed.connection.is_connected();

            if !prev_connected && now_connected {
                handle_event(&mut ctx, MonitorEvent::ChargerAttached, now_eval);
            } else if prev_connected && !now_connected {
                handle_event(&mut ctx, MonitorEvent::ChargerDetached, now_eval);
            }

            let can_retry_sample = ctx
                .observed
                .next_sensor_retry()
                .is_none_or(|t| now_eval >= t);

            // Optimasi: Skip pembacaan sensor snapshot sysfs jika charger tidak terhubung
            // atau masih dalam masa backoff karena pembacaan sebelumnya gagal.
            let sample = if power_state.is_plugged_in() && can_retry_sample {
                match Sample::read(power_state, now_eval) {
                    Ok(s) => {
                        ctx.sched.mark_snapshot_success();
                        ctx.adaptive_scheduler.update_sample(&s);
                        Some(s)
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "Failed reading battery snapshot");
                        let backoff = ctx.sched.mark_snapshot_failure();
                        ctx.observed.mark_sample_failed(now_eval + backoff);
                        None
                    }
                }
            } else {
                None
            };

            ctx.observed.update(power_state, sample, now_eval);

            let ps_str = format!("{power_state:?}");
            if ctx.diag.last_power_state_str.as_deref() != Some(&ps_str) {
                if let Ok(mut ps) = diagnostics.power_state.write() {
                    *ps = ps_str.clone();
                }
                ctx.diag.last_power_state_str = Some(ps_str);
            }

            if let Some(s) = &ctx.observed.sample {
                diagnostics
                    .battery_level_percent
                    .store(s.capacity as u8, Ordering::Relaxed);
                diagnostics
                    .battery_temperature_dc
                    .store((s.temperature_c * 10.0) as i32, Ordering::Relaxed);
            } else {
                if let Ok(cap) = reader::read_capacity() {
                    diagnostics
                        .battery_level_percent
                        .store(cap, Ordering::Relaxed);
                }
                if let Ok(temp_dc) = reader::read_temperature_dc() {
                    diagnostics
                        .battery_temperature_dc
                        .store(temp_dc, Ordering::Relaxed);
                }
            }

            // 3. Evaluasi Policy Engine & Stepped Thermal Regulation
            if let Some(s) = &ctx.observed.sample {
                let temp_dc = (s.temperature_c * 10.0) as i32;
                policy::evaluate_thermal_stepping(
                    temp_dc,
                    &ctx.config,
                    &mut ctx.policy_runtime,
                    now_eval,
                );
            }

            ctx.policy_result = policy::evaluate_policy(
                &ctx.observed,
                &ctx.config,
                &mut ctx.policy_runtime,
                now_eval,
            );

            // 4. Resolve Decision & Map to Desired Hardware State
            let decision =
                ChargingDecision::resolve(&ctx.observed, &ctx.intent, &ctx.policy_result, now_eval);
            ctx.diag.last_computed_decision = Some(decision.clone());
            let desired_hw = decision.to_desired_hardware();
            let current_reg =
                decision::resolve_current_regulation(&ctx.config, &ctx.policy_runtime, &decision);

            let fsm_str = match &ctx.policy_runtime.charge_limit_state {
                policy::ChargeLimitState::Normal => "Normal (Charging)".to_string(),
                policy::ChargeLimitState::Grace { started_at } => {
                    let elapsed = now_eval.duration_since(*started_at);
                    let remaining = policy::CHARGE_LIMIT_SUSPEND_DELAY
                        .saturating_sub(elapsed)
                        .as_secs();
                    format!("Grace Period ({remaining}s remaining top-off)")
                }
                policy::ChargeLimitState::Suspended => "Suspended (Limit reached)".to_string(),
            };
            if ctx.diag.last_fsm_str.as_deref() != Some(&fsm_str) {
                if let Ok(mut fsm) = diagnostics.fsm_state.write() {
                    *fsm = fsm_str.clone();
                }
                ctx.diag.last_fsm_str = Some(fsm_str);
            }

            let dec_str = match &decision {
                ChargingDecision::Allow => "Allow".to_string(),
                ChargingDecision::Block { cause } => format!("Block ({cause:?})"),
                ChargingDecision::Bypass => "Bypass".to_string(),
                ChargingDecision::Wait { reason } => format!("Wait ({reason:?})"),
            };
            if ctx.diag.last_decision_str.as_deref() != Some(&dec_str) {
                if let Ok(mut dec) = diagnostics.target_decision.write() {
                    *dec = dec_str.clone();
                }
                ctx.diag.last_decision_str = Some(dec_str);
            }

            let cur_str = match &current_reg {
                decision::CurrentRegulation::Unconstrained => {
                    "Unconstrained (Full Speed)".to_string()
                }
                decision::CurrentRegulation::ConfigLimit { target_ua } => {
                    format!("User Limit ({} mA)", target_ua / 1000)
                }
                decision::CurrentRegulation::ThermalThrottle { step, target_ua } => {
                    format!("Thermal Step {step} ({} mA)", target_ua / 1000)
                }
                decision::CurrentRegulation::GraceCap { target_ua } => {
                    format!("Grace Top-Off Cap ({} mA)", target_ua / 1000)
                }
                decision::CurrentRegulation::Disabled => "Disabled (0 mA)".to_string(),
            };
            if ctx.diag.last_current_reg_str.as_deref() != Some(&cur_str) {
                if let Ok(mut cur) = diagnostics.current_regulation.write() {
                    *cur = cur_str.clone();
                }
                ctx.diag.last_current_reg_str = Some(cur_str);
            }

            let fast_charge_pol = policy::evaluate_fast_charge_policy(
                &ctx.observed,
                &ctx.config,
                &ctx.policy_runtime,
                &ctx.policy_result,
                now_eval,
            );

            let fc_str = match &fast_charge_pol {
                policy::FastChargePolicy::Active { target_ua } => {
                    format!("Active (Force PD {} mA)", target_ua / 1000)
                }
                policy::FastChargePolicy::SuppressedSocLimit {
                    current_soc,
                    max_soc,
                } => {
                    format!("Suppressed (SOC {:.1}% >= {}%)", current_soc, max_soc)
                }
                policy::FastChargePolicy::SuppressedThermal => {
                    "Suppressed (Thermal Throttling)".to_string()
                }
                policy::FastChargePolicy::SuppressedChargeLimit => {
                    "Suppressed (Charge Limit)".to_string()
                }
                policy::FastChargePolicy::SuppressedSafety => "Suppressed (Safety)".to_string(),
                policy::FastChargePolicy::Disabled => "Disabled".to_string(),
            };
            if let Ok(mut fc) = diagnostics.fast_charge_status.write() {
                *fc = fc_str;
            }

            // 5. Rekonsiliasi Hardware (Binary Switch + Current Limit + Fast Charge Bypass)
            let is_emergency = matches!(
                decision,
                ChargingDecision::Block {
                    cause: BlockCause::ThermalEmergency
                }
            );
            let current_opts = hardware::CurrentReconcileOptions {
                bypass_retry_delay: is_emergency,
            };

            if let hardware::CurrentReconcileResult::Failed(error) = hardware::reconcile_current(
                desired_hw,
                current_reg,
                &mut ctx.hardware_track,
                current_opts,
                now_eval,
            ) {
                if let hardware::CurrentLimitStatus::Fault { error: fault, .. } =
                    ctx.hardware_track.current_limit.status
                {
                    if ctx.diag.should_log_error(fault, now_eval) {
                        tracing::warn!(?fault, error = %error, "Fast charge current limit reconciliation failed");
                    }
                }
            }

            if let hardware::CurrentReconcileResult::Failed(error) =
                hardware::reconcile_fast_charge(
                    desired_hw,
                    fast_charge_pol,
                    &mut ctx.hardware_track,
                    current_opts,
                    now_eval,
                )
            {
                if let hardware::FastChargeStatus::Fault { error: fault, .. } =
                    ctx.hardware_track.fast_charge.status
                {
                    if ctx.diag.should_log_error(fault, now_eval) {
                        tracing::warn!(?fault, error = %error, "Fast charge bypass reconciliation failed");
                    }
                }
            }

            let opts = hardware::ReconcileOptions {
                bypass_retry_delay: is_emergency,
                force_verification: is_emergency || ctx.sched.force_hardware_verification,
            };

            match hardware::reconcile(
                desired_hw,
                &mut ctx.hardware_track,
                ctx.has_distinct_bypass,
                opts,
                now_eval,
            ) {
                hardware::ReconcileResult::Stable(actual)
                | hardware::ReconcileResult::Changed(actual) => {
                    ctx.sched.clear_hardware_verification_request();

                    if ctx.diag.last_decision.as_ref() != Some(&decision)
                        || ctx.diag.last_hw_mode != Some(actual)
                    {
                        tracing::info!(?decision, ?desired_hw, ?actual, "State transition updated");
                        ctx.diag.last_decision = Some(decision.clone());
                        ctx.diag.last_hw_mode = Some(actual);

                        if let Ok(mut hw) = diagnostics.hardware_state.write() {
                            *hw = format!("{actual:?}");
                        }
                    } else if ctx
                        .diag
                        .last_heartbeat_log
                        .is_none_or(|t| now_eval.duration_since(t) >= Duration::from_secs(300))
                    {
                        tracing::debug!(?decision, ?actual, "Heartbeat status check OK");
                        ctx.diag.last_heartbeat_log = Some(now_eval);
                    }
                }
                // Skipped = desired was NoChange, hardware not actually verified.
                hardware::ReconcileResult::Skipped(actual) => {
                    if ctx.diag.last_decision.as_ref() != Some(&decision)
                        || ctx.diag.last_hw_mode != Some(actual)
                    {
                        tracing::info!(?decision, ?desired_hw, ?actual, "State transition updated");
                        ctx.diag.last_decision = Some(decision.clone());
                        ctx.diag.last_hw_mode = Some(actual);

                        if let Ok(mut hw) = diagnostics.hardware_state.write() {
                            *hw = format!("{actual:?}");
                        }
                    }
                }
                hardware::ReconcileResult::Deferred => {}
                hardware::ReconcileResult::Failed(error) => {
                    ctx.sched.clear_hardware_verification_request();

                    if let hardware::HardwareStatus::Fault { error: fault, .. } =
                        ctx.hardware_track.charger.status
                    {
                        if ctx.diag.should_log_error(fault, now_eval) {
                            tracing::warn!(?fault, error = %error, "Hardware reconciliation failed");
                        }
                    }
                }
            }

            let conv = ctx.hardware_track.overall_convergence();
            let conv_str = format!("{conv:?}");
            if ctx.diag.last_conv_str.as_deref() != Some(&conv_str) {
                if let Ok(mut c) = diagnostics.convergence_state.write() {
                    *c = conv_str.clone();
                }
                ctx.diag.last_conv_str = Some(conv_str);
            }

            ctx.clear_evaluation_request();
        }

        // 6. Konsolidasi Earliest Deadline
        let earliest_deadline = [
            ctx.intent.next_deadline(),
            ctx.observed.connection.next_transition(),
            ctx.hardware_track.next_deadline(),
            ctx.observed.next_sensor_retry(),
            ctx.policy_runtime.charge_limit_deadline(),
            ctx.policy_runtime.thermal_step_deadline(),
            if ctx.observed.connection.is_connected() {
                ctx.observed.sample_stale_deadline()
            } else {
                None
            },
        ]
        .iter()
        .copied()
        .flatten()
        .min();

        let decision_urgency = ctx
            .diag
            .last_computed_decision
            .as_ref()
            .map(|d| d.to_urgency())
            .unwrap_or(Urgency::Idle);
        let retry_urgency = if has_pending_recovery_deadline(&ctx, now_eval) {
            Urgency::Recovery
        } else {
            Urgency::Idle
        };
        let urgency = decision_urgency.max(retry_urgency);

        let truly_idle = can_sleep_forever(&ctx);
        diagnostics.is_idle.store(truly_idle, Ordering::Relaxed);

        // Strict Event-Driven Idle: Saat disconnected & no deadline → poll_timeout_ms = -1 (CPU 0.000%)
        let mut poll_timeout_ms = if truly_idle && netlink_fd.is_some() {
            -1
        } else {
            let next_interval = ctx.adaptive_scheduler.calculate_next_interval(
                urgency,
                earliest_deadline,
                now_eval,
            );
            match urgency {
                Urgency::Safety => (next_interval.as_millis() as u64).clamp(2_000, 5_000) as i32,
                Urgency::Recovery => (next_interval.as_millis() as u64).clamp(1_000, 30_000) as i32,
                Urgency::Monitoring => {
                    (next_interval.as_millis() as u64).clamp(1_000, 30_000) as i32
                }
                Urgency::Normal => (next_interval.as_millis() as u64).clamp(5_000, 60_000) as i32,
                Urgency::Idle => (next_interval.as_millis() as u64).clamp(60_000, 300_000) as i32,
            }
        };

        if netlink_fd.is_none() && poll_timeout_ms > 30_000 {
            poll_timeout_ms = 30_000;
        }

        diagnostics.poll_interval_ms.store(
            if poll_timeout_ms < 0 {
                u64::MAX
            } else {
                poll_timeout_ms as u64
            },
            Ordering::Relaxed,
        );

        let timeout = if poll_timeout_ms < 0 {
            None
        } else {
            Some(Duration::from_millis(poll_timeout_ms as u64))
        };

        if let Err(e) = poll.poll(&mut events, timeout) {
            if e.kind() != std::io::ErrorKind::Interrupted {
                tracing::error!(error = %e, "mio::Poll failed");
            }
        }

        let wake_now = Instant::now();
        diagnostics.total_wakeups.fetch_add(1, Ordering::Relaxed);

        if events.is_empty() {
            // Poll timeout (deadline atau interval monitoring telah tiba)
            if let Ok(mut r) = diagnostics.last_wake_reason.write() {
                *r = "Poll Timeout (Deadline)".to_string();
            }
            ctx.mark_evaluation_requested();
        } else {
            let mut batch = UeventBatch::default();
            let mut ipc_shutdown = false;
            let mut has_ipc = false;

            for event in events.iter() {
                match event.token() {
                    IPC => {
                        has_ipc = true;
                        loop {
                            match mio_rx.recv(&mut msg_buf) {
                                Ok(0) => {
                                    tracing::error!("Internal IPC channel closed unexpectedly");
                                    ipc_shutdown = true;
                                    break;
                                }
                                Ok(_len) => {
                                    let cmd_code = msg_buf[0];
                                    match cmd_code {
                                        1 => handle_event(
                                            &mut ctx,
                                            MonitorEvent::IpcCommand(DaemonCommand::Reload),
                                            wake_now,
                                        ),
                                        2 => {
                                            tracing::info!(
                                                "Shutdown signal received in monitor loop"
                                            );
                                            ipc_shutdown = true;
                                            break;
                                        }
                                        3 => handle_event(
                                            &mut ctx,
                                            MonitorEvent::IpcCommand(DaemonCommand::BypassOn),
                                            wake_now,
                                        ),
                                        4 => handle_event(
                                            &mut ctx,
                                            MonitorEvent::IpcCommand(DaemonCommand::BypassOff),
                                            wake_now,
                                        ),
                                        5 => handle_event(
                                            &mut ctx,
                                            MonitorEvent::IpcCommand(DaemonCommand::DisableOn),
                                            wake_now,
                                        ),
                                        6 => handle_event(
                                            &mut ctx,
                                            MonitorEvent::IpcCommand(DaemonCommand::DisableOff),
                                            wake_now,
                                        ),
                                        _ => {}
                                    }
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    break;
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "IPC recv error");
                                    break;
                                }
                            }
                        }
                    }
                    NETLINK => {
                        if event.is_error() || event.is_read_closed() {
                            batch.netlink_broken = true;
                        } else if event.is_readable() {
                            if let Some(ref nl_fd) = netlink_fd {
                                let raw_fd = nl_fd.as_raw_fd();
                                let mut buf = [0u8; 8192];
                                loop {
                                    let res = unsafe {
                                        libc::recv(
                                            raw_fd,
                                            buf.as_mut_ptr() as *mut libc::c_void,
                                            buf.len(),
                                            libc::MSG_DONTWAIT,
                                        )
                                    };
                                    if res < 0 {
                                        let err = std::io::Error::last_os_error();
                                        match err.raw_os_error() {
                                            // Buffer exhausted: treat as recoverable overflow.
                                            #[cfg(any(target_os = "android", target_os = "linux"))]
                                            Some(libc::ENOBUFS) => {
                                                batch.overflow = true;
                                                break;
                                            }
                                            // No more data available right now.
                                            Some(libc::EAGAIN) => {
                                                break;
                                            }
                                            // Fatal socket error — socket must be recreated.
                                            _ => {
                                                tracing::warn!(
                                                    error = %err,
                                                    "Netlink recv fatal error, marking socket broken"
                                                );
                                                batch.netlink_broken = true;
                                                break;
                                            }
                                        }
                                    }
                                    if res == 0 {
                                        break;
                                    }
                                    let bytes_read = res as usize;
                                    match classify_uevent(&buf[..bytes_read]) {
                                        UeventKind::Ac => batch.ac = true,
                                        UeventKind::Usb => batch.usb = true,
                                        UeventKind::TypeC => batch.typec = true,
                                        UeventKind::Battery => batch.battery = true,
                                        UeventKind::Bms => batch.bms = true,
                                        UeventKind::Other => {}
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            if ipc_shutdown {
                return;
            }

            if let Ok(mut r) = diagnostics.last_wake_reason.write() {
                if has_ipc {
                    *r = "IPC Command".to_string();
                } else if batch.ac || batch.usb || batch.typec {
                    *r = "Netlink Uevent (Charger/USB)".to_string();
                } else if batch.battery || batch.bms {
                    *r = "Netlink Uevent (Battery/BMS)".to_string();
                } else if batch.overflow || batch.netlink_broken {
                    *r = "Netlink Uevent (Recovery)".to_string();
                }
            }

            if batch.ac {
                handle_event(&mut ctx, MonitorEvent::AcChanged, wake_now);
            }
            if batch.usb {
                handle_event(&mut ctx, MonitorEvent::UsbChanged, wake_now);
            }
            if batch.typec {
                handle_event(&mut ctx, MonitorEvent::TypeCChanged, wake_now);
            }
            if batch.battery {
                handle_event(&mut ctx, MonitorEvent::BatteryChanged, wake_now);
            }
            if batch.bms {
                handle_event(&mut ctx, MonitorEvent::BmsChanged, wake_now);
            }
            if batch.overflow {
                ctx.hardware_track.mark_verification_needed();
                ctx.mark_evaluation_requested();
            }
            if batch.netlink_broken {
                ctx.hardware_track.mark_verification_needed();
                ctx.mark_force_hardware_verification();
                ctx.mark_evaluation_requested();
                if let Some(owned_fd) = netlink_fd.take() {
                    let raw_fd = owned_fd.as_raw_fd();
                    let mut source = SourceFd(&raw_fd);
                    let _ = poll.registry().deregister(&mut source);
                    // owned_fd is automatically closed via RAII on drop without manual unsafe libc::close
                    tracing::warn!("Netlink socket broken, will recreate on next loop");
                    diagnostics
                        .netlink_available
                        .store(false, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(not(unix))]
pub fn run_monitor_loop(
    _shared_config: Arc<RwLock<Config>>,
    _rx: (),
    _diagnostics: Arc<DaemonDiagnostics>,
) {
}

#[cfg(unix)]
fn setup_netlink_socket() -> Option<OwnedFd> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW,
                libc::NETLINK_KOBJECT_UEVENT,
            );
            if fd < 0 {
                return None;
            }
            let rcvbuf: libc::c_int = 64 * 1024;
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as u32,
            );

            let mut addr: libc::sockaddr_nl = std::mem::zeroed();
            addr.nl_family = libc::AF_NETLINK as u16;
            addr.nl_groups = 1;
            if libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            ) < 0
            {
                libc::close(fd);
                return None;
            }
            Some(OwnedFd::from_raw_fd(fd))
        }
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    {
        None
    }
}

#[cfg(not(unix))]
fn setup_netlink_socket() -> Option<OwnedFd> {
    None
}

#[cfg(any(unix, test))]
pub(crate) use charger_core::battery::uevent::classify_uevent;
