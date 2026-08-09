use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use charger_core::{
    battery::{control, reader},
    config::schema::Config,
};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);

const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600);

const NETLINK_COALESCE: Duration = Duration::from_millis(100);

const THERMAL_HYSTERESIS_DC: i32 = 20;

const EMA_ALPHA: f32 = 0.30;

const MAX_HISTORY: usize = 5;

/// Interval minimum yang masih diperbolehkan untuk polling normal.
/// Event Netlink tetap bisa memaksa evaluasi instan.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct Sample {
    capacity: f32,
    temp: f32,
    power_state: reader::PowerState,
    ts: Instant,
}

struct AdaptiveScheduler {
    limit: f32,
    thermal_cutoff: f32,

    /// Interval normal dari config.
    configured_interval: Duration,

    history: VecDeque<Sample>,

    ema_cap_rate: f32,
    ema_temp_rate: f32,

    last_interval: Duration,
}

impl AdaptiveScheduler {
    fn new(
        limit: u8,
        thermal_cutoff_dc: i32,
        poll_interval_secs: u64,
    ) -> Self {
        let configured_interval =
            Self::normalize_configured_interval(poll_interval_secs);

        Self {
            limit: limit.min(100) as f32,
            thermal_cutoff: thermal_cutoff_dc as f32 / 10.0,

            configured_interval,

            history: VecDeque::with_capacity(MAX_HISTORY),

            ema_cap_rate: 0.0,
            ema_temp_rate: 0.0,

            last_interval: MIN_INTERVAL,
        }
    }

    fn normalize_configured_interval(seconds: u64) -> Duration {
        let seconds = if seconds == 0 {
            DEFAULT_POLL_INTERVAL.as_secs()
        } else {
            seconds
        };

        Duration::from_secs(seconds).clamp(MIN_INTERVAL, MAX_INTERVAL)
    }

    fn update_config(
        &mut self,
        limit: u8,
        thermal_cutoff_dc: i32,
        poll_interval_secs: u64,
    ) {
        self.limit = limit.min(100) as f32;
        self.thermal_cutoff = thermal_cutoff_dc as f32 / 10.0;

        self.configured_interval =
            Self::normalize_configured_interval(poll_interval_secs);

        // Jangan biarkan interval lama melebihi konfigurasi baru.
        if self.last_interval > self.configured_interval {
            self.last_interval = self.configured_interval;
        }
    }

    fn reset(&mut self) {
        self.history.clear();

        self.ema_cap_rate = 0.0;
        self.ema_temp_rate = 0.0;

        self.last_interval = MIN_INTERVAL;
    }

