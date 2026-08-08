Ya, **versi ini memang lebih baik** daripada sebelumnya. Beberapa kritik sebelumnya sudah benar-benar ditindaklanjuti: `resume_limit` sekarang masuk scheduler, prediksi sudah membedakan charging/discharging, socket leak sudah diperbaiki, thermal hysteresis sudah ada, dan filter Netlink lebih ketat. Misalnya perubahan pada scheduler terlihat di `next_interval(is_charging)` dan penggunaan `resume_limit`/`dist_to_resume`. 

Tetapi kalau targetnya **daemon battery charging Android/root yang production-grade**, saya masih melihat beberapa masalah nyata. Bahkan ada **2 bug logika yang menurut saya lebih penting daripada optimasi kecil**.

## 1. Masalah terbesar: `is_charging` sebenarnya bukan status charging

Ini bagian yang paling saya kritik:

```rust
let is_charging = stop_reason == StopReason::None;
let timeout = scheduler.next_interval(is_charging);
```



Nama `is_charging` menyesatkan.

`StopReason::None` berarti:

> daemon tidak sedang memblokir charging

bukan:

> hardware sedang charging.

Contoh:

```text
charger dicabut
↓
online = false
↓
stop_reason = None
↓
is_charging = true
```

Padahal charging jelas **tidak sedang berlangsung**.

Memang scheduler kemudian mendeteksi:

```rust
if !s.online {
    return UNPLUGGED_HEARTBEAT;
}
```



sehingga kasus ini tidak langsung fatal. Tetapi secara arsitektur, state yang dikirim ke scheduler **salah secara semantik**.

### Perbaikannya

Pisahkan:

```rust
online
charging
charging_blocked
```

Minimal:

```rust
enum ChargingState {
    Charging,
    NotCharging,
    LimitReached,
    ThermalCutoff,
}
```

atau lebih baik lagi:

```rust
struct PowerState {
    online: bool,
    charging: bool,
    capacity: u8,
    temperature: f32,
}
```

Kemudian scheduler menerima **status hardware aktual**, bukan inferensi dari `StopReason`.

---

# 2. Lebih serius: `set_charging(true)` tidak menjamin hardware benar-benar charging

Boot sync:

```rust
if enabled && initial_level < initial_limit {
    let _ = control::set_charging(true);
}
```



Masalahnya adalah Anda menganggap:

```text
set_charging(true)
        ↓
charging = true
```

Padahal pada Android/Linux, khususnya vendor kernel/charger framework, itu **tidak selalu identik**.

Bisa terjadi:

```text
set_charging(true)
        ↓
kernel accepts request
        ↓
charger IC / power_supply state berbeda
```

Jadi daemon sebaiknya **verify-after-write**.

Contohnya:

```rust
control::set_charging(true)?;

sleep(...);

let state = battery_reader.read_charging_state()?;

if !state {
    tracing::warn!("Charging enable requested but hardware did not enter charging state");
}
```

Kalau `CachedReader` belum punya API untuk status charging, saya justru akan menambahkan itu.

---

# 3. Thermal hysteresis sudah bagus, tetapi implementasinya masih hard-coded

Ini sudah merupakan improvement nyata:

```rust
let thermal_resume = max_temp_dc.saturating_sub(30);
```



dan:

```rust
if temp_dc <= thermal_resume && level < limit
```



Ini jauh lebih baik daripada versi sebelumnya.

Tetapi saya tidak suka:

```rust
30 // 3°C
```

hard-coded.

Lebih baik konfigurasi:

```rust
thermal_resume_hysteresis_dc: i32
```

Misalnya:

```text
max_temp = 420
hysteresis = 30
resume = 390
```

Dan **validasi konfigurasi**:

```rust
resume_temp < cutoff_temp
```

---

# 4. Ada potensi bug ketika konfigurasi berubah

Ini cukup penting.

Anda mengambil:

```rust
let cfg = config.read()...clone();
```

kemudian meng-update:

```rust
scheduler.limit = limit as f32;
scheduler.resume_limit = effective_resume as f32;
scheduler.thermal_cutoff = max_temp_dc as f32 / 10.0;
```



Tetapi state:

```rust
stop_reason
```

tidak disesuaikan dengan konfigurasi baru.

Contoh:

```text
limit = 80
battery = 80
→ LimitReached

user mengubah limit = 90

stop_reason masih LimitReached
```

Kemudian battery:

```text
80 <= resume_limit?
```

bisa menyebabkan charging tetap mati sampai turun ke resume threshold.

Padahal user baru saja menaikkan limit menjadi 90.

### Solusi

Ketika config berubah, lakukan:

```text
CONFIG_CHANGED
      ↓
validate config
      ↓
reconcile state
      ↓
evaluate battery
      ↓
apply charging decision
      ↓
recalculate scheduler
```

Jangan hanya:

```rust
should_evaluate = true;
```

---

# 5. `ema_cap_rate` masih terlalu mudah terkontaminasi noise fuel gauge

Ini:

```rust
self.ema_cap_rate =
    ALPHA * ((s.capacity - prev.capacity) / dt)
    + (1.0 - ALPHA) * self.ema_cap_rate;
```



secara matematis valid.

Masalahnya adalah **battery capacity integer bukan sensor kontinu**.

Misalnya:

```text
79
79
79
80
79
80
```

Anda bisa mendapatkan rate yang sangat tidak representatif.

Lebih buruk lagi:

```text
capacity = 79
→ 80
```

dalam 2 detik bukan berarti battery benar-benar naik 0.5%/sec.

### Saya sarankan

Jangan gunakan capacity saja.

Anda sudah membaca current:

```rust
let current = battery_reader.read_current_ma().unwrap_or(0.0);
```



tetapi kemudian hanya disimpan sebagai:

```rust
_current_ma
```

Artinya **informasi current sebenarnya dibuang dari algoritma scheduler**.

Ini opportunity besar.

Gunakan:

```rust
current_ma
```

untuk menentukan:

```text
charging
discharging
idle
```

misalnya:

```rust
const CURRENT_DEADBAND_MA: f32 = 50.0;
```

Kemudian:

```text
current > +deadband → charging
current < -deadband → discharging
else                → idle
```

**Tetapi tanda current harus diverifikasi terhadap device**, karena beberapa kernel Android menggunakan konvensi tanda berbeda.

---

# 6. Saya justru akan menghapus `f32` dari capacity

Capacity adalah:

```rust
capacity: f32
```

Padahal sumbernya:

```rust
read_capacity() -> u8
```



Tidak ada keuntungan berarti menggunakan `f32` untuk capacity.

Lebih bersih:

```rust
capacity: u8
```

Temperature boleh:

```rust
temp_dc: i32
```

atau:

```rust
temp_c: f32
```

Tetapi saya lebih suka **unit integer yang eksplisit** untuk battery daemon:

```rust
struct Sample {
    capacity_pct: u8,
    temp_dc: i32,
    current_ma: i32,
    online: bool,
    ts: Instant,
}
```

Ini mengurangi conversion dan ambiguity.

---

# 7. `Sample` sebaiknya menyimpan state charging

Sekarang:

```rust
struct Sample {
    capacity: f32,
    temp: f32,
    _current_ma: f32,
    online: bool,
    ts: Instant,
}
```



Saya akan ubah menjadi:

```rust
struct Sample {
    capacity_pct: u8,
    temp_dc: i32,
    current_ma: i32,
    online: bool,
    charging: bool,
    ts: Instant,
}
```

Dengan demikian scheduler bisa membedakan:

```text
ONLINE + CHARGING
ONLINE + NOT_CHARGING
OFFLINE
```

Ini jauh lebih kuat.

---

# 8. `StopReason` sebaiknya menjadi state machine yang lebih formal

Sekarang:

```rust
enum StopReason {
    None,
    LimitReached,
    ThermalCutoff,
}
```



Untuk versi production saya akan upgrade menjadi:

```rust
enum ChargeState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    Fault,
}
```

Kemudian transition:

```text
Disabled
   ↓
Offline
   ↓
Charging
   ↓
LimitReached
   ↓
Charging
```

dan:

```text
Charging
   ↓ temperature high
ThermalCutoff
   ↓ temperature safe
Charging
```

serta:

```text
any state
   ↓ read failure
Fault
```

Ini akan jauh lebih mudah di-debug.

---

# 9. Jangan `unwrap_or(0)` untuk sensor penting

Ini:

```rust
let level = battery_reader.read_capacity().unwrap_or(0);
let temp_dc = battery_reader.read_temperature_dc().unwrap_or(0);
```



Saya anggap **cukup berbahaya** untuk charging controller.

Misalnya battery reader gagal:

```text
read_temperature()
→ Err
→ 0°C
```

Daemon kemudian menganggap temperature sangat aman.

Atau:

```text
read_capacity()
→ Err
→ 0%
```

Kemudian:

```text
0% < limit
→ set_charging(true)
```

Ini bukan fail-safe yang bagus.

### Lebih baik

```rust
let level = match battery_reader.read_capacity() {
    Ok(v) => v,
    Err(e) => {
        tracing::error!("battery capacity read failed: {e}");
        enter_fault();
        continue;
    }
};
```

Untuk **charging safety**, saya lebih memilih:

> sensor failure → jangan melakukan perubahan charging state secara agresif.

---

# 10. Netlink sudah jauh lebih bagus, tetapi masih bisa ditingkatkan

Sekarang:

```rust
s.contains("SUBSYSTEM=power_supply")
    && s.contains("ACTION=change")
```



Ini cukup baik.

Tetapi parsing sebagai `String` setiap event sebenarnya tidak ideal.

Uevent Netlink berbentuk:

```text
KEY=value\0KEY=value\0...
```

Jadi Anda bisa melakukan byte scanning:

```rust
fn is_power_supply_change(buf: &[u8]) -> bool
```

tanpa:

```rust
String::from_utf8_lossy(...)
```

Keuntungannya:

* tidak perlu UTF-8 conversion,
* lebih murah,
* tidak membuat temporary string,
* lebih tepat untuk protocol yang memang byte-oriented.

Namun jujur: **ini optimasi kecil**. Jangan lakukan sebelum masalah state machine selesai.

---

# 11. `nl_pid` masih saya ubah

Anda masih menggunakan:

```rust
addr.nl_pid = std::process::id() as u32;
```



Saya lebih menyarankan:

```rust
addr.nl_pid = 0;
```

Biarkan kernel menentukan Netlink port ID.

---

# 12. `RawFd` sebaiknya tidak dibiarkan sebagai resource manual

Sekarang:

```rust
fn create_netlink_socket() -> Option<RawFd>
```

Kemudian Anda bertanggung jawab sendiri terhadap lifetime FD.

Untuk Rust modern, saya lebih suka:

```rust
OwnedFd
```

atau wrapper RAII.

Contoh konseptual:

```rust
fn create_netlink_socket() -> io::Result<OwnedFd>
```

Dengan begitu:

```text
function return
↓
drop
↓
close(fd)
```

otomatis.

Ini terutama penting jika daemon nantinya memiliki lifecycle/restart/reload lebih kompleks.

---

# 13. Ada masalah lifecycle: Netlink FD tidak pernah ditutup

Kalau:

```rust
nl_fd >= 0
```

kemudian:

```rust
return;
```

di:

```rust
if buf[0] == 2 {
    ...
    return;
}
```



FD tidak ditutup secara eksplisit.

OS memang akan membersihkannya ketika proses mati, jadi **bukan leak jangka panjang selama process exit**, tetapi lifecycle resource tetap tidak rapi.

RAII `OwnedFd` menyelesaikan ini.

---

# 14. `poll()` sebaiknya menangani error

Sekarang:

```rust
let ret = unsafe {
    libc::poll(...)
};
```



lalu:

```rust
if ret > 0 {
    ...
} else {
    // dianggap timeout
}
```

Ini problem.

`poll()` bisa:

```text
> 0 → event
= 0 → timeout
< 0 → error
```

Tetapi kode Anda memperlakukan:

```text
ret < 0
```

sebagai:

```text
timeout
```

Ini harus diperbaiki.

Minimal:

```rust
match ret {
    r if r > 0 => { ... }
    0 => {
        should_evaluate = true;
        break;
    }
    -1 if errno == EINTR => {
        now = Instant::now();
        continue;
    }
    -1 => {
        tracing::error!("poll failed");
        should_evaluate = true;
        break;
    }
    _ => unreachable!(),
}
```

**Ini saya masukkan kategori bug nyata**, bukan sekadar optimasi.

---

