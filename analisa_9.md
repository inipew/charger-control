Ya. Saya akan evaluasi **versi kedua sebagai refactor dari versi pertama**, terutama correctness dan state machine-nya. Ada beberapa perbaikan bagus, tetapi **belum saya anggap production-safe**. Ada beberapa bug logika yang cukup penting.

### Temuan paling penting

| Area                  | Status | Masalah                                                                                               |
| --------------------- | ------ | ----------------------------------------------------------------------------------------------------- |
| Decision Engine       | ⚠️     | State `Fault` recovery punya perilaku yang agak berbahaya                                             |
| Hardware verification | ⚠️     | `ChargingDisabled` terlalu lemah untuk membuktikan charging benar-benar berhenti                      |
| `Unmanaged`           | ⚠️     | Tetap memanggil `set_charging(true)`, sehingga bukan benar-benar "unmanaged"                          |
| Scheduler ETA         | ⚠️     | Rumus `eta_to()` pada versi kedua sudah lebih benar, tetapi arah rate perlu ditangani lebih hati-hati |
| Scheduler history     | ⚠️     | Bisa menghasilkan rate yang tidak representatif karena kapasitas hanya integer                        |
| Netlink               | ✅/⚠️   | Struktur jauh lebih baik, tetapi ada masalah event/debounce yang membuat loop tidak optimal           |
| Main loop             | ⚠️     | Netlink event dapat menyebabkan evaluasi dua kali / timing kurang ideal                               |
| Config reload         | ✅      | Jauh lebih baik                                                                                       |
| Ownership FD          | ✅      | `OwnedFd` + `FromRawFd` sudah tepat                                                                   |
| Poll handling         | ✅      | Penanganan `EINTR`, `POLLERR/HUP/NVAL` jauh lebih baik                                                |
| State segregation     | ✅      | Arsitekturnya jauh lebih bersih daripada versi pertama                                                |

## 1. Bug terbesar: `Unmanaged` bukan benar-benar unmanaged

Ini bagian yang paling saya soroti:

```rust
HardwareTarget::Unmanaged => {
    match control::set_charging(true) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("Failed to restore charging: {}", e);
            false
        }
    }
}
```

Secara semantik:

```text
Unmanaged
```

seharusnya berarti:

> daemon tidak mengontrol charging.

Tetapi implementasi Anda berarti:

> daemon secara eksplisit menyuruh hardware untuk charging ON.

Ini berbeda.

Misalnya:

```text
daemon enabled
       ↓
charger offline
       ↓
Decision = Offline
       ↓
Target = Unmanaged
       ↓
set_charging(true)
```

Kalau charger offline, perintah `set_charging(true)` mungkin tidak berguna, tetapi secara arsitektur daemon tetap melakukan kontrol hardware.

Lebih penting lagi ketika:

```text
daemon disabled
```

Anda juga melakukan:

```text
Disabled → Unmanaged → set_charging(true)
```

Kalau tujuan `disabled` adalah **mengembalikan kontrol charger ke sistem/kernel**, Anda perlu membedakan:

```rust
ChargingEnabled
ChargingDisabled
Unmanaged
```

dan untuk `Unmanaged` **jangan melakukan write**.

Idealnya:

```rust
HardwareTarget::Unmanaged => {
    // Do not touch hardware.
    true
}
```

Tetapi ada konsekuensi: kalau sebelumnya daemon melakukan `set_charging(false)`, lalu daemon berubah menjadi `Unmanaged`, charging mungkin masih disabled.

Jadi sebenarnya Anda membutuhkan konsep yang lebih jelas:

```text
Managed + charging ON
Managed + charging OFF
Released / unmanaged
```

dan **"release"** harus didefinisikan berdasarkan kemampuan backend `control`.

Kalau driver Anda memang hanya punya:

```rust
set_charging(bool)
```

tanpa mekanisme `reset/release`, maka `Unmanaged` secara hardware memang tidak bisa benar-benar direalisasikan.

