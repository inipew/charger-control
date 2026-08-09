use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use charger_core::{
    battery::{control, reader},
    config::schema::Config,
};

// ============================================================
// SLEEP-FRIENDLY TIMING
// ============================================================
//
// Prinsip:
//   1. Event Netlink = fast path.
//   2. Timer hanya safety/reconciliation fallback.
//   3. Stable state = heartbeat panjang.
//   4. Near limit / thermal = heartbeat pendek.
//   5. Jangan pernah busy-loop.
//

const MIN_INTERVAL: Duration = Duration::from_secs(2);

// Safety heartbeat maksimum.
// 15 menit cukup sebagai fallback bila driver gagal mengirim uevent.
const MAX_INTERVAL: Duration = Duration::from_secs(15 * 60);

// Stable plugged-in heartbeat.
const STABLE_HEARTBEAT: Duration = Duration::from_secs(5 * 60);

// Saat charger baru attach dan AC belum online.
const ATTACHED_HEARTBEAT: Duration = Duration::from_secs(30);

// Bypass perlu direkonsiliasi, tetapi tidak perlu polling agresif.
const BYPASS_HEARTBEAT: Duration = Duration::from_secs(5 * 60);

// Saat dekat charge limit.
const NEAR_LIMIT_HEARTBEAT: Duration = Duration::from_secs(60);

// Saat sangat dekat limit.
const CRITICAL_LIMIT_HEARTBEAT: Duration = Duration::from_secs(15);

// Thermal dekat cutoff.
const THERMAL_HEARTBEAT: Duration = Duration::from_secs(30);

// Saat thermal protection sedang aktif.
const THERMAL_BLOCKED_HEARTBEAT: Duration = Duration::from_secs(60);

// Limit sedang aktif.
const LIMIT_BLOCKED_HEARTBEAT: Duration = Duration::from_secs(2 * 60);

// Tidak ada charger.
// Event plug-in tetap akan membangunkan daemon.
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(15 * 60);

// Netlink event coalescing.
const NETLINK_COALESCE: Duration = Duration::from_millis(150);

// Hysteresis temperature: 2.0 C.
const THERMAL_HYSTERESIS_DC: i32 = 20;

// EMA.
const EMA_ALPHA: f32 = 0.30;

// History kecil.
const MAX_HISTORY: usize = 5;

// Poll interval dari config hanya digunakan sebagai upper/lower
// scheduling hint, bukan sebagai alasan melakukan busy polling.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

// Error recovery backoff.
const ERROR_RETRY_MIN: Duration = Duration::from_secs(10);
const ERROR_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

// ============================================================
// SAMPLE
// ============================================================

#[derive(Clone, Debug)]
struct Sample {
    capacity: f32,
    temp: f32,
    power_state: reader::PowerState,
    ts: Instant,
}

// ============================================================
// OPERATING MODE
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatingMode {
    Normal,
    Bypass,
}

// ============================================================
// POLICY
// ============================================================

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