# 15. Poll `POLLERR | POLLHUP | POLLNVAL` juga perlu diperiksa

Saat ini hanya:

```rust
POLLIN
```

yang diperhatikan.

Untuk daemon:

```rust
pollfd {
    events: POLLIN,
}
```

Anda sebaiknya juga memeriksa:

```rust
POLLERR
POLLHUP
POLLNVAL
```

terutama untuk IPC.

Misalnya:

```rust
if pfds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
    ...
}
```

---

# 16. `recv()` IPC hanya membaca satu byte

Sekarang:

```rust
let mut buf = [0u8; 1];
```



Kalau protocol Anda memang:

```text
1 = reload
2 = shutdown
```

ini valid.

Tetapi kalau ke depan ingin:

```text
1 reload
2 shutdown
3 status
4 force-charge
5 force-stop
```

protocol akan cepat menjadi terbatas.

Saya sarankan minimal:

```rust
#[repr(u8)]
enum Command {
    Reload = 1,
    Shutdown = 2,
}
```

supaya magic number hilang.

---

# 17. Scheduler prediction masih bisa dibuat jauh lebih cerdas

Sekarang:

```rust
dist / rate * 0.5
```



Saya kurang suka magic:

```rust
* 0.5
```

Kenapa 0.5?

Kalau maksudnya safety factor, namakan:

```rust
const PREDICTION_SAFETY_FACTOR: f32 = 0.5;
```

Tetapi bahkan lebih baik:

```text
ETA
↓
safety margin
↓
minimum interval
↓
maximum interval
```

Contoh:

```rust
let eta = distance / rate;
let safe_eta = eta * 0.5;
```

jelas maksudnya.

---

# 18. Tetapi saya justru tidak akan terlalu mengandalkan ETA capacity

Untuk battery charger daemon Android, pendekatan yang lebih robust adalah:

```text
Event-driven
    +
Adaptive timer
    +
Threshold proximity
    +
Temperature trend
    +
Current state
```

bukan:

```text
capacity prediction
        ↓
ETA
        ↓
sleep
```

Karena fuel gauge Android sangat tidak linear.

Jadi saya akan membuat scheduler berbasis **urgency**, bukan murni prediction.

Misalnya:

```text
URGENT
  2 sec

NEAR_LIMIT
  5 sec

THERMAL_RISING
  3–5 sec

NORMAL_CHARGING
  15–30 sec

STABLE
  60–90 sec

OFFLINE
  10 min
```

Event Netlink tetap dapat membangunkan daemon kapan saja.

Ini kemungkinan lebih reliable daripada mencoba memprediksi SOC terlalu presisi.

---

# 19. Ada satu optimasi yang sangat saya rekomendasikan: jangan membaca semua sensor setiap event

Sekarang setiap wake:

```rust
read_capacity()
read_temperature()
read_current()
is_plugged_in()
```



Netlink `power_supply` event bisa terjadi cukup sering.

Misalnya kernel mengirim beberapa event:

```text
capacity
current
voltage
status
health
temperature
```

Daemon Anda bisa bangun beberapa kali.

Lebih baik tambahkan **debounce/coalescing**:

```text
event 1
event 2
event 3
event 4
   ↓
wait 100–300 ms
   ↓
read sensors ONCE
```

Tetapi jangan gunakan debounce panjang ketika:

```text
temperature >= cutoff
```

atau kondisi safety.

---

# 20. Saya akan menambahkan "reason" pada setiap keputusan

Saat ini log sudah cukup bagus:

```text
Limit reached
Charging resumed
Temperature normal
```

Tetapi untuk debugging daemon, saya ingin:

```text
Decision:
  state        = Charging
  capacity     = 77%
  limit        = 80%
  temperature  = 38.2°C
  thermal_max  = 42.0°C
  current      = 1840mA
  online       = true
  cap_rate     = +0.002%/s
  temp_rate    = +0.018°C/s
  next_wakeup  = 30s
  reason       = NORMAL_CHARGING
```

Dengan begitu ketika user melaporkan:

> "kok charging telat?"

Anda bisa langsung melihat alasannya.

---

# 21. Saya sarankan membuat `Decision` immutable

Ini upgrade arsitektur yang menurut saya sangat berharga.

Daripada logic:

```rust
if thermal ...
else if limit ...
else if resume ...
```

langsung memanggil `set_charging()`,

buat:

```rust
enum ChargeCommand {
    Enable,
    Disable,
    Noop,
}
```

dan:

```rust
struct Decision {
    command: ChargeCommand,
    state: ChargeState,
    reason: DecisionReason,
}
```

Flow:

```text
SensorSnapshot
      ↓
evaluate()
      ↓
Decision
      ↓
apply()
      ↓
verify()
```

Ini jauh lebih mudah dites.

---

# Arsitektur yang saya rekomendasikan

Kalau ini proyek yang serius, saya akan upgrade menjadi:

```text
                    ┌──────────────────┐
                    │ Config Manager   │
                    └────────┬─────────┘
                             │
                             ▼
┌──────────────┐      ┌───────────────┐
│ Netlink      │─────►│ Event Loop    │◄──── IPC
│ power_supply │      └───────┬───────┘
└──────────────┘              │
                              ▼
                    ┌──────────────────┐
                    │ Battery Reader   │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ SensorSnapshot   │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ State Machine    │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ Decision Engine  │
                    └────────┬─────────┘
                             │
                       Enable/Disable
                             │
                             ▼
                    ┌──────────────────┐
                    │ Charging Control │
                    └────────┬─────────┘
                             │
                             ▼
                         Verify
```

Scheduler berada di samping decision engine:

```text
SensorSnapshot
      │
      ├────► State Machine
      │
      └────► Adaptive Scheduler
                    │
                    ▼
               next wakeup
```

---

# Prioritas perbaikan

Kalau saya yang maintain kode ini, saya **tidak akan langsung melakukan optimasi micro-level**. Urutannya:

### P0 — wajib

1. Perbaiki `poll()` error handling.
2. Jangan gunakan `unwrap_or(0)` untuk sensor safety-critical.
3. Pisahkan `is_charging` dari `stop_reason`.
4. Tambahkan verifikasi setelah `set_charging()`.
5. Tangani config reload terhadap state machine.
6. Gunakan thermal hysteresis configurable.

### P1 — sangat disarankan

7. `SensorSnapshot`.
8. `ChargeState` formal.
9. `Decision` + `DecisionReason`.
10. Gunakan current untuk menentukan charging/discharging.
11. Debounce/coalesce Netlink event.
12. Hilangkan magic numbers.

### P2 — optimasi

13. `OwnedFd` / RAII.
14. Byte-level Netlink parser.
15. Kurangi allocation `Vec<pollfd>`.
16. Hindari `f32` untuk nilai integer.
17. Perbaiki EMA/rate estimator.

### P3 — advanced

18. Adaptive urgency scheduler.
19. Sensor confidence / stale-data detection.
20. Fault state + recovery.
21. Metrics/statistics.
22. Unit test state transitions.
23. Integration test menggunakan fake battery reader.

---

## Penilaian saya terhadap versi sekarang

Dibanding kode sebelumnya:

**sebelumnya ~6.5/10 → sekarang ~7.8/10.**

Perbaikannya nyata, bukan kosmetik.

Yang paling bagus adalah Anda sudah memperbaiki beberapa hal yang saya kritik sebelumnya:

* `resume_limit` sekarang benar-benar masuk scheduler. 
* charging/discharging prediction sudah dipisahkan. 
* FD ditutup ketika `bind()` gagal. 
* thermal hysteresis sudah ditambahkan. 
* Netlink sudah memfilter `SUBSYSTEM=power_supply` + `ACTION=change`. 

**Tapi saya belum akan menyebutnya production-grade**, terutama karena `is_charging` masih merupakan inferensi dari `stop_reason`, pembacaan sensor gagal diperlakukan sebagai nilai aman (`0`), dan error `poll()` belum dibedakan dari timeout.

Kalau tujuan akhirnya adalah **daemon Rust Android yang stabil 24/7, hemat wakeup, aman terhadap sensor failure, dan tahan terhadap perubahan config/vendor kernel**, upgrade terbesar berikutnya bukan memperumit EMA. **Bangun `SensorSnapshot → State Machine → Decision → Apply → Verify` terlebih dahulu.** Itu akan memberikan peningkatan reliability yang jauh lebih besar daripada optimasi kecil pada scheduler.
