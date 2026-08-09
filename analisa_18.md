Ya. Masalah utamanya memang `CachedReader` sekarang memilih **satu `CURRENT_NODES` saat `new()`**, lalu FD itu dipakai selamanya. Kalau node tersebut valid tetapi kemudian nilainya `0`, stale, atau node prioritas lain justru menjadi sumber arus yang benar, reader tidak pernah berpindah.

Yang lebih tepat: **cache semua current node yang tersedia**, lalu setiap polling baca semuanya dan pilih nilai yang valid. Jangan hanya memilih node pertama.

Saya sarankan juga mempertahankan urutan prioritas `CURRENT_NODES`: ambil **nilai non-zero pertama** berdasarkan urutan node yang didefinisikan. Jika semua `0`, return `0`. Jika node pertama error tetapi node kedua valid, otomatis lanjut.

Berikut versi `CachedReader` yang sudah diperbaiki.

```rust
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// A stateful reader that holds open File Descriptors for low-allocation polling.
///
/// Important:
/// - Does NOT lock onto a single current node.
/// - Keeps all available CURRENT_NODES open.
/// - Each read_current_ma() scans the nodes in CURRENT_NODES priority order.
/// - If the preferred node is unavailable/invalid/zero, it falls back to the next one.
/// - This is important on Android kernels where current_now may move between
///   battery/current, bms/current_now, main/current_now, etc.
pub struct CachedReader {
    capacity_fd: Option<File>,
    temp_fd: Option<File>,
    status_fd: Option<File>,

    /// All available current nodes, kept in CURRENT_NODES priority order.
    current_fds: Vec<(PathBuf, File)>,

    /// All available charger "online" nodes.
    online_fds: Vec<(PathBuf, File)>,

    buf: [u8; 32],
}

impl Default for CachedReader {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedReader {
    pub fn new() -> Self {
        let current_fds = Self::open_current_nodes();
        let online_fds = Self::open_online_nodes();

        tracing::debug!(
            "CachedReader initialized: {} current nodes, {} online nodes",
            current_fds.len(),
            online_fds.len()
        );

        for (path, _) in &current_fds {
            tracing::debug!("Current node: {}", path.display());
        }

        for (path, _) in &online_fds {
            tracing::debug!("Online node: {}", path.display());
        }

        Self {
            capacity_fd: File::open("/sys/class/power_supply/battery/capacity").ok(),
            temp_fd: File::open("/sys/class/power_supply/battery/temp").ok(),
            status_fd: File::open("/sys/class/power_supply/battery/status").ok(),
            current_fds,
            online_fds,
            buf: [0; 32],
        }
    }

    /// Open every available current node.
    ///
    /// The order follows CURRENT_NODES, so the first valid non-zero
    /// value remains the preferred source.
    fn open_current_nodes() -> Vec<(PathBuf, File)> {
        let mut result = Vec::with_capacity(CURRENT_NODES.len());

        for &path in CURRENT_NODES {
            let path_buf = PathBuf::from(path);

            match File::open(&path_buf) {
                Ok(file) => {
                    result.push((path_buf, file));
                }
                Err(e) => {
                    tracing::debug!(
                        "Current node unavailable: {} ({})",
                        path,
                        e
                    );
                }
            }
        }

        result
    }

    /// Discover all charger online nodes and keep their FDs open.
    fn open_online_nodes() -> Vec<(PathBuf, File)> {
        let mut result = Vec::new();

        let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
            return result;
        };

        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };

            let lower = name.to_ascii_lowercase();

            // battery/bms generally don't represent the physical charger.
            if lower.contains("battery") || lower.contains("bms") {
                continue;
            }

            let online_path = entry.path().join("online");

            match File::open(&online_path) {
                Ok(file) => {
                    result.push((online_path, file));
                }
                Err(e) => {
                    tracing::debug!(
                        "Online node unavailable: {} ({})",
                        online_path.display(),
                        e
                    );
                }
            }
        }

        result
    }

    /// Read a cached FD into the shared buffer.
    ///
    /// The FD is rewound before every read because sysfs pseudo-files
    /// are commonly read from offset 0.
    fn read_fd_to_str<'a>(
        fd: &mut File,
        buf: &'a mut [u8],
    ) -> Result<&'a str, std::io::Error> {
        fd.seek(SeekFrom::Start(0))?;

        let n = fd.read(buf)?;

        std::str::from_utf8(&buf[..n])
            .map(|s| s.trim())
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "sysfs value is not valid UTF-8",
                )
            })
    }

    pub fn read_capacity(&mut self) -> Result<u8, ChargerError> {
        let fd = self.capacity_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: PathBuf::from(
                    "/sys/class/power_supply/battery/capacity"
                ),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "FD not open",
                ),
            }
        })?;

        let s = Self::read_fd_to_str(fd, &mut self.buf).map_err(|e| {
            ChargerError::SysfsRead {
                path: PathBuf::from(
                    "/sys/class/power_supply/battery/capacity"
                ),
                source: e,
            }
        })?;

        s.parse::<u8>()
            .map_err(|_| ChargerError::ParseError("capacity"))
    }

    pub fn read_temperature_dc(&mut self) -> Result<i32, ChargerError> {
        let fd = self.temp_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: PathBuf::from(
                    "/sys/class/power_supply/battery/temp"
                ),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "FD not open",
                ),
            }
        })?;

        let s = Self::read_fd_to_str(fd, &mut self.buf).map_err(|e| {
            ChargerError::SysfsRead {
                path: PathBuf::from(
                    "/sys/class/power_supply/battery/temp"
                ),
                source: e,
            }
        })?;

        s.parse::<i32>()
            .map_err(|_| ChargerError::ParseError("temp"))
    }

    /// Read current from all cached current nodes.
    ///
    /// Selection policy:
    /// 1. Follow CURRENT_NODES priority order.
    /// 2. Ignore read errors.
    /// 3. Ignore malformed values.
    /// 4. Ignore zero values when another node may provide a real value.
    /// 5. Return the first valid non-zero value.
    /// 6. If every available node is zero, return 0.
    /// 7. If no current node is available at all, return a sysfs error.
    ///
    /// This avoids permanently binding the reader to one current node.
    pub fn read_current_ma(&mut self) -> Result<f32, ChargerError> {
        if self.current_fds.is_empty() {
            return Err(ChargerError::SysfsRead {
                path: PathBuf::from("CURRENT_NODES"),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no current sysfs node is available",
                ),
            });
        }

        let mut saw_zero = false;

        for (path, fd) in &mut self.current_fds {
            let s = match Self::read_fd_to_str(fd, &mut self.buf) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(
                        "Failed reading current node {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            };

            let raw = match s.parse::<i64>() {
                Ok(value) => value,
                Err(_) => {
                    tracing::debug!(
                        "Invalid current value '{}' from {}",
                        s,
                        path.display()
                    );
                    continue;
                }
            };

            if raw == 0 {
                saw_zero = true;
                continue;
            }

            let current_ma = Self::normalize_current_ma(raw);

            tracing::trace!(
                "Using current node {}: raw={} -> {:.2} mA",
                path.display(),
                raw,
                current_ma
            );

            return Ok(current_ma);
        }

        // All readable nodes reported zero.
        if saw_zero {
            return Ok(0.0);
        }

        // Nodes existed but none could be read/parsed.
        Err(ChargerError::ParseError("current_now"))
    }

    /// Normalize a current sysfs value to mA.
    ///
    /// Android/Linux kernels normally expose current_now in µA.
    /// Some vendor implementations expose mA, so retain the same
    /// compatibility heuristic as the standalone reader.
    fn normalize_current_ma(raw: i64) -> f32 {
        let mut value = raw as f32;

        // Typical current_now:
        //   500000 = 500 mA
        //   -500000 = -500 mA
        //
        // Small values may already be mA.
        if value.abs() > 10_000.0 {
            value /= 1000.0;
        }

        value
    }

    pub fn read_status(&mut self) -> Result<BatteryStatus, ChargerError> {
        let fd = self.status_fd.as_mut().ok_or_else(|| {
            ChargerError::SysfsRead {
                path: PathBuf::from(
                    "/sys/class/power_supply/battery/status"
                ),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "FD not open",
                ),
            }
        })?;

        let s = Self::read_fd_to_str(fd, &mut self.buf).map_err(|e| {
            ChargerError::SysfsRead {
                path: PathBuf::from(
                    "/sys/class/power_supply/battery/status"
                ),
                source: e,
            }
        })?;

        match s.to_ascii_lowercase().as_str() {
            "charging" => Ok(BatteryStatus::Charging),
            "discharging" => Ok(BatteryStatus::Discharging),
            "not charging" => Ok(BatteryStatus::NotCharging),
            "full" => Ok(BatteryStatus::Full),
            _ => Ok(BatteryStatus::Unknown),
        }
    }

    /// Check whether any physical charger reports online=1.
    pub fn is_plugged_in(&mut self) -> Result<bool, ChargerError> {
        if self.online_fds.is_empty() {
            return is_plugged_in();
        }

        let mut any_readable = false;

        for (path, fd) in &mut self.online_fds {
            let s = match Self::read_fd_to_str(fd, &mut self.buf) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(
                        "Failed reading online node {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            };

            any_readable = true;

            if s == "1" {
                return Ok(true);
            }
        }

        if any_readable {
            Ok(false)
        } else {
            // Existing cached FDs became unusable.
            // Fall back to dynamic discovery instead of assuming offline.
            is_plugged_in()
        }
    }
}
```

