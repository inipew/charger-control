use crate::{hardware::profile::*, error::ChargerError};
use std::{fs, path::Path};

/// Status of the battery from sysfs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryStatus {
    Charging,
    Discharging,
    NotCharging,
    Full,
    Unknown,
}

/// Read a sysfs node as a raw String, trimmed.
pub fn read_sysfs(path: &Path) -> Result<String, ChargerError> {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| ChargerError::SysfsRead {
            path: path.to_owned(),
            source: e,
        })
}

/// Read battery level (0..=100) from sysfs capacity node.
pub fn read_capacity() -> Result<u8, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/capacity");
    read_sysfs(path)?
        .parse::<u8>()
        .map_err(|_| ChargerError::ParseError("capacity"))
}

/// Read current in mA from first available node based on priority.
/// Returns signed i32 (negative = discharging).
pub fn read_current_ma(profile: &crate::hardware::profile::HardwareProfile) -> Result<i32, ChargerError> {
    let mut best_val: Option<i32> = None;
    let mut highest_prio: Option<u8> = None;

    for node in profile.sensor.current_nodes {
        if let Ok(raw) = read_sysfs(Path::new(node.path)) {
            if let Ok(value) = raw.parse::<i64>() {
                if value == 0 {
                    continue;
                }

                let ma = match node.unit {
                    CurrentUnit::MicroAmp => (value / 1000) as i32,
                    CurrentUnit::MilliAmp => value as i32,
                };

                let better = highest_prio.map(|p| node.priority > p).unwrap_or(true);

                if better {
                    best_val = Some(ma);
                    highest_prio = Some(node.priority);
                }
            }
        }
    }

    best_val.ok_or(ChargerError::ParseError("No valid current reading found"))
}

pub fn read_voltage_uv() -> Result<u32, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/voltage_now");
    read_sysfs(path)?
        .parse::<u32>()
        .map_err(|_| ChargerError::ParseError("voltage_now"))
}

pub fn read_temperature_dc() -> Result<i32, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/temp");
    read_sysfs(path)?
        .parse::<i32>()
        .map_err(|_| ChargerError::ParseError("temp"))
}

pub fn read_charge_full_design() -> Result<u32, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/charge_full_design",
        "/sys/class/power_supply/bms/charge_full_design",
        "/sys/class/power_supply/battery/capacity_design_uah",
    ];
    for p in paths {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<u32>() {
                if val > 0 {
                    let mah = if val > 100_000 { val / 1000 } else { val };
                    return Ok(mah);
                }
            }
        }
    }
    Err(ChargerError::ParseError("charge_full_design"))
}

pub fn read_cycle_count() -> Result<u32, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/cycle_count",
        "/sys/class/power_supply/bms/cycle_count",
        "/sys/class/power_supply/main/cycle_count",
    ];
    for p in paths {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if let Ok(val) = raw.parse::<u32>() {
                if val > 0 {
                    return Ok(val);
                }
            }
        }
    }
    Err(ChargerError::ParseError("cycle_count"))
}

pub fn read_technology() -> Result<String, ChargerError> {
    let paths = [
        "/sys/class/power_supply/battery/technology",
        "/sys/class/power_supply/battery/type",
        "/sys/class/power_supply/bms/battery_type",
    ];
    for p in paths {
        if let Ok(raw) = read_sysfs(Path::new(p)) {
            if !raw.is_empty() {
                return Ok(raw);
            }
        }
    }
    Ok("Li-ion".to_string())
}

pub fn calc_wattage_w(voltage_uv: u32, current_ma: i32) -> f32 {
    (voltage_uv as f32 / 1_000_000.0) * (current_ma as f32 / 1000.0)
}

pub fn is_plugged_in() -> Result<bool, ChargerError> {
    let mut found_online_node = false;

    if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };

            let lower = name.to_ascii_lowercase();
            if lower.contains("battery") || lower.contains("bms") {
                continue;
            }

            let online_path = entry.path().join("online");
            if !online_path.exists() {
                continue;
            }

            found_online_node = true;

            if let Ok(value) = read_sysfs(&online_path) {
                if value == "1" {
                    return Ok(true);
                }
            }
        }
    }

    if found_online_node {
        Ok(false)
    } else {
        Err(ChargerError::NoChargingNodeFound)
    }
}

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};
use std::sync::Arc;

// ============================================================================
// CachedReader
// ============================================================================

