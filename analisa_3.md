Sekarang **sudah jauh lebih solid**. Saya melihat Anda sudah menerapkan hampir semua perbaikan penting dari review sebelumnya: `BatteryStatus`, optional sensor, `Fault { retry_count }`, `ReleaseControl`, `DecisionReason`, `reconfigure()`, scheduler memakai snapshot aktual, `OwnedFd`, reconnect Netlink, dan verification asynchronous. Struktur dasarnya sekarang sudah layak dijadikan fondasi production daemon. 

Namun setelah membaca versi penuh 550 baris ini, saya menemukan beberapa **masalah baru yang cukup penting**. Ada **3 yang saya anggap harus diperbaiki sebelum dianggap production-grade**.

---

# 1. 🔴 BUG: Fault recovery counter terbalik

Ini yang paling penting.

Anda membuat:

```rust
ChargeState::Fault { retry_count: u8 }
```

lalu ketika sensor temperature gagal:

```rust
self.state = ChargeState::Fault {
    retry_count: FAULT_RECOVERY_READS
};
```



Kemudian:

```rust
if retry_count > 0 {
    self.state = ChargeState::Fault {
        retry_count: retry_count - 1
    };

    return Disable;
}
```



Masalahnya adalah ketika `retry_count == 0`:

```rust
self.state = ChargeState::Charging;
```



Jadi urutannya:

```text
sensor failure
    ↓
Fault(3)
    ↓
Fault(2)
    ↓
Fault(1)
    ↓
Fault(0)
    ↓
Charging
```

**Tetapi ini bukan "3 successful recovery reads".**

Justru Anda mengurangi counter pada setiap `evaluate()` sementara sensor temperature sudah valid.

Dengan kata lain, nama:

```text
FAULT_RECOVERY_READS
```

tidak menggambarkan implementasinya.

### Yang lebih benar

Anda membutuhkan:

```rust
fault_recovery_count: u8
```

yang hanya bertambah ketika:

```text
sensor valid
```

Contoh:

```text
Fault
 ↓ valid read #1
Fault(recovery=1)
 ↓ valid read #2
Fault(recovery=2)
 ↓ valid read #3
Charging
```

Bukan countdown.

Saya sarankan bahkan jangan simpan counter di enum:

```rust
Fault
```

dan engine punya:

```rust
fault_recovery_reads: u8,
```

Lebih sederhana.

---

# 2. 🔴 BUG: Verification deadline bisa hilang ketika event terjadi

Ini cukup halus.

Anda memiliki:

```rust
verification_deadline: Option<Instant>
```

dan:

```rust
pending_verification_state
```



Kemudian setelah command:

```rust
verification_deadline = Some(Instant::now() + VERIFY_DELAY);
```



Tetapi ketika Netlink event terjadi sebelum deadline:

```text
command
 ↓
verification deadline = +500ms
 ↓
Netlink event
 ↓
should_evaluate
 ↓
loop ulang
 ↓
sensor snapshot baru
```

Anda **memang masih memiliki deadline**, jadi tidak hilang.

Tetapi masalahnya adalah Anda melakukan:

```rust
scheduler.push_sample(snapshot.clone());
```

sebelum verification:

```rust
// Perform asynchronous verification
```



Jadi snapshot yang digunakan untuk verification sudah masuk ke EMA.

Ini menyebabkan:

```text
command transition
↓
500ms verification sample
↓
sample dimasukkan ke rate estimator
```

Padahal sample tersebut sebenarnya **verification sample**, bukan normal scheduler sample.

Saya sarankan:

```text
read snapshot
↓
verify pending command
↓
if normal sample → push scheduler
```

atau beri flag:

```rust
SampleKind::Verification
SampleKind::Normal
```

---

# 3. 🔴 `Disabled` sekarang benar secara semantic, tetapi implementasi `ReleaseControl` masih bukan release

Anda sudah memperbaiki:

```rust
ChargeCommand::ReleaseControl
```



Bagus.

Tetapi implementasinya:

```rust
ChargeCommand::ReleaseControl => {
    ...
    control::set_charging(true)
}
```



Ini bukan benar-benar:

> release control

Ini adalah:

> force enable charging.

Kalau backend `control::set_charging(true)` memang berarti "enable charging" dan tidak ada API untuk mengembalikan hardware ke automatic policy, maka namanya sebaiknya:

```rust
RestoreCharging
```

atau:

```rust
ResumeNormalCharging
```

Jangan menyebut `ReleaseControl` kalau secara teknis Anda masih melakukan write.

Kalau `charger_core` memang mempunyai mekanisme:

```rust
control::release()
```

maka gunakan itu.

---