### Perubahan paling penting

Sebelumnya:

```rust
let current_path = CURRENT_NODES
    .iter()
    .find(|&&p| Path::new(p).exists())
    .copied()
    .unwrap_or(...);

current_fd: File::open(current_path).ok(),
```

Ini memang membuat:

```text
CURRENT_NODES
    │
    └── first existing node
            │
            └── current_fd
                    │
                    └── dipakai selamanya
```

Sekarang menjadi:

```text
CURRENT_NODES
    │
    ├── current_node_1 → FD
    ├── current_node_2 → FD
    ├── current_node_3 → FD
    └── current_node_4 → FD
             │
             ▼
     read_current_ma()
             │
             ├── node 1 → error/invalid/0 → lanjut
             ├── node 2 → error/invalid/0 → lanjut
             ├── node 3 → valid → gunakan
             └── node 4 → tidak perlu
```

Jadi misalnya:

```text
battery/current_now = 0
bms/current_now     = 735000
main/current_now    = 0
```

hasilnya:

```text
735.0 mA
```

Kalau kemudian berubah menjadi:

```text
battery/current_now = 820000
bms/current_now     = 0
main/current_now    = 0
```

poll berikutnya otomatis menghasilkan:

```text
820.0 mA
```

**tanpa restart `CachedReader`.**

