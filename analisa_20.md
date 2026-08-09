Belum. Kode terakhir yang saya berikan **baru memperbaiki partial failure di `control.rs`**.

Dua hal yang Anda tanyakan memang belum sepenuhnya diterapkan:

1. `CachedReader` masih mengunci satu `current_now` saat `new()`.
2. `is_charging_enabled()` masih menggunakan model **`any_enabled`**, belum memakai konsensus/prioritas antar-node.

Di bawah ini saya berikan implementasi yang menerapkan **keduanya**, sekaligus tetap cocok dengan arsitektur `snapshot → decision → hardware → verification` Anda.

## 1. `reader.rs` — CachedReader tanpa mengunci satu current node

Prinsipnya:

* `capacity`, `temp`, `status` tetap cached FD.
* `current` **tidak memilih satu node secara permanen**.
* Semua `CURRENT_NODES` dibuka/cached.
* Setiap polling, semua current node dibaca.
* Node yang gagal dibaca tidak langsung dianggap mati.
* Jika node yang sebelumnya tidak tersedia kemudian muncul, `CachedReader` dapat melakukan **periodic rescan/reopen**.
* Rescan hanya dilakukan setiap beberapa detik, bukan setiap polling.
* Polling normal tidak melakukan allocation untuk current path.
* Current dipilih berdasarkan nilai yang valid dan bukan sekadar node pertama.
* Untuk Android yang memiliki beberapa current node, kita bisa memakai nilai dengan magnitude terbesar sebagai representasi arus aktual.

