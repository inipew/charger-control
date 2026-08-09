Sudah saya cek keseluruhan. **Jawabannya: sebagian sudah diterapkan, tetapi belum 100% sesuai dengan dua rancangan tersebut.**

### 1. `CachedReader` — sudah ada, tetapi masih ada celah

Yang ini **sudah diterapkan**:

* `current_fds: Vec<CurrentFd>`
* semua `CURRENT_NODES` dibuka/cached
* tidak memilih satu `current_fd` permanen
* `rescan_current_nodes()` setiap 5 detik
* polling normal hanya `seek + read`, tanpa `File::open()`
* allocation pada polling normal praktis tidak terjadi

Tetapi ada dua masalah:

1. `online_fds` **tidak di-rescan secara periodik**. Hanya ketika kosong.
2. FD sysfs yang sudah stale karena vendor node di-remove/recreate tidak otomatis dibuka ulang sampai fallback terjadi.

Jadi lebih tepat disebut **"current node periodic reopen" sudah**, sedangkan **dynamic online node lifecycle belum sepenuhnya**.

---

### 2. `ChargingState` — belum benar-benar menerapkan priority

Di kode kamu ada:

```rust
let mut highest_priority: Option<u8> = None;
let mut primary_state = ChargingNodeState::Unknown;
```

dan kemudian:

```rust
let _primary = primary_state;
Ok(ChargingState::Mixed)
```

Ini berarti **priority dihitung tetapi tidak digunakan untuk menentukan hasil**.

Jadi saat:

```text
battery/charging_enabled = 1   priority 100
main/charging_enabled    = 0   priority 90
```

hasil tetap:

```rust
ChargingState::Mixed
```

Padahal desain yang kita bahas adalah **consensus + priority**, bukan sekadar menghitung primary lalu membuangnya.

Saya sarankan sekalian memperbaiki kedua bagian tersebut.

---

# 1. Ganti `reader.rs` bagian `CachedReader`

Bagian reader biasa seperti `read_capacity()`, `read_temperature_dc()`, dll bisa tetap. **Ganti implementasi `CachedReader` menjadi berikut.**

