Ya — **secara arsitektur ini sudah jauh lebih baik** daripada versi sebelumnya. Pemisahan `snapshot → decision → hardware → scheduler → netlink → orchestrator` sekarang cukup matang, dan beberapa masalah yang sebelumnya saya soroti memang sudah diperbaiki.

Tapi saya **belum menganggapnya production-ready**. Ada beberapa bug/edge case yang menurut saya cukup penting, terutama di **ownership recovery, verification, `read_current_ma()`, scheduler ETA, dan Netlink debounce**.

## Penilaian singkat

| Bagian                       | Penilaian                                    |
| ---------------------------- | -------------------------------------------- |
| Arsitektur modul             | 🟢 Sangat baik                               |
| Ownership persistence        | 🟢 Baik, tapi recovery perlu diperkuat       |
| Hardware sync state machine  | 🟢 Baik                                      |
| Partial sysfs write handling | 🟢 Jauh lebih baik                           |
| Charging-state verification  | 🟡 Ada masalah semantik                      |
| Decision/hysteresis          | 🟢 Baik                                      |
| Fault recovery               | 🟢 Baik                                      |
| Netlink reconnect            | 🟢 Baik                                      |
| Netlink debounce             | 🟡 Implementasi belum ideal                  |
| Adaptive scheduler           | 🟡 Ada bug penting pada ETA                  |
| CachedReader                 | 🟢 Bagus, tetapi current heuristic berbahaya |
| Shutdown restore             | 🟢 Baik                                      |
| Overall                      | **~8.5/10**                                  |

---

# 1. Hal yang sekarang sudah sangat bagus

### Ownership persistence

Ini sudah jauh lebih aman:

```rust
save_persistent_ownership(original);
self.ownership = Ownership::Owned {
    original_charging: original,
};
```

dan:

```rust
hardware::recover_stale_ownership();
```

Ini penting untuk kasus:

```text
daemon
   ↓
ambil ownership
   ↓
disable charging
   ↓
process crash / reboot
   ↓
daemon tidak sempat restore
```

Kemudian startup berikutnya:

```text
ownership.state
       ↓
recover_stale_ownership()
       ↓
restore original state
       ↓
hapus ownership.state
```

Itu desain yang benar secara konsep.

---

# 2. `ChargingWriteResult` sekarang jauh lebih bagus

Ini perubahan yang saya suka:

```rust
pub struct ChargingWriteResult {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}
```

Daripada sekadar:

```rust
Result<(), Error>
```

karena hardware charging Android bisa memiliki beberapa control node.

Misalnya:

```text
battery/charging_enabled = berhasil
main/charging_enabled    = gagal
battery/input_suspend    = berhasil
```

Maka daemon **tidak boleh menganggap semuanya berhasil**.

Dan Anda sudah melakukan:

```rust
if res.all_succeeded() {
    ...
} else {
    self.mark_apply_failed();
}
```

Ini benar.

Bahkan:

```rust
ChargingState::Mixed
```

juga sudah diperlakukan sebagai failure.

Bagus.

---

# 3. Tapi ada bug penting pada `is_charging_enabled()`

Anda punya:

```rust
pub fn is_charging_enabled() -> Result<bool, ChargerError> {
    match read_charging_state()? {
        ChargingState::Enabled => Ok(true),
        ChargingState::Disabled => Ok(false),

        ChargingState::Mixed => {
            ...
            Ok(false)
        }
```

Ini **tidak ideal**.

Karena:

```text
Mixed
```

bukan berarti:

```text
Disabled
```

Misalnya:

```text
battery/charging_enabled = 1
main/charging_enabled    = 0
```

Kemudian:

```rust
is_charging_enabled()
```

mengembalikan:

```rust
Ok(false)
```

Caller bisa menginterpretasikan:

> charging memang disabled.

Padahal kenyataannya:

> state tidak konsisten.

### Lebih baik

Jangan gunakan `bool` untuk representasi state hardware.

Misalnya:

```rust
pub fn is_charging_enabled() -> Result<bool, ChargerError> {
    match read_charging_state()? {
        ChargingState::Enabled => Ok(true),
        ChargingState::Disabled => Ok(false),
        ChargingState::Mixed => Err(
            ChargerError::ChargingStateUnknown
        ),
        ChargingState::Unknown => Err(
            ChargerError::ChargingStateUnknown
        ),
    }
}
```

Atau bahkan lebih bagus, **hapus helper ini** dan langsung gunakan:

```rust
read_charging_state()
```

di ownership acquisition.

---

# 4. Masalah terbesar berikutnya: ownership recovery terlalu agresif

Sekarang:

```rust
pub fn recover_stale_ownership() {
    let Some(original) = load_persistent_ownership() else {
        return;
    };

    control::set_charging(original)
```

Masalahnya adalah daemon **tidak tahu apakah state file benar-benar stale**.

Contoh:

```text
daemon A
    ownership.state = 1

daemon A masih hidup

daemon B start
    recover_stale_ownership()
```

Daemon B akan langsung:

```text
set_charging(original)
clear ownership
```

Ini bisa merusak daemon A.

Memang pada desain normal hanya ada satu daemon, tetapi untuk daemon Android saya akan membuat ownership lebih robust.

Minimal state file sebaiknya menyimpan:

```text
version
pid
start/restart generation
original state
```

atau gunakan file lock.

**Yang paling penting: gunakan exclusive lock untuk ownership.**

---

# 5. Bug pada verification retry

Ini bagian yang perlu diperhatikan:

```rust
let index =
    (self.verification_failures as usize)
        .min(VERIFY_DELAYS.len() - 1);

self.verification = Some(Verification {
    generation: self.generation,
    target: self.applied_target,
    deadline: Instant::now()
        + VERIFY_DELAYS[index],
});
```

Dengan:

```rust
VERIFY_DELAYS = [
    500ms,
    1s,
    2s
]
```

dan:

```rust
verification_failures = 1
```

maka index = 1 → 1 detik.

Itu sebenarnya masuk akal.

Tetapi ada masalah lebih besar:

```rust
target: self.applied_target
```

Saya lebih suka menggunakan target yang **diverifikasi sejak awal**, bukan membaca mutable `applied_target`.

Walaupun generation melindungi sebagian race/state invalidation, lebih jelas kalau `Verification` menjadi immutable command:

```rust
struct Verification {
    generation: u64,
    target: HardwareTarget,
    deadline: Instant,
}
```

dan retry menggunakan:

```rust
target: v.target
```

bukan:

```rust
target: self.applied_target
```

Karena secara semantic:

> verification retry harus memverifikasi operation yang sama.

---

# 6. Masalah semantik besar pada verification ChargingEnabled

Anda punya:

```rust
Ok(control::ChargingState::Enabled) => {
    snapshot.online != Some(false)
}
```

Artinya:

```text
charging state Enabled
+
online unknown
```

→ SUCCESS.

Itu sebenarnya **terlalu permisif**.

Sedangkan DecisionEngine Anda menganggap:

```rust
snapshot.online.is_some()
```

sebagai sensor wajib.

Jadi seharusnya verification konsisten:

```rust
Ok(ChargingState::Enabled) => {
    snapshot.online == Some(true)
}
```

Namun ada nuansa penting:

**hardware charging enabled ≠ charger physically online.**

Kalau `online == false`, memang seharusnya target `Unmanaged`, jadi verification tidak perlu lagi dilakukan.

Jadi saya lebih suka:

```rust
Ok(ChargingState::Enabled) => {
    snapshot.online == Some(true)
}
```

---

# 7. `ChargingDisabled` verification cukup bagus

Ini:

```rust
let current_safe = snapshot
    .current_ma
    .is_none_or(|current| current <= 100.0);
```

secara konsep bagus.

Tetapi ada masalah:

```rust
None => true
```

Karena:

```rust
Option::is_none_or()
```

berarti current tidak tersedia → dianggap aman.

Untuk daemon charging control, saya justru akan lebih konservatif:

```rust
let current_safe = snapshot
    .current_ma
    .is_some_and(|current| current <= 100.0);
```