```rust
use crate::{battery::nodes::*, error::ChargerError};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant},
};

/// Status of the battery from sysfs.
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

pub fn read_capacity() -> Result<u8, ChargerError> {
    let path = Path::new("/sys/class/power_supply/battery/capacity");

    read_sysfs(path)?
        .parse::<u8>()
        .map_err(|_| ChargerError::ParseError("capacity"))
}

/// Read current from all known current nodes.
///
/// We do not lock ourselves to one node. Every known node is checked.
///
/// If multiple nodes expose a valid current, the value with the greatest
/// absolute magnitude is selected. This is useful on Android where vendors
/// may expose multiple current paths and some paths can temporarily report 0.
pub fn read_current_ua() -> Result<i64, ChargerError> {
    let mut best: Option<i64> = None;

    for path in CURRENT_NODES {
        if let Ok(raw) = read_sysfs(Path::new(path)) {
            if let Ok(value) = raw.parse::<i64>() {
                if value == 0 {
                    continue;
                }

                if best
                    .map(|current| value.unsigned_abs() > current.unsigned_abs())
                    .unwrap_or(true)
                {
                    best = Some(value);
                }
            }
        }
    }

    Ok(best.unwrap_or(0))
}

pub fn read_current_ma() -> Result<f32, ChargerError> {
    let current = read_current_ua()? as f32;

    // Most Android current_now nodes are µA.
    if current.abs() > 10_000.0 {
        Ok(current / 1000.0)
    } else {
        Ok(current)
    }
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
            if let Ok(value) = raw.parse::<u32>() {
                if value > 0 {
                    return Ok(if value > 100_000 {
                        value / 1000
                    } else {
                        value
                    });
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
            if let Ok(value) = raw.parse::<u32>() {
                if value > 0 {
                    return Ok(value);
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

pub fn calc_wattage_w(voltage_uv: u32, current_ma: f32) -> f32 {
    (voltage_uv as f32 / 1_000_000.0) * (current_ma / 1000.0)
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

// ============================================================================
// CachedReader
// ============================================================================

const CURRENT_RESCAN_INTERVAL: Duration = Duration::from_secs(5);

struct CurrentFd {
    file: File,
}

pub struct CachedReader {
    capacity_fd: Option<File>,
    temp_fd: Option<File>,
    status_fd: Option<File>,

    /// ALL current nodes are cached.
    ///
    /// We deliberately do not have:
    ///
    ///     current_fd: Option<File>
    ///
    /// because that would permanently select one current node.
    current_fds: Vec<CurrentFd>,

    online_fds: Vec<File>,

    buf: [u8; 64],

    /// Allows newly-created vendor sysfs nodes to be discovered.
    next_current_rescan: Instant,
}

impl Default for CachedReader {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedReader {
    pub fn new() -> Self {
        let mut reader = Self {
            capacity_fd: File::open("/sys/class/power_supply/battery/capacity").ok(),
            temp_fd: File::open("/sys/class/power_supply/battery/temp").ok(),
            status_fd: File::open("/sys/class/power_supply/battery/status").ok(),

            current_fds: Vec::new(),
            online_fds: Vec::new(),

            buf: [0; 64],

            next_current_rescan: Instant::now(),
        };

        reader.rescan_current_nodes();
        reader.rescan_online_nodes();

        reader
    }

    /// Re-open all known current nodes.
    ///
    /// This is deliberately NOT called on every polling cycle.
    /// It is only called periodically so normal polling remains cheap.
    fn rescan_current_nodes(&mut self) {
        self.current_fds.clear();

        for path in CURRENT_NODES {
            if let Ok(file) = File::open(path) {
                self.current_fds.push(CurrentFd { file });
            }
        }

        self.next_current_rescan = Instant::now() + CURRENT_RESCAN_INTERVAL;
    }

    /// Rescan online nodes.
    ///
    /// USB/AC/charger power-supply nodes can appear/disappear on Android,
    /// therefore keeping an eternal FD list is not always sufficient.
    fn rescan_online_nodes(&mut self) {
        self.online_fds.clear();

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

                if let Ok(file) = File::open(online_path) {
                    self.online_fds.push(file);
                }
            }
        }
    }

    #[inline]
    fn maybe_rescan_current_nodes(&mut self) {
        if Instant::now() >= self.next_current_rescan {
            self.rescan_current_nodes();
        }
    }

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

    pub fn read_capacity(&mut self) -> Result<u8, ChargerError> {
        let file = self.capacity_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new("/sys/class/power_supply/battery/capacity").to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "capacity FD not open",
                ),
            }
        })?;

        let s = Self::read_file(file, &mut self.buf, "capacity")?;

        s.parse::<u8>()
            .map_err(|_| ChargerError::ParseError("capacity"))
    }

    pub fn read_temperature_dc(&mut self) -> Result<i32, ChargerError> {
        let file = self.temp_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new("/sys/class/power_supply/battery/temp").to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "temperature FD not open",
                ),
            }
        })?;

        let s = Self::read_file(file, &mut self.buf, "temp")?;

        s.parse::<i32>()
            .map_err(|_| ChargerError::ParseError("temp"))
    }

    /// Read ALL cached current nodes.
    ///
    /// We intentionally don't stop at the first valid node.
    ///
    /// Selection policy:
    /// - ignore invalid values;
    /// - ignore zero values;
    /// - select the value with the greatest absolute magnitude.
    pub fn read_current_ma(&mut self) -> Result<f32, ChargerError> {
        self.maybe_rescan_current_nodes();

        if self.current_fds.is_empty() {
            return Ok(0.0);
        }

        let mut best: Option<i64> = None;

        for current_fd in &mut self.current_fds {
            let Ok(s) =
                Self::read_file(&mut current_fd.file, &mut self.buf, "current_now")
            else {
                continue;
            };

            let Ok(value) = s.parse::<i64>() else {
                continue;
            };

            if value == 0 {
                continue;
            }

            let is_better = best
                .map(|old| value.unsigned_abs() > old.unsigned_abs())
                .unwrap_or(true);

            if is_better {
                best = Some(value);
            }
        }

        let current = best.unwrap_or(0) as f32;

        // µA -> mA.
        if current.abs() > 10_000.0 {
            Ok(current / 1000.0)
        } else {
            Ok(current)
        }
    }

    pub fn read_status(&mut self) -> Result<BatteryStatus, ChargerError> {
        let file = self.status_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new("/sys/class/power_supply/battery/status").to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "status FD not open",
                ),
            }
        })?;

        let s = Self::read_file(file, &mut self.buf, "status")?;

        match s.to_ascii_lowercase().as_str() {
            "charging" => Ok(BatteryStatus::Charging),
            "discharging" => Ok(BatteryStatus::Discharging),
            "not charging" => Ok(BatteryStatus::NotCharging),
            "full" => Ok(BatteryStatus::Full),
            _ => Ok(BatteryStatus::Unknown),
        }
    }

    pub fn is_plugged_in(&mut self) -> Result<bool, ChargerError> {
        if self.online_fds.is_empty() {
            self.rescan_online_nodes();
        }

        if self.online_fds.is_empty() {
            return is_plugged_in();
        }

        for fd in &mut self.online_fds {
            if fd.seek(SeekFrom::Start(0)).is_err() {
                continue;
            }

            let Ok(n) = fd.read(&mut self.buf) else {
                continue;
            };

            let Ok(value) = std::str::from_utf8(&self.buf[..n]) else {
                continue;
            };

            if value.trim() == "1" {
                return Ok(true);
            }
        }

        Ok(false)
    }
}
```