---

# 2. `ChargingDisabled` verification masih terlalu lemah

Sekarang:

```rust
HardwareTarget::ChargingDisabled => {
    snapshot.charging_state() != ChargingState::Charging
}
```

Ini menganggap semua kondisi berikut sukses:

```text
NotCharging  → SUCCESS
Full         → SUCCESS
Unknown      → SUCCESS
```

Karena:

```rust
Unknown != Charging
```

Jadi misalnya sensor status gagal:

```text
status = None
```

maka:

```rust
charging_state() = Unknown
```

dan verification dianggap:

```text
SUCCESS
```

Ini tidak aman.

Lebih baik:

```rust
HardwareTarget::ChargingDisabled => {
    matches!(
        snapshot.charging_state(),
        ChargingState::NotCharging | ChargingState::Full
    )
}
```

Dengan demikian:

```text
Charging       ❌
Unknown        ❌
NotCharging    ✅
Full           ✅
```

Ini penting karena Anda sedang membuat daemon yang mengontrol hardware charging.

---

# 3. Verification `ChargingEnabled` juga perlu diperjelas

Sekarang:

```rust
snapshot.online == Some(true)
    && snapshot.charging_state() == ChargingState::Charging
```

Ini cukup bagus.

Tetapi ada edge case:

```text
capacity = 100
status = Full
online = true
```

Target:

```text
ChargingEnabled
```

verification akan gagal karena:

```text
Full != Charging
```

Padahal pada baterai penuh, hardware mungkin memang tidak perlu berada dalam state `Charging`.

Jadi apakah:

```text
ChargingEnabled
```

berarti:

> charger path diizinkan

atau:

> baterai harus sedang menerima arus?

Itu dua konsep berbeda.

Saya sangat menyarankan target hardware didefinisikan sebagai:

```rust
enum HardwareTarget {
    AllowCharging,
    StopCharging,
    Unmanaged,
}
```

Kemudian verification:

```text
AllowCharging:
    online == true
    &&
    status ∈ {Charging, Full}

StopCharging:
    status ∈ {NotCharging, Full}
```

Namun apakah `Full` benar-benar valid untuk `StopCharging` harus disesuaikan dengan semantics driver Anda.

---

# 4. Bug kecil tapi penting pada Fault Recovery

Anda punya:

```rust
if snapshot.temp_dc.is_none() {
    self.fault_recovery_reads = 0;
    self.policy = ChargePolicyState::Fault;
    return ...
}
```

Kemudian:

```rust
if self.policy == ChargePolicyState::Fault {
    self.fault_recovery_reads += 1;

    if self.fault_recovery_reads < FAULT_RECOVERY_READS {
        return ...
    }

    self.fault_recovery_reads = 0;
    self.policy = ChargePolicyState::Charging;
}
```

Ini memang berarti:

```text
Fault
 ↓
temperature tersedia
 ↓
read #1
 ↓
FaultRecovering
 ↓
read #2
 ↓
FaultRecovering
 ↓
read #3
 ↓
Charging
```

Bagus secara konsep.

Tetapi setelah keluar dari Fault, Anda langsung lanjut:

```rust
let cap = snapshot.capacity_pct.unwrap();
```

Jika `capacity_pct` masih `None`, Anda:

```rust
CapacityUnavailable
```

dengan:

```rust
target: self.policy_to_target(self.policy)
```

Karena policy sudah:

```text
Charging
```

maka target:

```text
ChargingEnabled
```

Padahal hanya temperature yang sudah pulih, capacity belum tentu.

Untuk safety-critical-ish battery control, saya lebih suka:

```text
Fault
 ↓
sensor recovery
 ↓
Recovering
 ↓
semua sensor valid
 ↓
Normal
```

Bukan:

```text
temperature valid → langsung Charging
```

---

# 5. `capacity_pct == None` tidak seharusnya otomatis mempertahankan Charging

Bagian:

```rust
if snapshot.capacity_pct.is_none() {
    return Decision {
        policy: self.policy,
        target: self.policy_to_target(self.policy),
        reason: DecisionReason::CapacityUnavailable,
    };
}
```

Ini punya potensi masalah.

Misalnya sebelumnya:

```text
Charging
```

kemudian:

```text
capacity sensor hilang
```

maka:

```text
policy = Charging
target = ChargingEnabled
```

Jadi daemon tetap mengizinkan charging.

Untuk battery charge limiter, saya lebih memilih:

```text
temperature tersedia
capacity hilang
        ↓
FAIL SAFE
        ↓
ChargingDisabled
```

atau minimal mempertahankan hardware state terakhir sambil masuk ke explicit degraded state.

Misalnya:

```rust
ChargePolicyState::Degraded
```

atau:

```rust
ChargePolicyState::SensorFault
```

---

# 6. Scheduler versi kedua memang lebih baik

Ini perubahan yang bagus:

```rust
let cap_target = if let Some(rate) = self.cap_rate_ema {
    if rate < -0.01 {
        self.resume_limit
    } else {
        self.limit
    }
} else {
    self.limit
};
```

Dibanding versi pertama yang selalu:

```rust
eta_to(capacity, limit, ...)
```

versi kedua sudah memperhitungkan:

```text
Charging:
    target = charge_limit

Discharging:
    target = resume_limit
```

Ini lebih masuk akal.

Tetapi ada masalah konseptual:

```rust
if rate < -0.01
```

Anda menganggap semua penurunan capacity berarti:

```text
discharging → menuju resume_limit
```

Padahal battery percentage memiliki resolusi integer.

Misalnya:

```text
80 → 79
```

dalam 30 detik.

Rate:

```text
-0.033 %/s
```

bisa benar-benar discharge.

Tetapi:

```text
80 → 79
```

juga bisa merupakan noise / fuel-gauge quantization.

EMA membantu, tetapi belum sepenuhnya menghilangkan masalah.

---

# 7. Ada masalah lebih besar pada ETA scheduler

Kode:

```rust
let distance = threshold - current;

if distance.signum() == rate.signum()
```

Ini benar untuk beberapa kasus, tetapi membuat scheduler agak sulit dipahami.

Contoh charging:

```text
current = 70
threshold = 80
distance = +10
rate = +0.01
```

→ valid.

Discharging:

```text
current = 90
threshold = 78
distance = -12
rate = -0.01
```

→ valid.

Jadi matematikanya benar.

Tetapi:

```rust
seconds = (distance.abs() / rate.abs()) * safety;
```

dengan:

```rust
SAFETY_FACTOR = 0.25
```

berarti scheduler bangun pada:

```text
25% ETA
```

Contoh:

```text
ETA = 1000 detik
```

poll:

```text
250 detik
```

Ini mungkin terlalu agresif untuk sebuah charger daemon jika tujuannya efisiensi wakeup, tetapi sangat aman untuk respons.

Untuk Android saya justru akan mempertimbangkan:

```text
charging normal:
    20–25%

thermal:
    10–15%
```

jadi angka Anda masih masuk akal.

---

# 8. Ada bug semantic di `fallback_interval()`

Ini:

```rust
((self.limit - c) / self.limit).clamp(0.0, 1.0)
```

Misalnya:

```text
limit = 80
capacity = 20
```

hasil:

```text
60 / 80 = 0.75
```

maka interval:

```text
2 + 88 * 0.75
= 68 detik
```

Masuk akal.

Tetapi:

```text
capacity = 79
```

hasil:

```text
1 / 80
```

→ sekitar:

```text
3.1 detik
```

Bagus.

Namun untuk discharge:

```rust
((c - self.resume_limit) / (100.0 - self.resume_limit))
```

Jika:

```text
resume = 78
capacity = 90
```

maka:

```text
12 / 22 = 0.545
```

→ sekitar 50 detik.

Masuk akal.

Jadi bagian ini secara umum **cukup bagus**.

---