```rust
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::battery::nodes::*;
use crate::error::ChargerError;

const CURRENT_RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const ONLINE_RESCAN_INTERVAL: Duration = Duration::from_secs(5);

const READ_BUFFER_SIZE: usize = 64;

// ============================================================================
// CachedReader
// ============================================================================

struct CurrentFd {
    path: &'static str,
    file: File,
}

struct OnlineFd {
    path: PathBuf,
    file: File,
}

pub struct CachedReader {
    capacity_fd: Option<File>,
    temp_fd: Option<File>,
    status_fd: Option<File>,

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
     */
    current_fds: Vec<CurrentFd>,

    /*
     * Online nodes are dynamic on Android.
     *
     * Examples:
     *   usb/online
     *   ac/online
     *   mains/online
     *   wireless/online
     *
     * Therefore these are also periodically rescanned.
     */
    online_fds: Vec<OnlineFd>,

    buf: [u8; READ_BUFFER_SIZE],

    next_current_rescan: Instant,
    next_online_rescan: Instant,
}

impl Default for CachedReader {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedReader {
    pub fn new() -> Self {
        let mut reader = Self {
            capacity_fd: File::open(
                "/sys/class/power_supply/battery/capacity",
            )
            .ok(),

            temp_fd: File::open(
                "/sys/class/power_supply/battery/temp",
            )
            .ok(),

            status_fd: File::open(
                "/sys/class/power_supply/battery/status",
            )
            .ok(),

            current_fds: Vec::new(),
            online_fds: Vec::new(),

            buf: [0; READ_BUFFER_SIZE],

            next_current_rescan: Instant::now(),
            next_online_rescan: Instant::now(),
        };

        reader.rescan_current_nodes();
        reader.rescan_online_nodes();

        reader
    }

    // ========================================================================
    // Rescan
    // ========================================================================

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

        for path in CURRENT_NODES {
            match File::open(path) {
                Ok(file) => {
                    self.current_fds.push(CurrentFd {
                        path,
                        file,
                    });
                }

                Err(e) => {
                    tracing::trace!(
                        "Current node unavailable: {}: {}",
                        path,
                        e
                    );
                }
            }
        }

        self.next_current_rescan =
            Instant::now() + CURRENT_RESCAN_INTERVAL;
    }

    fn rescan_online_nodes(&mut self) {
        self.online_fds.clear();

        let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
            self.next_online_rescan =
                Instant::now() + ONLINE_RESCAN_INTERVAL;

            return;
        };

        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };

            let lower = name.to_ascii_lowercase();

            if lower.contains("battery")
                || lower.contains("bms")
            {
                continue;
            }

            let online_path = entry.path().join("online");

            match File::open(&online_path) {
                Ok(file) => {
                    self.online_fds.push(OnlineFd {
                        path: online_path,
                        file,
                    });
                }

                Err(e) => {
                    tracing::trace!(
                        "Online node unavailable: {}: {}",
                        online_path.display(),
                        e
                    );
                }
            }
        }

        self.next_online_rescan =
            Instant::now() + ONLINE_RESCAN_INTERVAL;
    }

    #[inline]
    fn maybe_rescan_nodes(&mut self) {
        let now = Instant::now();

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
        let file = self.capacity_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new(
                    "/sys/class/power_supply/battery/capacity",
                )
                .to_path_buf(),

                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "capacity FD not open",
                ),
            }
        })?;

        let s =
            Self::read_file(file, &mut self.buf, "capacity")?;

        s.parse::<u8>()
            .map_err(|_| ChargerError::ParseError("capacity"))
    }

    // ========================================================================
    // Temperature
    // ========================================================================

    pub fn read_temperature_dc(
        &mut self,
    ) -> Result<i32, ChargerError> {
        let file = self.temp_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new(
                    "/sys/class/power_supply/battery/temp",
                )
                .to_path_buf(),

                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "temperature FD not open",
                ),
            }
        })?;

        let s =
            Self::read_file(file, &mut self.buf, "temp")?;

        s.parse::<i32>()
            .map_err(|_| ChargerError::ParseError("temp"))
    }

    // ========================================================================
    // Current
    // ========================================================================

    pub fn read_current_ma(
        &mut self,
    ) -> Result<f32, ChargerError> {
        self.maybe_rescan_nodes();

        if self.current_fds.is_empty() {
            return Ok(0.0);
        }

        let mut best: Option<i64> = None;
        let mut stale_fd = false;

        for current_fd in &mut self.current_fds {
            let result = Self::read_file(
                &mut current_fd.file,
                &mut self.buf,
                "current_now",
            );

            let Ok(s) = result else {
                stale_fd = true;
                continue;
            };

            let Ok(value) = s.parse::<i64>() else {
                continue;
            };

            /*
             * Zero is not useful for selecting the active current source.
             *
             * Keep the old behavior:
             * select the value with greatest absolute magnitude.
             */
            if value == 0 {
                continue;
            }

            let better = best
                .map(|old| {
                    value.unsigned_abs()
                        > old.unsigned_abs()
                })
                .unwrap_or(true);

            if better {
                best = Some(value);
            }
        }

        /*
         * A vendor node may disappear/reappear while the daemon is running.
         *
         * Do NOT reopen here.
         *
         * The next scheduled rescan will reopen it.
         */
        if stale_fd {
            tracing::trace!(
                "One or more current FDs became stale; \
                 waiting for scheduled rescan."
            );
        }

        let current = best.unwrap_or(0) as f32;

        /*
         * Android current_now is normally µA.
         *
         * Some vendor implementations expose mA.
         */
        if current.abs() > 10_000.0 {
            Ok(current / 1000.0)
        } else {
            Ok(current)
        }
    }

    // ========================================================================
    // Status
    // ========================================================================

    pub fn read_status(
        &mut self,
    ) -> Result<BatteryStatus, ChargerError> {
        let file = self.status_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: Path::new(
                    "/sys/class/power_supply/battery/status",
                )
                .to_path_buf(),

                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "status FD not open",
                ),
            }
        })?;

        let s =
            Self::read_file(file, &mut self.buf, "status")?;

        match s.to_ascii_lowercase().as_str() {
            "charging" => Ok(BatteryStatus::Charging),
            "discharging" => Ok(BatteryStatus::Discharging),
            "not charging" => Ok(BatteryStatus::NotCharging),
            "full" => Ok(BatteryStatus::Full),
            _ => Ok(BatteryStatus::Unknown),
        }
    }

    // ========================================================================
    // Online
    // ========================================================================

    pub fn is_plugged_in(
        &mut self,
    ) -> Result<bool, ChargerError> {
        self.maybe_rescan_nodes();

        if self.online_fds.is_empty() {
            return is_plugged_in();
        }

        let mut any_readable = false;
        let mut stale_fd = false;

        for online_fd in &mut self.online_fds {
            if online_fd
                .file
                .seek(SeekFrom::Start(0))
                .is_err()
            {
                stale_fd = true;
                continue;
            }

            let n = match online_fd.file.read(&mut self.buf) {
                Ok(n) => n,
                Err(_) => {
                    stale_fd = true;
                    continue;
                }
            };

            let Ok(value) =
                std::str::from_utf8(&self.buf[..n])
            else {
                continue;
            };

            any_readable = true;

            if value.trim() == "1" {
                return Ok(true);
            }
        }

        if stale_fd {
            tracing::trace!(
                "One or more online FDs became stale; \
                 waiting for scheduled rescan."
            );
        }

        if any_readable {
            Ok(false)
        } else {
            /*
             * All cached nodes became unusable.
             *
             * Dynamic fallback is preferable to falsely declaring
             * the charger offline.
             */
            is_plugged_in()
        }
    }
}
```