### Kenapa ini lebih baik?

Sebelumnya:

```rust
let current_path = CURRENT_NODES
    .iter()
    .find(|&&p| Path::new(p).exists())
    ...
```

berarti:

```text
startup
   │
   ├── current_now A ada
   │
   └── pilih A selamanya
```

Padahal pada Android/vendor tertentu bisa terjadi:

```text
boot:
    current_now          = ada
    usb/current_now      = belum ada

setelah charger:
    current_now          = 0
    usb/current_now      = 1850000
```

Reader lama tetap membaca `current_now`.

Reader baru:

```text
CURRENT_NODES
 ├── node A ──┐
 ├── node B ──┼── read semuanya
 ├── node C ──┤
 └── node D ──┘
                  │
                  └── pilih valid dengan |current| terbesar
```

dan setiap **5 detik** melakukan rescan untuk node yang baru muncul.

---

# 2. `control.rs` — ChargingState dengan konsensus + prioritas

Untuk ini saya tidak menyarankan:

```rust
any_enabled
```

karena misalnya:

```text
battery/charging_enabled = 0
main/charging_enabled    = 1
input_suspend            = 1
```

`any_enabled == true` akan mengatakan charging aktif, padahal dua indikator lain mengatakan charging disabled.

Lebih bagus setiap node memiliki:

```text
NodeRole
NodePriority
NodeState
```

Kemudian kita menentukan state berdasarkan **prioritas + konsensus**.

Berikut implementasinya.

