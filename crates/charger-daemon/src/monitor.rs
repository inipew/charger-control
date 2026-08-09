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

/// Fallback jika Netlink tidak tersedia.
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600);

/// Saat Netlink tersedia, disconnected tidak perlu heartbeat sama sekali.
const ATTACHED_SETTLE_INTERVAL: Duration = Duration::from_secs(3);

const NETLINK_COALESCE: Duration = Duration::from_millis(100);

const THERMAL_HYSTERESIS_DC: i32 = 20;

const EMA_ALPHA: f32 = 0.30;

const MAX_HISTORY: usize = 5;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Retry/backoff ketika sysfs atau driver sedang error.
const ERROR_BACKOFF_INITIAL: Duration = Duration::from_secs(2);

const ERROR_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Hardware reconciliation tidak perlu dilakukan setiap event biasa.
const HARDWARE_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Setelah charger attach, beri driver sedikit waktu untuk settle.
const ATTACH_SETTLE_WINDOW: Duration = Duration::from_secs(5);

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

    configured_interval: Duration,

    history: VecDeque<Sample>,

    ema_cap_rate: f32,
    ema_temp_rate: f32,

    last_interval: Duration,
}

impl AdaptiveScheduler {
    fn new(limit: u8, thermal_cutoff_dc: i32, poll_interval_secs: u64) -> Self {
        let configured_interval = Self::normalize_configured_interval(poll_interval_secs);

        Self {
            limit: limit.min(100) as f32,
            thermal_cutoff: thermal_cutoff_dc as f32 / 10.0,

            configured_interval,

            history: VecDeque::with_capacity(MAX_HISTORY),

            ema_cap_rate: 0.0,
            ema_temp_rate: 0.0,

            last_interval: configured_interval,
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

    fn update_config(&mut self, limit: u8, thermal_cutoff_dc: i32, poll_interval_secs: u64) {
        self.limit = limit.min(100) as f32;

        self.thermal_cutoff = thermal_cutoff_dc as f32 / 10.0;

        self.configured_interval = Self::normalize_configured_interval(poll_interval_secs);

        if self.last_interval > self.configured_interval {
            self.last_interval = self.configured_interval;
        }
    }

    fn reset(&mut self) {
        self.history.clear();

        self.ema_cap_rate = 0.0;
        self.ema_temp_rate = 0.0;

        /*
         * Jangan reset ke MIN_INTERVAL.
         *
         * MIN_INTERVAL hanya untuk kondisi high-risk.
         * Setelah reset kita ingin kembali ke configured interval.
         */
        self.last_interval = self.configured_interval;
    }

    fn push_sample(&mut self, sample: Sample) {
        if let Some(prev) = self.history.back() {
            let dt = (sample.ts - prev.ts).as_secs_f32();

            /*
             * Duplicate/burst Netlink event.
             *
             * Tetap simpan sample terbaru tetapi jangan
             * menggunakan event burst untuk rate calculation.
             */
            if dt < 0.5 {
                self.history.push_back(sample);

                if self.history.len() > MAX_HISTORY {
                    self.history.pop_front();
                }

                return;
            }

            /*
             * Wake setelah deep sleep.
             *
             * Data sebelum suspend tidak cocok untuk
             * memprediksi charging rate.
             */
            if dt > 300.0 {
                self.reset();

                self.history.push_back(sample);

                return;
            }

            let capacity_delta = sample.capacity - prev.capacity;

            let capacity_rate = capacity_delta.abs() / dt.max(0.1);

            /*
             * Abaikan perubahan SOC yang tidak masuk akal.
             */
            if capacity_rate <= 1.0 {
                let new_cap_rate =
                    EMA_ALPHA * (capacity_delta / dt) + (1.0 - EMA_ALPHA) * self.ema_cap_rate;

                let temp_delta = sample.temp - prev.temp;

                let new_temp_rate =
                    EMA_ALPHA * (temp_delta / dt) + (1.0 - EMA_ALPHA) * self.ema_temp_rate;

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

            None => {
                self.last_interval = self.configured_interval;

                return self.last_interval;
            }
        };

        match sample.power_state {
            reader::PowerState::Disconnected => {
                /*
                 * Caller akan mengubah ini menjadi infinite wait
                 * jika Netlink tersedia.
                 */
                self.last_interval = UNPLUGGED_HEARTBEAT;

                return self.last_interval;
            }

            reader::PowerState::Attached => {
                /*
                 * Attached bukan alasan untuk polling 2 detik
                 * selamanya.
                 *
                 * Kita hanya perlu short settle period.
                 */
                self.last_interval = ATTACHED_SETTLE_INTERVAL;

                return self.last_interval;
            }

            _ => {}
        }

        if operating_mode == OperatingMode::Bypass {
            self.last_interval = Duration::from_secs(30);

            return self.last_interval;
        }

        let dist_to_limit = (self.limit - sample.capacity).max(0.0);

        let dist_to_thermal = (self.thermal_cutoff - sample.temp).max(0.0);

        // =====================================================
        // HIGH RISK
        // =====================================================

        let mut danger = dist_to_limit < 2.0 && !limit_blocked;

        if thermal_protection_enabled {
            danger = danger
                || ((dist_to_thermal < 3.0 || self.ema_temp_rate > 0.15) && !thermal_blocked);
        }

        if danger {
            self.last_interval = MIN_INTERVAL;

            return self.last_interval;
        }

        // =====================================================
        // BLOCKED
        // =====================================================

        if thermal_blocked {
            self.last_interval = Duration::from_secs(10);

            return self.last_interval;
        }

        if limit_blocked {
            self.last_interval = Duration::from_secs(15);

            return self.last_interval;
        }

        // =====================================================
        // PREDICTIVE
        // =====================================================

        let predicted = if sample.power_state == reader::PowerState::Charging
            && self.ema_cap_rate > 0.01
            && dist_to_limit > 0.0
        {
            let seconds = dist_to_limit / self.ema_cap_rate * 0.5;

            Duration::from_secs_f32(seconds.max(0.0))
        } else {
            self.configured_interval
        };

        let target = predicted
            .max(self.configured_interval)
            .clamp(MIN_INTERVAL, MAX_INTERVAL);

        /*
         * Turun cepat.
         */
        if target < self.last_interval {
            self.last_interval = target;

            return self.last_interval;
        }

        /*
         * Naik perlahan agar wakeup frequency tidak
         * berubah secara ekstrem.
         */
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

fn evaluate_policy(
    power_state: reader::PowerState,
    level: f32,
    temp_dc: i32,
    previous: PolicyState,
    cfg: &Config,
) -> PolicyState {
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
    // THERMAL
    // =========================================================

    if !cfg.thermal_cutoff {
        thermal_blocked = false;
    } else {
        let max_temp_dc = cfg.max_temp_dc;

        let thermal_resume_dc = max_temp_dc.saturating_sub(THERMAL_HYSTERESIS_DC);

        if temp_dc >= max_temp_dc {
            thermal_blocked = true;
        } else if previous.thermal_blocked && temp_dc <= thermal_resume_dc {
            thermal_blocked = false;
        }
    }

    // =========================================================
    // CHARGE LIMIT
    // =========================================================

    let limit = cfg.charge_limit.min(100) as f32;

    if level >= limit {
        limit_blocked = true;
    } else if previous.limit_blocked {
        let resume = if cfg.resume_limit > 0 && cfg.resume_limit < cfg.charge_limit {
            cfg.resume_limit as f32
        } else {
            cfg.charge_limit.saturating_sub(1) as f32
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

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };

    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;

    addr.nl_pid = 0;
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
                name = &part[b"POWER_SUPPLY_NAME=".len()..];
            }
        }

        if !is_power_supply {
            continue;
        }

        let mut fast = false;

        for part in data.split(|b| *b == 0) {
            if name == b"ac" && part.starts_with(b"POWER_SUPPLY_ONLINE=") {
                fast = true;
            }

            if name == b"battery"
                && (part.starts_with(b"POWER_SUPPLY_STATUS=")
                    || part.starts_with(b"POWER_SUPPLY_CAPACITY=")
                    || part.starts_with(b"POWER_SUPPLY_TEMP="))
            {
                fast = true;
            }

            if name == b"usb"
                && (part.starts_with(b"POWER_SUPPLY_TYPEC_MODE=")
                    || part.starts_with(b"POWER_SUPPLY_ONLINE=")
                    || part.starts_with(b"POWER_SUPPLY_PRESENT="))
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

/// Apply charging state and verify it.
///
/// IMPORTANT:
/// - `set_charging()` performs the write.
/// - `get_actual_charging_state()` performs exactly one verification read.
/// - The verification result becomes `applied_state`.
///
/// Caller must NOT call `get_actual_charging_state()` again after this
/// function during the same evaluation.
fn apply_charging_state(enable: bool, applied_state: &mut control::ActualHardwareMode) -> bool {
    let expected = if enable {
        control::ActualHardwareMode::ChargingEnabled
    } else {
        control::ActualHardwareMode::ChargingDisabled
    };

    match control::set_charging(enable) {
        Ok(()) => {
            let actual = control::get_actual_charging_state();

            if actual == expected {
                *applied_state = actual;

                true
            } else {
                tracing::warn!(
                    "Hardware verification mismatch: expected={:?}, actual={:?}",
                    expected,
                    actual
                );

                *applied_state = control::ActualHardwareMode::Unknown;

                false
            }
        }

        Err(e) => {
            *applied_state = control::ActualHardwareMode::Unknown;

            tracing::error!("Failed applying charging={}: {}", enable, e);

            false
        }
    }
}

/// Apply BYPASS and verify it.
///
/// Exactly one hardware verification read is performed.
fn apply_bypass_state(
    expected: control::ActualHardwareMode,
    applied_state: &mut control::ActualHardwareMode,
) -> bool {
    match control::enter_bypass_mode() {
        Ok(()) => {
            let actual = control::get_actual_charging_state();

            if actual == expected {
                *applied_state = actual;

                true
            } else {
                tracing::warn!(
                    "BYPASS verification mismatch: expected={:?}, actual={:?}",
                    expected,
                    actual
                );

                *applied_state = control::ActualHardwareMode::Unknown;

                false
            }
        }

        Err(e) => {
            *applied_state = control::ActualHardwareMode::Unknown;

            tracing::error!("Failed applying BYPASS: {}", e);

            false
        }
    }
}

/// Return timeout in milliseconds suitable for poll().
fn duration_to_poll_ms(duration: Duration) -> i32 {
    if duration.is_zero() {
        return 0;
    }

    duration.as_millis().min(i32::MAX as u128) as i32
}

/// Read all policy inputs.
///
/// One evaluation uses one coherent policy snapshot.
fn read_monitor_snapshot() -> Result<(f32, i32, reader::PowerState), &'static str> {
    let level = match reader::read_capacity_raw() {
        Ok(value) if value.is_finite() => value.clamp(0.0, 100.0),

        Ok(_) => {
            return Err("battery_capacity_non_finite");
        }

        Err(_) => {
            return Err("battery_capacity_read_failed");
        }
    };

    let temp_dc = reader::read_temperature_dc().map_err(|_| "battery_temperature_read_failed")?;

    let power_state = reader::get_power_state().map_err(|_| "power_state_read_failed")?;

    if power_state == reader::PowerState::Unknown {
        return Err("power_state_unknown");
    }

    Ok((level, temp_dc, power_state))
}

/// Main charger monitor.
///
/// Design goals:
///
/// - event driven whenever possible
/// - no periodic wakeup while disconnected if Netlink works
/// - bounded fallback heartbeat when Netlink is unavailable
/// - adaptive polling only while charger is relevant
/// - exponential error backoff
/// - hardware reconciliation without unnecessary sysfs traffic
/// - at most one get_actual_charging_state() read per evaluation
pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    tracing::info!("Monitor loop started (low-power event-driven monitor)");

    let (initial_limit, initial_temp, initial_poll) = {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner());

        (cfg.charge_limit, cfg.max_temp_dc, cfg.poll_interval_secs)
    };

    // =========================================================
    // NETLINK
    // =========================================================

    let nl_fd = create_netlink_socket().unwrap_or(-1);

    let _nl_fd_guard = NetlinkFd(nl_fd);

    let netlink_available = nl_fd >= 0;

    if netlink_available {
        tracing::info!("NETLINK_KOBJECT_UEVENT ready; disconnected state will sleep indefinitely");
    } else {
        tracing::warn!("Netlink unavailable; using low-frequency fallback heartbeat");
    }

    // =========================================================
    // SCHEDULER
    // =========================================================

    let mut scheduler = AdaptiveScheduler::new(initial_limit, initial_temp, initial_poll);

    // =========================================================
    // STATE
    // =========================================================

    let mut last_eval_time = Instant::now() - Duration::from_secs(60);

    let mut last_hardware_reconcile = Instant::now() - HARDWARE_RECONCILE_INTERVAL;

    let mut attach_time: Option<Instant> = None;

    let mut force_next_eval = true;
    let mut pending_netlink_eval = false;

    /*
     * Event dari power_supply/netlink berarti hardware mungkin
     * berubah di luar monitor. Event tersebut harus memicu
     * reconciliation walaupun applied_state masih terlihat benar.
     */
    let mut hardware_event_pending = false;

    let mut applied_state = control::ActualHardwareMode::Unknown;

    let mut operating_mode = OperatingMode::Normal;

    let mut policy_state = PolicyState::clear();

    let mut last_power_state = reader::PowerState::Unknown;

    let mut error_backoff = ERROR_BACKOFF_INITIAL;

    // =========================================================
    // MAIN LOOP
    // =========================================================

    loop {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()).clone();

        scheduler.update_config(cfg.charge_limit, cfg.max_temp_dc, cfg.poll_interval_secs);

        // =====================================================
        // DAEMON DISABLED
        // =====================================================

        if !cfg.enabled {
            /*
             * Disabled daemon harus meninggalkan charging normal.
             */
            if applied_state != control::ActualHardwareMode::ChargingEnabled {
                if applied_state == control::ActualHardwareMode::Bypass {
                    if let Err(e) = control::exit_bypass_mode() {
                        tracing::warn!("Failed exiting BYPASS while daemon disabled: {}", e);
                    }
                }

                if apply_charging_state(true, &mut applied_state) {
                    tracing::info!("Daemon disabled: charging restored");
                } else {
                    tracing::error!("Daemon disabled: failed restoring charging");
                }
            }

            operating_mode = OperatingMode::Normal;

            policy_state = PolicyState::clear();

            scheduler.reset();

            attach_time = None;
            pending_netlink_eval = false;
            hardware_event_pending = false;
            force_next_eval = false;

            /*
             * BENAR-BENAR IDLE.
             *
             * Tidak ada polling periodik ketika disabled.
             */
            let mut pfd = libc::pollfd {
                fd: rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pfd, 1, -1) };

            if ret < 0 {
                let error = std::io::Error::last_os_error();

                if error.kind() != std::io::ErrorKind::Interrupted {
                    tracing::error!("poll() failed while disabled: {}", error);

                    std::thread::sleep(Duration::from_secs(1));
                }

                continue;
            }

            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }

            /*
             * Drain IPC sampai queue kosong.
             */
            loop {
                let mut buf = [0u8; 1];

                match rx.recv(&mut buf) {
                    Ok(_) => match buf[0] {
                        2 => {
                            tracing::info!("Monitor loop shutting down");

                            return;
                        }

                        1 => {
                            tracing::info!("Config reloaded while disabled");

                            break;
                        }

                        _ => {}
                    },

                    Err(_) => break,
                }
            }

            continue;
        }

        // =====================================================
        // DETERMINE WAIT
        // =====================================================

        let mut timeout = scheduler.next_interval(
            policy_state.limit_blocked,
            policy_state.thermal_blocked,
            cfg.thermal_cutoff,
            operating_mode,
        );

        /*
         * Jika state disconnected dan Netlink tersedia,
         * jangan bangunkan CPU berdasarkan timer.
         *
         * Hanya IPC atau kernel uevent yang membangunkan.
         */
        if let Some(sample) = scheduler.history.back() {
            if sample.power_state == reader::PowerState::Disconnected
                && netlink_available
                && !force_next_eval
                && !pending_netlink_eval
            {
                timeout = Duration::from_secs(u64::MAX / 2);
            }
        }

        if force_next_eval {
            timeout = Duration::ZERO;
        } else if pending_netlink_eval {
            let elapsed = last_eval_time.elapsed();

            if elapsed >= NETLINK_COALESCE {
                timeout = Duration::ZERO;
            } else {
                timeout = timeout.min(NETLINK_COALESCE - elapsed);
            }
        }

        /*
         * Setelah attach, beri kesempatan driver settle.
         */
        if let Some(attached_at) = attach_time {
            let elapsed = attached_at.elapsed();

            if elapsed < ATTACH_SETTLE_WINDOW {
                timeout = timeout.min(ATTACH_SETTLE_WINDOW - elapsed);
            } else {
                attach_time = None;
            }
        }

        /*
         * Error backoff.
         *
         * Jangan hammer sysfs ketika driver sedang bermasalah.
         */
        if error_backoff > ERROR_BACKOFF_INITIAL && !force_next_eval {
            timeout = timeout.max(error_backoff);
        }

        // =====================================================
        // POLL
        // =====================================================

        let mut pfds = [
            libc::pollfd {
                fd: rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: nl_fd,
                events: if netlink_available { libc::POLLIN } else { 0 },
                revents: 0,
            },
        ];

        let nfds: libc::nfds_t = if netlink_available { 2 } else { 1 };

        let timeout_ms = duration_to_poll_ms(timeout);

        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, timeout_ms) };

        if ret < 0 {
            let error = std::io::Error::last_os_error();

            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }

            tracing::error!("poll() failed: {}", error);

            std::thread::sleep(Duration::from_secs(1));

            continue;
        }

        let mut needs_evaluation = ret == 0;

        // =====================================================
        // IPC
        // =====================================================

        if ret > 0 && pfds[0].revents & libc::POLLIN != 0 {
            /*
             * Drain IPC queue supaya burst config/bypass/shutdown
             * tidak menghasilkan satu wakeup per message.
             */
            loop {
                let mut buf = [0u8; 1];

                match rx.recv(&mut buf) {
                    Ok(_) => {
                        match buf[0] {
                            // shutdown
                            2 => {
                                tracing::info!("Monitor loop shutting down via IPC");

                                return;
                            }

                            // config reload
                            1 => {
                                tracing::info!("Config reload requested");

                                needs_evaluation = true;

                                force_next_eval = true;
                            }

                            // bypass ON
                            3 => {
                                tracing::info!("Bypass mode enabled via IPC");

                                operating_mode = OperatingMode::Bypass;

                                needs_evaluation = true;

                                force_next_eval = true;

                                hardware_event_pending = true;
                            }

                            // bypass OFF
                            4 => {
                                tracing::info!("Bypass mode disabled via IPC");

                                operating_mode = OperatingMode::Normal;

                                if let Err(e) = control::exit_bypass_mode() {
                                    tracing::warn!("Failed exiting BYPASS: {}", e);
                                }

                                /*
                                 * Kita tidak melakukan read-back di sini.
                                 *
                                 * State menjadi UNKNOWN dan evaluasi
                                 * berikutnya melakukan reconciliation.
                                 */
                                applied_state = control::ActualHardwareMode::Unknown;

                                needs_evaluation = true;

                                force_next_eval = true;

                                hardware_event_pending = true;
                            }

                            _ => {}
                        }
                    }

                    Err(_) => break,
                }
            }
        }

        // =====================================================
        // NETLINK
        // =====================================================

        if netlink_available && ret > 0 && pfds[1].revents & libc::POLLIN != 0 {
            match drain_and_parse_netlink(nl_fd) {
                NetlinkEvent::FastPath => {
                    needs_evaluation = true;

                    pending_netlink_eval = false;

                    /*
                     * Hardware mungkin berubah.
                     *
                     * Jangan langsung percaya applied_state.
                     * Evaluasi berikutnya akan melakukan satu
                     * reconciliation read.
                     */
                    hardware_event_pending = true;

                    /*
                     * Jika disconnected -> attach event,
                     * short settle window dimulai.
                     */
                    attach_time = Some(Instant::now());

                    tracing::debug!("Netlink fast-path evaluation");
                }

                NetlinkEvent::Coalesce => {
                    pending_netlink_eval = true;

                    hardware_event_pending = true;

                    if last_eval_time.elapsed() >= NETLINK_COALESCE {
                        needs_evaluation = true;
                    }
                }

                NetlinkEvent::None => {}
            }
        }

        // =====================================================
        // COALESCED EVENT
        // =====================================================

        if pending_netlink_eval && last_eval_time.elapsed() >= NETLINK_COALESCE {
            needs_evaluation = true;
        }

        if !needs_evaluation {
            continue;
        }

        force_next_eval = false;
        pending_netlink_eval = false;

        // =====================================================
        // READ POLICY SNAPSHOT
        // =====================================================

        let (level, temp_dc, power_state) = match read_monitor_snapshot() {
            Ok(snapshot) => {
                error_backoff = ERROR_BACKOFF_INITIAL;

                snapshot
            }

            Err(reason) => {
                tracing::error!("Monitor read failed: {}", reason);

                /*
                 * Conservative failure policy.
                 *
                 * Hanya mematikan charging jika ada
                 * alasan safety yang jelas.
                 *
                 * State dibuat UNKNOWN karena kita tidak
                 * mempunyai verification result yang valid.
                 */
                if cfg.thermal_cutoff {
                    let _ = control::set_charging(false);

                    applied_state = control::ActualHardwareMode::Unknown;
                }

                last_eval_time = Instant::now();

                error_backoff = (error_backoff * 2).min(ERROR_BACKOFF_MAX);

                /*
                 * Safety action di atas sengaja tidak
                 * melakukan read-back tambahan.
                 */
                continue;
            }
        };

        // =====================================================
        // POWER STATE TRANSITION
        // =====================================================

        let power_changed = power_state != last_power_state;

        if power_changed {
            tracing::debug!(
                "Power state changed: {:?} -> {:?}",
                last_power_state,
                power_state
            );

            if power_state == reader::PowerState::Disconnected {
                /*
                 * Reset state yang hanya relevan saat charger
                 * terpasang.
                 */
                policy_state = PolicyState::clear();

                scheduler.reset();

                attach_time = None;
            }

            if power_state.is_plugged_in() && last_power_state == reader::PowerState::Disconnected {
                attach_time = Some(Instant::now());

                /*
                 * Charger baru dipasang:
                 * jangan gunakan rate lama.
                 */
                scheduler.reset();

                /*
                 * Driver mungkin mengubah charging state
                 * selama attach.
                 */
                hardware_event_pending = true;
            }

            last_power_state = power_state;
        }

        // =====================================================
        // UPDATE SCHEDULER
        // =====================================================

        scheduler.limit = cfg.charge_limit.min(100) as f32;

        scheduler.thermal_cutoff = cfg.max_temp_dc as f32 / 10.0;

        scheduler.push_sample(Sample {
            capacity: level,
            temp: temp_dc as f32 / 10.0,
            power_state,
            ts: Instant::now(),
        });

        // =====================================================
        // BYPASS
        // =====================================================

        if operating_mode == OperatingMode::Bypass {
            let expected = if control::has_distinct_bypass_node() {
                control::ActualHardwareMode::Bypass
            } else {
                control::ActualHardwareMode::ChargingDisabled
            };

            /*
             * =================================================
             * IMPORTANT
             * =================================================
             *
             * Jika state kita sudah sesuai dan tidak ada
             * reconciliation yang diperlukan -> ZERO sysfs read.
             *
             * Jika ada event/interval -> satu read saja.
             *
             * Jika read menemukan mismatch -> state dibuat
             * UNKNOWN dan APPLY ditunda ke evaluasi berikutnya.
             * Ini mencegah:
             *
             *     get_actual()
             *     enter_bypass()
             *     get_actual()
             *
             * dalam satu evaluasi.
             */
            let reconciliation_due = hardware_event_pending
                || last_hardware_reconcile.elapsed() >= HARDWARE_RECONCILE_INTERVAL;

            if applied_state == expected && reconciliation_due {
                let actual = control::get_actual_charging_state();

                last_hardware_reconcile = Instant::now();

                hardware_event_pending = false;

                if actual == expected {
                    applied_state = actual;
                } else {
                    tracing::warn!(
                        "BYPASS hardware drift detected: expected={:?}, actual={:?}; deferring reconciliation"
                        ,
                        expected,
                        actual
                    );

                    applied_state = control::ActualHardwareMode::Unknown;

                    /*
                     * Evaluasi berikutnya akan apply BYPASS.
                     */
                    force_next_eval = true;
                }
            } else if applied_state == control::ActualHardwareMode::Unknown
                || applied_state == control::ActualHardwareMode::Inconsistent
            {
                /*
                 * Unknown/Inconsistent:
                 *
                 * Satu probe dulu.
                 *
                 * Jika sudah sesuai -> selesai.
                 * Jika tidak -> defer apply ke evaluasi berikutnya.
                 */
                let actual = control::get_actual_charging_state();

                last_hardware_reconcile = Instant::now();

                hardware_event_pending = false;

                if actual == expected {
                    applied_state = actual;
                } else {
                    applied_state = control::ActualHardwareMode::Unknown;

                    force_next_eval = true;
                }
            } else if applied_state != expected {
                /*
                 * Kita sudah tahu state terakhir berbeda.
                 *
                 * Tidak perlu read sebelum write.
                 *
                 * apply_bypass_state() melakukan:
                 *
                 *     write
                 *     verify
                 *
                 * dan hasil verification langsung menjadi
                 * applied_state.
                 */
                if apply_bypass_state(expected, &mut applied_state) {
                    tracing::info!("Hardware BYPASS applied and verified");

                    last_hardware_reconcile = Instant::now();

                    hardware_event_pending = false;

                    error_backoff = ERROR_BACKOFF_INITIAL;
                } else {
                    tracing::error!("Hardware BYPASS failed verification");

                    last_hardware_reconcile = Instant::now();

                    hardware_event_pending = false;

                    error_backoff = (error_backoff * 2).min(ERROR_BACKOFF_MAX);
                }
            }

            last_eval_time = Instant::now();

            continue;
        }

        // =====================================================
        // UNPLUGGED
        // =====================================================

        if power_state == reader::PowerState::Disconnected {
            policy_state = PolicyState::clear();

            scheduler.reset();

            /*
             * Charger dicabut -> charging harus kembali normal.
             *
             * Tidak perlu read hardware terlebih dahulu.
             * State yang tersimpan sudah cukup untuk mengetahui
             * bahwa kita perlu restore.
             */
            if applied_state != control::ActualHardwareMode::ChargingEnabled {
                if apply_charging_state(true, &mut applied_state) {
                    tracing::info!("Charger disconnected: charging restored");

                    last_hardware_reconcile = Instant::now();

                    error_backoff = ERROR_BACKOFF_INITIAL;
                } else {
                    tracing::error!("Failed restoring charging after unplug");

                    error_backoff = (error_backoff * 2).min(ERROR_BACKOFF_MAX);
                }
            }

            hardware_event_pending = false;

            last_eval_time = Instant::now();

            /*
             * Kalau Netlink tersedia, loop berikutnya
             * akan tidur tanpa timeout.
             */
            continue;
        }

        // =====================================================
        // POLICY
        // =====================================================

        let previous_policy = policy_state;

        policy_state = evaluate_policy(power_state, level, temp_dc, policy_state, &cfg);

        let desired_charging = !policy_state.thermal_blocked && !policy_state.limit_blocked;

        let desired_state = if desired_charging {
            control::ActualHardwareMode::ChargingEnabled
        } else {
            control::ActualHardwareMode::ChargingDisabled
        };

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
        // EXIT BYPASS IF NEEDED
        // =====================================================

        if applied_state == control::ActualHardwareMode::Bypass {
            match control::exit_bypass_mode() {
                Ok(()) => {
                    /*
                     * Jangan read-back di sini.
                     *
                     * Kita tidak tahu hasil hardware final sampai
                     * evaluasi berikutnya. Jadikan UNKNOWN.
                     */
                    applied_state = control::ActualHardwareMode::Unknown;

                    hardware_event_pending = true;

                    tracing::info!("Exited BYPASS before normal charging policy");
                }

                Err(e) => {
                    applied_state = control::ActualHardwareMode::Unknown;

                    tracing::error!("Failed exiting BYPASS: {}", e);

                    last_eval_time = Instant::now();

                    error_backoff = (error_backoff * 2).min(ERROR_BACKOFF_MAX);

                    continue;
                }
            }
        }

        // =====================================================
        // HARDWARE RECONCILIATION
        // =====================================================

        /*
         * Tiga kondisi utama:
         *
         * 1. applied == desired
         *    -> tidak ada sysfs read kecuali reconciliation
         *       memang jatuh tempo / ada Netlink event.
         *
         * 2. applied != desired
         *    -> state terakhir sudah memberi tahu kita bahwa
         *       write diperlukan. Jangan read dulu.
         *
         * 3. applied == UNKNOWN / INCONSISTENT
         *    -> satu probe read.
         *
         * Jika probe menemukan mismatch:
         *
         *     applied = UNKNOWN
         *     force_next_eval = true
         *
         * lalu write dilakukan pada evaluasi berikutnya.
         *
         * Dengan desain ini tidak pernah terjadi:
         *
         *     get_actual()
         *     set_charging()
         *     get_actual()
         *
         * dalam satu evaluasi.
         */

        let reconciliation_due = hardware_event_pending
            || last_hardware_reconcile.elapsed() >= HARDWARE_RECONCILE_INTERVAL;

        if applied_state == desired_state {
            if reconciliation_due {
                /*
                 * Satu-satunya read pada evaluasi ini.
                 */
                let actual = control::get_actual_charging_state();

                last_hardware_reconcile = Instant::now();

                hardware_event_pending = false;

                if actual == desired_state {
                    /*
                     * Hardware masih sesuai.
                     *
                     * Tidak ada write dan tidak ada read kedua.
                     */
                    applied_state = actual;
                } else {
                    /*
                     * Hardware berubah di luar daemon.
                     *
                     * Jangan apply pada evaluasi yang sama.
                     * Mark UNKNOWN agar evaluasi berikutnya
                     * melakukan reconciliation/apply.
                     */
                    tracing::warn!(
                        "Hardware state drift detected: expected={:?}, actual={:?}; deferring reconciliation"
                        ,
                        desired_state,
                        actual
                    );

                    applied_state = control::ActualHardwareMode::Unknown;

                    force_next_eval = true;
                }
            }
        } else if applied_state == control::ActualHardwareMode::Unknown
            || applied_state == control::ActualHardwareMode::Inconsistent
        {
            /*
             * UNKNOWN/INCONSISTENT:
             *
             * Probe sekali.
             */
            let actual = control::get_actual_charging_state();

            last_hardware_reconcile = Instant::now();

            hardware_event_pending = false;

            if actual == desired_state {
                /*
                 * Hardware ternyata sudah benar.
                 *
                 * Tidak perlu write.
                 */
                applied_state = actual;
            } else {
                /*
                 * Hardware diketahui berbeda.
                 *
                 * Jangan write pada evaluasi ini karena probe
                 * sudah menggunakan satu get_actual().
                 *
                 * Evaluasi berikutnya akan langsung apply
                 * tanpa pre-read.
                 */
                tracing::debug!(
                    "Hardware requires state change: expected={:?}, actual={:?}; deferring apply",
                    desired_state,
                    actual
                );

                applied_state = actual;

                force_next_eval = true;
            }
        }

        // =====================================================
        // APPLY HARDWARE STATE
        // =====================================================

        /*
         * Hanya masuk jika kita memang sudah tahu dari state
         * sebelumnya bahwa desired_state berbeda.
         *
         * apply_charging_state():
         *
         *     set_charging()
         *     get_actual_charging_state()
         *
         * tepat satu verification read.
         */
        if applied_state != desired_state && !force_next_eval {
            if apply_charging_state(desired_charging, &mut applied_state) {
                last_hardware_reconcile = Instant::now();

                hardware_event_pending = false;

                error_backoff = ERROR_BACKOFF_INITIAL;

                if desired_charging {
                    tracing::info!(
                        "Charging ON | SOC={:.2}% | Temp={:.1}C | limit_blocked={} | thermal_blocked={}",
                        level,
                        temp_dc as f32 / 10.0,
                        policy_state.limit_blocked,
                        policy_state.thermal_blocked
                    );
                } else if policy_state.limit_blocked {
                    let resume = if cfg.resume_limit > 0 && cfg.resume_limit < cfg.charge_limit {
                        cfg.resume_limit
                    } else {
                        cfg.charge_limit.saturating_sub(1)
                    };

                    tracing::warn!(
                        "Charging OFF by charge limit | SOC={:.2}% | limit={} | resume_limit={} | Temp={:.1}C",
                        level,
                        cfg.charge_limit,
                        resume,
                        temp_dc as f32 / 10.0
                    );
                } else if policy_state.thermal_blocked {
                    tracing::warn!(
                        "Charging OFF by thermal protection | SOC={:.2}% | Temp={:.1}C | cutoff={:.1}C",
                        level,
                        temp_dc as f32 / 10.0,
                        cfg.max_temp_dc as f32 / 10.0
                    );
                }
            } else {
                tracing::error!(
                    "Hardware state could not be verified after charging={}",
                    desired_charging
                );

                /*
                 * apply_charging_state() sudah melakukan verification
                 * dan mengubah applied_state menjadi UNKNOWN bila gagal.
                 *
                 * Tidak ada read-back tambahan.
                 */
                error_backoff = (error_backoff * 2).min(ERROR_BACKOFF_MAX);

                last_hardware_reconcile = Instant::now();
            }
        }

        // =====================================================
        // FINAL STATE
        // =====================================================

        /*
         * TIDAK ADA:
         *
         *     get_actual_charging_state()
         *
         * di sini.
         *
         * Jika state baru saja di-apply:
         *     apply_charging_state()
         * sudah memverifikasi.
         *
         * Jika state sudah benar:
         *     reconciliation read sudah dilakukan hanya
         *     jika event/interval memang memerlukannya.
         *
         * Jika hardware berubah:
         *     state dibuat UNKNOWN dan force_next_eval=true.
         *
         * Evaluasi berikutnya akan melakukan reconciliation
         * tanpa read-back ganda.
         */

        last_eval_time = Instant::now();
    }
}