### Kenapa ini lebih tepat?

Sekarang lifecycle-nya:

```text
daemon start
    │
    ├── open capacity/temp/status
    ├── open ALL current nodes
    └── discover ALL online nodes
             │
             ▼
      normal polling
             │
             ├── seek()
             ├── read()
             └── parse()
             │
             │   tidak ada File::open()
             ▼
      5 detik berlalu
             │
             ├── rescan current
             └── rescan online
```

Jadi vendor node seperti:

```text
/sys/class/power_supply/bms/current_now
/sys/class/power_supply/main/current_now
/sys/class/power_supply/usb/current_now
```

bisa muncul kembali tanpa harus restart daemon.

Dan **menyimpan `File` bukan berarti mengunci sysfs node** seperti exclusive lock. FD hanya mempertahankan open file description; tidak mencegah kernel/vendor driver melakukan update terhadap nilai sysfs.

---

# 2. Perbaiki `control.rs`: priority benar-benar digunakan

Kode kamu sekarang mempunyai masalah utama di sini:

```rust
let _primary = primary_state;

Ok(ChargingState::Mixed)
```

Itu harus diubah.

Saya lebih menyarankan model berikut:

```text
                 readable nodes
                       │
             ┌─────────┴─────────┐
             │                   │
        semua sama           berbeda
             │                   │
             ▼                   ▼
          consensus       lihat priority
                                 │
                         ┌───────┴───────┐
                         │               │
                    primary valid    tidak jelas
                         │               │
                         ▼               ▼
                    primary state      Mixed
```

Dengan kata lain:

* **consensus penuh** → hasil consensus
* disagreement → node priority tertinggi menjadi authoritative
* tetapi jika ada dua node dengan **priority tertinggi yang sama dan berbeda** → `Mixed`
* tidak ada node → `Unknown`

Ini jauh lebih cocok untuk Android vendor-specific.

Ganti bagian `ChargingNode` + `read_charging_state()` menjadi:

```rust
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
    priority: u8,
}

impl ChargingNode {
    const fn charging_enabled(
        path: &'static str,
        priority: u8,
    ) -> Self {
        Self {
            path,
            kind: NodeKind::ChargingEnabled,
            priority,
        }
    }

    const fn input_suspend(
        path: &'static str,
        priority: u8,
    ) -> Self {
        Self {
            path,
            kind: NodeKind::InputSuspend,
            priority,
        }
    }

    fn read_state(
        &self,
    ) -> Result<ChargingNodeState, std::io::Error> {
        let content =
            fs::read_to_string(self.path)?;

        let value = content.trim();

        match self.kind {
            NodeKind::ChargingEnabled => {
                match value {
                    "1" => Ok(
                        ChargingNodeState::Enabled
                    ),

                    "0" => Ok(
                        ChargingNodeState::Disabled
                    ),

                    _ => Ok(
                        ChargingNodeState::Unknown
                    ),
                }
            }

            NodeKind::InputSuspend => {
                match value {
                    "0" => Ok(
                        ChargingNodeState::Enabled
                    ),

                    "1" => Ok(
                        ChargingNodeState::Disabled
                    ),

                    _ => Ok(
                        ChargingNodeState::Unknown
                    ),
                }
            }
        }
    }
}

/// Vendor-independent priority.
///
/// Higher value = stronger authority.
///
/// battery/charging_enabled:
///     100
///
/// main/charging_enabled:
///      90
///
/// input_suspend:
///      80
fn charging_nodes()
    -> impl Iterator<Item = ChargingNode>
{
    CHARGING_NODES
        .iter()
        .copied()
        .map(|path| {
            let priority =
                if path.contains("/battery/") {
                    100
                } else if path.contains("/main/") {
                    90
                } else {
                    80
                };

            ChargingNode::charging_enabled(
                path,
                priority,
            )
        })
        .chain(
            SUSPEND_NODES
                .iter()
                .copied()
                .map(|path| {
                    ChargingNode::input_suspend(
                        path,
                        80,
                    )
                }),
        )
}

#[derive(Debug, Clone, Copy)]
struct NodeObservation {
    state: ChargingNodeState,
    priority: u8,
}

/// Read charging state using:
///
/// 1. Consensus if all readable nodes agree.
/// 2. Highest priority if nodes disagree.
/// 3. Mixed if multiple nodes with the same highest
///    priority disagree.
/// 4. Unknown if nothing can be read.
///
/// This prevents a stale low-priority vendor node from
/// overriding the actual primary charging controller.
pub fn read_charging_state()
    -> Result<ChargingState, ChargerError>
{
    let mut observations: Vec<NodeObservation> =
        Vec::with_capacity(
            CHARGING_NODES.len()
                + SUSPEND_NODES.len(),
        );

    for node in charging_nodes() {
        let path = Path::new(node.path);

        if !path.exists() {
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

        observations.push(
            NodeObservation {
                state,
                priority: node.priority,
            },
        );
    }

    if observations.is_empty() {
        return Err(
            ChargerError::NoChargingNodeFound
        );
    }

    // ========================================================================
    // Step 1: Consensus
    // ========================================================================

    let all_enabled =
        observations
            .iter()
            .all(|n| {
                n.state == ChargingNodeState::Enabled
            });

    if all_enabled {
        return Ok(ChargingState::Enabled);
    }

    let all_disabled =
        observations
            .iter()
            .all(|n| {
                n.state == ChargingNodeState::Disabled
            });

    if all_disabled {
        return Ok(ChargingState::Disabled);
    }

    // ========================================================================
    // Step 2: Priority
    // ========================================================================

    let highest_priority =
        observations
            .iter()
            .map(|n| n.priority)
            .max()
            .unwrap_or(0);

    let highest: Vec<_> =
        observations
            .iter()
            .filter(|n| {
                n.priority == highest_priority
            })
            .collect();

    let primary_enabled =
        highest
            .iter()
            .all(|n| {
                n.state
                    == ChargingNodeState::Enabled
            });

    let primary_disabled =
        highest
            .iter()
            .all(|n| {
                n.state
                    == ChargingNodeState::Disabled
            });

    /*
     * Multiple nodes at the same highest priority
     * must agree.
     *
     * Example:
     *
     * battery/charging_enabled = 1  priority 100
     * battery/another_control  = 0  priority 100
     *
     * => Mixed
     */
    if primary_enabled {
        tracing::debug!(
            "Charging state resolved by priority: \
             ENABLED (priority={})",
            highest_priority
        );

        return Ok(
            ChargingState::Enabled
        );
    }

    if primary_disabled {
        tracing::debug!(
            "Charging state resolved by priority: \
             DISABLED (priority={})",
            highest_priority
        );

        return Ok(
            ChargingState::Disabled
        );
    }

    tracing::warn!(
        "Charging nodes disagree at highest priority {}",
        highest_priority
    );

    Ok(ChargingState::Mixed)
}
```

