Ya, **arah arsitekturnya sudah jauh lebih benar** dibanding versi sebelumnya. Pemisahan menjadi:

`SensorSnapshot → DecisionEngine → HardwareController → Scheduler/Netlink`

itu sudah cocok untuk daemon charger Android yang ingin robust.

Tapi saya **belum akan menyebut kode ini final**, karena ada beberapa bug/edge case yang cukup penting. Yang paling serius justru ada di pembacaan current dan scheduler.

### 1. `SensorSnapshot` sudah bagus

```rust
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temp_dc: Option<i32>,
    pub current_ma: Option<i32>,
    pub status: Option<BatteryStatus>,
    pub online: Option<bool>,
    pub ts: Instant,
}
```

Ini sudah tepat sebagai **immutable snapshot satu siklus evaluasi**.

Saya hanya akan mengubah `current_ma` menjadi `Option<f32>` kalau memang ingin mempertahankan presisi:

```rust
pub current_ma: Option<f32>,
```

karena reader kamu memang menghasilkan `f32`.

Kalau memang seluruh logic hanya butuh integer mA, `i32` juga tidak masalah.

---

# 2. Ada bug penting di `read_current_ma()`

Ini yang paling saya sarankan diperbaiki.

Sekarang:

```rust
let mut ua = val as f32;

if ua.abs() > 10_000.0 {
    ua /= 1000.0;
}

return Ok(ua);
```

Ini **tidak aman**.

Misalnya Android memberikan:

```text
current_now = 5000
```

dan unitnya µA.

Artinya:

```text
5000 µA = 5 mA
```

Tetapi kode kamu menghasilkan:

```text
5000 mA
```

karena hanya nilai `> 10000` yang dibagi 1000.

Akibatnya:

```rust
let current_safe = snapshot.current_ma.map_or(true, |c| c <= 100);
```

akan menganggap:

```text
5000 µA → 5000 mA → tidak safe
```

Padahal sebenarnya:

```text
5000 µA → 5 mA → sangat safe
```

### Lebih baik jangan tebak unit berdasarkan magnitude

Kalau node Android kamu memang `current_now`, umumnya gunakan µA secara eksplisit:

```rust
pub fn read_current_ma() -> Result<f32, ChargerError> {
    let ua = read_current_ua()?;
    Ok(ua as f32 / 1000.0)
}
```

Dan `CachedReader`:

```rust
pub fn read_current_ma(&mut self) -> Result<f32, ChargerError> {
    let s = Self::read_fd_to_str(
        &mut self.current_fd,
        &mut self.buf,
        "current_now",
    )?;

    match s.parse::<i64>() {
        Ok(ua) => Ok(ua as f32 / 1000.0),
        Err(_) => Err(ChargerError::ParseError("current_now")),
    }
}
```

Kalau perangkat kamu punya vendor node yang memang satuannya mA, **buat metadata/unit per node**, jangan heuristic `> 10_000`.

---

# 3. `is_plugged_in()` punya fallback yang terlalu berbahaya

Ini:

```rust
Ok(true)
```

sebagai default:

```rust
pub fn is_plugged_in() -> Result<bool, ChargerError>
```

menurut saya **jangan dilakukan**.

Karena:

> "Tidak tahu charger terpasang"

berubah menjadi:

> "Anggap charger terpasang."

Untuk charger controller, itu bukan safe fallback.

Misalnya:

```text
/sys/class/power_supply/
```

berubah karena vendor/kernel/USB subsystem.

Kalau tidak menemukan `online`, lebih aman:

```rust
Err(ChargerError::NoChargingNodeFound)
```

atau kalau ingin membedakan:

```rust
Ok(None)
```

dengan tipe:

```rust
Result<Option<bool>, ChargerError>
```

Untuk arsitektur kamu, saya malah lebih suka:

```rust
pub online: Option<bool>
```

sehingga:

```text
Some(true)  = online
Some(false) = offline
None        = unknown
```

dan **jangan pernah mengubah `None` menjadi `true`**.

---

# 4. `DecisionEngine` sudah benar secara konsep

Bagian ini bagus:

```rust
if snapshot.online == Some(false) {
    self.policy = ChargePolicyState::Offline;
    return self.build_decision(
        DecisionReason::ChargerOffline,
        current_target
    );
}
```

Ini penting karena saat kabel dicabut, daemon tidak langsung mengambil alih hardware hanya karena charger offline.

Kemudian:

```rust
ChargePolicyState::Disabled => HardwareTarget::Unmanaged,
```

juga bagus.

Artinya:

```text
daemon disabled
       ↓
Unmanaged
       ↓
restore original state
       ↓
daemon tidak lagi mengontrol charging
```

Ini jauh lebih sehat daripada:

```text
disabled → charging_enabled = 1
```

karena itu bisa merusak state yang sebelumnya memang dibuat user/kernel/vendor.

---

# 5. Fault handling kamu masih punya masalah kecil

Ini:

```rust
if snapshot.temp_dc.is_none()
    || snapshot.capacity_pct.is_none()
    || snapshot.online.is_none()
    || snapshot.status.is_none()
```

kemudian:

```rust
self.policy = ChargePolicyState::Fault;
```

dan:

```rust
target: self.policy_to_target(self.policy)
```

akan menghasilkan:

```rust
Fault → ChargingDisabled
```

Ini **sangat bagus untuk safety**.

Tetapi setelah fault:

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

Ada satu detail:

`fault_recovery_reads` hanya bertambah ketika **semua sensor kembali tersedia**.

Itu benar.

Tetapi sebaiknya recovery juga memastikan:

```text
online == true
status valid
capacity valid
temperature valid
```

sebelum kembali ke normal.

Kamu sudah hampir melakukan itu karena validasi sensor berada sebelum blok recovery.

Jadi bagian ini sebenarnya **sudah cukup baik**.

---

# 6. Hardware ownership kamu sudah bagus

Bagian ini menurut saya salah satu bagian terbaik:

```rust
if self.ownership == Ownership::NotOwned {
    match control::is_charging_enabled() {
        Ok(original) => {
            save_persistent_ownership(original);
            self.ownership =
                Ownership::Owned {
                    original_charging: original
                };
        }
```

Jadi:

```text
daemon mulai mengubah hardware
        ↓
baca state asli
        ↓
persist original state
        ↓
baru takeover
```

Kemudian kalau crash:

```text
process mati
        ↓
ownership.state masih ada
        ↓
daemon berikutnya restore
        ↓
hapus ownership.state
```

Itu sudah pola **crash recovery** yang bagus.

Atomic-ish persistence kamu juga sudah bagus:

```rust
write(temp)
rename(temp, state)
```

---

# 7. Tapi ada masalah pada `set_charging()`

Sekarang:

```rust
for node in CHARGING_NODES {
    if path.exists() && write_sysfs(path, charge_val).is_ok() {
        any_written = true;
    }
}
```

Misalnya ada 3 node:

```text
battery/charging_enabled     berhasil
main/charging_enabled        gagal
usb/charging_enabled         gagal
```

fungsi tetap:

```rust
Ok(())
```

padahal hardware bisa berada dalam **partial state**.

Lebih bagus track:

```rust
found
success
failed
```

misalnya konsepnya:

```rust
let mut found = false;
let mut success = false;
let mut failures = 0;

for node in ... {
    let path = Path::new(node);

    if !path.exists() {
        continue;
    }

    found = true;

    match write_sysfs(path, value) {
        Ok(()) => {
            success = true;
        }
        Err(e) => {
            failures += 1;
            tracing::warn!("Failed writing {}: {}", node, e);
        }
    }
}
```

Kemudian minimal:

```text
tidak ada node      → Err(NoChargingNodeFound)
ada + ada sukses    → Ok
ada + semua gagal   → Err
```

Jadi jangan hanya:

```rust
any_written
```

---

# 8. `enter_bypass_mode()` / `exit_bypass_mode()` saat ini misleading

Kamu punya:

```rust
pub fn enter_bypass_mode() -> Result<(), ChargerError>
```

tetapi:

```rust
for ... {
    if p.exists() {
        let _ = write_sysfs(p, val);
    }
}

Ok(())
```

Artinya bahkan kalau **semua write gagal**, hasilnya:

```rust
Ok(())
```

Ini berbahaya.

Harus mengikuti prinsip yang sama seperti `set_charging()`.

Misalnya:

```text
0 node ditemukan → Err
node ditemukan tetapi semua write gagal → Err
minimal satu write sukses → Ok
```

Dan sebenarnya kalau arsitektur baru kamu memang memakai:

```rust
HardwareTarget::ChargingEnabled
HardwareTarget::ChargingDisabled
HardwareTarget::Unmanaged
```

maka bypass sebaiknya **jangan memiliki jalur kontrol terpisah** kecuali memang ada mode bypass yang berbeda dari charging-disabled.

---

# 9. Scheduler: ada bug konseptual di `eta_to()`

Ini:

```rust
let distance = threshold - current;

if distance.signum() == rate.signum()
    && rate.abs() > 0.01
```

secara matematika memang bekerja untuk:

```text
current 50
target 80
rate +1
distance +30
```

dan:

```text
current 80
target 50
rate -1
distance -30
```

Tetapi nama:

```rust
safety
```

dan:

```rust
seconds = ETA * safety
```

perlu diperjelas.

Kalau:

```text
ETA = 100 detik
SAFETY_FACTOR = 0.25
```

maka polling:

```text
25 detik
```

Itu berarti bangun **25% sebelum threshold**, yang memang masuk akal.

Tetapi:

```rust
THERMAL_SAFETY_FACTOR = 0.15
```

berarti polling pada:

```text
15% ETA
```

juga masuk akal karena thermal lebih sensitif.

Jadi **bukan bug**, hanya naming-nya bisa lebih jelas:

```rust
const CAPACITY_WAKE_FACTOR: f32 = 0.25;
const THERMAL_WAKE_FACTOR: f32 = 0.15;
```

lebih jelas daripada:

```rust
SAFETY_FACTOR
THERMAL_SAFETY_FACTOR
```

---

# 10. Ada masalah lebih besar di scheduler: polling berdasarkan capacity rate bisa terlalu lambat

Contoh:

```text
battery 50%
limit 80%
charging rate +0.01 %/s
```

ETA:

```text
3000 detik
```

dengan factor:

```text
0.25
```

scheduler bisa memilih:

```text
750 detik
```

lalu akhirnya di-clamp:

```rust
MAX_INTERVAL = 90s
```

Jadi aman.

Namun kalau:

```text
rate sangat kecil
```

dan thermal juga tidak berubah:

```text
fallback
```

akan digunakan.

Ini masih oke karena:

```rust
MAX_INTERVAL = 90s
```

Jadi tidak sampai sleep berjam-jam.

---

# 11. Namun unplugged heartbeat 600 detik cukup panjang

Ini:

```rust
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600);
```

berarti:

```text
charger dicabut
↓
netlink hidup
↓
tidur sampai 10 menit
```

Memang netlink akan menangkap:

```text
SUBSYSTEM=power_supply
ACTION=change
```

jadi secara desain masuk akal.

Saya malah **suka pendekatan ini**:

```text
Netlink = event-driven
Scheduler = fallback polling
```

Daripada polling setiap 1–2 detik.

Kalau netlink gagal:

```rust
UNPLUGGED_HEARTBEAT_NO_NETLINK = 30s
```

juga bagus.

---

# 12. Ada bug kecil pada event netlink

Kamu hanya menerima:

```rust
ACTION=change
```

dan:

```rust
SUBSYSTEM=power_supply
```

Bagus.

Tetapi setelah `handle_events()` menemukan event:

```rust
self.debounce_target = Some(now + NETLINK_DEBOUNCE);
```

Kalau event terus datang:

```text
t=0     change
t=100ms change
t=200ms change
```

maka deadline terus digeser:

```text
350ms
450ms
500ms
```

Ini sebenarnya bisa dianggap **debounce yang benar**, tetapi artinya event burst dapat menunda evaluasi.

Untuk power supply, itu biasanya justru diinginkan.

---

# 13. Ada masalah pada loop utama yang perlu kamu perhatikan

Bagian:

```rust
let decision = engine.evaluate(...);
```

kemudian:

```rust
if decision.target != hardware.desired_target {
    ...
    hardware.desired_target = decision.target;
}
```

lalu:

```rust
if hardware.needs_apply(decision.target, now) {
    hardware.apply_target(decision.target);
}
```

Sebenarnya `desired_target` tidak perlu diubah manual di luar `apply_target()`.

Lebih bersih:

```rust
if decision.target != hardware.desired_target {
    tracing::info!(...);

    hardware.invalidate_verification();
    hardware.force_apply = true;
}
```

lalu:

```rust
if hardware.needs_apply(decision.target, now) {
    hardware.apply_target(decision.target);
}
```

Karena:

```rust
apply_target()
```

sendiri sudah:

```rust
self.desired_target = target;
```

Dengan begitu hanya ada **satu tempat** yang bertanggung jawab mengubah desired target.

---

# 14. Ini yang paling saya ubah di `main loop`

Sekarang:

```rust
if decision.target != hardware.desired_target {
    hardware.invalidate_verification();
    hardware.force_apply = true;
    hardware.desired_target = decision.target;
}
```

Saya rekomendasikan:

```rust
if decision.target != hardware.desired_target {
    tracing::info!(
        "Policy target changed: {:?} -> {:?} (reason={}, policy={:?})",
        hardware.desired_target,
        decision.target,
        decision.reason,
        decision.policy
    );

    hardware.invalidate_verification();
    hardware.force_apply = true;
}
```

Lalu:

```rust
if hardware.needs_apply(decision.target, now) {
    hardware.apply_target(decision.target);
}
```

Jadi lifecycle-nya:

```text
Decision
   │
   ▼
target berbeda?
   │
   ├── no ────────► nothing
   │
   └── yes
       │
       ▼
invalidate verification
       │
       ▼
force apply
       │
       ▼
apply_target(target)
       │
       ▼
desired_target = target
       │
       ▼
write sysfs
       │
       ▼
Pending verification
```

Lebih deterministic.

---

# 15. Satu hal lagi: verification disabled charging

Kamu punya:

```rust
let current_safe = snapshot.current_ma.map_or(true, |c| c <= 100);
```

Ini sebenarnya terlalu sederhana.

Kalau `current_ma` negatif ketika discharge:

```text
-500 mA
```

maka:

```rust
-500 <= 100
```

→ true.

Bagus.

Tapi jika:

```text
+50 mA
```

→ true.

Juga bagus.

Yang lebih tepat:

```rust
let current_safe = snapshot
    .current_ma
    .map_or(true, |ma| ma <= 100);
```

sudah secara konsep benar **jika positive = charging**.

Tetapi sekali lagi, ini bergantung pada unit current yang benar.

Jadi fix #2 tadi sangat penting.

---

# 16. `status` sekarang sebenarnya belum dimanfaatkan oleh decision engine

Kamu membaca:

```rust
status: Option<BatteryStatus>
```

dan memvalidasinya:

```rust
snapshot.status.is_none()
```

tetapi setelah itu tidak digunakan.

Misalnya:

```text
status = Discharging
online = true
capacity = 70
```

DecisionEngine masih bisa memutuskan:

```text
ChargingEnabled
```

padahal secara aktual battery sedang discharge meskipun charger online.

Itu **bisa normal** pada kondisi tertentu, tetapi status seharusnya digunakan untuk sanity checking.

Minimal saya akan tambahkan:

```rust
if snapshot.online == Some(true)
    && snapshot.status == Some(BatteryStatus::Discharging)
{
    tracing::warn!("Charger online but battery reports discharging");
}
```

Jangan langsung Fault hanya karena itu, karena Android/vendor charger bisa memang melaporkan transisi seperti ini.

---

# Kesimpulan

**Ya, desain besarnya sudah benar.** Bahkan menurut saya sekarang sudah masuk arah arsitektur daemon yang jauh lebih profesional:

```text
                    ┌──────────────────┐
                    │   CachedReader   │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ SensorSnapshot   │
                    └────────┬─────────┘
                             │
              ┌──────────────┴──────────────┐
              ▼                             ▼
      ┌───────────────┐             ┌───────────────┐
      │ DecisionEngine│             │   Scheduler   │
      └───────┬───────┘             └───────────────┘
              │
              ▼
      ┌───────────────────┐
      │ HardwareController│
      └─────────┬─────────┘
                │
                ▼
             sysfs
```

Dan:

```text
Netlink
   │
   ├── power_supply change
   │
   ▼
debounce
   │
   ▼
re-evaluate
```

adalah desain yang bagus untuk menghindari polling agresif.

### Tetapi sebelum dianggap final, saya akan prioritaskan 5 perubahan ini:

| Prioritas | Masalah                                                | Status               |
| --------- | ------------------------------------------------------ | -------------------- |
| 🔴 1      | `current_now` unit heuristic                           | **Harus diperbaiki** |
| 🔴 2      | `is_plugged_in() -> Ok(true)` fallback                 | **Harus diperbaiki** |
| 🔴 3      | `set_charging()` menerima partial write sebagai sukses | **Harus diperbaiki** |
| 🟠 4      | `enter/exit_bypass_mode()` selalu `Ok(())`             | **Perlu diperbaiki** |
| 🟡 5      | `desired_target` diubah dari dua tempat                | **Rapikan**          |

Selain itu, **state machine hardware + ownership persistence + verification + backoff + netlink + adaptive scheduler-nya sudah berada di arah yang tepat**.

Yang paling penting: **jangan lanjut optimasi scheduler dulu sebelum `current_now` dan semantics `is_plugged_in()` dibereskan**, karena dua hal tersebut menyentuh keputusan safety utama (`ChargingDisabled` vs `ChargingEnabled`).