const BATTERY_RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const CURRENT_RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const ONLINE_RESCAN_INTERVAL: Duration = Duration::from_secs(5);

const READ_BUFFER_SIZE: usize = 64;

struct BatteryFd {
    path: &'static str,
    file: Option<File>,
}

struct CurrentFd {
    config: CurrentNodeConfig,
    file: File,
}

/// Hasil pembacaan online node sysfs ("1"/"0").
/// Membedakan antara "node tidak terkonfigurasi" vs "node error saat dibaca".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineReading {
    /// Tidak ada online node yang dikonfigurasi di profile.
    /// Boleh fallback ke input_current sebagai sinyal presence.
    Unavailable,
    /// Node terbaca dengan nilai valid.
    Online,
    /// Node terbaca, tidak ada yang bernilai "1".
    Offline,
    /// Node terkonfigurasi tetapi gagal dibaca (I/O error, stale FD).
    /// Jangan fallback ke input_current jika ini terjadi.
    Error,
}

/// Sinyal mentah kehadiran charger sebelum diproses PresenceTracker.
/// Reader mengumpulkan kedua sinyal ini; PresenceTracker yang menginterpretasinya.
pub struct PresenceReading {
    /// Arus input aktual (mA). None = sensor tidak tersedia/error.
    /// None ≠ Offline; None berarti pembacaan gagal.
    pub input_current_ma: Option<i32>,
    /// Status online dari online node yang terkonfigurasi.
    /// `Unavailable` = tidak ada node dikonfigurasi → boleh fallback ke input_current.
    /// `Error` = node ada tapi gagal dibaca → jangan berpura-pura node tidak ada.
    pub online: OnlineReading,
}

pub struct CachedReader {
    profile: Arc<crate::hardware::profile::HardwareProfile>,
    clock: Arc<dyn crate::time::Clock>,

    capacity: BatteryFd,
    temperature: BatteryFd,
    status: BatteryFd,

    /*
     * IMPORTANT:
     *
     * Do not cache a single "best" current node.
     *
     * Android vendor kernels may expose multiple current nodes:
     *
     *   battery/current_now
     *   bms/current_now
     *   main/current_now
     *   usb/current_now
     *
     * and the active node can change after reconnect/restart.
     * All nodes are kept in one collection and filtered by CurrentRole.
     */
    current_fds: Vec<CurrentFd>,

    /*
     * Online nodes from PresenceProfile.online_nodes.
     * These are "1"/"0" sysfs nodes used as primary presence signal
     * (higher priority than input_current).
     */
    online_fds: Vec<File>,

    buf: [u8; READ_BUFFER_SIZE],

    next_battery_rescan: Instant,
    next_current_rescan: Instant,
    next_online_rescan: Instant,
}

impl CachedReader {
    pub fn new(profile: Arc<crate::hardware::profile::HardwareProfile>, clock: Arc<dyn crate::time::Clock>) -> Self {
        let capacity_path = profile.sensor.capacity_path;
        let temperature_path = profile.sensor.temperature_path;
        let status_path = profile.sensor.status_path;

        let mut reader = Self {
            profile,
            clock: clock.clone(),
            capacity: BatteryFd { path: capacity_path, file: None },
            temperature: BatteryFd { path: temperature_path, file: None },
            status: BatteryFd { path: status_path, file: None },

            current_fds: Vec::new(),
            online_fds: Vec::new(),

            buf: [0; READ_BUFFER_SIZE],

            next_battery_rescan: clock.now(),
            next_current_rescan: clock.now(),
            next_online_rescan: clock.now(),
        };

        reader.rescan_battery_nodes();
        reader.rescan_current_nodes();
        reader.rescan_online_nodes();

        reader
    }

    // ========================================================================
    // Rescan
    // ========================================================================

    pub fn invalidate_battery_fds(&mut self) {
        self.capacity.file = None;
        self.temperature.file = None;
        self.status.file = None;
        // Also force a rescan immediately
        self.next_battery_rescan = self.clock.now();
    }

    fn rescan_battery_nodes(&mut self) {
        if self.capacity.file.is_none() {
            self.capacity.file = File::open(self.capacity.path).ok();
        }
        if self.temperature.file.is_none() {
            self.temperature.file = File::open(self.temperature.path).ok();
        }
        if self.status.file.is_none() {
            self.status.file = File::open(self.status.path).ok();
        }
        self.next_battery_rescan = self.clock.now() + BATTERY_RESCAN_INTERVAL;
    }