// ============================================================
// ADAPTIVE SCHEDULER
// ============================================================

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
    fn new(
        limit: u8,
        thermal_cutoff_dc: i32,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            limit: limit.min(100) as f32,
            thermal_cutoff: thermal_cutoff_dc as f32 / 10.0,

            configured_interval:
                Self::normalize_configured_interval(
                    poll_interval_secs,
                ),

            history: VecDeque::with_capacity(MAX_HISTORY),

            ema_cap_rate: 0.0,
            ema_temp_rate: 0.0,

            last_interval: STABLE_HEARTBEAT,
        }
    }

    fn normalize_configured_interval(
        seconds: u64,
    ) -> Duration {
        let requested = if seconds == 0 {
            DEFAULT_POLL_INTERVAL
        } else {
            Duration::from_secs(seconds)
        };

        requested.clamp(
            Duration::from_secs(10),
            MAX_INTERVAL,
        )
    }

    fn update_config(
        &mut self,
        limit: u8,
        thermal_cutoff_dc: i32,
        poll_interval_secs: u64,
    ) {
        self.limit = limit.min(100) as f32;
        self.thermal_cutoff =
            thermal_cutoff_dc as f32 / 10.0;

        self.configured_interval =
            Self::normalize_configured_interval(
                poll_interval_secs,
            );

        // Config baru hanya boleh memendekkan interval.
        if self.last_interval
            > self.configured_interval
        {
            self.last_interval =
                self.configured_interval;
        }
    }

    fn reset(&mut self) {
        self.history.clear();

        self.ema_cap_rate = 0.0;
        self.ema_temp_rate = 0.0;

        self.last_interval = STABLE_HEARTBEAT;
    }

    fn push_sample(&mut self, sample: Sample) {
        if let Some(prev) = self.history.back() {
            let dt =
                (sample.ts - prev.ts).as_secs_f32();

            // Duplicate/burst event.
            if dt < 0.5 {
                self.history.push_back(sample);

                if self.history.len()
                    > MAX_HISTORY
                {
                    self.history.pop_front();
                }

                return;
            }

            // Device sleep / long suspend recovery.
            if dt > 300.0 {
                self.reset();

                self.history.push_back(sample);

                return;
            }

            let capacity_delta =
                sample.capacity - prev.capacity;

            let capacity_rate =
                capacity_delta.abs() / dt.max(0.1);

            // Reject impossible SOC jumps.
            if capacity_rate <= 1.0 {
                let rate =
                    capacity_delta / dt;

                let new_cap_rate =
                    EMA_ALPHA * rate
                        + (1.0 - EMA_ALPHA)
                            * self.ema_cap_rate;

                if new_cap_rate.is_finite() {
                    self.ema_cap_rate =
                        new_cap_rate;
                }

                let temp_delta =
                    sample.temp - prev.temp;

                let temp_rate =
                    temp_delta / dt;

                let new_temp_rate =
                    EMA_ALPHA * temp_rate
                        + (1.0 - EMA_ALPHA)
                            * self.ema_temp_rate;

                if new_temp_rate.is_finite() {
                    self.ema_temp_rate =
                        new_temp_rate;
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
        thermal_enabled: bool,
        operating_mode: OperatingMode,
    ) -> Duration {
        let sample =
            match self.history.back() {
                Some(v) => v,
                None => {
                    return self.configured_interval
                        .min(STABLE_HEARTBEAT);
                }
            };

        // --------------------------------------------------------
        // UNPLUGGED
        // --------------------------------------------------------

        if sample.power_state
            == reader::PowerState::Disconnected
        {
            self.last_interval =
                UNPLUGGED_HEARTBEAT;

            return self.last_interval;
        }

        // --------------------------------------------------------
        // ATTACHED
        // --------------------------------------------------------

        if sample.power_state
            == reader::PowerState::Attached
        {
            self.last_interval =
                ATTACHED_HEARTBEAT;

            return self.last_interval;
        }

        // --------------------------------------------------------
        // BYPASS
        // --------------------------------------------------------

        if operating_mode
            == OperatingMode::Bypass
        {
            self.last_interval =
                BYPASS_HEARTBEAT;

            return self.last_interval;
        }

        let dist_limit =
            (self.limit - sample.capacity)
                .max(0.0);

        let dist_thermal =
            (self.thermal_cutoff - sample.temp)
                .max(0.0);

        // --------------------------------------------------------
        // CRITICAL CHARGE LIMIT
        // --------------------------------------------------------

        if !limit_blocked
            && dist_limit <= 0.5
        {
            self.last_interval =
                CRITICAL_LIMIT_HEARTBEAT;

            return self.last_interval;
        }

        if !limit_blocked
            && dist_limit <= 2.0
        {
            self.last_interval =
                NEAR_LIMIT_HEARTBEAT;

            return self.last_interval;
        }

        // --------------------------------------------------------
        // THERMAL
        // --------------------------------------------------------

        if thermal_enabled {
            if thermal_blocked {
                self.last_interval =
                    THERMAL_BLOCKED_HEARTBEAT;

                return self.last_interval;
            }

            if dist_thermal <= 3.0
                || self.ema_temp_rate > 0.15
            {
                self.last_interval =
                    THERMAL_HEARTBEAT;

                return self.last_interval;
            }
        }

        // --------------------------------------------------------
        // BLOCKED STATES
        // --------------------------------------------------------

        if limit_blocked {
            self.last_interval =
                LIMIT_BLOCKED_HEARTBEAT;

            return self.last_interval;
        }

        // --------------------------------------------------------
        // PREDICTIVE SAFETY
        // --------------------------------------------------------

        let mut target =
            STABLE_HEARTBEAT;

        if sample.power_state
            == reader::PowerState::Charging
            && self.ema_cap_rate > 0.01
            && dist_limit > 0.0
        {
            let seconds =
                dist_limit
                    / self.ema_cap_rate;

            let predicted =
                Duration::from_secs_f32(
                    (seconds * 0.5)
                        .max(
                            NEAR_LIMIT_HEARTBEAT
                                .as_secs_f32(),
                        ),
                );

            target =
                predicted.clamp(
                    NEAR_LIMIT_HEARTBEAT,
                    MAX_INTERVAL,
                );
        }

        // Config interval masih dihormati sebagai
        // batas minimum safety heartbeat.
        //
        // Tetapi kita tidak menggunakan interval
        // 2-10 detik secara default karena itu buruk
        // untuk deep sleep.
        let configured =
            self.configured_interval
                .max(Duration::from_secs(30));

        target = target
            .max(configured)
            .min(MAX_INTERVAL);

        // Jangan langsung melompat dari 15 menit
        // ke interval yang terlalu panjang.
        if target < self.last_interval {
            self.last_interval = target;
            return target;
        }

        self.last_interval =
            self.last_interval
                .mul_f32(1.5)
                .max(configured)
                .min(target)
                .min(MAX_INTERVAL);

        self.last_interval
    }
}

// ============================================================
// NETLINK FD
// ============================================================

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

// ============================================================
// NETLINK EVENT
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetlinkEvent {
    None,
    FastPath,
    Coalesce,
}

// ============================================================
// CREATE NETLINK
// ============================================================

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

    // Besarkan RX buffer supaya burst uevent tidak
    // gampang overflow.
    let rcvbuf: libc::c_int = 64 * 1024;

    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of_val(&rcvbuf)
                as libc::socklen_t,
        );
    }

    let mut addr:
        libc::sockaddr_nl =
        unsafe { std::mem::zeroed() };

    addr.nl_family =
        libc::AF_NETLINK as libc::sa_family_t;

    addr.nl_pid = 0;

    addr.nl_groups = 1;

    let result = unsafe {
        libc::bind(
            fd,
            &addr as *const _
                as *const libc::sockaddr,
            std::mem::size_of::<
                libc::sockaddr_nl,
            >() as libc::socklen_t,
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

// ============================================================
// DRAIN NETLINK
// ============================================================

fn drain_and_parse_netlink(
    fd: RawFd,
) -> NetlinkEvent {
    let mut buf = [0u8; 8192];

    let mut result =
        NetlinkEvent::None;

    loop {
        let received = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr()
                    as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };

        if received <= 0 {
            break;
        }

        let data =
            &buf[..received as usize];

        let mut is_power_supply = false;
        let mut name: &[u8] = b"";

        for part in data.split(|b| *b == 0) {
            if part
                == b"SUBSYSTEM=power_supply"
            {
                is_power_supply = true;
            } else if part.starts_with(
                b"POWER_SUPPLY_NAME=",
            ) {
                name =
                    &part[
                        b"POWER_SUPPLY_NAME="
                            .len()..
                    ];
            }
        }

        if !is_power_supply {
            continue;
        }

        let mut fast = false;

        for part in data.split(|b| *b == 0) {
            if name == b"ac"
                && part.starts_with(
                    b"POWER_SUPPLY_ONLINE=",
                )
            {
                fast = true;
            }

            if name == b"battery"
                && (
                    part.starts_with(
                        b"POWER_SUPPLY_STATUS=",
                    )
                    || part.starts_with(
                        b"POWER_SUPPLY_CAPACITY=",
                    )
                    || part.starts_with(
                        b"POWER_SUPPLY_TEMP=",
                    )
                )
            {
                fast = true;
            }

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
            result =
                NetlinkEvent::FastPath;

            continue;
        }

        if result
            == NetlinkEvent::None
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
            result =
                NetlinkEvent::Coalesce;
        }
    }

    result
}