# 4. 🟠 `SensorSnapshot::is_charging()` sudah jauh lebih benar

Ini improvement yang saya suka:

```rust
match self.status {
    Some(BatteryStatus::Charging) => true,
    _ => false,
}
```



Ini jauh lebih baik daripada:

```text
current > 50mA
```

sebagai primary state.

Tetapi ada satu masalah:

```rust
None => false
```

Artinya:

```text
status read gagal
↓
is_charging() == false
```

Itu masih salah secara semantik.

Harusnya:

```rust
fn charging_state(&self) -> Option<bool>
```

misalnya:

```rust
match self.status {
    Some(BatteryStatus::Charging) => Some(true),
    Some(BatteryStatus::Discharging)
    | Some(BatteryStatus::NotCharging)
    | Some(BatteryStatus::Full) => Some(false),
    None => None,
}
```

Dengan begitu:

```text
Some(true)  = charging
Some(false) = not charging
None        = unknown
```

Ini penting untuk scheduler.

---

# 5. 🟠 Scheduler masih memperlakukan `status = None` sebagai discharging

Sekarang:

```rust
let is_charging = s.is_charging();
```



dan `is_charging()` mengembalikan `false` untuk unknown.

Kemudian:

```rust
let danger_low = !is_charging && dist_to_resume < DANGER_CAP_MARGIN;
```



Jadi:

```text
status unknown
+
battery dekat resume
↓
anggap discharging
↓
MIN_INTERVAL
```

Lebih aman:

```rust
match s.charging_state() {
    Some(true) => ...
    Some(false) => ...
    None => conservative fallback
}
```

---

# 6. 🟠 `Fault` hanya dipicu temperature failure

Sekarang:

```rust
if snapshot.temp_dc.is_none() {
    Fault
}
```



Ini sebenarnya keputusan yang masuk akal.

Dan saya suka komentar:

```rust
// Missing capacity is non-critical
```



Tetapi ada satu hal yang perlu ditentukan secara eksplisit:

### `status` failure?

Sekarang:

```rust
status: battery_reader.read_status().ok(),
```



Kalau status gagal:

```text
status = None
```

daemon tetap bisa:

```text
Charging
```

atau:

```text
LimitReached
```

berdasarkan capacity/temperature.

Itu bisa diterima **jika policy Anda memang begitu**.

Saya justru menyarankan dokumentasikan:

```text
Temperature = safety-critical
Capacity = policy-critical
Status = advisory
Current = advisory
Online = routing/event signal
```

Itu akan membuat behavior daemon jauh lebih jelas.

---

# 7. 🔴 Ada bug potensial pada `Offline → Charging`

Ini:

```rust
ChargeState::Disabled | ChargeState::Offline => {
    self.state = ChargeState::Charging;
    self.evaluate(snapshot, cfg)
}
```



Secara teori ketika `Offline`, bagian awal:

```rust
if snapshot.online == Some(false) {
    self.state = ChargeState::Offline;
    return Noop;
}
```



akan dieksekusi dulu.

Jadi recursion hanya terjadi kalau:

```text
previous state = Offline
current online = Some(true)
```

Itu benar.

Namun ada hal yang perlu diperhatikan:

```text
Offline
↓
charger plugged
↓
Charging
↓
command Enable hanya jika state berubah
```

Ini akan bekerja.

Jadi **bukan bug**, tetapi saya sarankan transition ini ditulis eksplisit agar tidak bergantung pada recursion.

---

# 8. 🟠 Config `reconfigure()` masih belum lengkap

Sekarang:

```rust
ChargeState::ThermalCutoff if !cfg.thermal_cutoff
```

dan:

```rust
ChargeState::LimitReached if cfg.charge_limit >= 100
```



Masalah:

Misalnya:

```text
limit lama = 80
battery = 82
state = LimitReached

user ubah limit → 90
```

`reconfigure()` tidak melakukan recovery karena:

```rust
cfg.charge_limit >= 100
```

false.

Hasil:

```text
82%
limit = 90%
state = LimitReached
```

kemudian engine:

```text
82 > resume 88?
yes
→ tetap Disable
```

Ini **bug nyata**.

### Seharusnya

`reconfigure()` harus membandingkan state dengan **current sensor**, atau minimal membatalkan `LimitReached` jika limit dinaikkan melewati SOC.

Saya lebih suka:

```rust
fn reconfigure(&mut self, cfg: &Config, snapshot: Option<&SensorSnapshot>)
```

atau jangan melakukan state reconciliation di `reconfigure()` sama sekali.

Biarkan:

```text
config changed
↓
force evaluate
↓
evaluate(snapshot, new_cfg)
```

