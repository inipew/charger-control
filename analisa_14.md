Ya. Dari kode yang kamu kirim, pola log itu **bukan terutama masalah di scheduler/netlink**, tetapi ada masalah pada desain **state machine hardware verification**. Ada satu bagian yang sangat mungkin menjadi sumber loop:

```rust
HardwareTarget::ChargingDisabled => {
    let control_disabled = control::is_charging_enabled().unwrap_or(true) == false;
    let battery_safe = matches!(
        snapshot.charging_state(),
        ChargingState::NotCharging | ChargingState::Full
    );

    control_disabled && battery_safe
}
```

Khususnya kombinasi `control::is_charging_enabled()` + `snapshot.charging_state()`.

---

# 1. Alur yang sebenarnya terjadi

Dari log:

```text
Applying hardware target: ChargingDisabled (Force: true, SyncState: Failed)
Verification failed for target ChargingDisabled
Verification failed for target ChargingDisabled
Verification failed for target ChargingDisabled
Verification failed for target ChargingDisabled
```

Kode kamu melakukan:

### Pertama

```rust
hardware.sync == SyncState::Failed
```

maka:

```rust
hardware.needs_apply(...)
```

menghasilkan `true`:

```rust
self.applied_target != new_target
    || self.force_apply
    || self.sync == SyncState::Failed
```

kemudian:

```rust
apply_target(ChargingDisabled)
```

memanggil:

```rust
control::set_charging(false)
```

dan kalau syscall berhasil:

```rust
self.mark_apply_success(target);
```

yang membuat:

```text
sync = Pending
force_apply = false
verification = Some(...)
```

---

# 2. Kemudian verification berjalan

Setelah 500 ms:

```rust
hardware.verify(&snapshot);
```

Untuk `ChargingDisabled`:

```rust
let control_disabled =
    control::is_charging_enabled().unwrap_or(true) == false;

let battery_safe =
    matches!(
        snapshot.charging_state(),
        ChargingState::NotCharging | ChargingState::Full
    );

control_disabled && battery_safe
```

Artinya **dua-duanya harus benar**.

Misalnya kondisi hardware:

```text
is_charging_enabled() = false
status = Charging
```

maka:

```text
control_disabled = true
battery_safe     = false
```

hasil:

```text
false
```

Verification gagal.

---

# 3. Masalah besar: kamu memverifikasi dua hal yang tidak necessarily sinkron

Ini menurut saya adalah kandidat **root cause utama**.

Kamu menganggap:

```text
set_charging(false)
        ↓
BatteryStatus berubah menjadi NotCharging
```

padahal belum tentu.

Pada Android/Linux power supply, ada perbedaan antara:

### A. Charging control

Misalnya:

```rust
control::is_charging_enabled()
```

Ini bisa berarti:

> apakah charging controller/kernel charging switch di-enable?

### B. Battery status

```rust
BatteryStatus::Charging
BatteryStatus::NotCharging
BatteryStatus::Full
```

Ini adalah:

> status yang dilaporkan battery/power_supply.

Keduanya **tidak harus berubah bersamaan**.

Bahkan pada beberapa implementasi driver Android, setelah charging dinonaktifkan:

```text
charging_enabled = false
status            = Charging
```

bisa tetap terjadi sementara atau bahkan cukup lama.

Jadi verification kamu terlalu ketat.

---

# 4. Kenapa akhirnya masuk loop?

Ini bagian yang membuat masalahnya menjadi loop.

Setelah verification gagal:

```rust
fn verification_failed(&mut self) {
    self.verification_failures =
        self.verification_failures.saturating_add(1);

    if self.verification_failures > MAX_VERIFICATION_RETRIES {
        self.sync = SyncState::Failed;
        self.verification = None;
        self.force_apply = true;
        return;
    }
```

Dengan:

```rust
const MAX_VERIFICATION_RETRIES: u8 = 3;
```

karena menggunakan:

```rust
>
```

bukan:

```rust
>=
```

maka sebenarnya kamu mendapatkan:

```text
failure #1 → retry
failure #2 → retry
failure #3 → retry
failure #4 → Failed
```

Jadi `MAX_VERIFICATION_RETRIES = 3` sebenarnya menghasilkan **4 verification failure**.

Setelah itu:

```rust
sync = Failed
force_apply = true
```

dan pada loop berikutnya:

```rust
needs_apply()
```

menjadi true lagi:

```rust
self.sync == SyncState::Failed
```

lalu:

```rust
set_charging(false)
```

lagi.

Kemudian verification lagi.

Kemudian gagal.