    fn rescan_current_nodes(&mut self) {
        /*
         * Rebuilding this Vec is intentionally outside normal polling.
         *
         * This is the important property:
         *
         *     normal poll -> no File::open()
         *
         * Only this periodic maintenance path opens nodes.
         */
        self.current_fds.clear();

        for config in self.profile.sensor.current_nodes {
            match File::open(config.path) {
                Ok(file) => {
                    self.current_fds.push(CurrentFd { config: *config, file });
                }
                Err(e) => {
                    tracing::trace!(
                        "Current node unavailable: {}: {}",
                        config.path,
                        e
                    );
                }
            }
        }

        self.next_current_rescan = self.clock.now() + CURRENT_RESCAN_INTERVAL;
    }

    fn rescan_online_nodes(&mut self) {
        self.online_fds.clear();

        for path in self.profile.sensor.presence.online_nodes {
            match File::open(path) {
                Ok(file) => {
                    self.online_fds.push(file);
                }
                Err(e) => {
                    tracing::trace!(
                        "Online node unavailable: {}: {}",
                        path,
                        e
                    );
                }
            }
        }

        self.next_online_rescan = self.clock.now() + ONLINE_RESCAN_INTERVAL;
    }

    #[inline]
    fn maybe_rescan_nodes(&mut self) {
        let now = self.clock.now();

        if now >= self.next_battery_rescan {
            self.rescan_battery_nodes();
        }

        if now >= self.next_current_rescan {
            self.rescan_current_nodes();
        }

        if now >= self.next_online_rescan {
            self.rescan_online_nodes();
        }
    }

    // ========================================================================
    // Generic cached FD reader
    // ========================================================================