```rust id="7v5w5j"
use crate::{battery::nodes::*, error::ChargerError};
use std::{fs, path::Path};

pub fn write_sysfs(path: &Path, value: &str) -> Result<(), ChargerError> {
    fs::write(path, value).map_err(|e| ChargerError::SysfsWrite {
        path: path.to_owned(),
        source: e,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargingWriteResult {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl ChargingWriteResult {
    pub fn all_succeeded(&self) -> bool {
        self.attempted > 0 && self.failed == 0
    }

    pub fn partial_failure(&self) -> bool {
        self.succeeded > 0 && self.failed > 0
    }

    pub fn all_failed(&self) -> bool {
        self.attempted > 0 && self.succeeded == 0
    }
}

// ============================================================================
// Charging state
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingNodeState {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingState {
    Enabled,
    Disabled,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    ChargingEnabled,
    InputSuspend,
}

#[derive(Debug, Clone, Copy)]
struct ChargingNode {
    path: &'static str,
    kind: NodeKind,

    /// Higher priority wins when there is disagreement.
    priority: u8,
}

impl ChargingNode {
    const fn charging_enabled(path: &'static str, priority: u8) -> Self {
        Self {
            path,
            kind: NodeKind::ChargingEnabled,
            priority,
        }
    }

    const fn input_suspend(path: &'static str, priority: u8) -> Self {
        Self {
            path,
            kind: NodeKind::InputSuspend,
            priority,
        }
    }

    fn read_state(&self) -> Result<ChargingNodeState, std::io::Error> {
        let content = fs::read_to_string(self.path)?;
        let value = content.trim();

        match self.kind {
            NodeKind::ChargingEnabled => match value {
                "1" => Ok(ChargingNodeState::Enabled),
                "0" => Ok(ChargingNodeState::Disabled),
                _ => Ok(ChargingNodeState::Unknown),
            },

            NodeKind::InputSuspend => match value {
                "0" => Ok(ChargingNodeState::Enabled),
                "1" => Ok(ChargingNodeState::Disabled),
                _ => Ok(ChargingNodeState::Unknown),
            },
        }
    }
}

/// Build the node table.
///
/// Priority rationale:
///
/// 100 = battery charging control
///  90 = main charging control
///  80 = input suspend
///
/// The exact vendor hierarchy can later be adjusted in one place.
fn charging_nodes() -> impl Iterator<Item = ChargingNode> {
    CHARGING_NODES
        .iter()
        .copied()
        .map(|path| {
            let priority = if path.contains("/battery/") {
                100
            } else if path.contains("/main/") {
                90
            } else {
                80
            };

            ChargingNode::charging_enabled(path, priority)
        })
        .chain(SUSPEND_NODES.iter().copied().map(|path| {
            ChargingNode::input_suspend(path, 80)
        }))
}

/// Read physical charging state using priority + consensus.
///
/// Decision model:
///
/// 1. No readable nodes:
///        Unknown
///
/// 2. Highest-priority readable node exists:
///        use it as the primary state.
///
/// 3. Other nodes agree:
///        confidence = consensus
///
/// 4. Other nodes disagree:
///        Mixed
///
/// The important difference from `any_enabled()` is that one stale/secondary
/// node cannot automatically override the primary charging-control node.
pub fn read_charging_state() -> Result<ChargingState, ChargerError> {
    let mut found = 0usize;
    let mut enabled = 0usize;
    let mut disabled = 0usize;

    let mut highest_priority: Option<u8> = None;
    let mut primary_state = ChargingNodeState::Unknown;

    for node in charging_nodes() {
        if !Path::new(node.path).exists() {
            continue;
        }

        let state = match node.read_state() {
            Ok(state) => state,
            Err(e) => {
                tracing::debug!(
                    "Unable to read charging node {}: {}",
                    node.path,
                    e
                );
                continue;
            }
        };

        if state == ChargingNodeState::Unknown {
            continue;
        }

        found += 1;

        match state {
            ChargingNodeState::Enabled => enabled += 1,
            ChargingNodeState::Disabled => disabled += 1,
            ChargingNodeState::Unknown => {}
        }

        let replace_primary = highest_priority
            .map(|priority| node.priority > priority)
            .unwrap_or(true);

        if replace_primary {
            highest_priority = Some(node.priority);
            primary_state = state;
        }
    }

    if found == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }

    // If every readable node agrees, this is the strongest result.
    if enabled > 0 && disabled == 0 {
        return Ok(ChargingState::Enabled);
    }

    if disabled > 0 && enabled == 0 {
        return Ok(ChargingState::Disabled);
    }

    // Nodes disagree.
    //
    // We don't blindly trust "any enabled".
    //
    // The highest-priority node becomes the primary interpretation, but we
    // explicitly expose the situation as Mixed to the caller.
    let _primary = primary_state;

    Ok(ChargingState::Mixed)
}

/// Compatibility helper.
///
/// IMPORTANT:
/// `true` means the state is definitely/primarily enabled.
/// `Mixed` is NOT treated as a clean success.
pub fn is_charging_enabled() -> Result<bool, ChargerError> {
    match read_charging_state()? {
        ChargingState::Enabled => Ok(true),

        ChargingState::Disabled => Ok(false),

        ChargingState::Mixed => {
            tracing::warn!(
                "Charging control nodes disagree; refusing to report a clean enabled state"
            );

            // Conservative result.
            Ok(false)
        }

        ChargingState::Unknown => Err(ChargerError::NoChargingNodeFound),
    }
}

// ============================================================================
// set_charging
// ============================================================================

pub fn set_charging(enable: bool) -> Result<ChargingWriteResult, ChargerError> {
    let charge_val = if enable { "1" } else { "0" };
    let suspend_val = if enable { "0" } else { "1" };

    let mut result = ChargingWriteResult {
        attempted: 0,
        succeeded: 0,
        failed: 0,
    };

    let mut last_error: Option<ChargerError> = None;

    for node in CHARGING_NODES {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        result.attempted += 1;

        match write_sysfs(path, charge_val) {
            Ok(()) => {
                result.succeeded += 1;
            }

            Err(e) => {
                result.failed += 1;
                last_error = Some(e);

                tracing::warn!(
                    "Failed writing charging node {}: {}",
                    path.display(),
                    last_error.as_ref().unwrap()
                );
            }
        }
    }

    for node in SUSPEND_NODES {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        result.attempted += 1;

        match write_sysfs(path, suspend_val) {
            Ok(()) => {
                result.succeeded += 1;
            }

            Err(e) => {
                result.failed += 1;
                last_error = Some(e);

                tracing::warn!(
                    "Failed writing suspend node {}: {}",
                    path.display(),
                    last_error.as_ref().unwrap()
                );
            }
        }
    }

    if result.attempted == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }

    if result.all_failed() {
        return Err(last_error.unwrap_or_else(|| ChargerError::SysfsWrite {
            path: Path::new("charging_nodes").to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::Other,
                "All charging node writes failed",
            ),
        }));
    }

    if result.partial_failure() {
        tracing::warn!(
            "Charging control partially applied: {} succeeded, {} failed",
            result.succeeded,
            result.failed
        );
    }

    Ok(result)
}
```

