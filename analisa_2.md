Nah, **sekarang arsitekturnya sudah jauh lebih matang**. Anda sudah bergerak dari sekadar polling battery menjadi:

**snapshot → state machine → decision → apply → verify → adaptive/event-driven wait.**

Itu arah yang benar.

Tetapi saya akan cukup kritis: **saya belum akan menyebut kode ini production-grade**. Ada beberapa masalah yang sekarang justru muncul karena arsitekturnya semakin kompleks. Beberapa adalah bug nyata, bukan sekadar style.

## Penilaian sekarang

| Area                     |       Nilai |
| ------------------------ | ----------: |
| Architecture             |    **9/10** |
| State machine            |    **8/10** |
| Event-driven design      |  **8.5/10** |
| Safety                   |    **7/10** |
| Scheduler                |  **7.5/10** |
| Rust resource management |  **8.5/10** |
| Error handling           |    **7/10** |
| Android robustness       |    **7/10** |
| Maintainability          |    **8/10** |
| Production readiness     | **~7.8/10** |

Perubahan yang paling bagus:

* `SensorSnapshot`
* `ChargeState`
* `ChargeCommand`
* `Decision`
* `DecisionEngine`
* `OwnedFd`
* byte-level Netlink parsing
* Netlink debounce
* `poll()` error handling
* thermal hysteresis
* sensor fault state
* separation antara decision dan apply

Ini sudah bukan sekadar patch kecil lagi.

---

# 🔴 1. BUG PALING PENTING: `Disabled` akan mengaktifkan charging

Ini saya anggap **bug nyata**.

Anda punya:

```rust
if !cfg.enabled {
    self.state = ChargeState::Disabled;
    return Decision {
        command: ChargeCommand::Enable,
        state: ChargeState::Disabled,
        reason: "Daemon Disabled"
    };
}
```

Ini berarti:

> daemon disabled → `set_charging(true)`

Secara policy mungkin memang maksud Anda adalah "kembalikan charging normal". Tetapi nama `ChargeCommand::Enable` membuat semantic-nya rancu.

Lebih parah lagi, di `Apply & Verify`:

```rust
if prev_state != decision.state {
    control::set_charging(true)
}
```

Jadi saat:

```text
Charging
↓
user disable daemon
↓
Disabled
↓
Enable charging
```

Secara efek mungkin benar, tetapi **state machine sedang menggunakan command "Enable" untuk "restore unmanaged charging"**.

Saya sarankan pisahkan:

```rust
enum ChargeCommand {
    Enable,
    Disable,
    ReleaseControl,
    Noop,
}
```

Kemudian:

```rust
Disabled => ReleaseControl
```

Ini jauh lebih jelas.

---

# 🔴 2. BUG: `Offline` tidak mengembalikan charging state

Anda melakukan:

```rust
if !snapshot.online {
    self.state = ChargeState::Offline;
    return Decision {
        command: ChargeCommand::Noop,
        ...
    };
}
```

Ini benar secara umum.

Tetapi ketika:

```text
ThermalCutoff
↓
charger dicabut
↓
Offline
```

kemudian charger dipasang kembali:

```text
Offline
↓
Charging
```

karena:

```rust
ChargeState::Offline | Fault => {
    self.state = ChargeState::Charging;
    self.evaluate(...)
}
```

Ini cukup masuk akal.

**Tetapi** Anda tidak memverifikasi apakah hardware benar-benar sudah masuk charging.

Jadi masalah yang saya kritik sebelumnya masih ada.

---

# 🔴 3. `Fault` tidak benar-benar menjadi Fault state

Ini bagian yang sangat penting:

```rust
ChargeState::Disabled | ChargeState::Offline | ChargeState::Fault => {
    self.state = ChargeState::Charging;
    self.evaluate(snapshot, cfg)
}
```

Artinya:

```text
Fault
 ↓
next successful sensor read
 ↓
Charging
```