    fn read_file<'a>(
        file: &mut File,
        buf: &'a mut [u8],
        node_name: &'static str,
    ) -> Result<&'a str, ChargerError> {
        file.seek(SeekFrom::Start(0))
            .map_err(|e| ChargerError::SysfsRead {
                path: Path::new(node_name).to_path_buf(),
                source: e,
            })?;

        let n = file
            .read(buf)
            .map_err(|e| ChargerError::SysfsRead {
                path: Path::new(node_name).to_path_buf(),
                source: e,
            })?;

        std::str::from_utf8(&buf[..n])
            .map(str::trim)
            .map_err(|_| ChargerError::ParseError(node_name))
    }

    // ========================================================================
    // Capacity
    // ========================================================================

    pub fn read_capacity(&mut self) -> Result<u8, ChargerError> {
        let file = self.capacity.file.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new(self.capacity.path).to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "capacity FD not open",
                ),
            }
        })?;

        let s = match Self::read_file(file, &mut self.buf, "capacity") {
            Ok(s) => s,
            Err(e) => {
                self.capacity.file = None;
                return Err(e);
            }
        };

        let value = s.parse::<u8>().map_err(|_| ChargerError::ParseError("capacity"))?;

        if value > 100 {
            return Err(ChargerError::ParseError("capacity out of bounds"));
        }

        Ok(value)
    }

    // ========================================================================
    // Temperature
    // ========================================================================

    pub fn read_temperature_dc(&mut self) -> Result<i32, ChargerError> {
        let file = self.temperature.file.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new(self.temperature.path).to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "temperature FD not open",
                ),
            }
        })?;

        let s = match Self::read_file(file, &mut self.buf, "temp") {
            Ok(s) => s,
            Err(e) => {
                self.temperature.file = None;
                return Err(e);
            }
        };

        let value = s.parse::<i32>().map_err(|_| ChargerError::ParseError("temp"))?;

        if !( -400..=1000 ).contains(&value) {
            return Err(ChargerError::ParseError("temperature out of bounds"));
        }

        Ok(value)
    }

    // ========================================================================
    // Current
    // ========================================================================

    // ========================================================================
    // Current (by role)
    // ========================================================================

    /// Internal helper: baca arus berdasarkan role (Battery atau Input).
    /// Mengembalikan nilai dari node dengan priority tertinggi untuk role tersebut.
    fn read_current_role(&mut self, role: crate::hardware::profile::CurrentRole) -> Result<i32, ChargerError> {

        self.maybe_rescan_nodes();

        let mut best_val: Option<i32> = None;
        let mut highest_prio: Option<u8> = None;
        let mut stale_fd = false;

        for current_fd in &mut self.current_fds {
            if current_fd.config.role != role {
                continue;
            }

            let result = Self::read_file(
                &mut current_fd.file,
                &mut self.buf,
                "current_now",
            );

            let Ok(s) = result else {
                // Node gagal dibaca (stale FD, I/O error) — boleh coba node berikutnya
                stale_fd = true;
                continue;
            };

            let Ok(value) = s.parse::<i64>() else {
                continue;
            };

            let ma = match current_fd.config.unit {
                CurrentUnit::MicroAmp => (value / 1000) as i32,
                CurrentUnit::MilliAmp => value as i32,
            };

            // 0 adalah nilai valid — jangan skip atau fallback ke node priority lebih rendah.
            // Fallback hanya terjadi jika node ini read-error/unavailable.
            let better = highest_prio
                .map(|p| current_fd.config.priority > p)
                .unwrap_or(true);

            if better {
                best_val = Some(ma);
                highest_prio = Some(current_fd.config.priority);
            }
        }

        if let Some(val) = best_val {
            if !( -20000..=20000 ).contains(&val) {
                return Err(ChargerError::ParseError("current out of bounds"));
            }
        }

        if stale_fd {
            tracing::trace!(
                "One or more current FDs became stale; \
                 waiting for scheduled rescan."
            );
        }

        best_val.ok_or(ChargerError::ParseError("No valid current reading found in cache"))
    }

    /// Baca arus baterai aktual (mA). Sign bersifat vendor-specific — jangan dibalik.
    pub fn read_battery_current_ma(&mut self) -> Result<i32, ChargerError> {
        self.read_current_role(crate::hardware::profile::CurrentRole::Battery)
    }

    /// Baca arus input dari charger/USB (mA).
    pub fn read_input_current_ma(&mut self) -> Result<i32, ChargerError> {
        self.read_current_role(crate::hardware::profile::CurrentRole::Input)
    }

    // ========================================================================
    // Online node (untuk PresenceReading)
    // ========================================================================

    /// Baca semua online nodes yang terkonfigurasi di PresenceProfile.
    /// Membedakan antara node tidak dikonfigurasi (Unavailable) vs error pembacaan (Error).
    fn read_online_node(&mut self) -> OnlineReading {
        self.maybe_rescan_nodes();

        if self.online_fds.is_empty() {
            // Tidak ada online node yang terkonfigurasi di profile
            return OnlineReading::Unavailable;
        }

        let mut any_readable = false;
        let mut stale = false;

        for file in &mut self.online_fds {
            if file.seek(SeekFrom::Start(0)).is_err() {
                stale = true;
                continue;
            }
            let n = match file.read(&mut self.buf) {
                Ok(n) => n,
                Err(_) => { stale = true; continue; }
            };
            let Ok(value) = std::str::from_utf8(&self.buf[..n]) else {
                continue;
            };
            any_readable = true;
            if value.trim() == "1" {
                return OnlineReading::Online;
            }
        }

        if stale {
            tracing::trace!("One or more online FDs became stale; waiting for scheduled rescan.");
        }

        if any_readable {
            // Node terbaca tapi tidak ada yang "1"
            OnlineReading::Offline
        } else {
            // Node terkonfigurasi tapi semua stale/error — beda dari Unavailable
            OnlineReading::Error
        }
    }

    /// Kumpulkan semua sinyal presence mentah.
    /// PresenceTracker yang akan menginterpretasikan hasilnya.
    pub fn read_presence(&mut self) -> PresenceReading {
        let online = self.read_online_node();
        // input_current hanya dibaca jika online node tidak authoritative
        let input_current_ma = self.read_input_current_ma().ok();
        PresenceReading { input_current_ma, online }
    }

    // ========================================================================
    // Status
    // ========================================================================

    pub fn read_status(&mut self) -> Result<BatteryStatus, ChargerError> {
        let file = self.status.file.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new(self.status.path).to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "status FD not open",
                ),
            }
        })?;

        let s = match Self::read_file(file, &mut self.buf, "status") {
            Ok(s) => s,
            Err(e) => {
                self.status.file = None;
                return Err(e);
            }
        };

        match s.to_ascii_lowercase().as_str() {
            "charging" => Ok(BatteryStatus::Charging),
            "discharging" => Ok(BatteryStatus::Discharging),
            "not charging" => Ok(BatteryStatus::NotCharging),
            "full" => Ok(BatteryStatus::Full),
            _ => Ok(BatteryStatus::Unknown),
        }
    }
}