// ============================================================
// HARDWARE VERIFICATION
// ============================================================

fn verify_hardware_state(
    expected: control::ActualHardwareMode,
) -> bool {
    let actual =
        control::get_actual_charging_state();

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

// ============================================================
// APPLY CHARGING
// ============================================================

fn apply_charging_state(
    enable: bool,
    applied_state:
        &mut control::ActualHardwareMode,
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
                *applied_state =
                    expected;

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

// ============================================================
// APPLY BYPASS
// ============================================================

fn apply_bypass_state(
    expected:
        control::ActualHardwareMode,
    applied_state:
        &mut control::ActualHardwareMode,
) -> bool {
    match control::enter_bypass_mode() {
        Ok(()) => {
            if verify_hardware_state(expected) {
                *applied_state =
                    expected;

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

// ============================================================
// POLICY
// ============================================================

fn evaluate_policy(
    power_state: reader::PowerState,
    level: f32,
    temp_dc: i32,
    previous: PolicyState,
    cfg: &Config,
) -> PolicyState {
    if power_state
        == reader::PowerState::Disconnected
    {
        return PolicyState::clear();
    }

    let mut thermal_blocked =
        previous.thermal_blocked;

    let mut limit_blocked =
        previous.limit_blocked;

    if !power_state.is_plugged_in() {
        return PolicyState {
            thermal_blocked,
            limit_blocked,
        };
    }

    // --------------------------------------------------------
    // THERMAL
    // --------------------------------------------------------

    if !cfg.thermal_cutoff {
        thermal_blocked = false;
    } else {
        let cutoff =
            cfg.max_temp_dc;

        let resume =
            cutoff.saturating_sub(
                THERMAL_HYSTERESIS_DC,
            );

        if temp_dc >= cutoff {
            thermal_blocked = true;
        } else if previous.thermal_blocked
            && temp_dc <= resume
        {
            thermal_blocked = false;
        }
    }

    // --------------------------------------------------------
    // CHARGE LIMIT
    // --------------------------------------------------------

    let limit =
        cfg.charge_limit.min(100) as f32;

    if level >= limit {
        limit_blocked = true;
    } else if previous.limit_blocked {
        let resume =
            if cfg.resume_limit > 0
                && cfg.resume_limit
                    < cfg.charge_limit
            {
                cfg.resume_limit as f32
            } else {
                cfg.charge_limit
                    .saturating_sub(1)
                    as f32
            };

        // Resume hanya jika SOC <= resume.
        limit_blocked =
            level > resume;
    }

    PolicyState {
        thermal_blocked,
        limit_blocked,
    }
}

// ============================================================
// ERROR BACKOFF
// ============================================================

struct ErrorBackoff {
    current: Duration,
}

impl ErrorBackoff {
    fn new() -> Self {
        Self {
            current: ERROR_RETRY_MIN,
        }
    }

    fn reset(&mut self) {
        self.current =
            ERROR_RETRY_MIN;
    }

    fn next(&mut self) -> Duration {
        let current =
            self.current;

        self.current =
            self.current
                .mul_f32(2.0)
                .min(ERROR_RETRY_MAX);

        current
    }
}

// ============================================================
// IPC
// ============================================================

fn drain_ipc(
    rx: &UnixDatagram,
) -> Option<u8> {
    let mut latest =
        None;

    let mut buf = [0u8; 1];

    loop {
        match rx.recv(&mut buf) {
            Ok(1) => {
                // Ambil command terakhir.
                //
                // Ini penting jika beberapa config reload
                // masuk sekaligus.
                latest = Some(buf[0]);
            }

            Ok(_) => {}

            Err(e)
                if e.kind()
                    == std::io::ErrorKind::WouldBlock =>
            {
                break;
            }

            Err(_) => {
                break;
            }
        }
    }

    latest
}

// ============================================================
// MAIN MONITOR
// ============================================================

pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    rx: UnixDatagram,
) {
    tracing::info!(
        "Monitor loop started (event-driven + deep-sleep scheduler)"
    );

    // --------------------------------------------------------
    // INITIAL CONFIG
    // --------------------------------------------------------

    let (
        initial_limit,
        initial_temp,
        initial_poll,
    ) = {
        let cfg =
            config.read()
                .unwrap_or_else(
                    |e| e.into_inner(),
                );

        (
            cfg.charge_limit,
            cfg.max_temp_dc,
            cfg.poll_interval_secs,
        )
    };

    // --------------------------------------------------------
    // IPC NONBLOCKING
    // --------------------------------------------------------

    if let Err(e) =
        rx.set_nonblocking(true)
    {
        tracing::warn!(
            "Failed to set IPC nonblocking: {}",
            e
        );
    }

    // --------------------------------------------------------
    // NETLINK
    // --------------------------------------------------------

    let nl_fd =
        create_netlink_socket()
            .unwrap_or(-1);

    let _nl_guard =
        NetlinkFd(nl_fd);

    if nl_fd >= 0 {
        tracing::info!(
            "NETLINK_KOBJECT_UEVENT ready"
        );
    } else {
        tracing::warn!(
            "Netlink unavailable; safety heartbeat will be used"
        );
    }

    // --------------------------------------------------------
    // SCHEDULER
    // --------------------------------------------------------

    let mut scheduler =
        AdaptiveScheduler::new(
            initial_limit,
            initial_temp,
            initial_poll,
        );

    // --------------------------------------------------------
    // STATE
    // --------------------------------------------------------

    let mut last_eval_time =
        Instant::now()
            - Duration::from_secs(60);

    let mut force_next_eval =
        true;

    let mut pending_netlink_eval =
        false;

    let mut applied_state =
        control::ActualHardwareMode::Unknown;

    let mut operating_mode =
        OperatingMode::Normal;

    let mut policy_state =
        PolicyState::clear();

    let mut error_backoff =
        ErrorBackoff::new();

    // --------------------------------------------------------
    // MAIN LOOP
    // --------------------------------------------------------

    loop {
        let cfg = config
            .read()
            .unwrap_or_else(
                |e| e.into_inner(),
            )
            .clone();

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
                            "Failed exiting BYPASS while disabled: {}",
                            e
                        );
                    }
                }

                if apply_charging_state(
                    true,
                    &mut applied_state,
                ) {
                    tracing::info!(
                        "Daemon disabled: charging restored"
                    );
                }
            }

            operating_mode =
                OperatingMode::Normal;

            policy_state =
                PolicyState::clear();

            scheduler.reset();

            // -------------------------------------------------
            // SLEEP FOREVER UNTIL IPC
            // -------------------------------------------------

            let mut pfd =
                libc::pollfd {
                    fd: rx.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };

            let ret = unsafe {
                libc::poll(
                    &mut pfd,
                    1,
                    -1,
                )
            };

            if ret < 0 {
                continue;
            }

            if pfd.revents
                & libc::POLLIN
                != 0
            {
                if let Some(command) =
                    drain_ipc(&rx)
                {
                    match command {
                        2 => {
                            tracing::info!(
                                "Monitor loop shutting down"
                            );

                            break;
                        }

                        1 => {
                            tracing::info!(
                                "Config reloaded while disabled"
                            );

                            force_next_eval =
                                true;
                        }

                        _ => {}
                    }
                }
            }

            continue;
        }

        // =====================================================
        // CALCULATE WAIT
        // =====================================================

        let mut timeout =
            scheduler.next_interval(
                policy_state.limit_blocked,
                policy_state.thermal_blocked,
                cfg.thermal_cutoff,
                operating_mode,
            );

        if force_next_eval {
            timeout =
                Duration::ZERO;
        } else if pending_netlink_eval {
            let elapsed =
                last_eval_time.elapsed();

            if elapsed
                >= NETLINK_COALESCE
            {
                timeout =
                    Duration::ZERO;
            } else {
                timeout = timeout.min(
                    NETLINK_COALESCE
                        - elapsed,
                );
            }
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
                events: if nl_fd >= 0 {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
        ];

        let nfds =
            if nl_fd >= 0 {
                2
            } else {
                1
            };

        let timeout_ms =
            timeout
                .as_millis()
                .min(i32::MAX as u128)
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

                // Jangan retry agresif.
                std::thread::sleep(
                    ERROR_RETRY_MIN,
                );
            }

            continue;
        }

        let mut needs_evaluation =
            ret == 0;

        // =====================================================
        // IPC
        // =====================================================

        if ret > 0
            && pfds[0].revents
                & libc::POLLIN
                != 0
        {
            if let Some(command) =
                drain_ipc(&rx)
            {
                match command {
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

                        force_next_eval =
                            true;

                        needs_evaluation =
                            true;
                    }

                    // bypass ON
                    3 => {
                        tracing::info!(
                            "Bypass mode enabled via IPC"
                        );

                        operating_mode =
                            OperatingMode::Bypass;

                        force_next_eval =
                            true;

                        needs_evaluation =
                            true;
                    }

                    // bypass OFF
                    4 => {
                        tracing::info!(
                            "Bypass mode disabled via IPC"
                        );

                        operating_mode =
                            OperatingMode::Normal;

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

                        force_next_eval =
                            true;

                        needs_evaluation =
                            true;
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
            && pfds[1].revents
                & libc::POLLIN
                != 0
        {
            match drain_and_parse_netlink(
                nl_fd,
            ) {
                NetlinkEvent::FastPath => {
                    needs_evaluation =
                        true;

                    pending_netlink_eval =
                        false;
                }

                NetlinkEvent::Coalesce => {
                    pending_netlink_eval =
                        true;

                    if last_eval_time
                        .elapsed()
                        >= NETLINK_COALESCE
                    {
                        needs_evaluation =
                            true;
                    }
                }

                NetlinkEvent::None => {}
            }
        }

        // =====================================================
        // COALESCED EVENT
        // =====================================================

        if pending_netlink_eval
            && last_eval_time
                .elapsed()
                >= NETLINK_COALESCE
        {
            needs_evaluation =
                true;
        }

        if !needs_evaluation {
            continue;
        }

        force_next_eval =
            false;

        pending_netlink_eval =
            false;

        // =====================================================
        // READ BATTERY
        // =====================================================

        let level =
            match reader::read_capacity_raw()
            {
                Ok(value)
                    if value.is_finite() =>
                {
                    value.clamp(
                        0.0,
                        100.0,
                    )
                }

                Ok(value) => {
                    tracing::error!(
                        "Battery capacity non-finite: {}",
                        value
                    );

                    let delay =
                        error_backoff.next();

                    last_eval_time =
                        Instant::now();

                    std::thread::sleep(
                        delay,
                    );

                    continue;
                }

                Err(e) => {
                    tracing::error!(
                        "Failed reading capacity: {}",
                        e
                    );

                    last_eval_time =
                        Instant::now();

                    // Fail safe hanya bila memang
                    // ada limit aktif.
                    if cfg.charge_limit
                        < 100
                        && applied_state
                            != control::ActualHardwareMode::ChargingDisabled
                        {
                            let _ =
                                apply_charging_state(
                                    false,
                                    &mut applied_state,
                                );
                        }

                    std::thread::sleep(
                        error_backoff.next(),
                    );

                    continue;
                }
            };

        // =====================================================
        // TEMPERATURE
        // =====================================================

        let temp_dc =
            match reader::read_temperature_dc()
            {
                Ok(value) => value,

                Err(e) => {
                    tracing::error!(
                        "Failed reading temperature: {}",
                        e
                    );

                    last_eval_time =
                        Instant::now();

                    if cfg.thermal_cutoff
                        && applied_state
                            != control::ActualHardwareMode::ChargingDisabled
                        {
                            let _ =
                                apply_charging_state(
                                    false,
                                    &mut applied_state,
                                );
                        }

                    std::thread::sleep(
                        error_backoff.next(),
                    );

                    continue;
                }
            };

        // =====================================================
        // POWER STATE
        // =====================================================

        let power_state =
            match reader::get_power_state()
            {
                Ok(value) => value,

                Err(e) => {
                    tracing::error!(
                        "Failed reading power state: {}",
                        e
                    );

                    last_eval_time =
                        Instant::now();

                    if applied_state
                        != control::ActualHardwareMode::ChargingDisabled
                    {
                        let _ =
                            apply_charging_state(
                                false,
                                &mut applied_state,
                            );
                    }

                    std::thread::sleep(
                        error_backoff.next(),
                    );

                    continue;
                }
            };

        if power_state
            == reader::PowerState::Unknown
        {
            tracing::error!(
                "Power state UNKNOWN"
            );

            last_eval_time =
                Instant::now();

            if applied_state
                != control::ActualHardwareMode::ChargingDisabled
            {
                let _ =
                    apply_charging_state(
                        false,
                        &mut applied_state,
                    );
            }

            std::thread::sleep(
                error_backoff.next(),
            );

            continue;
        }

        // Berhasil membaca semua sumber.
        error_backoff.reset();

        // =====================================================
        // UPDATE SCHEDULER
        // =====================================================

        let now =
            Instant::now();

        scheduler.push_sample(
            Sample {
                capacity: level,
                temp:
                    temp_dc as f32
                        / 10.0,
                power_state,
                ts: now,
            },
        );

        // =====================================================
        // HARDWARE STATE
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
        // BYPASS
        // =====================================================

        if operating_mode
            == OperatingMode::Bypass
        {
            let expected =
                if control::has_distinct_bypass_node()
                {
                    control::ActualHardwareMode::Bypass
                } else {
                    control::ActualHardwareMode::ChargingDisabled
                };

            if applied_state != expected
                && apply_bypass_state(
                    expected,
                    &mut applied_state,
                ) {
                    tracing::info!(
                        "Hardware BYPASS applied and verified"
                    );
                }

            last_eval_time =
                Instant::now();

            continue;
        }

        // =====================================================
        // UNPLUGGED
        // =====================================================

        if power_state
            == reader::PowerState::Disconnected
        {
            policy_state =
                PolicyState::clear();

            scheduler.reset();

            if applied_state
                != control::ActualHardwareMode::ChargingEnabled
                && apply_charging_state(
                    true,
                    &mut applied_state,
                ) {
                    tracing::info!(
                        "Charger disconnected: charging restored"
                    );
                }

            last_eval_time =
                Instant::now();

            continue;
        }

        // =====================================================
        // POLICY
        // =====================================================

        let previous_policy =
            policy_state;

        policy_state =
            evaluate_policy(
                power_state,
                level,
                temp_dc,
                policy_state,
                &cfg,
            );

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
        // POLICY CHANGE
        // =====================================================

        if previous_policy
            != policy_state
        {
            tracing::info!(
                "Policy changed | SOC={:.2}% | limit {} -> {} | thermal {} -> {}",
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

        if applied_state
            != desired_state
        {
            if applied_state
                == control::ActualHardwareMode::Bypass
            {
                match control::exit_bypass_mode()
                {
                    Ok(()) => {
                        applied_state =
                            control::ActualHardwareMode::Unknown;

                        tracing::info!(
                            "Exited BYPASS before normal policy"
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
                        "Charging ON | SOC={:.2}% | Temp={:.1}C",
                        level,
                        temp_dc as f32
                            / 10.0
                    );
                } else if policy_state
                    .limit_blocked
                {
                    let resume =
                        if cfg.resume_limit
                            > 0
                            && cfg.resume_limit
                                < cfg.charge_limit
                        {
                            cfg.resume_limit
                        } else {
                            cfg.charge_limit
                                .saturating_sub(
                                    1,
                                )
                        };

                    tracing::warn!(
                        "Charging OFF by limit | SOC={:.2}% | limit={} | resume={} | Temp={:.1}C",
                        level,
                        cfg.charge_limit,
                        resume,
                        temp_dc as f32
                            / 10.0
                    );
                } else if policy_state
                    .thermal_blocked
                {
                    tracing::warn!(
                        "Charging OFF by thermal protection | SOC={:.2}% | Temp={:.1}C | cutoff={:.1}C",
                        level,
                        temp_dc as f32
                            / 10.0,
                        cfg.max_temp_dc as f32
                            / 10.0
                    );
                }
            }
        }

        // =====================================================
        // FINAL STATE
        // =====================================================
        //
        // Jangan lakukan read-back ekstra setiap evaluasi.
        //
        // apply_charging_state() sudah verify.
        //
        // Kita hanya mempertahankan state yang sudah
        // diverifikasi agar tidak melakukan sysfs read
        // tambahan setiap wake-up.
        //

        last_eval_time =
            Instant::now();
    }
}