Jadi `Fault` sebenarnya cuma **transient marker**, bukan state fault yang sesungguhnya.

Lebih buruk:

```rust
engine.state = ChargeState::Fault;
control::set_charging(false);
```

kemudian 5 detik kemudian sensor berhasil:

```rust
Fault → Charging
```

tanpa recovery policy.

### Saya sarankan:

```rust
Fault {
    retry_count,
    entered_at,
}
```

atau minimal:

```rust
ChargeState::Fault
```

harus punya explicit recovery:

```text
Fault
 ↓
sensor valid N kali berturut-turut
 ↓
Recovery
 ↓
Charging
```

Misalnya:

```text
1 successful read → belum cukup
3 successful reads → recover
```

Ini mencegah sensor flapping.

---

# 🔴 4. `unwrap()` setelah `is_err()` masih tidak ideal

Anda punya:

```rust
if capacity_pct.is_err() || temp_dc.is_err() {
    ...
    continue;
}
```

kemudian:

```rust
capacity_pct.unwrap()
temp_dc.unwrap()
```

Secara logika sekarang memang aman.

Tapi Rust yang lebih bagus:

```rust
let (capacity_pct, temp_dc) = match (
    battery_reader.read_capacity(),
    battery_reader.read_temperature_dc(),
) {
    (Ok(capacity), Ok(temp)) => (capacity, temp),
    (cap, temp) => {
        tracing::error!("Sensor failure: cap={cap:?}, temp={temp:?}");
        ...
        continue;
    }
};
```

Lebih readable dan tidak ada `unwrap()` di safety-critical code.

---

# 🔴 5. Anda masih memiliki masalah sensor `current`

Ini:

```rust
let current = current_ma.unwrap_or(0.0) as i32;
```

Artinya:

```text
current sensor gagal
↓
current = 0
↓
charging = false
```

Padahal `_charging` kemudian:

```rust
_charging: current > CURRENT_DEADBAND_MA
```

Jadi sensor current failure diam-diam berubah menjadi:

> "battery tidak charging."

Ini dangerous untuk scheduler.

### Harusnya

Current menjadi optional:

```rust
current_ma: Option<i32>
```

dan:

```rust
charging: Option<bool>
```

Jadi:

```text
Some(+1500) → charging
Some(-800)  → discharging
Some(10)    → idle
None        → unknown
```

---

# 🔴 6. `_charging` dan `_current_ma` jangan diberi underscore

Anda sekarang punya:

```rust
_current_ma
_charging
```

Padahal keduanya **dipakai**.

Contoh:

```rust
prev._charging != s._charging
```

Jadi underscore tidak lagi bermakna "unused".

Ubah:

```rust
current_ma: i32,
charging: bool,
```

Ini kecil tetapi penting untuk readability.

---

# 🔴 7. Lebih penting lagi: charging detection Anda belum tentu benar di Android

Anda menggunakan:

```rust
current > CURRENT_DEADBAND_MA
```



Tetapi ini sangat vendor-dependent.

Beberapa Android/kernel bisa memberikan:

```text
charging current > 0
```

sementara yang lain bisa memakai konvensi berbeda.

Jangan menganggap:

```rust
current > 50
```

sebagai ground truth.

Idealnya `CachedReader` expose:

```rust
read_status()
```

misalnya:

```text
Charging
Discharging
NotCharging
Full
Unknown
```

dari `POWER_SUPPLY_STATUS`.

Current kemudian menjadi **secondary signal**, bukan primary state.

---

# 🟠 8. Scheduler sekarang masih menggunakan parameter `is_charging` dari state machine

Anda punya:

```rust
scheduler.next_interval(decision.state == ChargeState::Charging)
```

Masih ada masalah konseptual.

`ChargeState::Charging` berarti:

> policy mengizinkan charging.

Bukan:

> hardware sedang charging.

Jadi:

```text
charger plugged
policy = Charging
hardware = NotCharging
```

scheduler tetap menganggap:

```rust
is_charging = true
```

Ini seharusnya:

```rust
scheduler.next_interval(snapshot.charging)
```

atau lebih baik:

```rust
scheduler.next_interval(snapshot.power_state)
```

---

# 🟠 9. `DecisionEngine` melakukan recursion

Ini:

```rust
self.state = ChargeState::Charging;
self.evaluate(snapshot, cfg)
```

dan:

```rust
self.state = ChargeState::Charging;
self.evaluate(snapshot, cfg)
```

Memang recursion-nya maksimal satu-dua level, jadi **bukan stack problem**.

Tapi desainnya kurang bagus.

Lebih bersih:

```rust
match self.state {
    ...
}
```

langsung menentukan transition.

Atau buat:

```rust
fn transition(...) -> ChargeState
```

dan:

```rust
fn command_for(state) -> ChargeCommand
```

Dengan begitu engine lebih deterministic dan mudah di-unit-test.

---

# 🟠 10. Ada bug logic thermal ketika thermal cutoff disabled

Anda menghitung:

```rust
let thermal_resume =
    thermal_max.saturating_sub(cfg.thermal_resume_hysteresis_dc);
```

tetapi:

```rust
if cfg.thermal_cutoff && snapshot.temp_dc >= thermal_max
```

hanya cutoff jika enabled.

Kemudian dalam state:

```rust
ChargeState::ThermalCutoff => {
    if snapshot.temp_dc <= thermal_resume {
```

Tidak masalah kalau state thermal tidak pernah tercipta ketika disabled.

Tetapi jika config berubah:

```text
thermal cutoff ON
↓
ThermalCutoff
↓
config thermal cutoff OFF
```

engine tidak memiliki explicit reconciliation.

Ini kembali ke masalah config reload.

---

# 🔴 11. Config reload masih terlalu manual

Ini:

```rust
if engine.state == ChargeState::LimitReached
    || engine.state == ChargeState::ThermalCutoff
{
    engine.state = ChargeState::Charging;
}
```

Saya tidak suka pendekatan ini.

Karena Anda mulai punya:

```text
Disabled
Offline
Charging
LimitReached
ThermalCutoff
Fault
```

Setiap state/config interaction akan menjadi:

```rust
if state == X || state == Y ...
```

lama-lama akan menjadi spaghetti.

Lebih baik:

```rust
engine.reconfigure(&cfg);
```

Contoh:

```rust
fn reconfigure(&mut self, cfg: &Config) {
    // validate
    // reconcile state
}
```

---

# 🟠 12. `initial_cfg` scheduler hard-code `95, 420`

Ini:

```rust
let mut scheduler =
    AdaptiveScheduler::new(initial_cfg.charge_limit, 95, 420);
```

Padahal Anda punya config:

```rust
cfg.resume_limit
cfg.max_temp_dc
```

Jadi ini inkonsisten.

Seharusnya:

```rust
let resume = effective_resume(&initial_cfg);

let mut scheduler = AdaptiveScheduler::new(
    initial_cfg.charge_limit,
    resume,
    initial_cfg.max_temp_dc,
);
```

Kalau tidak, scheduler punya nilai awal yang salah sampai loop berikutnya mengoreksinya.

---

# 🔴 13. `thermal_resume_hysteresis_dc` harus divalidasi

Misalnya:

```text
max_temp = 420
hysteresis = 500
```

Maka:

```rust
saturating_sub()
```

menghasilkan:

```text
0°C
```

Akibatnya daemon baru resume pada 0°C.

Config validation harus memastikan:

```rust
hysteresis > 0
hysteresis < max_temp_dc
```

Saya akan melakukan ini saat config load, bukan di monitor loop.

---

# 🟠 14. `Fault → set_charging(false)` belum tentu merupakan fail-safe terbaik

Anda memilih:

```rust
control::set_charging(false);
```

Ini **bisa benar**, tetapi perlu dibedakan antara:

### Safety policy

Jika:

```text
temperature sensor unavailable
```

maka disable charging masuk akal.

Tetapi kalau:

```text
capacity sensor unavailable
```

sementara temperature masih valid:

```text
disable charging
```

mungkin terlalu agresif.

Saya lebih suka:

```text
Critical sensor failure
→ disable

Non-critical sensor failure
→ retain current state + retry
```

Misalnya:

| Sensor      | Failure               |
| ----------- | --------------------- |
| Temperature | Disable               |
| Capacity    | Conservative fallback |
| Current     | Unknown               |
| Online      | Unknown               |
| Status      | Unknown               |

Ini perlu disesuaikan dengan kemampuan `charger_core`.

---

# 🟠 15. Verification hanya dilakukan ketika `Disable`

Ini:

```rust
ChargeCommand::Disable => {
    ...
    read_current_ma()
}
```

Tetapi `Enable` tidak diverifikasi.

Anda seharusnya punya:

```text
Apply Enable
      ↓
wait 200–500ms
      ↓
read status/current
      ↓
verify
```

Kalau tidak:

```text
set_charging(true)
```

bisa gagal diam-diam.

Saya akan buat:

```rust
apply_command()
verify_command()
```

sebagai dua tahap eksplisit.

---

# 🟠 16. `sleep(300ms)` memblokir event loop

Ini:

```rust
std::thread::sleep(Duration::from_millis(300));
```

merupakan satu-satunya bagian yang menurut saya agak bertentangan dengan desain event-driven Anda.

Selama 300 ms:

* IPC tidak diproses,
* Netlink tidak diproses,
* shutdown tertunda.

300 ms memang kecil, tetapi bisa dihilangkan.

Gunakan verification deadline di event loop:

```text
command applied
↓
verification deadline = now + 300ms
↓
poll()
↓
verify
```

Kalau ingin benar-benar clean.

---

# 🟢 17. `OwnedFd` sudah bagus

Ini improvement yang saya setujui:

```rust
fn create_netlink_socket() -> Option<OwnedFd>
```

dan:

```rust
OwnedFd::from_raw_fd(fd)
```



RAII jauh lebih bagus daripada `RawFd`.

Tetapi Rust modern bisa dibuat lebih idiomatis lagi dengan `std::os::fd` daripada:

```rust
std::os::unix::io
```

Pada toolchain modern:

```rust
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
```

Lebih portable terhadap evolusi API Rust.

---

# 🟢 18. Zero-allocation `pollfd` bagus

Ini:

```rust
let mut pfds = [
    ...
];
```

dan:

```rust
let mut num_fds = 1;
```

adalah optimasi yang masuk akal.

Anda juga menghilangkan:

```rust
Vec<pollfd>
```

dari inner loop.

**Saya setuju dengan perubahan ini.**

Tetapi jangan terlalu mengejar "zero allocation". Bottleneck daemon seperti ini bukan allocation `pollfd`; battery sysfs reads dan wakeups jauh lebih signifikan.

---

# 🟢 19. Byte-level Netlink parsing juga bagus

Ini:

```rust
contains_subslice(...)
```

lebih cocok daripada:

```rust
String::from_utf8_lossy()
```



Tetapi optimasi ini juga bukan prioritas utama.

Kalau Netlink hanya event battery, syscall + sensor reads akan jauh lebih mahal daripada `windows()`.

---

# 🔴 20. Netlink debounce Anda belum sepenuhnya optimal

Anda sudah punya:

```rust
const NETLINK_DEBOUNCE: Duration =
    Duration::from_millis(250);
```

Bagus.

Namun:

```rust
if debounce_target.is_none() {
    debounce_target = Some(now + NETLINK_DEBOUNCE);
}
```

Artinya event berikutnya dalam window 250 ms **tidak memperpanjang debounce**.

Itu sebenarnya bagus kalau tujuan Anda adalah leading-edge debounce.

Namun istilah yang tepat lebih dekat ke:

> **coalescing window**

daripada debounce.

Saya justru menyukai perilaku sekarang untuk battery daemon karena mencegah event storm.

---

# 🔴 21. Tapi ada masalah besar dengan event Netlink: Anda tidak mengecek `POLLERR`

Untuk Netlink:

```rust
if nl_events & libc::POLLIN != 0
```

Sebaiknya:

```rust
if nl_events & (POLLERR | POLLHUP | POLLNVAL) != 0 {
    // recreate socket / fallback
}
```

Saat ini kalau Netlink socket rusak setelah daemon berjalan:

```text
nl_sock exists
↓
socket malfunction
↓
no event
↓
scheduler timer tetap berjalan
```

Daemon tidak pernah mencoba recover.

Saya akan membuat:

```text
Netlink failure
    ↓
close
    ↓
recreate
    ↓
if fail → timer fallback
```

---

# 🔴 22. `create_netlink_socket()` tidak memberi alasan error

Sekarang:

```rust
Option<OwnedFd>
```

Saya lebih suka:

```rust
io::Result<OwnedFd>
```

Supaya log bisa:

```text
socket() failed: Permission denied
bind() failed: Address already in use
```

daripada:

```text
Failed to bind Netlink socket
```

Untuk Android vendor debugging, informasi ini sangat berguna.

---

# 🟠 23. Scheduler masih terlalu agresif ketika normal

Anda punya:

```rust
MAX_INTERVAL = 90 sec
```

Ini sebenarnya cukup reasonable.

Tapi:

```rust
UNPLUGGED_HEARTBEAT = 600 sec
```

jika charger dicabut dan kemudian dipasang kembali, Netlink akan membangunkan daemon.

Bagus.

Jadi heartbeat 10 menit memang bisa dipertahankan sebagai fallback.

---

# 🟠 24. `ema_temp_rate` reset ketika charging berubah

Ini:

```rust
if prev._charging != s._charging {
    self.ema_cap_rate = 0.0;
    self.ema_temp_rate = 0.0;
}
```

saya setuju untuk `ema_cap_rate`.

Tapi **tidak yakin untuk temperature**.

Temperature trend tidak selalu harus reset ketika charging state berubah.

Contoh:

```text
charging
temp +0.2°C/min
↓
charging disabled
↓
temperature masih naik +0.2°C/min
```

Thermal safety justru masih membutuhkan trend tersebut.

Saya akan:

```rust
if charging_changed {
    ema_cap_rate = 0.0;
}
```

tetapi mempertahankan:

```rust
ema_temp_rate
```

atau memberikan decay lebih cepat.

---

# 🟢 25. Saya suka constants sekarang

Ini improvement yang bagus:

```rust
const CURRENT_DEADBAND_MA: i32 = 50;
const DANGER_TEMP_MARGIN: f32 = 3.0;
const DANGER_CAP_MARGIN: f32 = 2.0;
const EMA_ALPHA: f32 = 0.3;
const NETLINK_DEBOUNCE: Duration = Duration::from_millis(250);
```

Daripada magic numbers tersebar.

Tapi saya masih akan membuat:

```rust
const PREDICTION_SAFETY_FACTOR: f32 = 0.5;
const TEMP_RATE_DANGER: f32 = 0.15;
const EMA_HISTORY_LEN: usize = 5;
const VERIFY_DELAY: Duration = ...
```

---

# Yang paling saya sarankan sekarang: ubah struktur sedikit

Kode Anda sudah hampir sampai. Saya **tidak menyarankan rewrite total**.

Saya akan membuat 5 komponen:

```text
SensorSnapshot
      │
      ▼
SensorReader
      │
      ▼
DecisionEngine
      │
      ▼
ChargeCommand
      │
      ▼
ChargeController
      │
      ▼
Verification
```

Scheduler terpisah:

```text
SensorSnapshot
      │
      ▼
AdaptiveScheduler
      │
      ▼
next_wakeup
```

