Ya. Untuk kode Anda sekarang, **P2 dan P3 bukan perubahan arsitektur besar**, tetapi lebih ke hardening agar daemon benar-benar tahan terhadap kondisi Android/vendor kernel yang aneh, mudah dites, dan mudah di-debug.

Saya akan membahasnya berdasarkan implementasi Anda sekarang dan juga menjelaskan **mana yang benar-benar perlu diubah dan mana yang opsional**.

---

# 🟠 P2 — Hardening Production

## 13. Rescan `capacity/temp/status` FD

### Masalah sekarang

`CachedReader::new()` membuka:

```rust
capacity_fd: File::open(
    "/sys/class/power_supply/battery/capacity"
).ok(),

temp_fd: File::open(
    "/sys/class/power_supply/battery/temp"
).ok(),

status_fd: File::open(
    "/sys/class/power_supply/battery/status"
).ok(),
```

Kemudian FD tersebut dipakai selamanya.

Sementara `current_fds` dan `online_fds` sudah mempunyai:

```rust
CURRENT_RESCAN_INTERVAL = 5s
ONLINE_RESCAN_INTERVAL = 5s
```

Jadi ada inkonsistensi:

```text
capacity/temp/status
        │
        └── buka sekali ───────────────→ selamanya

current/online
        │
        └── rescan setiap 5 detik
```

---

## Kenapa ini penting di Android?

`/sys/class/power_supply` adalah sysfs, bukan filesystem biasa.

Vendor Android bisa melakukan:

```text
charger unplug
      ↓
power_supply unregister
      ↓
power_supply register
```

atau power HAL/driver melakukan reinitialization.

Akibatnya FD lama bisa menjadi tidak usable.

Misalnya:

```text
status_fd
   ↓
read()
   ↓
ENOENT / EIO / ENODEV
```

Tetapi `CachedReader` tidak pernah mencoba membuka kembali file tersebut.

---

## Solusi

Jadikan sensor statis juga cached + rescan.

Misalnya:

```rust
struct BatteryFd {
    path: &'static str,
    file: Option<File>,
}
```

Kemudian:

```rust
capacity: BatteryFd,
temperature: BatteryFd,
status: BatteryFd,
```

dan:

```rust
const BATTERY_RESCAN_INTERVAL: Duration =
    Duration::from_secs(5);
```

Kemudian:

```rust
fn rescan_battery_nodes(&mut self) {
    self.capacity_fd =
        File::open("/sys/class/power_supply/battery/capacity").ok();

    self.temp_fd =
        File::open("/sys/class/power_supply/battery/temp").ok();

    self.status_fd =
        File::open("/sys/class/power_supply/battery/status").ok();

    self.next_battery_rescan =
        Instant::now() + BATTERY_RESCAN_INTERVAL;
}
```

---

## Lebih bagus lagi

Daripada hanya rescan berdasarkan timer, gunakan dua mekanisme:

```text
normal:
    read cached FD

read gagal:
    tandai stale

timer:
    rescan

netlink power_supply event:
    rescan lebih cepat
```

Jadi:

```text
Netlink ACTION=change
        ↓
invalidate cached battery FDs
        ↓
next read
        ↓
reopen
```

Ini sangat cocok dengan arsitektur daemon Anda.

### Rekomendasi saya

**P2 wajib**, tetapi tidak perlu dibuat terlalu kompleks.

Cukup:

```text
5–10 detik periodic rescan
+
stale FD flag
```

sudah bagus.

---

# 14. Perbaiki online-node discovery menjadi explicit/configurable

Sekarang:

```rust
if lower.contains("battery")
    || lower.contains("bms")
{
    continue;
}

let online_path = entry.path().join("online");
```

Artinya:

> Semua power_supply selain battery/BMS yang memiliki `online` dianggap sebagai charger input.

Ini terlalu generik.

---

## Contoh

Android bisa mempunyai:

```text
/sys/class/power_supply/
├── battery/
├── bms/
├── usb/
├── ac/
├── wireless/
├── main/
├── dc/
└── parallel/
```

Tidak semuanya berarti:

> charger sedang terhubung.

Misalnya vendor tertentu mempunyai power_supply virtual:

```text
main
parallel
```

yang bukan input physical charger.

---

## Lebih baik gunakan konfigurasi node

Di `nodes.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub struct OnlineNodeConfig {
    pub path: &'static str,
    pub priority: u8,
}
```