    fn push_sample(&mut self, sample: Sample) {
        if let Some(prev) = self.history.back() {
            let dt = (sample.ts - prev.ts).as_secs_f32();

            // Event burst / Netlink duplicate.
            if dt < 0.5 {
                self.history.push_back(sample);

                if self.history.len() > MAX_HISTORY {
                    self.history.pop_front();
                }

                return;
            }

            // Deep sleep / Doze recovery.
            //
            // Jangan memakai data sebelum sleep untuk memprediksi
            // charging rate setelah wake-up.
            if dt > 300.0 {
                self.reset();

                self.history.push_back(sample);

                return;
            }

            let capacity_delta = sample.capacity - prev.capacity;

            let capacity_rate =
                capacity_delta.abs() / dt.max(0.1);

            // Abaikan perubahan SOC yang tidak masuk akal.
            if capacity_rate <= 1.0 {
                let new_cap_rate =
                    EMA_ALPHA * (capacity_delta / dt)
                        + (1.0 - EMA_ALPHA) * self.ema_cap_rate;

                let temp_delta = sample.temp - prev.temp;

                let new_temp_rate =
                    EMA_ALPHA * (temp_delta / dt)
                        + (1.0 - EMA_ALPHA) * self.ema_temp_rate;

                if new_cap_rate.is_finite() {
                    self.ema_cap_rate = new_cap_rate;
                }

                if new_temp_rate.is_finite() {
                    self.ema_temp_rate = new_temp_rate;
                }
            }
        }

        self.history.push_back(sample);

        if self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    fn next_interval(
        &mut self,
        limit_blocked: bool,
        thermal_blocked: bool,
        thermal_protection_enabled: bool,
        operating_mode: OperatingMode,
    ) -> Duration {
        let sample = match self.history.back() {
            Some(sample) => sample,
            None => return self.configured_interval,
        };

        match sample.power_state {
            reader::PowerState::Disconnected => {
                self.last_interval = UNPLUGGED_HEARTBEAT;

                return self.last_interval;
            }

            reader::PowerState::Attached => {
                self.last_interval = MIN_INTERVAL;

                return self.last_interval;
            }

            _ => {}
        }

        // Bypass perlu tetap direkonsiliasi secara berkala.
        if operating_mode == OperatingMode::Bypass {
            self.last_interval = Duration::from_secs(15);

            return self.last_interval;
        }

        let dist_to_limit =
            (self.limit - sample.capacity).max(0.0);

        let dist_to_thermal =
            (self.thermal_cutoff - sample.temp).max(0.0);

        // =========================================================
        // HIGH RISK
        // =========================================================

        let mut danger =
            dist_to_limit < 2.0 && !limit_blocked;

        if thermal_protection_enabled {
            danger = danger
                || (
                    (dist_to_thermal < 3.0
                        || self.ema_temp_rate > 0.15)
                    && !thermal_blocked
                );
        }

        if danger {
            self.last_interval = MIN_INTERVAL;

            return self.last_interval;
        }

        // =========================================================
        // BLOCKED STATES
        // =========================================================

        if thermal_blocked {
            self.last_interval = Duration::from_secs(10);

            return self.last_interval;
        }

        if limit_blocked {
            self.last_interval = Duration::from_secs(15);

            return self.last_interval;
        }

        // =========================================================
        // PREDICTIVE SCHEDULING
        // =========================================================

        let predicted = if sample.power_state
            == reader::PowerState::Charging
            && self.ema_cap_rate > 0.01
            && dist_to_limit > 0.0
        {
            let seconds =
                dist_to_limit / self.ema_cap_rate * 0.5;

            Duration::from_secs_f32(seconds.max(0.0))
        } else {
            self.configured_interval
        };

        let target = predicted
            .max(self.configured_interval)
            .clamp(MIN_INTERVAL, MAX_INTERVAL);

        // Jangan polling normal lebih lambat daripada konfigurasi
        // hanya karena prediksi terlalu agresif.
        let target = target.min(MAX_INTERVAL);

        // =========================================================
        // ASYMMETRIC ADAPTATION
        // =========================================================

        // Turun cepat ketika mendekati limit/thermal.
        if target < self.last_interval {
            self.last_interval = target;

            return self.last_interval;
        }

        // Naik perlahan untuk menghindari polling terlalu jarang
        // secara tiba-tiba.
        self.last_interval = self
            .last_interval
            .mul_f32(1.5)
            .max(self.configured_interval)
            .min(target)
            .min(MAX_INTERVAL);

        self.last_interval
    }
}

struct NetlinkFd(RawFd);

impl Drop for NetlinkFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatingMode {
    Normal,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PolicyState {
    thermal_blocked: bool,
    limit_blocked: bool,
}

impl PolicyState {
    fn clear() -> Self {
        Self {
            thermal_blocked: false,
            limit_blocked: false,
        }
    }
}

/// Unified policy engine.
///
/// Charge limit:
///
///   level >= charge_limit
///       -> charging OFF
///
/// Resume:
///
///   level > resume_limit
///       -> tetap OFF setelah sebelumnya mencapai limit
///
///   level <= resume_limit
///       -> charging ON
///
/// Jika resume_limit invalid / 0:
///
///   fallback = charge_limit - 1
fn evaluate_policy(
    power_state: reader::PowerState,
    level: f32,
    temp_dc: i32,
    previous: PolicyState,
    cfg: &Config,
) -> PolicyState {
    // Tidak ada charger.
    //
    // Jangan mempertahankan limit block karena ketika charger
    // dipasang kembali kita ingin policy dievaluasi dari SOC aktual.
    if power_state == reader::PowerState::Disconnected {
        return PolicyState::clear();
    }

    let mut thermal_blocked = previous.thermal_blocked;
    let mut limit_blocked = previous.limit_blocked;

    if !power_state.is_plugged_in() {
        return PolicyState {
            thermal_blocked,
            limit_blocked,
        };
    }

    // =========================================================
    // THERMAL POLICY
    // =========================================================

    if !cfg.thermal_cutoff {
        thermal_blocked = false;
    } else {
        let max_temp_dc = cfg.max_temp_dc;

        let thermal_resume_dc =
            max_temp_dc.saturating_sub(THERMAL_HYSTERESIS_DC);

        if temp_dc >= max_temp_dc {
            thermal_blocked = true;
        } else if previous.thermal_blocked
            && temp_dc <= thermal_resume_dc
        {
            thermal_blocked = false;
        }
    }

    // =========================================================
    // CHARGE LIMIT POLICY
    // =========================================================

    let limit = cfg.charge_limit.min(100) as f32;

    // Hard upper boundary.
    //
    // charge_limit = 100 tetap berarti 100% adalah batas.
    if level >= limit {
        limit_blocked = true;
    } else if previous.limit_blocked {
        // =====================================================
        // RESUME HYSTERESIS
        // =====================================================

        let resume = if cfg.resume_limit > 0
            && cfg.resume_limit < cfg.charge_limit
        {
            cfg.resume_limit as f32
        } else {
            // Fallback aman.
            //
            // Contoh:
            // limit 80 -> resume 79
            // limit 100 -> resume 99
            cfg.charge_limit
                .saturating_sub(1) as f32
        };

        limit_blocked = level > resume;
    }

    PolicyState {
        thermal_blocked,
        limit_blocked,
    }
}

fn create_netlink_socket() -> Option<RawFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };

    if fd < 0 {
        return None;
    }

    let mut addr: libc::sockaddr_nl =
        unsafe { std::mem::zeroed() };

    addr.nl_family =
        libc::AF_NETLINK as libc::sa_family_t;

    // Kernel memilih port-id.
    addr.nl_pid = 0;

    // Subscribe ke kernel uevent broadcast.
    addr.nl_groups = 1;

    let result = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if result < 0 {
        unsafe {
            libc::close(fd);
        }

        return None;
    }

    Some(fd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetlinkEvent {
    None,
    FastPath,
    Coalesce,
}

/// Drain seluruh queue Netlink.
///
/// FastPath:
/// - AC online
/// - battery status
/// - battery capacity
/// - battery temperature
/// - USB Type-C state
///
/// Coalesce:
/// - event power_supply lain yang relevan.
fn drain_and_parse_netlink(fd: RawFd) -> NetlinkEvent {
    let mut buf = [0u8; 8192];

    let mut result = NetlinkEvent::None;

    loop {
        let received = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };

        if received <= 0 {
            break;
        }

        let data = &buf[..received as usize];

        let mut is_power_supply = false;
        let mut name: &[u8] = b"";

        for part in data.split(|b| *b == 0) {
            if part == b"SUBSYSTEM=power_supply" {
                is_power_supply = true;
            } else if part.starts_with(b"POWER_SUPPLY_NAME=") {
                name =
                    &part[b"POWER_SUPPLY_NAME=".len()..];
            }
        }

        if !is_power_supply {
            continue;
        }

        let mut fast = false;

        for part in data.split(|b| *b == 0) {
            // AC online/offline.
            if name == b"ac"
                && part.starts_with(b"POWER_SUPPLY_ONLINE=")
            {
                fast = true;
            }

            // Battery charging status.
            if name == b"battery"
                && part.starts_with(b"POWER_SUPPLY_STATUS=")
            {
                fast = true;
            }

            // Battery SOC.
            if name == b"battery"
                && part.starts_with(b"POWER_SUPPLY_CAPACITY=")
            {
                fast = true;
            }

            // Battery temperature.
            if name == b"battery"
                && part.starts_with(b"POWER_SUPPLY_TEMP=")
            {
                fast = true;
            }

            // USB / Type-C attach state.
            if name == b"usb"
                && (
                    part.starts_with(
                        b"POWER_SUPPLY_TYPEC_MODE=",
                    )
                    || part.starts_with(
                        b"POWER_SUPPLY_ONLINE=",
                    )
                    || part.starts_with(
                        b"POWER_SUPPLY_PRESENT=",
                    )
                )
            {
                fast = true;
            }
        }

        if fast {
            result = NetlinkEvent::FastPath;
            continue;
        }

        if result == NetlinkEvent::None
            && matches!(
                name,
                b"usb"
                    | b"battery"
                    | b"main"
                    | b"ac"
                    | b"wireless"
                    | b"bms"
                    | b"mtk-charger"
                    | b"mt_charger"
            )
        {
            result = NetlinkEvent::Coalesce;
        }
    }

    result
}

/// Verify that the actual hardware matches the requested state.
///
/// Tidak menambah ChargerError baru. Jika verification gagal,
/// kita hanya mengembalikan false sehingga monitor akan mencoba
/// reconciliation pada evaluasi berikutnya.
fn verify_hardware_state(
    expected: control::ActualHardwareMode,
) -> bool {
    let actual = control::get_actual_charging_state();

    if actual == expected {
        true
    } else {
        tracing::warn!(
            "Hardware verification mismatch: expected={:?}, actual={:?}",
            expected,
            actual
        );

        false
    }
}

/// Apply charging state and verify it.
///
/// Jika write sukses tetapi hardware tidak sesuai setelah read-back,
/// state dianggap UNKNOWN sehingga evaluasi berikutnya akan mencoba
/// recovery lagi.
fn apply_charging_state(
    enable: bool,
    applied_state: &mut control::ActualHardwareMode,
) -> bool {
    let expected =
        if enable {
            control::ActualHardwareMode::ChargingEnabled
        } else {
            control::ActualHardwareMode::ChargingDisabled
        };

    match control::set_charging(enable) {
        Ok(()) => {
            if verify_hardware_state(expected) {
                *applied_state = expected;

                true
            } else {
                *applied_state =
                    control::ActualHardwareMode::Unknown;

                false
            }
        }

        Err(e) => {
            *applied_state =
                control::ActualHardwareMode::Unknown;

            tracing::error!(
                "Failed applying charging={}: {}",
                enable,
                e
            );

            false
        }
    }
}

/// Apply bypass and verify.
///
/// `get_actual_charging_state()` dapat membedakan Bypass hanya
/// jika main/charging_enabled tersedia.
fn apply_bypass_state(
    expected: control::ActualHardwareMode,
    applied_state: &mut control::ActualHardwareMode,
) -> bool {
    match control::enter_bypass_mode() {
        Ok(()) => {
            if verify_hardware_state(expected) {
                *applied_state = expected;

                true
            } else {
                *applied_state =
                    control::ActualHardwareMode::Unknown;

                false
            }
        }

        Err(e) => {
            *applied_state =
                control::ActualHardwareMode::Unknown;

            tracing::error!(
                "Failed applying BYPASS: {}",
                e
            );

            false
        }
    }
}

pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    rx: UnixDatagram,
) {
    tracing::info!(
        "Monitor loop started (event-driven + adaptive scheduler)"
    );

    // =========================================================
    // INITIAL CONFIG
    // =========================================================

    let (initial_limit, initial_temp, initial_poll) = {
        let cfg =
            config.read().unwrap_or_else(|e| e.into_inner());

        (
            cfg.charge_limit,
            cfg.max_temp_dc,
            cfg.poll_interval_secs,
        )
    };

    // =========================================================
    // NETLINK
    // =========================================================

    let nl_fd = create_netlink_socket().unwrap_or(-1);

    let _nl_fd_guard = NetlinkFd(nl_fd);

    if nl_fd >= 0 {
        tracing::info!(
            "NETLINK_KOBJECT_UEVENT ready"
        );
    } else {
        tracing::warn!(
            "Netlink unavailable; using adaptive timer"
        );
    }

    // =========================================================
    // SCHEDULER
    // =========================================================

    let mut scheduler = AdaptiveScheduler::new(
        initial_limit,
        initial_temp,
        initial_poll,
    );

    // =========================================================
    // STATE
    // =========================================================

    let mut last_eval_time =
        Instant::now() - Duration::from_secs(60);

    let mut force_next_eval = true;
    let mut pending_netlink_eval = false;

    let mut applied_state =
        control::ActualHardwareMode::Unknown;

    let mut operating_mode = OperatingMode::Normal;

    let mut policy_state = PolicyState::clear();

    // =========================================================
    // MAIN LOOP
    // =========================================================

    loop {
        let cfg = config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // Update scheduler config every loop.
        scheduler.update_config(
            cfg.charge_limit,
            cfg.max_temp_dc,
            cfg.poll_interval_secs,
        );

        // =====================================================
        // DAEMON DISABLED
        // =====================================================

        if !cfg.enabled {
            if applied_state
                != control::ActualHardwareMode::ChargingEnabled
            {
                if applied_state
                    == control::ActualHardwareMode::Bypass
                {
                    if let Err(e) =
                        control::exit_bypass_mode()
                    {
                        tracing::warn!(
                            "Failed exiting BYPASS while daemon disabled: {}",
                            e
                        );
                    }
                }

                if apply_charging_state(
                    true,
                    &mut applied_state,
                ) {
                    tracing::info!(
                        "Daemon disabled: charging restored and verified"
                    );
                } else {
                    tracing::error!(
                        "Daemon disabled: failed restoring/verifying charging"
                    );
                }
            }

            operating_mode = OperatingMode::Normal;
            policy_state = PolicyState::clear();

            scheduler.reset();

            // Ketika daemon disabled, jangan polling.
            // Tunggu IPC/config reload/shutdown.
            let mut pfds = [libc::pollfd {
                fd: rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];

            unsafe {
                libc::poll(
                    pfds.as_mut_ptr(),
                    1,
                    -1,
                );
            }

            let mut buf = [0u8; 1];

            if rx.recv(&mut buf).is_ok() {
                match buf[0] {
                    // shutdown
                    2 => {
                        tracing::info!(
                            "Monitor loop shutting down"
                        );

                        break;
                    }

                    // config reload
                    1 => {
                        tracing::info!(
                            "Config reloaded while disabled"
                        );

                        force_next_eval = true;
                    }

                    _ => {}
                }
            }

            continue;
        }

        // =====================================================
        // SCHEDULER
        // =====================================================

        let mut timeout = scheduler.next_interval(
            policy_state.limit_blocked,
            policy_state.thermal_blocked,
            cfg.thermal_cutoff,
            operating_mode,
        );

        // Forced evaluation.
        if force_next_eval {
            timeout = Duration::ZERO;
        }

        // Deferred Netlink evaluation.
        else if pending_netlink_eval {
            let elapsed = last_eval_time.elapsed();

            if elapsed >= NETLINK_COALESCE {
                timeout = Duration::ZERO;
            } else {
                let remaining =
                    NETLINK_COALESCE - elapsed;

                timeout = timeout.min(remaining);
            }
        }

        // =====================================================
        // POLL IPC + NETLINK
        // =====================================================

        let mut pfds = [
            libc::pollfd {
                fd: rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: nl_fd,
                events: if nl_fd >= 0 {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
        ];

        let nfds = if nl_fd >= 0 { 2 } else { 1 };

        let timeout_ms =
            timeout.as_millis().min(i32::MAX as u128)
                as i32;

        let ret = unsafe {
            libc::poll(
                pfds.as_mut_ptr(),
                nfds,
                timeout_ms,
            )
        };

        if ret < 0 {
            let error =
                std::io::Error::last_os_error();

            if error.kind()
                != std::io::ErrorKind::Interrupted
            {
                tracing::error!(
                    "poll() failed: {}",
                    error
                );

                std::thread::sleep(
                    Duration::from_secs(1),
                );
            }

            continue;
        }

        let mut needs_evaluation = ret == 0;

        // =====================================================
        // IPC
        // =====================================================

        if ret > 0
            && pfds[0].revents & libc::POLLIN != 0
        {
            let mut buf = [0u8; 1];

            if rx.recv(&mut buf).is_ok() {
                match buf[0] {
                    // shutdown
                    2 => {
                        tracing::info!(
                            "Monitor loop shutting down via IPC"
                        );

                        break;
                    }

                    // config reload
                    1 => {
                        tracing::info!(
                            "Config reload requested"
                        );

                        needs_evaluation = true;
                        force_next_eval = true;
                    }

                    // bypass ON
                    3 => {
                        tracing::info!(
                            "Bypass mode enabled via IPC"
                        );

                        operating_mode =
                            OperatingMode::Bypass;

                        needs_evaluation = true;
                    }

                    // bypass OFF
                    4 => {
                        tracing::info!(
                            "Bypass mode disabled via IPC"
                        );

                        operating_mode =
                            OperatingMode::Normal;

                        // Jangan langsung menganggap normal
                        // berhasil. Paksa reconciliation.
                        if let Err(e) =
                            control::exit_bypass_mode()
                        {
                            tracing::warn!(
                                "Failed exiting BYPASS: {}",
                                e
                            );
                        }

                        applied_state =
                            control::ActualHardwareMode::Unknown;

                        needs_evaluation = true;
                    }

                    _ => {}
                }
            }
        }

        // =====================================================
        // NETLINK
        // =====================================================

        if ret > 0
            && nl_fd >= 0
            && pfds[1].revents & libc::POLLIN != 0
        {
            match drain_and_parse_netlink(nl_fd) {
                NetlinkEvent::FastPath => {
                    needs_evaluation = true;
                    pending_netlink_eval = false;

                    tracing::debug!(
                        "Netlink fast-path evaluation"
                    );
                }

                NetlinkEvent::Coalesce => {
                    pending_netlink_eval = true;

                    if last_eval_time.elapsed()
                        >= NETLINK_COALESCE
                    {
                        needs_evaluation = true;
                    }
                }

                NetlinkEvent::None => {}
            }
        }

        // Deferred event sekarang sudah cukup umur.
        if pending_netlink_eval
            && last_eval_time.elapsed()
                >= NETLINK_COALESCE
        {
            needs_evaluation = true;
        }

        if !needs_evaluation {
            continue;
        }

        force_next_eval = false;
        pending_netlink_eval = false;

        // =====================================================
        // READ SOC
        // =====================================================

        let level = match reader::read_capacity_raw() {
            Ok(value) if value.is_finite() => {
                value.clamp(0.0, 100.0)
            }

            Ok(value) => {
                tracing::error!(
                    "Battery capacity is non-finite: {}",
                    value
                );

                last_eval_time = Instant::now();

                continue;
            }

            Err(e) => {
                tracing::error!(
                    "Failed reading capacity: {}",
                    e
                );

                last_eval_time = Instant::now();

                // Fail-safe hanya jika charge_limit memang
                // berada di bawah 100%.
                if cfg.charge_limit < 100 {
                    let _ =
                        control::set_charging(false);
                }

                continue;
            }
        };

        // =====================================================
        // READ TEMPERATURE
        // =====================================================

        let temp_dc =
            match reader::read_temperature_dc() {
                Ok(value) => value,

                Err(e) => {
                    tracing::error!(
                        "Failed reading temperature: {}",
                        e
                    );

                    last_eval_time = Instant::now();

                    if cfg.thermal_cutoff {
                        let _ =
                            control::set_charging(false);
                    }

                    continue;
                }
            };

        // =====================================================
        // READ POWER STATE
        // =====================================================

        let power_state =
            match reader::get_power_state() {
                Ok(value) => value,

                Err(e) => {
                    tracing::error!(
                        "Failed reading power state: {}",
                        e
                    );

                    last_eval_time = Instant::now();

                    let _ =
                        control::set_charging(false);

                    continue;
                }
            };

        if power_state
            == reader::PowerState::Unknown
        {
            tracing::error!(
                "Power state is UNKNOWN"
            );

            last_eval_time = Instant::now();

            let _ =
                control::set_charging(false);

            continue;
        }

        // =====================================================
        // UPDATE SCHEDULER
        // =====================================================

        scheduler.limit =
            cfg.charge_limit.min(100) as f32;

        scheduler.thermal_cutoff =
            cfg.max_temp_dc as f32 / 10.0;

        scheduler.push_sample(Sample {
            capacity: level,
            temp: temp_dc as f32 / 10.0,
            power_state,
            ts: Instant::now(),
        });

        // =====================================================
        // HARDWARE RECONCILIATION
        // =====================================================

        applied_state =
            control::get_actual_charging_state();

        tracing::debug!(
            "Evaluation | SOC={:.2}% | Temp={:.1}C | Power={:?} | Hardware={:?} | limit_blocked={} | thermal_blocked={}",
            level,
            temp_dc as f32 / 10.0,
            power_state,
            applied_state,
            policy_state.limit_blocked,
            policy_state.thermal_blocked
        );

        // =====================================================
        // BYPASS MODE
        // =====================================================

        if operating_mode == OperatingMode::Bypass {
            let expected =
                if control::has_distinct_bypass_node()
                {
                    control::ActualHardwareMode::Bypass
                } else {
                    control::ActualHardwareMode::ChargingDisabled
                };

            if applied_state != expected {
                if apply_bypass_state(
                    expected,
                    &mut applied_state,
                ) {
                    tracing::info!(
                        "Hardware BYPASS applied and verified"
                    );
                } else {
                    tracing::error!(
                        "Hardware BYPASS failed verification"
                    );
                }
            }

            last_eval_time = Instant::now();

            continue;
        }

        // =====================================================
        // UNPLUGGED
        // =====================================================

        if power_state
            == reader::PowerState::Disconnected
        {
            // Saat charger dicabut, hysteresis limit harus
            // di-reset.
            policy_state = PolicyState::clear();

            // EMA lama tidak relevan setelah unplug.
            scheduler.reset();

            if applied_state
                != control::ActualHardwareMode::ChargingEnabled
            {
                if apply_charging_state(
                    true,
                    &mut applied_state,
                ) {
                    tracing::info!(
                        "Charger disconnected: charging restored and verified"
                    );
                } else {
                    tracing::error!(
                        "Failed restoring charging after unplug"
                    );
                }
            }

            last_eval_time = Instant::now();

            continue;
        }

        // =====================================================
        // POLICY EVALUATION
        // =====================================================

        let new_policy = evaluate_policy(
            power_state,
            level,
            temp_dc,
            policy_state,
            &cfg,
        );

        let previous_policy = policy_state;

        policy_state = new_policy;

        let desired_charging =
            !policy_state.thermal_blocked
                && !policy_state.limit_blocked;

        let desired_state =
            if desired_charging {
                control::ActualHardwareMode::ChargingEnabled
            } else {
                control::ActualHardwareMode::ChargingDisabled
            };

        // =====================================================
        // POLICY STATE LOGGING
        // =====================================================

        if previous_policy != policy_state {
            tracing::info!(
                "Policy changed | SOC={:.2}% | limit_blocked {} -> {} | thermal_blocked {} -> {}",
                level,
                previous_policy.limit_blocked,
                policy_state.limit_blocked,
                previous_policy.thermal_blocked,
                policy_state.thermal_blocked
            );
        }

        // =====================================================
        // HARDWARE ACTION
        // =====================================================

        if applied_state != desired_state {
            // Jika masih BYPASS, keluar dulu.
            if applied_state
                == control::ActualHardwareMode::Bypass
            {
                match control::exit_bypass_mode() {
                    Ok(()) => {
                        applied_state =
                            control::ActualHardwareMode::Unknown;

                        tracing::info!(
                            "Exited BYPASS before normal charging policy"
                        );
                    }

                    Err(e) => {
                        applied_state =
                            control::ActualHardwareMode::Unknown;

                        tracing::error!(
                            "Failed exiting BYPASS: {}",
                            e
                        );

                        last_eval_time =
                            Instant::now();

                        continue;
                    }
                }
            }

            if apply_charging_state(
                desired_charging,
                &mut applied_state,
            ) {
                if desired_charging {
                    tracing::info!(
                        "Charging ON | SOC={:.2}% | Temp={:.1}C | limit_blocked={} | thermal_blocked={}",
                        level,
                        temp_dc as f32 / 10.0,
                        policy_state.limit_blocked,
                        policy_state.thermal_blocked
                    );
                } else {
                    if policy_state.limit_blocked {
                        let resume =
                            if cfg.resume_limit > 0
                                && cfg.resume_limit
                                    < cfg.charge_limit
                            {
                                cfg.resume_limit
                            } else {
                                cfg.charge_limit
                                    .saturating_sub(1)
                            };

                        tracing::warn!(
                            "Charging OFF by charge limit | SOC={:.2}% | limit={} | resume_limit={} | Temp={:.1}C",
                            level,
                            cfg.charge_limit,
                            resume,
                            temp_dc as f32 / 10.0
                        );
                    } else if policy_state
                        .thermal_blocked
                    {
                        tracing::warn!(
                            "Charging OFF by thermal protection | SOC={:.2}% | Temp={:.1}C | cutoff={:.1}C",
                            level,
                            temp_dc as f32 / 10.0,
                            cfg.max_temp_dc as f32 / 10.0
                        );
                    } else {
                        tracing::warn!(
                            "Charging OFF | SOC={:.2}% | Temp={:.1}C",
                            level,
                            temp_dc as f32 / 10.0
                        );
                    }
                }
            } else {
                tracing::error!(
                    "Hardware state could not be verified after charging={}",
                    desired_charging
                );
            }
        }

        // =====================================================
        // FINAL RECONCILIATION
        // =====================================================

        //
        // Jika hardware sebelumnya UNKNOWN/INCONSISTENT,
        // lakukan satu pembacaan final pada evaluasi ini.
        //
        // Ini membuat daemon lebih cepat recover dari kondisi
        // driver/sysfs yang sempat tidak sinkron.
        //
        let actual_after =
            control::get_actual_charging_state();

        if actual_after != desired_state {
            tracing::warn!(
                "Final hardware reconciliation mismatch | desired={:?} | actual={:?}",
                desired_state,
                actual_after
            );

            applied_state =
                control::ActualHardwareMode::Unknown;
        } else {
            applied_state = actual_after;
        }

        last_eval_time = Instant::now();
    }
}