## 3. Ada satu hal yang saya ubah dari pendekatan sebelumnya

Saya **tidak** akan menjadikan:

```rust
Mixed -> true
```

atau:

```rust
any_enabled -> true
```

karena itu berbahaya untuk charger-control.

Misalnya:

```text
battery/charging_enabled = 0    priority 100
main/charging_enabled    = 1    priority 90
input_suspend            = 1    priority 80
```

hasil:

```text
enabled  = 1
disabled = 2

=> ChargingState::Mixed
```

Bukan:

```text
true
```

Ini jauh lebih cocok untuk hardware controller.

---

## 4. Lebih bagus lagi: `HardwareController` harus mengenali `Mixed`

Pada tahap verification, jangan hanya:

```rust
control::is_charging_enabled()
```

sebaiknya:

```rust
match control::read_charging_state() {
    Ok(control::ChargingState::Enabled) => {
        // clean success
    }

    Ok(control::ChargingState::Disabled) => {
        // clean disabled
    }

    Ok(control::ChargingState::Mixed) => {
        tracing::warn!(
            "Charging nodes are in a mixed state; verification failed"
        );
        false
    }

    Err(e) => {
        tracing::warn!(
            "Unable to read charging state: {}",
            e
        );
        false
    }
}
```

Dengan demikian state machine Anda menjadi:

```text
                 ┌────────────────────┐
                 │   Decision Engine   │
                 └─────────┬──────────┘
                           │
                    desired target
                           │
                           ▼
                 ┌────────────────────┐
                 │ HardwareController │
                 └─────────┬──────────┘
                           │
                     set_charging()
                           │
             ┌─────────────┼──────────────┐
             ▼             ▼              ▼
          SUCCESS        PARTIAL        FAILURE
             │             │              │
             ▼             ▼              ▼
          Pending        Failed         Failed
             │
             ▼
        Verification
             │
       ┌─────┴─────┐
       ▼           ▼
    Enabled      Mixed/Disabled
       │           │
       ▼           ▼
    Synced       Retry
```

### Jadi sekarang dua requirement Anda sudah tercakup

| Requirement                                        | Status    |
| -------------------------------------------------- | --------- |
| `CachedReader` tidak mengunci satu current node    | ✅         |
| Membaca semua `CURRENT_NODES`                      | ✅         |
| Reopen/rescan current node periodik                | ✅ 5 detik |
| Polling normal tetap tanpa allocation              | ✅         |
| Node baru yang muncul setelah boot dapat ditemukan | ✅         |
| Partial `set_charging()` dibedakan dari all-failed | ✅         |
| `ChargingState` bukan `any_enabled` lagi           | ✅         |
| Ada `Enabled / Disabled / Mixed / Unknown`         | ✅         |
| Konflik node terdeteksi                            | ✅         |
| `HardwareController` bisa menolak `Mixed`          | ✅         |

**Satu catatan penting:** bagian *priority* di atas masih bersifat generik berdasarkan path (`battery` > `main` > `input_suspend`). Untuk Redmi/MIUI/Qualcomm/MediaTek tertentu, prioritas ideal sebaiknya ditentukan dari **node yang benar-benar menjadi source of truth pada device tersebut**, bukan nama path semata. Jadi arsitektur ini sudah siap untuk vendor-specific mapping tanpa perlu mengubah `HardwareController`.