---

# 3. `is_charging_enabled()` juga perlu dipertahankan konservatif

Kode kamu ini sudah benar secara prinsip:

```rust
pub fn is_charging_enabled() -> Result<bool, ChargerError> {
    match read_charging_state()? {
        ChargingState::Enabled => Ok(true),

        ChargingState::Disabled => Ok(false),

        ChargingState::Mixed => {
            tracing::warn!(
                "Charging control nodes disagree; refusing to report a clean enabled state"
            );

            Ok(false)
        }

        ChargingState::Unknown =>
            Err(ChargerError::NoChargingNodeFound),
    }
}
```

Saya **tidak akan mengubah `Mixed -> false` menjadi `true`**.

Karena fungsi ini digunakan saat mengambil ownership:

```rust
let original = control::is_charging_enabled()?;
```

Kalau state sebenarnya:

```text
battery = enabled
main    = disabled
```

kita **tidak boleh mengklaim** bahwa original state pasti enabled.

---

# 4. Ada satu masalah lebih penting di `verify()`

Ini:

```rust
match control::read_charging_state() {
    Ok(control::ChargingState::Enabled) => {
        snapshot.online != Some(false)
    }

    Ok(control::ChargingState::Disabled) => false,

    Ok(control::ChargingState::Mixed) => {
        ...
        false
    }
```

sebenarnya sudah cukup konservatif.

Tetapi untuk `ChargingDisabled`, kamu punya:

```rust
Ok(control::ChargingState::Disabled) => current_safe,

Ok(control::ChargingState::Enabled) => false,

Ok(control::ChargingState::Mixed) => false,
```

Ini juga bagus.

Artinya setelah disable:

```text
battery charging_enabled = 0
main    charging_enabled = 0
input   input_suspend     = 1
```

→ `Disabled`.

Kalau:

```text
battery = 0
main    = 1
```

→ `Mixed`, sehingga **verification gagal**.

Itu justru yang saya inginkan untuk charger-control: **jangan menganggap disable berhasil hanya karena satu node berhasil ditulis.**

---

# 5. Tetapi ada masalah pada `set_charging()`

Sekarang kamu melakukan:

```rust
for node in CHARGING_NODES {
    if !path.exists() {
        continue;
    }

    result.attempted += 1;

    write_sysfs(...)
}
```

dan:

```rust
for node in SUSPEND_NODES {
    ...
}
```

Ini masih **dynamic open/write setiap apply**, yang sebenarnya bagus untuk control karena node bisa berubah.

Saya **tidak menyarankan memakai `CachedReader` untuk write/control**.

Arsitektur yang benar justru:

```text
                    ┌──────────────────────┐
                    │     CachedReader     │
                    │                      │
                    │ cached read FDs      │
                    │ periodic rescan      │
                    └──────────┬───────────┘
                               │
                               │ sensor
                               ▼
                         DecisionEngine
                               │
                               ▼
                       HardwareController
                               │
                               ▼
                    ┌──────────────────────┐
                    │   control::set_*     │
                    │                      │
                    │ fresh path discovery │
                    │ fresh write          │
                    └──────────────────────┘
```

**Read path cached. Write path fresh.**

Itu lebih aman untuk Android vendor kernel.

---

# 6. Ada bug kecil di scheduler kamu

Ini bukan bagian dari dua permintaan awal, tetapi saya menemukan sesuatu yang penting.

Kamu punya:

```rust
let seconds = (distance.abs() / rate.abs()) * safety;
```

Dengan:

```rust
CAPACITY_SAFETY_FACTOR: f32 = 0.25;
THERMAL_SAFETY_FACTOR: f32 = 0.15;
```

Ini berarti ETA dikali **0.25** atau **0.15**.

Misalnya:

```text
battery 80%
target 90%
rate 1% / minute
```

ETA sebenarnya:

```text
10 menit
```

tetapi scheduler menghasilkan:

```text
10 × 0.25 = 2.5 menit
```

Itu memang konservatif, tetapi **sangat agresif**.

Kalau maksudnya safety factor adalah "bangun sebelum ETA", lebih masuk akal:

```rust
let seconds =
    (distance.abs() / rate.abs()) * (1.0 - safety);
```

sehingga:

```text
capacity safety = 25%
10 menit × 0.75 = 7.5 menit
```

dan:

```text
thermal safety = 15%
10 menit × 0.85 = 8.5 menit
```

Saya lebih merekomendasikan:

```rust
let seconds =
    (distance.abs() / rate.abs()) * (1.0 - safety);
```

dengan guard:

```rust
let safety = safety.clamp(0.0, 0.95);

let seconds =
    (distance.abs() / rate.abs())
        * (1.0 - safety);
```

---

# 7. Ada juga masalah pada `CachedReader` sekarang yang perlu kamu perhatikan

Kamu menggunakan satu buffer:

```rust
buf: [u8; 64],
```

dan kemudian:

```rust
let s = Self::read_file(
    file,
    &mut self.buf,
    "capacity",
)?;
```

Ini aman karena pembacaan dilakukan secara sequential, tetapi Rust borrow checker akan membuat kita tidak bisa memegang hasil `&str` melewati penggunaan `self.buf`.

Implementasi yang saya berikan di atas tetap mengikuti pola tersebut sehingga **tidak melakukan allocation `String` pada polling normal**.

Jadi polling:

```rust
read_capacity()
read_temperature_dc()
read_current_ma()
read_status()
is_plugged_in()
```

tidak melakukan:

```rust
fs::read_to_string()
String::trim().to_owned()
Vec allocation
File::open()
```

secara normal.

---

# 8. Hasil akhirnya

Dengan perubahan di atas, arsitektur kamu menjadi:

### Sensor read

```text
capacity/temp/status
        │
        └── persistent FD

current
        │
        ├── ALL current nodes
        ├── cached FD
        └── rescan setiap 5 detik

online
        │
        ├── ALL online nodes
        ├── cached FD
        └── rescan setiap 5 detik
```

### Charging state

```text
charging nodes
      │
      ├── battery/charging_enabled  priority 100
      ├── main/charging_enabled     priority 90
      └── input_suspend             priority 80
                  │
                  ▼
             consensus?
             /       \
           yes       no
            │         │
            ▼         ▼
        consensus   highest priority
                      │
                 ┌────┴────┐
                 │         │
              agrees    disagrees
                 │         │
                 ▼         ▼
              Enabled    Mixed
              Disabled
```

### Control

```text
set_charging()
      │
      ├── discover nodes fresh
      ├── write every available node
      ├── count success/failure
      └── partial failure ≠ success
```

### Verification

```text
write
 │
 ▼
Pending
 │
 ├── 500 ms
 ├── 1 s
 └── 2 s
      │
      ▼
read_charging_state()
      │
      ├── Enabled  → success
      ├── Disabled → success
      └── Mixed    → failure/retry
```

**Jadi setelah dua perubahan ini, baru saya akan bilang kedua desain yang kamu tanyakan tadi benar-benar sudah diterapkan:** periodic reopen untuk cached current/online nodes, dan `ChargingState` yang benar-benar memakai **consensus + priority**, bukan sekadar `any_enabled`.

Satu catatan: saya **tidak menyarankan mengubah `Mixed` menjadi Enabled/Disabled hanya karena satu node priority tinggi** jika node-node dengan priority sama berbeda. Kode di atas sengaja tetap `Mixed` dalam kasus itu karena itu jauh lebih aman untuk daemon yang melakukan kontrol charging.