Sehingga:

```text
current <= 100mA → safe
current > 100mA  → unsafe
current unknown  → unsafe
```

Ini jauh lebih aman.

---

# 8. Ada masalah serius pada `read_current_ua()`

Anda memilih current dengan:

```rust
if best
    .map(|current| value.unsigned_abs() > current.unsigned_abs())
```

Artinya memilih:

> nilai absolute terbesar.

Misalnya:

```text
battery/current_now =  150000 µA
bms/current_now     =  800000 µA
usb/current_now     =  500000 µA
```

Anda mengambil:

```text
800000 µA
```

Padahal belum tentu `bms/current_now` adalah current charging yang sebenarnya.

Lebih buruk:

```text
battery = -100000
bms     = +900000
```

Anda mengambil:

```text
+900mA
```

padahal mungkin angka itu bukan current battery yang relevan.

### Ini menurut saya salah satu hal paling penting untuk diperbaiki.

Anda sudah punya konsep priority di `control`.

Gunakan konsep yang sama untuk current:

```text
battery current
    ↓
bms current
    ↓
main current
```

dengan vendor-specific priority.

Atau lebih bagus:

* pilih satu canonical battery-current node
* fallback ke node berikutnya
* jangan memilih magnitude terbesar.

---

# 9. Unit conversion current juga masih heuristik

Ini:

```rust
if current.abs() > 10_000.0 {
    Ok(current / 1000.0)
} else {
    Ok(current)
}
```

misalnya:

```text
5000
```

bisa berarti:

```text
5000 µA = 5mA
```

atau:

```text
5000 mA = 5A
```

Anda tidak bisa mengetahui unit hanya dari magnitude dengan reliable.

Biasanya Android power_supply:

```text
current_now → µA
```

tetapi vendor tertentu memang bisa menyimpang.

Lebih baik unit ditentukan **per node**, bukan berdasarkan angka.

Contoh:

```rust
struct CurrentNode {
    path: &'static str,
    unit: CurrentUnit,
    priority: u8,
}
```

Kemudian:

```rust
enum CurrentUnit {
    MicroAmp,
    MilliAmp,
}
```

---

# 10. `CachedReader` secara umum sangat bagus

Saya suka perubahan ini:

```rust
current_fds: Vec<CurrentFd>,
online_fds: Vec<OnlineFd>,
```

dan:

```rust
CURRENT_RESCAN_INTERVAL: 5s
ONLINE_RESCAN_INTERVAL: 5s
```

Ini jauh lebih baik daripada:

```text
setiap polling:
    File::open()
    read
    close
```

Sekarang:

```text
normal loop
   ↓
seek
   ↓
read
```

dan hanya setiap 5 detik:

```text
rescan
   ↓
File::open()
```

Untuk daemon Android yang hidup lama, ini desain yang bagus.

---

# 11. Tapi `CachedReader` punya bug borrowing potensial

Di:

```rust
pub fn read_capacity(&mut self) -> Result<u8, ChargerError> {
    let file = self.capacity_fd.as_mut().ok_or_else(...)?;

    let s = Self::read_file(file, &mut self.buf, "capacity")?;
```

Anda meminjam:

```rust
self.capacity_fd
```

dan kemudian:

```rust
&mut self.buf
```

secara simultan.

Rust modern bisa menerima pola tertentu tergantung bagaimana borrow berlangsung, tetapi desain ini mudah menjadi masalah borrow-checker ketika kode berkembang.

Lebih bersih membuat:

```rust
fn read_cached(
    file: &mut File,
    buf: &mut [u8],
    node_name: &'static str,
)
```

lalu mengambil field secara terpisah atau menggunakan buffer lokal.

Tidak urgent, tetapi saya akan rapikan.

---

# 12. Scheduler punya bug konseptual yang cukup penting

Ini:

```rust
let seconds = (distance.abs() / rate.abs()) * (1.0 - safety);
```

Secara matematis Anda menghitung:

```text
ETA × 75%
```

untuk safety 25%.

Misalnya:

```text
capacity = 80%
target   = 90%
rate     = +1%/minute
```

ETA:

```text
10 menit
```

scheduler menjadi:

```text
7.5 menit
```

Itu sebenarnya masuk akal sebagai safety margin.

Tetapi problemnya adalah **scheduler memilih interval berdasarkan threshold terdekat tanpa mempertimbangkan arah state secara kuat**.

Misalnya ketika:

```text
capacity = 95
limit = 90
rate = -1%/minute
```

target:

```rust
self.resume_limit
```

misalnya:

```text
88
```

ETA:

```text
7 menit
```

Padahal policy sedang:

```text
LimitReached
```

dan hardware mungkin disabled.

Scheduler tidak salah sepenuhnya, tetapi coupling antara:

```text
policy state
```

dan:

```text
scheduler prediction
```

belum eksplisit.

Saya lebih suka scheduler menerima:

```rust
policy: ChargePolicyState
```

atau minimal:

```rust
target: SchedulerTarget
```

---

# 13. Scheduler temperature ETA juga perlu hati-hati

Anda menghitung:

```rust
temp_eta = eta_to(
    snapshot.temp_dc,
    self.thermal_cutoff_dc,
    self.temp_rate_ema,
    ...
)
```

Ini bagus untuk:

```text
40°C
+
temperature naik
→
prediksi 50°C
```

Tetapi `temp_rate_ema` bisa sangat noisy karena battery temperature biasanya berubah secara non-linear.

Misalnya:

```text
40.0
40.2
40.5
41.0
```

kemudian charger berubah:

```text
41.0
40.7
40.2
```

EMA masih membawa momentum kenaikan.

Jadi untuk thermal prediction, saya akan lebih konservatif daripada capacity prediction.

Misalnya:

```rust
THERMAL_SAFETY_FACTOR = 0.30
```

atau gunakan minimum margin rule seperti yang sudah Anda punya:

```rust
margin <= 3°C → 5 sec
margin <= 5°C → 15 sec
```

Yang terakhir justru sangat bagus.

---

# 14. Netlink: satu masalah penting

Anda sudah memperbaiki debounce menjadi:

```rust
self.debounce_target = Some(now + DEBOUNCE);
```

Bagus.

Tetapi ada kemungkinan event baru datang selama debounce.

Misalnya:

```text
t=0      event
t=250ms  debounce due

t=100ms  event kedua
```

Kode Anda:

```rust
self.debounce_target = Some(now + DEBOUNCE);
```

akan memperpanjang deadline.

Bagus.

Namun saat `poll()` timeout karena:

```rust
netlink.next_deadline()
```

kemudian:

```rust
netlink.debounce_due()
```

dan:

```rust
break;
```

daemon langsung melakukan satu iteration baru.

Itu benar.

---

# 15. Tetapi filter Netlink masih terlalu kasar

Anda hanya mengecek:

```rust
SUBSYSTEM=power_supply
```

dan:

```rust
ACTION=change
```

Ini berarti **semua power_supply change** membangunkan daemon.

Misalnya:

```text
battery
usb
ac
wireless
bms
main
```

Sebenarnya tidak masalah untuk correctness, hanya agak noisy.

Untuk daemon charging controller, saya malah menganggap ini acceptable.

Jangan terlalu agresif memfilter berdasarkan nama device kecuali sudah terbukti perlu.

---

# 16. Bug lain: Netlink reconnect bisa tidak membangunkan scheduler dengan benar

Anda punya:

```rust
if netlink.should_reconnect(now) {
    netlink.try_reconnect(now);
}
```

kemudian scheduler menentukan:

```rust
next_interval(...)
```

Jika netlink disconnected:

```rust
UNPLUGGED_HEARTBEAT_NO_NETLINK = 30s
```

bagus.

Tetapi jika charger online:

```rust
MAX_INTERVAL = 90s
```

dan Netlink reconnect deadline misalnya 60s, inner loop memang melihat:

```rust
netlink.next_deadline()
```

jadi akan bangun.

Bagian ini sekarang sudah cukup baik.