yang menentukan state.

---

# 9. 🟠 `ChargeState::Disabled` juga bisa menyebabkan command tidak dijalankan

Anda punya:

```rust
prev_state = engine.state;
decision = engine.evaluate(...)
```

Kemudian:

```rust
if prev_state != decision.state {
    set_charging(...)
}
```

Misalnya daemon startup:

```text
engine.state = Charging
cfg.enabled = false
```

maka:

```text
Charging → Disabled
```

dan `ReleaseControl` dijalankan.

Bagus.

Tetapi kalau daemon restart dan:

```rust
DecisionEngine::new()
→ Charging
```

sementara config disabled:

```text
Charging → Disabled
```

juga bagus.

Namun ketika:

```text
Disabled
↓
config remains disabled
```

tidak ada command.

Itu benar.

---

# 10. 🟢 Async verification sekarang jauh lebih bagus

Perubahan ini saya nilai positif:

```rust
const VERIFY_DELAY: Duration = Duration::from_millis(500);
```



dan:

```rust
verification_deadline
pending_verification_state
```



Kemudian poll menghitung deadline sebagai wakeup terdekat:

```rust
next_wake = next_wake.min(vd);
```



**Ini jauh lebih bagus daripada `sleep(300ms)` versi sebelumnya.**

Saya pertahankan desain ini.

---

# 11. 🟠 Verification belum mengubah state ketika gagal

Sekarang:

```text
Enable
↓
500ms
↓
hardware tidak charging
↓
WARN
```



Tetapi state tetap:

```text
Charging
```

Kalau hardware sebenarnya gagal enable:

```text
daemon believes Charging
hardware NotCharging
```

Anda hanya log warning.

Saya sarankan minimal:

```rust
enum HardwareSyncState {
    Unknown,
    Synchronized,
    Desynchronized,
}
```

atau sederhana:

```text
verification failure
↓
retry command
↓
after N failures
↓
Fault
```

Misalnya:

```text
Enable failed 1x → retry
Enable failed 2x → retry
Enable failed 3x → Fault
```

Ini sangat berguna untuk Android vendor kernel.

---

# 12. 🟠 `ReleaseControl` verification sebenarnya kurang tepat

Untuk:

```rust
ReleaseControl
```

Anda melakukan:

```text
set_charging(true)
↓
verify Charging
```



Kalau maksudnya adalah:

> daemon disabled, biarkan Android/charger framework mengatur sendiri

maka verification seharusnya bukan:

```text
must be Charging
```

karena mungkin battery:

```text
95%
charger connected
framework memilih NotCharging
```

Jadi `ReleaseControl` tidak boleh diverifikasi sebagai "must charge".

Ini alasan tambahan kenapa saya menyarankan rename atau benar-benar membuat API release.

---

# 13. 🟢 Scheduler sekarang jauh lebih baik

Anda sudah memperbaiki hal penting:

```rust
let timeout = scheduler.next_interval(&snapshot);
```



bukan lagi:

```text
decision.state == Charging
```

Ini tepat.

Scheduler sekarang membaca snapshot aktual.

---

# 14. 🟠 Tetapi `unwrap_or(0)` di scheduler masih problem

Anda punya:

```rust
let cap = s.capacity_pct.unwrap_or(0) as f32;
let temp = s.temp_dc.unwrap_or(0) as f32 / 10.0;
```



Walaupun beberapa baris kemudian:

```rust
s.capacity_pct.is_none() || s.temp_dc.is_none()
```

akan menghasilkan `MIN_INTERVAL`, saya tetap tidak suka.

Ini membuat temporary semantic:

```text
sensor unknown
↓
cap = 0
temp = 0
```

Sebelum akhirnya kembali MIN_INTERVAL.

Lebih bersih:

```rust
let (Some(cap), Some(temp)) = (s.capacity_pct, s.temp_dc) else {
    self.last_interval = MIN_INTERVAL;
    return self.last_interval;
};
```

Kemudian baru lakukan perhitungan.

---

# 15. 🟢 Netlink reconnect sudah bagus

Ini:

```rust
if nl_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
```



kemudian:

```rust
_nl_sock = None;
num_fds = 1;
```

dan recreate:

```rust
create_netlink_socket()
```



**Ini saya anggap sudah benar secara desain.**

Satu catatan: reconnect dilakukan langsung pada event loop. Kalau socket gagal terus, daemon akan mencoba lagi pada event berikutnya. Lebih baik tambahkan **backoff**.

Misalnya:

```text
1s
2s
5s
10s
30s
60s
```

bukan retry setiap event.

---

# 16. 🟠 Anda masih menggunakan API Unix lama