# 9. Netlink `handle_events()` sudah jauh lebih baik

Versi kedua:

```rust
if found {
    if self.debounce_target.is_none() {
        self.debounce_target = Some(now + NETLINK_DEBOUNCE);
    }
    return true;
}
```

Tetapi ada masalah pada orchestrator:

```rust
if nl_events & libc::POLLIN != 0 &&
    netlink.handle_events(loop_now) {
    should_evaluate = true;
    break;
}
```

Anda langsung:

```text
handle_events()
→ true
→ break
→ evaluate
```

Padahal Anda baru saja membuat:

```rust
debounce_target = now + 250ms
```

Artinya debounce sebenarnya **tidak digunakan sebagaimana mestinya**.

Anda mengatakan:

> tunggu 250 ms setelah event terakhir.

Tetapi orchestrator justru:

> evaluasi sekarang juga.

Kemudian pada iterasi berikutnya baru deadline debounce mungkin diproses.

Ini membuat debounce tidak benar-benar menjadi debounce.

Lebih benar:

```rust
if nl_events & libc::POLLIN != 0 {
    netlink.handle_events(loop_now);
}
```

**jangan langsung `should_evaluate = true`.**

Kemudian:

```rust
next_deadline()
```

akan mengarahkan `poll()` ke:

```text
now + 250ms
```

dan saat deadline tercapai:

```rust
netlink.debounce_due(loop_now)
```

baru:

```rust
should_evaluate = true
```

Dengan begitu alurnya benar:

```text
uevent #1
   ↓
debounce 250 ms
   ↓
uevent #2
   ↓
reset debounce 250 ms
   ↓
uevent #3
   ↓
reset debounce
   ↓
250 ms tanpa event
   ↓
evaluate
```

Itu yang seharusnya Anda inginkan.

---

# 10. `handle_events()` sebaiknya tidak mengembalikan `bool`

Dengan desain sekarang:

```rust
pub fn handle_events(...) -> bool
```

return value sebenarnya mencampur dua hal:

```text
event diterima
```

dengan:

```text
evaluation harus dilakukan sekarang
```

Padahal itu bukan tanggung jawab NetlinkMonitor.

Saya akan ubah menjadi:

```rust
pub fn handle_events(&mut self, now: Instant) {
    ...
}
```

Netlink hanya bertugas:

```text
read event
↓
filter power_supply
↓
update debounce deadline
```

Orchestrator yang memutuskan:

```text
deadline reached?
→ evaluate
```

Ini membuat separation of concerns jauh lebih bersih.

---

# 11. Ada satu masalah reconnect

Ketika socket error:

```rust
netlink.disconnect();
netlink.schedule_reconnect(loop_now);
```

kemudian loop Anda bisa terus berjalan sampai:

```rust
netlink.next_deadline()
```

Ini bagus.

Tetapi:

```rust
if netlink.should_reconnect(loop_now) {
    should_evaluate = true;
    break;
}
```

lagi-lagi sebenarnya tidak perlu melakukan policy evaluation hanya karena reconnect timer jatuh tempo.

Lebih bersih:

```text
reconnect deadline
       ↓
try_reconnect()
       ↓
lanjut monitor
```

Bukan:

```text
reconnect deadline
       ↓
policy evaluate
```

Reconnect dan policy evaluation adalah event berbeda.

---

# 12. Struktur terbaik menurut saya

Saya akan pisahkan event menjadi tiga jenis:

```text
                    ┌─────────────────┐
                    │  Sensor Reader  │
                    └────────┬────────┘
                             │
                             ▼
                    ┌─────────────────┐
                    │ Decision Engine  │
                    └────────┬────────┘
                             │
                             ▼
                    ┌─────────────────┐
                    │ Hardware Control │
                    └─────────────────┘


Netlink ────────────────┐
                        │
IPC ────────────────────┼──► Event Loop
                        │
Timer ──────────────────┘
```

Dan event loop memiliki alasan wakeup:

```rust
enum WakeReason {
    Timer,
    NetlinkChange,
    ConfigReload,
    HardwareVerification,
    NetlinkReconnect,
    Shutdown,
}
```

Tidak perlu `bool should_evaluate` yang menjadi "semua hal".

---

# 13. Saya juga akan mengubah state hardware

Saat ini:

```rust
HardwareTarget
SyncState
force_apply
generation
verification
verification_failures
```

sudah lumayan bagus.

Tetapi saya akan membuat state lebih eksplisit:

```rust
pub enum HardwareState {
    Unknown,

    Applying {
        target: HardwareTarget,
        generation: u64,
    },

    Synced {
        target: HardwareTarget,
    },

    Failed {
        target: HardwareTarget,
        attempts: u8,
    },
}
```

Ini mengurangi kemungkinan kombinasi state yang tidak valid seperti:

```text
sync = Synced
force_apply = true
verification = Some(...)
```

yang secara kombinatorial sebenarnya bisa terjadi.

---

# 14. Yang sudah sangat bagus dari refactor Anda

Saya justru menilai versi kedua **jauh lebih baik** daripada versi pertama dalam beberapa aspek.

Terutama:

### Separation

```text
snapshot.rs
decision.rs
hardware.rs
scheduler.rs
netlink.rs
mod.rs
```

Ini sudah bagus.

### `OwnedFd`

```rust
OwnedFd::from_raw_fd(fd)
```

jauh lebih aman daripada memegang raw fd secara manual.

### Poll error

Ini bagus:

```rust
if ret < 0 {
    let err = std::io::Error::last_os_error();

    if err.kind() == std::io::ErrorKind::Interrupted {
        continue;
    }

    ...
}
```

### IPC error handling

Ini juga benar secara umum:

```rust
POLLERR | POLLHUP | POLLNVAL
```

### Config synchronization

Ini improvement yang penting:

```rust
scheduler.sync_config(&cfg);
```

sehingga scheduler tidak menggunakan threshold lama setelah reload.

### Generation-based verification

Ini juga bagus:

```rust
generation
```

untuk mencegah verification lama memengaruhi target baru.

Contoh:

```text
Target A
  ↓
verification pending
  ↓
config berubah
  ↓
Target B
  ↓
generation++
  ↓
verification A menjadi obsolete
```

Ini desain yang bagus.

---

# Kesimpulan

Kalau saya beri nilai:

**Versi pertama:** sekitar **6.5/10**

**Versi kedua:** sekitar **8/10**

Tetapi saya **belum akan merge versi kedua sebagai final production version**.

Urutan perbaikan yang saya prioritaskan:

```text
P0
├── Perbaiki semantics Unmanaged
├── Perbaiki verification ChargingDisabled
├── Jangan menganggap Unknown sensor = charging stopped
└── Perbaiki debounce Netlink yang sekarang belum benar-benar debounce

P1
├── Perbaiki Fault recovery agar semua sensor wajib pulih
├── Pisahkan reconnect event dari policy evaluation
├── Tangani capacity unavailable sebagai explicit degraded/fault state
└── Perjelas semantics ChargingEnabled vs AllowCharging

P2
├── Refactor HardwareController menjadi explicit state machine
├── Pisahkan Netlink event detection dari evaluation trigger
└── Tambahkan unit test untuk DecisionEngine + Scheduler + Hardware verification
```

**Yang paling penting:** jangan langsung mengutak-atik scheduler dulu. Scheduler versi kedua sudah cukup baik. Masalah yang lebih berbahaya justru berada pada **semantics `Unmanaged`, sensor failure, verification, dan debounce Netlink**.

Kalau tujuan akhirnya adalah daemon charger Android yang **benar-benar robust setelah reboot, charger plug/unplug, config reload, core restart, sensor error, dan driver charging yang kadang gagal**, saya akan menjadikan **state machine hardware + decision engine + event loop** sebagai tiga komponen yang dites secara formal sebelum optimasi scheduler.