### Dengan struktur:

```rust
struct SensorSnapshot {
    capacity_pct: u8,
    temp_dc: i32,
    current_ma: Option<i32>,
    online: Option<bool>,
    charging: Option<bool>,
    ts: Instant,
}
```

Kemudian:

```rust
struct Decision {
    command: ChargeCommand,
    next_state: ChargeState,
    reason: DecisionReason,
}
```

dan:

```rust
enum DecisionReason {
    Disabled,
    Offline,
    Normal,
    LimitReached,
    LimitResume,
    ThermalCutoff,
    ThermalResume,
    SensorFault,
}
```

Saya **lebih memilih enum daripada `&'static str`**.

---

# Dan satu perubahan yang sangat penting

Sekarang Anda punya:

```rust
reason: &'static str
```

Ini nyaman untuk logging, tapi kurang bagus untuk logic/metrics.

Gunakan:

```rust
#[derive(Debug, Clone, Copy)]
enum DecisionReason {
    DaemonDisabled,
    ChargerOffline,
    NormalCharging,
    ChargeLimitReached,
    WaitingForLimitResume,
    ThermalLimitReached,
    WaitingForThermalResume,
    SensorFault,
}
```

Kemudian:

```rust
impl Display for DecisionReason
```

Jadi bisa:

```text
reason = ThermalLimitReached
```

dan logging:

```text
reason = "thermal_limit_reached"
```

Lebih mudah untuk statistik.

---

# Ada satu upgrade yang saya anggap "next level"

Tambahkan **confidence/staleness**.

Misalnya sensor berhasil dibaca:

```text
capacity = 80
temperature = 39.2
current = ERROR
```

Snapshot jangan dianggap sama dengan snapshot lengkap.

Gunakan:

```rust
struct SensorSnapshot {
    capacity_pct: Option<u8>,
    temp_dc: Option<i32>,
    current_ma: Option<i32>,
    online: Option<bool>,
    charging: Option<bool>,
    ts: Instant,
}
```

Lalu policy:

```text
temperature unknown
    → conservative

capacity unknown
    → conservative

current unknown
    → scheduler prediction disabled

online unknown
    → timer fallback
```

Ini jauh lebih cocok untuk Android yang hardware/vendor behavior-nya tidak seragam.

---

# Kesimpulan akhir

**Ini sudah lompatan besar dari versi pertama.**

Yang menurut saya sudah benar-benar bagus:

```text
                ┌────────────────┐
                │ SensorSnapshot │
                └───────┬────────┘
                        ↓
                ┌────────────────┐
                │ DecisionEngine │
                └───────┬────────┘
                        ↓
                  ChargeCommand
                        ↓
                Apply + Verify
                        ↓
                Adaptive Timer
                        ↑
                  Netlink Event
```

Itu **arsitektur yang tepat** untuk daemon seperti ini.

Tetapi sebelum Anda menambahkan fitur lain, saya akan memperbaiki **7 hal ini dulu**:

1. **Jangan gunakan `state == Charging` sebagai hardware charging status.**
2. **`current_ma` failure jangan diubah menjadi `0`.**
3. **Buat `Fault` punya recovery policy nyata.**
4. **Pisahkan `ReleaseControl` dari `Enable`.**
5. **Hilangkan hard-coded `95, 420` pada scheduler initialization.**
6. **Handle Netlink `POLLERR/HUP/NVAL` + reconnect.**
7. **Hilangkan `sleep(300ms)` dan jadikan verification non-blocking.**

Setelah itu, saya akan fokus pada **scheduler**. Menurut saya scheduler adalah bagian yang masih paling "heuristic": EMA 5 sample, threshold `0.01`, safety factor `0.5`, margin 2%/3°C, dan ramp `1.5×` semuanya masuk akal, tetapi belum punya dasar model yang konsisten. Itu bukan bug, tetapi merupakan bagian yang paling layak di-upgrade setelah state machine benar-benar solid.