Contoh:

```rust
pub const ONLINE_NODES: &[OnlineNodeConfig] = &[
    OnlineNodeConfig {
        path: "/sys/class/power_supply/usb/online",
        priority: 100,
    },
    OnlineNodeConfig {
        path: "/sys/class/power_supply/ac/online",
        priority: 90,
    },
    OnlineNodeConfig {
        path: "/sys/class/power_supply/wireless/online",
        priority: 80,
    },
    OnlineNodeConfig {
        path: "/sys/class/power_supply/dc/online",
        priority: 70,
    },
];
```

Lalu `CachedReader` hanya membuka node yang dikenal.

---

## Tetapi apakah harus hardcode?

Tidak harus.

Untuk proyek Anda, saya malah menyarankan dua level:

### Default profile

```text
usb
ac
wireless
dc
```

### Vendor profile

Misalnya:

```text
rosemary
```

dapat menentukan:

```text
battery
main
usb
```

tanpa mengubah core daemon.

Ini nanti menjadi dasar P3 #22.

---

## Semantik yang lebih baik

Jangan:

```text
ada satu online = 1
→ true
```

secara buta.

Gunakan:

```text
OnlineNodeObservation {
    online: bool,
    priority: u8,
}
```

Kemudian:

```text
any trusted input online
    → true

semua trusted input offline
    → false

tidak ada node valid
    → Unknown
```

Ini penting:

```text
Unknown != Offline
```

---

# 15. Satukan deadline verification + retry

Sekarang controller mempunyai:

```rust
verification: Option<Verification>
retry_at: Option<Instant>
```

tetapi:

```rust
next_deadline()
```

hanya melihat:

```rust
verification.deadline
```

Akibatnya scheduler harus tahu detail internal:

```rust
if hardware.sync == SyncState::Failed {
    ...
}
```

Ini bukan desain ideal.

---

## Seharusnya HardwareController yang menentukan kapan perlu dibangunkan

Buat:

```rust
pub fn next_deadline(&self) -> Option<Instant>
```

yang berarti:

> waktu paling awal controller membutuhkan perhatian.

Contohnya:

```rust
pub fn next_deadline(&self) -> Option<Instant> {
    match (
        self.verification.as_ref().map(|v| v.deadline),
        self.retry_at,
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
```

---

## Lalu scheduler tidak perlu tahu state internal

Sekarang:

```text
HardwareController
        ↓
next_deadline()
        ↓
monitor loop
        ↓
poll()
```

Jauh lebih bersih.

---

# 16. Hilangkan polling 2 detik ketika hanya menunggu retry

Ini berhubungan langsung dengan #15.

Sekarang ada:

```rust
if hardware.sync == SyncState::Failed {
    next_wake = next_wake.min(
        loop_now + Duration::from_secs(2)
    );
}
```

Ini membuat:

```text
failure
retry_at = +30 sec
```

tetapi daemon:

```text
t=0   wake
t=2   wake
t=4   wake
t=6   wake
...
t=30  retry
```

Padahal tidak ada pekerjaan di t=2,4,6,...

---

## Mengapa ini buruk?

Bukan fatal, tetapi untuk Android daemon:

```text
CPU wakeup
→ scheduler
→ poll
→ sensor read
→ decision
→ sleep
```

berulang 15 kali hanya untuk menunggu retry.

Tidak ada manfaat.

---

## Idealnya

```text
failure
   ↓
retry_at = +30s
   ↓
poll(timeout = 30s)
   │
   ├── IPC datang → wake
   │
   ├── netlink event → wake
   │
   └── timeout → retry
```

Dengan demikian:

```text
event-driven
+
deadline-driven
```

bukan:

```text
periodic polling
```

---

## Arsitektur final loop

```text
                ┌──────────────┐
                │ IPC socket   │
                └──────┬───────┘
                       │
                ┌──────┴───────┐
                │    poll()    │
                └──────┬───────┘
                       │
             ┌─────────┼─────────┐
             ↓         ↓         ↓
          IPC       Netlink   timeout
             │         │         │
             └─────────┼─────────┘
                       ↓
                  process cycle
```

Timeout berasal dari:

```text
min(
    scheduler deadline,
    verification deadline,
    retry deadline,
    netlink reconnect,
    netlink debounce
)
```

---

# 17. Rapikan Netlink backoff