Kemudian `Failed`.

Kemudian apply lagi.

### Siklusnya:

```text
                 ┌─────────────────────┐
                 │                     │
                 ▼                     │
          set_charging(false)          │
                 │                     │
                 ▼                     │
             Pending                   │
                 │                     │
                 ▼                     │
            verification              │
                 │                     │
                 ├── success ──→ Synced
                 │
                 └── failure
                       │
                       ▼
                retry 1 / 2 / 3
                       │
                       ▼
                  failure #4
                       │
                       ▼
                    Failed
                       │
                       ▼
                 force_apply
                       │
                       ▼
                set_charging(false) ──┘
```

**Inilah loop utamanya.**

---

# 5. Ada masalah desain yang lebih serius

Menurut saya `SyncState::Failed` sekarang diperlakukan sebagai:

> "hardware harus langsung dicoba lagi"

Padahal `Failed` seharusnya lebih tepat berarti:

> "hardware belum berhasil diverifikasi; jangan terus-menerus melakukan write."

Saat ini:

```rust
if self.sync == SyncState::Failed {
    true
}
```

membuat `Failed` = **reapply terus-menerus**.

Itu desain yang berbahaya untuk daemon hardware control.

Terutama karena `set_charging(false)` bisa menyebabkan:

* write sysfs berulang
* wakeup kernel
* log spam
* state transition berulang
* race dengan Android battery service
* bahkan konflik dengan thermal/charging framework

---

# 6. Saya juga menemukan bug pada `verify()`

Perhatikan:

```rust
tracing::warn!(
    "Verification failed for target {:?}",
    self.applied_target
);
```

Lebih benar menggunakan:

```rust
v.target
```

Karena verification object punya target sendiri:

```rust
struct Verification {
    generation: u64,
    target: HardwareTarget,
    deadline: Instant,
}
```

Jadi:

```rust
tracing::warn!("Verification failed for target {:?}", v.target);
```

lebih konsisten.

Ini bukan penyebab loop, tetapi memperbaiki observability.

---

# 7. Ada bug kecil lain: `unwrap_or(true)`

Ini:

```rust
control::is_charging_enabled().unwrap_or(true) == false
```

secara logika berarti:

> kalau gagal membaca charging state, anggap charging masih enabled.

Untuk safety memang konservatif, tetapi menyebabkan verification **mustahil sukses** ketika read gagal.

Misalnya:

```rust
is_charging_enabled()
    -> Err(...)
```

maka:

```rust
unwrap_or(true)
```

menjadi:

```rust
true
```

kemudian:

```rust
true == false
```

→ false.

Akibatnya verification gagal.

Saya justru menyarankan jangan menyembunyikan error di sini.

---

# 8. Lebih baik verification dipisahkan menjadi tiga kondisi

Daripada:

```rust
control_disabled && battery_safe
```

buat status yang lebih eksplisit.

Misalnya:

```rust
enum VerificationResult {
    Verified,
    Pending,
    Failed,
}
```

Lalu:

```rust
ChargingDisabled => {
    match control::is_charging_enabled() {
        Ok(false) => {
            match snapshot.charging_state() {
                ChargingState::NotCharging | ChargingState::Full => {
                    VerificationResult::Verified
                }

                ChargingState::Charging => {
                    VerificationResult::Pending
                }

                ChargingState::Unknown => {
                    VerificationResult::Pending
                }
            }
        }

        Ok(true) => VerificationResult::Failed,

        Err(_) => VerificationResult::Pending,
    }
}
```

Tetapi ini masih tergantung semantics driver kamu.

---

# 9. Yang paling penting: tentukan apa arti `set_charging(false)`

Sebelum memperbaiki state machine, saya sangat menyarankan cek implementasi:

```rust
charger_core::battery::control
```

khususnya:

```rust
set_charging()
is_charging_enabled()
```

Karena dari kode monitor saja kita belum bisa memastikan apakah:

```rust
is_charging_enabled()
```

benar-benar merepresentasikan:

```text
charging switch
```

atau:

```text
actual charging state
```

Ini **sangat penting**.

Kalau misalnya implementasinya seperti:

```rust
pub fn set_charging(enabled: bool) -> Result<()> {
    fs::write("/sys/.../charging_enabled", ...)
}
```

dan:

```rust
pub fn is_charging_enabled() -> Result<bool> {
    ...
}
```

maka verification seharusnya memprioritaskan nilai control tersebut.

`BatteryStatus::Charging` jangan langsung dianggap sebagai kegagalan.

---

# 10. Saya juga melihat masalah di `DecisionEngine`