### Satu hal yang saya sarankan untuk kode kamu

Ada satu potensi masalah lain yang lebih penting dari sekadar caching node: **memilih "first non-zero" belum tentu selalu benar secara semantik.**

Misalnya:

```text
battery/current_now = 500000
bms/current_now     = 480000
```

keduanya valid tetapi berbeda sedikit. Kita tidak ingin setiap perubahan kecil membuat sumber berpindah-pindah. Karena itu `CURRENT_NODES` sebaiknya memang memiliki **urutan prioritas yang jelas**.

Contohnya:

```rust
pub const CURRENT_NODES: &[&str] = &[
    "/sys/class/power_supply/battery/current_now",
    "/sys/class/power_supply/bms/current_now",
    "/sys/class/power_supply/main/current_now",
];
```

Dengan implementasi di atas, prioritas tetap:

```text
battery → bms → main
```

dan fallback hanya terjadi kalau node prioritas tersebut **tidak bisa dibaca, invalid, atau 0**.

**Saya juga sengaja tidak menggunakan `Path::exists()` setiap polling.** FD dibuka sekali saat startup sehingga tetap ringan untuk monitor loop Android yang polling terus-menerus.

Namun ada satu improvement lanjutan yang menurut saya cukup penting untuk daemon kamu: **kalau sebuah current node gagal dibaca setelah startup, `CachedReader` saat ini belum mencoba membuka ulang node tersebut.** Kalau sysfs node benar-benar muncul belakangan/dibuat ulang oleh driver, perlu mekanisme *lazy re-discovery*. Untuk arsitektur charger-control kamu, saya akan membuat `CachedReader` versi berikutnya yang bisa **reopen current nodes secara periodik tanpa mengunci node dan tanpa alokasi pada polling normal**.