Sekarang:

```rust
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
```



Untuk Rust modern saya akan pindahkan ke:

```rust
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
```

Ini bukan bug, tetapi lebih modern.

---

# 17. 🟢 `DecisionReason` adalah improvement bagus

Ini:

```rust
enum DecisionReason {
    ...
}
```



dan `Display`:

```rust
"daemon_disabled"
"charger_offline"
...
```



Saya lebih suka ini daripada `&'static str`.

Bahkan saya akan melangkah satu tingkat:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionReason
```

supaya bisa digunakan dalam assertion/unit test.

---

# 18. 🟠 Scheduler prediction masih punya satu masalah matematis

Bagian:

```rust
dist_to_resume = (cap - self.resume_limit).max(0.0);
```



kemudian:

```rust
!is_charging && ema_cap_rate < -0.01
```



Kalau:

```text
cap = 79
resume = 80
```

maka:

```text
dist_to_resume = 0
```

dan prediction menjadi:

```text
0 sec
```

lalu clamp ke:

```text
2 sec
```

Ini sebenarnya aman.

Tetapi scheduler seharusnya tidak menghitung ETA terhadap resume jika:

```text
cap <= resume
```

karena engine sudah berada pada kondisi yang seharusnya memungkinkan charging.

Saya akan buat explicit:

```rust
if !is_charging && cap <= self.resume_limit {
    return MIN_INTERVAL;
}
```

Ini lebih jelas.

---

# 19. 🟢 Saya suka Anda memisahkan criticality sensor

Ini:

```rust
if snapshot.temp_dc.is_none()
```

→ Fault

sementara:

```rust
if snapshot.capacity_pct.is_none()
```

→ Noop



adalah **policy yang jauh lebih matang** daripada `unwrap_or(0)`.

Saya pertahankan konsep ini.

---

# 20. Tetapi saya akan memperbaiki `Fault` sekarang

Desain ideal menurut saya:

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

Engine:

```rust
struct DecisionEngine {
    state: ChargeState,
    fault_recovery_reads: u8,
}
```

Ketika temperature gagal:

```text
fault_recovery_reads = 0
state = Fault
```

Kemudian:

```text
valid temperature #1
→ recovery_reads = 1
→ remain Fault

valid temperature #2
→ recovery_reads = 2
→ remain Fault

valid temperature #3
→ recovery_reads = 3
→ state = Charging
```

Ini secara konsep jauh lebih bersih daripada `Fault { retry_count }`.

---

# Overall

Saya naikkan penilaian dari **~7.8 → ~8.5/10**.

Sekarang desainnya sudah cukup bagus:

```text
                    ┌───────────────┐
                    │ Config        │
                    └───────┬───────┘
                            ↓
┌──────────┐       ┌─────────────────┐
│ Netlink  │──────►│ Event Loop      │◄──── IPC
└──────────┘       └────────┬────────┘
                            ↓
                    ┌───────────────┐
                    │ SensorSnapshot│
                    └───────┬───────┘
                            ↓
                    ┌───────────────┐
                    │ DecisionEngine│
                    └───────┬───────┘
                            ↓
                    ┌───────────────┐
                    │ ChargeCommand │
                    └───────┬───────┘
                            ↓
                     Apply command
                            ↓
                      500ms wait
                            ↓
                         Verify
                            ↓
                     Adaptive timer
```

**Saya tidak menyarankan rewrite lagi.** Fondasinya sudah benar.

### Yang wajib saya perbaiki sebelum lanjut optimasi

1. **Ubah `Fault { retry_count }` menjadi recovery-success counter.**
2. **`status: None` harus berarti `charging = Unknown`, bukan `false`.**
3. **Perbaiki `reconfigure()` agar perubahan limit 80 → 90 langsung direkonsiliasi.**
4. **Pisahkan `ReleaseControl` dari `set_charging(true)`, atau rename menjadi `RestoreCharging`.**
5. **Verification failure harus punya retry/fault policy, jangan hanya `warn!`.**
6. **Hapus `unwrap_or(0)` dari scheduler.**
7. **Tambahkan reconnect backoff untuk Netlink.**
8. **Gunakan `std::os::fd` untuk Rust modern.**

Setelah delapan hal itu, saya justru **tidak akan menambah fitur dulu**. Tahap berikutnya seharusnya adalah **unit-test state machine + scheduler secara deterministik**: test `80→90`, `limit→resume`, thermal cutoff/resume, unplug/replug, sensor failure/recovery, config disable/enable, dan hardware verification failure. Itu akan memberi peningkatan kualitas yang jauh lebih besar daripada mengoptimalkan `contains_subslice()` atau allocation.