---

# 17. Ada satu bug yang lebih penting di orchestrator

Urutannya:

```rust
if hardware.verification_due(now) {
    hardware.verify(&snapshot);
}
```

kemudian:

```rust
if hardware.sync == hardware::SyncState::Synced {
    scheduler.observe(&snapshot);
}
```

kemudian:

```rust
let decision = engine.evaluate(...)
```

kemudian apply.

Artinya snapshot yang sama bisa:

```text
verify hardware
↓
observe scheduler
↓
evaluate decision
↓
apply hardware
```

Ini acceptable.

Tetapi ketika verification gagal dan menghasilkan:

```rust
SyncState::Failed
```

scheduler tidak observe.

Bagus.

Namun ketika verification sukses:

```rust
SyncState::Synced
```

scheduler langsung observe snapshot yang **sama dengan snapshot sebelum verification**.

Tidak fatal.

---

# 18. DecisionEngine sudah jauh lebih matang

Ini:

```rust
const FAULT_RECOVERY_READS: u8 = 3;
```

dan:

```rust
if self.policy == ChargePolicyState::Fault {
    ...
}
```

adalah improvement besar.

Sekarang sensor error tidak langsung:

```text
error
→ charging enabled
```

melainkan:

```text
sensor fault
      ↓
ChargingDisabled
      ↓
3 valid reads
      ↓
recover
```

Ini tepat untuk charger controller.

---

# 19. Tetapi `status` sebenarnya belum digunakan dalam decision

Anda menentukan:

```rust
let sensors_valid =
    snapshot.capacity_pct.is_some()
    && snapshot.temp_dc.is_some()
    && snapshot.online.is_some()
    && snapshot.status.is_some();
```

Tetapi setelah itu:

```rust
let capacity = ...
let temp_dc = ...
```

`status` tidak dipakai.

Jadi:

```text
status = Unknown
```

tetap:

```text
Some(BatteryStatus::Unknown)
```

dan dianggap sensor valid.

Itu bug.

Karena:

```rust
snapshot.status.is_some()
```

tidak sama dengan:

```text
status valid
```

Harus:

```rust
let status_valid = matches!(
    snapshot.status,
    Some(
        BatteryStatus::Charging
        | BatteryStatus::Discharging
        | BatteryStatus::NotCharging
        | BatteryStatus::Full
    )
);
```

Kemudian:

```rust
let sensors_valid =
    snapshot.capacity_pct.is_some()
    && snapshot.temp_dc.is_some()
    && snapshot.online.is_some()
    && status_valid;
```

---

# 20. Bahkan status `Full` perlu dipertimbangkan

Misalnya:

```text
capacity = 100
status = Full
```

policy Anda:

```text
capacity >= charge_limit
```

→ disable.

Benar.

Tetapi:

```text
capacity = 90
status = Full
```

bisa terjadi pada beberapa Android/vendor implementations.

Jadi jangan menjadikan `status` sebagai sumber keputusan utama.

Lebih tepat:

```text
capacity
temperature
online
```

adalah critical sensors.

`status` digunakan sebagai **sanity check**, bukan decision source.

---

# 21. `Offline → Unmanaged` sudah tepat

Ini perubahan penting:

```rust
if snapshot.online == Some(false) {
    return ... HardwareTarget::Unmanaged
}
```

Ini jauh lebih baik daripada:

```text
offline
→ disable charging
```

Karena ketika charger dicabut, daemon seharusnya tidak meninggalkan hardware override.

---

# 22. Namun ada satu edge case startup yang perlu diperhatikan

Startup:

```rust
hardware::recover_stale_ownership();

let mut hardware = HardwareController::new();
```

Kemudian:

```rust
DecisionEngine::new()
```

default:

```rust
policy: Charging
```

dan:

```rust
HardwareTarget::Unmanaged
```

Kemudian snapshot pertama.

Jika charger online dan:

```text
capacity < limit
```

target:

```text
ChargingEnabled
```

ownership diambil.

Bagus.

Tetapi jika:

```text
capacity >= limit
```

target:

```text
ChargingDisabled
```

ownership diambil.

Juga benar.

---

# 23. Ada masalah pada `recover_stale_ownership()`: hasil `set_charging()` partial

Anda melakukan:

```rust
match control::set_charging(original) {
    Ok(_) => {
        clear_persistent_ownership();
    }
}
```

Ini **bug serius**.

Karena:

```rust
set_charging()
```

mengembalikan:

```rust
Ok(ChargingWriteResult)
```

bahkan ketika:

```text
partial failure
```

Jadi recovery bisa:

```text
battery node restored
main node FAILED
       ↓
Ok(...)
       ↓
clear ownership.state
```

Padahal hardware belum benar-benar restored.

### Harus:

```rust
match control::set_charging(original) {
    Ok(res) if res.all_succeeded() => {
        clear_persistent_ownership();
    }

    Ok(res) => {
        tracing::error!(
            "Stale ownership recovery incomplete: {}/{} succeeded, {} failed",
            res.succeeded,
            res.attempted,
            res.failed
        );
    }

    Err(e) => ...
}
```

**Ini salah satu perubahan yang paling wajib Anda lakukan.**

---

# 24. Hal yang sama untuk `shutdown_restore()`

Di sini Anda sudah benar:

```rust
Ok(res) if res.all_succeeded()
```

Jadi shutdown path lebih aman daripada stale recovery.

---

# 25. `enter_bypass_mode()` masih terlalu permisif

Sekarang:

```rust
if any_success {
    Ok(())
}
```

Jadi:

```text
node A success
node B failed
node C failed
```

→ `Ok(())`

Ini berbeda dengan `set_charging()` yang sudah benar-benar membedakan partial failure.

Kalau `enter_bypass_mode()` memang akan digunakan untuk **hard charging safety**, saya sarankan hasilnya juga:

```rust
attempted
succeeded
failed
```

seperti `ChargingWriteResult`.

Kalau bypass API hanya legacy/CLI convenience, masih bisa dipertahankan.

---

# 26. `grant_node_permissions()` saya justru akan hapus dari daemon

Ini:

```rust
perms.set_mode(0o644);
fs::set_permissions(...)
```

cukup berbahaya secara desain.

Sysfs permission Android biasanya dikelola:

```text
kernel
ueventd
init
vendor policy
SELinux
```

Daemon tidak seharusnya setiap startup melakukan:

```text
chmod sysfs
```

Apalagi:

```rust
let _ = fs::set_permissions(...)
```

mengabaikan failure.

Kalau memang dibutuhkan untuk device tertentu, lebih baik:

```text
Magisk service/init setup
```

yang mengatur permission sebelum daemon berjalan.

---

# 27. Saya juga akan mengubah `SensorSnapshot.current_ma`

Sekarang:

```rust
pub current_ma: Option<f32>,
```

Untuk battery controller saya sebenarnya lebih suka:

```rust
pub current_ma: Option<i32>,
```

atau:

```rust
pub current_ua: Option<i64>
```

Alasannya:

**current sysfs adalah integer**, dan tidak membutuhkan floating point.

Kemudian scheduler/policy bisa mengubah ke `f32` jika diperlukan.

Misalnya:

```rust
current_ua: Option<i64>
```

lebih presisi dan unitnya jelas.

---

# 28. `f32` di current juga menyebabkan threshold agak ambigu

Sekarang:

```rust
current <= 100.0
```

100 apa?

```text
100mA
```

memang maksud Anda.

Lebih self-documenting:

```rust
const SAFE_CURRENT_MA: i32 = 100;
```

atau:

```rust
const SAFE_CURRENT_UA: i64 = 100_000;
```

Saya lebih suka yang kedua.

---

# 29. Satu hal yang sangat bagus: `generation`

Ini:

```rust
generation: u64
```

dan:

```rust
if v.generation != self.generation {
    self.verification = None;
    return;
}
```

adalah desain yang bagus.

Contoh:

```text
disable charging
→ verification scheduled

config reload
→ enable charging
→ generation++

old verification arrives
→ generation mismatch
→ discarded
```