Sekarang ada dua mekanisme:

```rust
schedule_reconnect()
```

dan:

```rust
try_reconnect()
```

Keduanya mengubah:

```rust
backoff
reconnect_at
```

Ini rawan divergence.

---

## Contoh

`try_reconnect()`:

```rust
self.reconnect_at = Some(now + self.backoff);
self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
```

Sedangkan `schedule_reconnect()`:

```rust
if self.reconnect_at.is_none() {
    self.reconnect_at = Some(now + self.backoff);
}

self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
```

Kalau keduanya dipanggil untuk error yang sama, backoff bisa meloncat lebih cepat dari yang dimaksud.

---

## Buat satu fungsi

Misalnya:

```rust
fn schedule_retry(&mut self, now: Instant) {
    self.reconnect_at = Some(now + self.backoff);
    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
}
```

Kemudian semua failure:

```rust
schedule_retry(now);
```

Sukses:

```rust
self.backoff = INITIAL_BACKOFF;
self.reconnect_at = None;
```

---

## Saya juga sarankan exponential backoff dengan jitter

Untuk daemon tunggal, jitter sebenarnya tidak wajib.

Tetapi kalau banyak daemon/device melakukan hal sama, jitter membantu:

```text
1s
2s
4s
8s
16s
32s
60s
```

menjadi misalnya:

```text
1.1s
2.3s
4.1s
7.6s
15.8s
31.2s
58.4s
```

Namun karena Anda membuat daemon Android single-device, **jitter adalah P3/P4**, bukan kebutuhan.

---

# 18. Tambahkan sensor sanity validation

Ini menurut saya salah satu P2 yang paling penting.

Sekarang:

```rust
s.parse::<u8>()
```

belum berarti:

> nilai valid.

---

## Capacity

Harus:

```rust
0..=100
```

Jadi:

```rust
let value = s
    .parse::<u8>()
    .map_err(...)?;

if value > 100 {
    return Err(...);
}

Ok(value)
```

---

## Temperature

Anda menggunakan deci-Celsius:

```text
250 = 25.0°C
400 = 40.0°C
```

Tetapi jangan menerima sembarang `i32`.

Buat sanity range, misalnya:

```text
-400 ..= 1000
```

yang berarti:

```text
-40°C ..= 100°C
```

Batas ini **bukan thermal policy**.

Ini hanya:

> apakah sensor value masuk akal?

Jadi jangan menggunakan:

```rust
cfg.max_temp_dc
```

untuk sanity validation.

Contoh:

```text
max_temp = 450  // 45°C

sensor = 800    // 80°C
```

Ini bukan hanya "di atas cutoff".

Ini tetap sensor valid secara fisik, hanya thermal policy harus disable.

Sedangkan:

```text
sensor = 9999
```

kemungkinan besar sensor corruption.

---

## Current

Current lebih rumit karena vendor.

Misalnya:

```text
+3000000 µA = +3000 mA
```

bisa valid untuk fast charging tertentu.

Jadi jangan terlalu agresif.

Buat batas sangat longgar:

```text
±20 A
```

atau configurable per hardware profile.

Kalau:

```text
+50000000 µA
```

mungkin:

```text
+50 A
```

dan kemungkinan besar invalid untuk smartphone.

---

## Voltage

Kalau digunakan:

```text
2.5V - 5.0V
```

atau vendor-specific.

Tetapi voltage bukan bagian snapshot sekarang.

Tidak perlu dipaksakan kalau belum digunakan.

---

# 🟢 P3 — Engineering Quality

P3 berbeda dari P2.

P2:

> membuat daemon lebih tahan.

P3:

> membuat daemon bisa dibuktikan, dipelihara, dan dikembangkan.

---

# 19. State-machine invariant tests

Ini **sangat saya rekomendasikan**.

Anda mempunyai state machine kompleks:

```text
DecisionEngine
HardwareController
Ownership
Verification
Retry
```

Testing harus fokus pada invariant, bukan hanya coverage.

---

## Contoh invariant

### Ownership

Jika:

```rust
ownership == Ownership::NotOwned
```

maka:

```text
daemon tidak boleh menghapus/restore hardware
```

---

### Owned

Jika:

```rust
ownership == Owned { .. }
```

maka:

```text
ownership.state harus ada
```

Secara test bisa dibuat abstract persistence backend supaya tidak harus menyentuh `/data/adb`.