Ada bagian:

```rust
if snapshot.online == Some(false) {
    self.policy = ChargePolicyState::Offline;

    return self.build_decision(
        DecisionReason::ChargerOffline,
        current_target
    );
}
```

Ini memang sengaja mempertahankan target.

Jadi ketika charger dicabut:

```text
Offline
    ↓
current_target
```

Hardware tidak otomatis menjadi `Unmanaged`.

Itu mungkin benar, tapi perlu diperhatikan karena ownership hardware tetap dipertahankan.

---

# 11. Bug lain di Fault Recovery

Ini cukup penting:

```rust
if snapshot.temp_dc.is_none()
    || snapshot.capacity_pct.is_none()
    || snapshot.online.is_none()
    || snapshot.status.is_none()
{
    self.fault_recovery_reads = 0;
    self.policy = ChargePolicyState::Fault;
```

Kemudian:

```rust
if self.policy == ChargePolicyState::Fault {
    self.fault_recovery_reads += 1;

    if self.fault_recovery_reads < FAULT_RECOVERY_READS {
        return Decision {
            policy: self.policy,
            target: HardwareTarget::ChargingDisabled,
            reason: DecisionReason::FaultRecovering,
        };
    }
}
```

Ini agak aneh karena `fault_recovery_reads` hanya meningkat kalau **snapshot sekarang sudah valid**.

Itu memang bisa benar.

Tetapi kalau satu snapshot berikutnya invalid:

```rust
self.fault_recovery_reads = 0;
```

Jadi harus mendapatkan 3 snapshot valid berturut-turut.

Ini bukan bug fatal, justru cukup masuk akal untuk safety.

---

# 12. Scheduler juga memiliki bug konseptual kecil

Ini:

```rust
let distance = threshold - current;

if distance.signum() == rate.signum()
```

cukup tricky.

Untuk charging:

```text
current = 80
threshold = 90
distance = +10
rate = +0.1
```

benar.

Untuk discharging:

```text
current = 80
threshold = 70
distance = -10
rate = -0.1
```

juga benar.

Jadi secara matematika benar.

Tetapi:

```rust
SAFETY_FACTOR = 0.25
```

berarti kalau ETA ke threshold adalah 100 detik:

```text
poll = 25 detik
```

cukup masuk akal.

Bukan sumber log spam yang kamu tunjukkan.

---

# 13. Netlink juga bukan penyebab utama

Netlink kamu secara umum cukup bagus:

```rust
NETLINK_DEBOUNCE = 250ms
```

dan:

```rust
MSG_DONTWAIT
```

serta:

```rust
poll()
```

Sudah menghindari busy loop.

Netlink hanya menyebabkan:

```rust
should_evaluate = true;
```

dan tidak memanggil `set_charging()` secara langsung.

Jadi saya tidak akan membongkar `NetlinkMonitor` terlebih dahulu.

---

# 14. Masalah utama ada di hubungan 3 komponen ini

Arsitektur sekarang:

```text
DecisionEngine
      │
      ▼
HardwareController
      │
      ▼
set_charging()
      │
      ▼
Android / kernel
      │
      ▼
SensorSnapshot
      │
      ▼
verify()
      │
      ├── success → Synced
      │
      └── failure → Failed
                         │
                         ▼
                    force_apply
                         │
                         └──────→ set_charging()
```

Problemnya:

```text
verification gagal
       ↓
Failed
       ↓
force_apply
       ↓
write hardware lagi
       ↓
verification
       ↓
gagal lagi
```

Padahal kegagalan verification belum tentu berarti:

> "command set_charging() gagal."

Bisa saja:

> "command berhasil, tetapi sensor belum mencerminkan perubahan."

Ini dua hal yang berbeda.

---

# 15. Perbaikan minimal yang saya sarankan

Pertama, ubah:

```rust
if self.verification_failures > MAX_VERIFICATION_RETRIES {
```

menjadi:

```rust
if self.verification_failures >= MAX_VERIFICATION_RETRIES {
```

supaya angka retry sesuai namanya.

---

Kemudian **jangan langsung reapply ketika verification gagal**.

Saat verification timeout:

```rust
self.sync = SyncState::Failed;
self.verification = None;
self.force_apply = true;
```

jangan langsung seperti itu.

Lebih aman:

```rust
self.sync = SyncState::Failed;
self.verification = None;
self.force_apply = false;
```

Kemudian `needs_apply()` jangan:

```rust
|| self.sync == SyncState::Failed
```

melainkan:

```rust
pub fn needs_apply(&self, new_target: HardwareTarget) -> bool {
    self.applied_target != new_target || self.force_apply
}
```

Dengan demikian:

```text
apply
 ↓
Pending
 ↓
verify
 ↓
failure
 ↓
Failed
 ↓
STOP
```

bukan:

```text
failure
 ↓
Failed
 ↓
apply
 ↓
failure
 ↓
Failed
 ↓
apply
 ↓
...
```

---

# 16. Tapi saya tidak akan berhenti di situ

Untuk daemon charger, saya lebih menyarankan state machine:

```text
Unknown
   │
   ▼
Applying
   │
   ▼
Verifying
   │
   ├───────────────┐
   │               │
   ▼               ▼
Synced          RetryWait
                   │
                   ▼
                Applying
                   │
                   ▼
              max retries
                   │
                   ▼
               Failed
                   │
                   ▼
             BackoffWait
```

Jadi `Failed` **bukan berarti apply setiap 2 detik**.

Contohnya:

```text
Apply
  ↓
500ms verify
  ↓
1s verify
  ↓
2s verify
  ↓
Failed
  ↓
30s
  ↓
recovery attempt
```

Bahkan bisa exponential backoff:

```text
30s
60s
120s
300s
```

Ini jauh lebih sehat untuk Android.

---

# 17. Ada satu hal lagi yang saya ingin kamu cek dari log

Tambahkan logging sementara di `verify()`:

```rust
let control_state = control::is_charging_enabled();

tracing::warn!(
    target = ?v.target,
    control_state = ?control_state,
    battery_state = ?snapshot.charging_state(),
    online = ?snapshot.online,
    capacity = ?snapshot.capacity_pct,
    temp = ?snapshot.temp_dc,
    failures = self.verification_failures,
    "Hardware verification"
);
```

Kalau hasilnya misalnya:

```text
target=ChargingDisabled
control_state=Ok(false)
battery_state=Charging
online=Some(true)
```

maka kita langsung tahu:

**`set_charging(false)` sebenarnya berhasil, tetapi `BatteryStatus` masih `Charging`.**

Dengan kata lain, masalahnya bukan hardware apply.

Kalau hasilnya:

```text
control_state=Ok(true)
battery_state=Charging
```

berarti:

**`set_charging(false)` tidak benar-benar membuat charging control disable**, sehingga kita harus masuk ke `charger_core::battery::control`.

Kalau:

```text
control_state=Err(...)
```

maka masalahnya ada pada pembacaan control state.

---

# Kesimpulan

Dari kode yang kamu berikan, saya menilai masalahnya seperti ini:

| Bagian                                     | Penilaian                         |
| ------------------------------------------ | --------------------------------- |
| `NetlinkMonitor`                           | Bukan sumber utama                |
| `AdaptiveScheduler`                        | Bukan sumber utama                |
| `DecisionEngine`                           | Secara umum OK                    |
| `set_charging()`                           | Perlu diperiksa implementasinya   |
| `ChargingDisabled` verification            | **Sangat mungkin terlalu ketat**  |
| `unwrap_or(true)`                          | Kurang bagus untuk diagnosis      |
| `MAX_VERIFICATION_RETRIES`                 | Off-by-one                        |
| `SyncState::Failed`                        | **Desain retry menyebabkan loop** |
| `needs_apply()`                            | **Salah satu sumber loop utama**  |
| `force_apply` setelah verification failure | **Memperkuat loop**               |

Jadi akar masalah paling penting adalah:

> **`Verification failed` diperlakukan sebagai alasan untuk mengulang `set_charging(false)` terus-menerus, padahal verification failure bisa saja hanya berarti sensor `BatteryStatus` belum sesuai dengan hardware control.**

Dan ada indikasi kuat dari interval log kamu:

```text
38.255
39.255
41.257
43.258
```

yang **persis mengikuti `VERIFY_DELAYS`**:

```text
500 ms
1 s
2 s
2 s
```

Artinya verification state machine memang bekerja sesuai kode. Yang salah adalah **kondisi verifikasinya atau kebijakan setelah verification gagal**, bukan `poll()` yang busy-loop.

Kalau kamu kirim juga **kode `charger_core::battery::control` (`set_charging()` + `is_charging_enabled()`) dan `CachedReader` (`read_status()` + `is_plugged_in()`)**, saya bisa telusuri sampai level syscall/sysfs dan menentukan **persis apakah yang salah adalah verification condition, driver semantics, atau `set_charging()`-nya**, lalu kita bisa redesign `HardwareController` supaya tidak looping lagi.