Ini mencegah stale verification.

**Pertahankan.**

---

# 30. `wrapping_add()` juga tepat

```rust
self.generation = self.generation.wrapping_add(1);
```

Tidak perlu khawatir overflow dalam praktik.

Bagus.

---

# Prioritas perbaikan saya

Kalau ini kode yang akan saya review untuk release, saya akan urutkan:

### 🔴 Wajib diperbaiki

**1. Stale ownership recovery harus cek `all_succeeded()`**

Saat ini:

```rust
Ok(_) => clear_persistent_ownership()
```

harus:

```rust
Ok(res) if res.all_succeeded() => clear
Ok(res) => keep state
```

---

**2. `is_charging_enabled()` jangan ubah `Mixed` menjadi `false`**

`Mixed` harus tetap error/unknown.

---

**3. `current_ma` jangan pilih absolute magnitude terbesar**

Ini berpotensi menghasilkan current yang salah.

Gunakan priority/canonical node.

---

**4. `current_ma` unit detection jangan berdasarkan `> 10_000`**

Tentukan unit per node.

---

**5. `status.is_some()` bukan validasi status**

`Some(Unknown)` sekarang lolos.

---

**6. `current_ma == None` jangan dianggap safe saat verification disabled**

Ganti:

```rust
is_none_or(...)
```

menjadi:

```rust
is_some_and(...)
```

untuk verification safety.

---

### 🟡 Sangat disarankan

**7. Ownership gunakan locking**, bukan hanya state file.

**8. `enter_bypass_mode()` gunakan semantics partial failure yang sama.**

**9. Pertimbangkan menghapus `grant_node_permissions()` dari daemon.**

**10. Jadikan current integer/unit-explicit.**

---

# Arsitektur akhirnya

Dengan sedikit perbaikan di atas, arsitekturnya menurut saya sudah sangat solid:

```text
                    ┌────────────────────┐
                    │     Config/IPC     │
                    └─────────┬──────────┘
                              │
                              ▼
┌─────────────┐       ┌───────────────┐
│ CachedReader│───────▶│SensorSnapshot │
└─────────────┘       └───────┬───────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ DecisionEngine  │
                     │                 │
                     │ hysteresis      │
                     │ thermal         │
                     │ fault recovery  │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ HardwareTarget  │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │HardwareController│
                     │                 │
                     │ ownership       │
                     │ apply           │
                     │ verification    │
                     │ retry           │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ control.rs      │
                     │                 │
                     │ sysfs writes    │
                     │ state reading   │
                     │ partial failure │
                     └─────────────────┘

          ┌──────────────────┐
          │ NetlinkMonitor   │
          │ power_supply     │
          │ debounce         │
          │ reconnect        │
          └────────┬─────────┘
                   │
                   ▼
             wake orchestrator

          ┌──────────────────┐
          │AdaptiveScheduler │
          │                  │
          │ EMA              │
          │ ETA              │
          │ thermal margin   │
          │ heartbeat        │
          └──────────────────┘
```

**Secara keseluruhan: ya, ini sudah jauh lebih baik.** Bahkan saya akan mengatakan struktur daemon-nya sekarang sudah berada di level yang layak dijadikan basis final. Yang tersisa bukan lagi masalah arsitektur besar, tetapi beberapa **correctness/safety bug** yang harus dibereskan sebelum dianggap final.

Kalau saya harus memilih **3 hal paling penting untuk Anda perbaiki sekarang**, urutannya:

1. **`recover_stale_ownership()` wajib memeriksa `ChargingWriteResult::all_succeeded()`**
2. **Perbaiki model current (`priority + unit eksplisit`, jangan magnitude terbesar)**
3. **Jangan pernah menganggap `Mixed`, `Unknown`, atau current `None` sebagai state hardware yang aman/valid**

Setelah tiga ini dibereskan, saya akan jauh lebih nyaman menyebut implementasi ini **production-grade untuk daemon charger-control Android**, dengan catatan tetap perlu diuji terhadap beberapa layout `power_supply` vendor nyata.