---

### Synced

Jika:

```rust
sync == Synced
```

maka:

```text
verification tidak boleh aktif
```

dan:

```text
force_apply == false
```

---

### Failed

Jika:

```rust
sync == Failed
```

maka:

```text
force_apply == true
```

dan:

```text
retry_at.is_some()
```

---

## Contoh test

```rust
#[test]
fn partial_write_never_becomes_synced() {
    ...
}
```

Expected:

```text
partial write
    ↓
SyncState::Failed
```

bukan:

```text
Synced
```

---

# 20. Fault-injection tests

Ini lebih tinggi levelnya.

Normal unit test:

```text
set_charging(true)
→ Ok
```

Fault injection:

```text
node A → success
node B → EACCES
node C → ENODEV
```

Kemudian pastikan:

```text
partial failure
→ Failed
→ retry
```

---

## Test yang sangat penting

### Persistence failure

```text
write temp → EIO
```

Expected:

```text
ownership == NotOwned
```

---

### Rename failure

```text
write temp → success
rename → EIO
```

Expected:

```text
ownership == NotOwned
```

---

### Hardware partial write

```text
battery/charging_enabled = success
main/charging_enabled = failure
```

Expected:

```text
ChargingState::Mixed
```

atau:

```text
SyncState::Failed
```

---

### Verification unavailable

```text
control state = Disabled
current = None
```

Expected:

```text
NOT Synced
```

Ini akan menangkap bug `Ok(0)` yang kita bahas sebelumnya.

---

### Crash recovery

Simulasikan:

```text
ownership.state = 1
```

kemudian:

```text
daemon startup
```

Expected:

```text
set_charging(true)
clear state
```

---

# 21. Logging/metrics

Saat ini logging Anda sudah lumayan bagus:

```rust
tracing::info!
tracing::warn!
tracing::error!
tracing::debug!
```

Tetapi production daemon sebaiknya punya **structured events**.

Misalnya:

```text
event=decision
policy=limit_reached
target=charging_disabled
reason=charge_limit_reached
```

bukan hanya:

```text
Decision: policy=LimitReached ...
```

---

## Event penting

Saya akan membuat event taxonomy:

```text
daemon_started
daemon_stopped

ownership_acquired
ownership_recovery_started
ownership_recovery_succeeded
ownership_recovery_failed
ownership_released

hardware_apply_started
hardware_apply_succeeded
hardware_apply_partial
hardware_apply_failed

hardware_verification_started
hardware_verification_succeeded
hardware_verification_failed

sensor_fault
sensor_recovered

netlink_connected
netlink_disconnected
netlink_reconnect_failed

policy_changed
target_changed
```

---

## Metrics

Tidak perlu Prometheus.

Untuk Android Magisk daemon, cukup internal counters:

```rust
struct Metrics {
    hardware_apply_success: u64,
    hardware_apply_failure: u64,
    hardware_partial_failure: u64,

    verification_success: u64,
    verification_failure: u64,

    sensor_faults: u64,

    netlink_reconnects: u64,

    ownership_recoveries: u64,
}
```

Kemudian expose melalui IPC/status command jika diperlukan.

Misalnya:

```text
charger-control status
```

menghasilkan:

```text
Policy: Charging
Target: ChargingEnabled
Sync: Synced
Ownership: Owned
Capacity: 67%
Temperature: 31.2C
Current: 1840mA

Hardware applies: 32
Verification failures: 0
Sensor faults: 0
Netlink reconnects: 1
```

Untuk troubleshooting Android ini **sangat berguna**.

---

# 22. Hardware/vendor profile abstraction

Ini yang paling besar manfaatnya untuk masa depan.

Saat ini `nodes.rs` berisi:

```rust
CHARGING_NODES
SUSPEND_NODES
CURRENT_NODES
```

Artinya knowledge hardware tertanam langsung di core.

Misalnya Redmi Note 10S:

```text
battery/charging_enabled
battery/input_suspend
battery/current_now
```

Tetapi device lain mungkin:

```text
main/charging_enabled
usb/input_suspend
bms/current_now
```

---

## Buat `HardwareProfile`

Misalnya:

```rust
pub struct HardwareProfile {
    pub name: &'static str,

    pub charging_nodes:
        &'static [ChargingNodeConfig],

    pub suspend_nodes:
        &'static [SuspendNodeConfig],

    pub current_nodes:
        &'static [CurrentNodeConfig],

    pub online_nodes:
        &'static [OnlineNodeConfig],

    pub capacity_path: &'static str,

    pub temperature_path: &'static str,

    pub status_path: &'static str,
}
```

Kemudian:

```rust
pub const GENERIC_PROFILE: HardwareProfile = ...;
```

dan:

```rust
pub const ROSEMARY_PROFILE: HardwareProfile = ...;
```

---

# Bahkan lebih bagus: profile detection

Android memberi informasi seperti:

```text
ro.product.device
ro.product.vendor.device
ro.board.platform
```

Daemon bisa memilih:

```text
rosemary
   ↓
RosemaryProfile
```

atau:

```text
unknown device
   ↓
GenericProfile
```

---

# Jangan memasukkan vendor logic ke DecisionEngine

Ini sangat penting.

Jangan sampai:

```rust
if device == "rosemary" {
   ...
}
```

di dalam:

```text
DecisionEngine
```

Karena DecisionEngine seharusnya tidak tahu Android hardware.

Arsitektur yang benar:

```text
                    ┌──────────────────┐
                    │ Hardware Profile │
                    └────────┬─────────┘
                             ↓
                     CachedReader
                             ↓
                     SensorSnapshot
                             ↓
                     DecisionEngine
                             ↓
                  HardwareController
                             ↓
                     Hardware Profile
                             ↓
                        sysfs
```

DecisionEngine tetap portable.

---

# Saya bahkan akan membagi project menjadi 3 layer

Dengan target production-grade, saya akan menuju:

```text
charger-core/
│
├── policy/
│   └── decision.rs
│
├── hardware/
│   ├── controller.rs
│   ├── profile.rs
│   └── verification.rs
│
├── battery/
│   ├── reader.rs
│   ├── snapshot.rs
│   └── nodes.rs
│
└── persistence/
    └── ownership.rs


charger-daemon/
│
├── monitor.rs
├── scheduler.rs
├── netlink.rs
├── ipc.rs
└── signals.rs
```

Kemudian:

```text
charger-core
```

bisa dites tanpa Android.

Sedangkan:

```text
charger-daemon
```

bertanggung jawab terhadap Linux/Android runtime.

---

# Prioritas P2/P3 yang saya sarankan

Tidak semuanya harus dikerjakan sekaligus.

Saya akan urutkan:

### P2 — implementasikan sekarang

```text
13. Rescan capacity/temp/status       █████
14. Explicit online nodes              █████
15. Unified deadlines                  █████
16. Remove 2s failure polling          █████
17. Unified netlink backoff            ████
18. Sensor sanity validation           █████
```

Nomor **13, 15, 16, 18** terutama sangat worth it.

---

### P3 — setelah P0/P1/P2 stabil

```text
19. State-machine invariant tests      █████
20. Fault injection                    █████
21. Structured logging + metrics       ████
22. Hardware profiles                  ████
```

Kalau harus memilih:

> **19 + 20 lebih penting daripada 21 + 22.**

Karena daemon charger menyentuh hardware. Lebih baik punya 100 test state-machine yang kuat daripada dashboard metrics yang bagus tetapi state machine tidak terbukti.

---

# Target akhir

Setelah P2 + P3, saya ingin melihat kontrak berikut:

```text
Sensor
  │
  ├── valid
  └── invalid
       ↓
     Fault


Decision
  │
  └── DesiredTarget


Hardware
  │
  ├── apply
  ├── verify
  └── reconcile


Ownership
  │
  ├── acquire
  ├── persist
  ├── restore
  └── release


Scheduler
  │
  └── menentukan kapan bangun


Netlink
  │
  └── event-driven wakeup


Persistence
  │
  └── crash recovery


Tests
  │
  ├── invariants
  └── fault injection
```

Dengan pemisahan ini, daemon Anda tidak lagi sekadar **"program yang menulis sysfs berdasarkan battery percentage"**, tetapi menjadi **stateful hardware-control daemon** yang memiliki lifecycle, ownership, verification, reconciliation, persistence, recovery, dan testable failure semantics.

Dan menurut saya, **P3 #22 (hardware profile) sebaiknya dilakukan setelah P2 selesai**, karena profile abstraction akan jauh lebih mudah didesain setelah semantik `online`, `current`, `charging nodes`, dan sanity validation sudah final.
