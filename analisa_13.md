Ya, versi ini **lebih aman daripada versi sebelumnya**, terutama karena sudah ada `owns_hardware` dan jalur `POLLHUP/POLLERR` juga memanggil `shutdown_restore()`.

Tetapi setelah saya telusuri state machine-nya, masih ada beberapa bug desain yang cukup penting. Yang paling besar justru ada pada konsep **ownership dan restore state**.

## 1. Bug terbesar: `Unmanaged` belum benar-benar melepaskan ownership

Sekarang:

```rust
HardwareTarget::Unmanaged => {
    tracing::debug!("Entering Unmanaged state; charging hardware left untouched");

    self.force_apply = false;
    self.sync = SyncState::Synced;
    self.verification = None;
    self.verification_failures = 0;
}
```

Masalahnya: kalau sebelumnya daemon melakukan:

```text
ChargingEnabled
        ↓
ChargingDisabled
        ↓
Unmanaged
```

`owns_hardware` tetap:

```rust
true
```

Karena pada `Unmanaged` tidak pernah:

```rust
self.owns_hardware = false;
```

Tetapi lebih penting lagi, **jangan langsung mengubahnya menjadi false**.

Kenapa?

Karena `Unmanaged` seharusnya berarti:

> daemon berhenti mengontrol hardware dan mengembalikan hardware ke keadaan yang seharusnya dimiliki sistem.

Kalau sebelumnya daemon melakukan:

```text
set_charging(false)
```

kemudian config daemon dibuat disabled:

```text
Disabled
→ Unmanaged
```

kode sekarang **tidak melakukan `set_charging(true)`**.

Akibatnya charging tetap disabled.

Jadi:

```text
daemon aktif
    ↓
limit 80%
    ↓
set_charging(false)
    ↓
user disable daemon
    ↓
Unmanaged
    ↓
❌ charging masih disabled
```

Ini bug nyata.

---

# 2. `shutdown_restore()` masih salah secara konsep

Sekarang:

```rust
match control::set_charging(true) {
```

Ini berarti daemon selalu mengasumsikan:

> keadaan sistem sebelum daemon mengambil alih = charging enabled.

Padahal belum tentu.

Contoh:

```text
Sistem:
charging control = OFF

daemon start
    ↓
daemon set OFF
    ↓
shutdown
    ↓
daemon set ON
```

Daemon justru **mengubah keadaan sistem**.

Yang benar adalah daemon harus menyimpan **original hardware state sebelum modifikasi pertama**.

Misalnya:

```rust
pub struct HardwareController {
    ...
    original_state: Option<bool>,
    owns_hardware: bool,
}
```

Saat pertama kali daemon akan melakukan perubahan:

```rust
if !self.owns_hardware {
    self.original_state = Some(control::get_charging()?);
    self.owns_hardware = true;
}
```

Kemudian daemon bebas melakukan:

```text
ON
OFF
ON
OFF
...
```

dan ketika berhenti:

```rust
if let Some(original) = self.original_state {
    control::set_charging(original)?;
}
```

Jadi:

```text
System sebelum daemon
        │
        ▼
   original = OFF
        │
        ▼
daemon mengambil ownership
        │
   ┌────┴────┐
   ▼         ▼
  ON        OFF
   │         │
   └────┬────┘
        ▼
daemon shutdown
        │
        ▼
restore OFF
```

**Ini jauh lebih benar daripada selalu `set_charging(true)`.**

---

# 3. `owns_hardware` baru diset setelah `set_charging()` sukses

Ini juga problem.

Sekarang:

```rust
match control::set_charging(true) {
    Ok(()) => self.mark_apply_success(target),
```

dan:

```rust
fn mark_apply_success(...) {
    ...
    self.owns_hardware = true;
}
```

Artinya ownership baru dicatat **setelah perubahan berhasil**.

Secara umum ini masuk akal, tetapi untuk lifecycle daemon lebih aman konsepnya:

```text
read original state
      ↓
record ownership
      ↓
attempt modification
      ↓
verify modification
```

Kenapa?

Karena ownership bukan sama dengan "last write succeeded".

Ownership berarti:

> daemon sudah mengambil tanggung jawab terhadap resource tersebut.

Jadi lebih baik:

```rust
if !self.owns_hardware {
    self.capture_original_state()?;
    self.owns_hardware = true;
}
```

baru kemudian:

```rust
control::set_charging(...)
```

Kalau write gagal, daemon tetap tahu bahwa lifecycle-nya sudah mencoba mengambil resource dan punya state awal yang perlu diperhatikan.

---

# 4. `Unmanaged` harus dibedakan dari "jangan sentuh hardware"

Ini konsep penting.

Saat ini Anda mendefinisikan:

```rust
Unmanaged
```

seolah:

> jangan sentuh hardware.

Padahal ada dua keadaan berbeda:

### A. Daemon belum pernah mengambil hardware

```text
Unmanaged
owns_hardware = false
```

→ jangan sentuh.

### B. Daemon sudah mengambil hardware lalu ingin melepaskan

```text
Unmanaged
owns_hardware = true
```

→ **restore original state terlebih dahulu**, baru:

```text
owns_hardware = false
```

Jadi `Unmanaged` sebenarnya merupakan **transition/lifecycle operation**, bukan sekadar hardware target.

Saya akan membuat:

```rust
pub enum HardwareTarget {
    ChargingEnabled,
    ChargingDisabled,
    Unmanaged,
}
```

tetap boleh, tetapi implementasinya:

```rust
Unmanaged => {
    self.release_ownership();
}
```

bukan sekadar:

```rust
// Do NOT touch kernel
```

---

# 5. Ada bug penting ketika `cfg.enabled = false`

Misalnya:

```text
cfg.enabled = true
capacity = 80
```

daemon:

```text
ChargingDisabled
```

Kemudian user mematikan daemon:

```text
cfg.enabled = false
```

Decision:

```rust
ChargePolicyState::Disabled
→ HardwareTarget::Unmanaged
```

Lalu:

```rust
hardware.apply_target(Unmanaged)
```

Tidak melakukan restore.

Jadi kondisi akhirnya:

```text
daemon OFF
charging control = OFF
```

Padahal tujuan Anda:

```text
daemon OFF
charging control = keadaan sistem semula
```

**Ini salah satu bug yang paling perlu diperbaiki.**

---

# 6. `Disabled` dan `Offline` tidak seharusnya memiliki arti hardware yang sama

Sekarang:

```rust
ChargePolicyState::Disabled | ChargePolicyState::Offline
    => HardwareTarget::Unmanaged
```

Padahal secara lifecycle keduanya berbeda.

### Disabled

User sengaja mematikan daemon.

Ideal:

```text
Disabled
→ release ownership
→ restore original state
```

### Offline

Charger dicabut.

Tidak perlu melakukan restore ke original state.

Misalnya:

```text
daemon start
original = charging ON

charger plugged
daemon limit → charging OFF

charger unplugged
→ Offline
```

Jangan melakukan:

```text
Offline → restore original
```

karena charger memang sedang tidak terhubung.

Jadi `Offline` lebih tepat sebagai:

```text
daemon tetap memiliki ownership
hardware policy temporarily inactive
```

sedangkan `Disabled`:

```text
daemon relinquishes ownership
```

Ini penting.

---

# 7. `Unmanaged` juga bermasalah ketika charger dicabut

Contoh:

```text
charging = ON
charger dicabut
```

decision:

```text
Offline
→ Unmanaged
```

Lalu daemon tidak lagi dianggap mengontrol hardware secara eksplisit.

Kemudian charger dipasang lagi:

```text
Netlink event
→ Charging
→ set_charging(true)
```

Ini kemungkinan tetap bekerja.

Tetapi state ownership menjadi ambigu:

```text
owns_hardware = true
target = Unmanaged
sync = Synced
```

Jadi:

```text
Unmanaged ≠ owns_hardware false
```

dalam implementasi Anda sekarang.

Itu tanda bahwa state model perlu sedikit dirapikan.

---

# 8. `shutdown_restore()` tidak benar-benar menjamin restore ketika proses mati

Ini sangat penting di Android/Linux.

Kode Anda menangani:

```text
IPC shutdown
POLLHUP
POLLERR
```

Tetapi tidak menangani:

```text
SIGKILL
kill -9
kernel crash
OOM killer
power loss
battery disconnect
force stop
```

Contoh:

```text
daemon
 ↓
set_charging(false)
 ↓
process crash
```

Tidak ada:

```rust
shutdown_restore()
```

Akibatnya:

```text
charging = OFF
daemon = DEAD
```

### Jadi jangan menganggap `shutdown_restore()` sebagai absolute guarantee.

`SIGTERM`/graceful shutdown → bisa restore.

`SIGKILL` → **tidak bisa**.

---

# 9. Solusi profesional: persistent ownership marker

Kalau ini daemon Android/root yang mengontrol charging, saya sangat menyarankan menyimpan state ke file.

Misalnya:

```text
/data/adb/.../charger-state.json
```

atau:

```text
/data/local/tmp/charger-daemon-state
```

Isi konseptual:

```json
{
    "owns_hardware": true,
    "original_charging": true
}
```

Lifecycle:

```text
daemon start
    ↓
cek state file
    ↓
ada ownership dari instance sebelumnya?
    ↓
restore original
    ↓
hapus stale state
    ↓
start normal
```

Saat mengambil ownership:

```text
capture original state
        ↓
atomic write state file
        ↓
set_charging(...)
```

Saat shutdown:

```text
restore original
        ↓
hapus state file
        ↓
exit
```

Dengan begitu kalau:

```text
daemon crash
```

instance berikutnya masih tahu:

```text
daemon sebelumnya pernah mengubah charging
original state = ON
```

---

# 10. Bahkan lebih aman: write state sebelum hardware mutation

Urutan yang saya sarankan:

```text
READ current hardware state
        ↓
WRITE persistent ownership record
        ↓
fsync
        ↓
MODIFY hardware
        ↓
VERIFY
```

Bukan:

```text
MODIFY hardware
        ↓
WRITE state
```

Karena kalau proses mati tepat di antara dua operasi:

```text
set_charging(false)
        ↓
💥 crash
        ↓
belum sempat save state
```

Anda kehilangan informasi bahwa daemon mengubah hardware.

Dengan:

```text
save state
        ↓
set hardware
```

crash setelah state disimpan masih recoverable.

---

# 11. `mark_apply_failed()` bisa meninggalkan state yang membingungkan

Sekarang:

```rust
fn mark_apply_failed(&mut self) {
    self.invalidate_verification();
    self.force_apply = true;
    self.sync = SyncState::Failed;
}
```

Tetapi:

```rust
self.target
```

sudah diubah sebelumnya:

```rust
self.target = target;
```

Jadi:

```text
target = ChargingDisabled
set_charging(false) gagal
```

state menjadi:

```text
target = ChargingDisabled
sync = Failed
```

Padahal hardware **belum tentu ChargingDisabled**.

Ini berarti `target` sekarang lebih mirip:

> desired target

bukan:

> actual target.

Itu sebenarnya boleh, tetapi namanya sebaiknya jelas.

Saya sarankan pisahkan:

```rust
desired: HardwareTarget,
actual: Option<bool>,
```

atau minimal:

```rust
desired_target
```

daripada:

```rust
target
```

---

# 12. `verify()` untuk `ChargingDisabled` terlalu longgar

Sekarang:

```rust
matches!(
    snapshot.charging_state(),
    ChargingState::NotCharging | ChargingState::Full
)
```

Masalah:

```text
ChargingDisabled
```

tetapi:

```text
battery status = Full
```

dianggap sukses.

Padahal `Full` tidak membuktikan bahwa:

```text
charging control = disabled
```

Bisa saja charger control masih enabled tetapi battery sudah full.

Jadi verification:

```rust
ChargingDisabled
```

idealnya memverifikasi **control interface**, bukan hanya battery status.

Misalnya kalau kernel menyediakan:

```text
charge_control
charging_enabled
input_suspend
charge_disable
```

baca kembali nilai control tersebut.

Dengan kata lain:

```text
write control
   ↓
read control
   ↓
compare
```

lebih kuat daripada:

```text
write control
   ↓
lihat BatteryStatus
```

---

# 13. `ChargingEnabled` juga verification-nya agak lemah

Sekarang:

```rust
snapshot.online == Some(true)
&& matches!(
    charging_state,
    Charging | Full
)
```

Kalau battery sudah:

```text
100%
Full
```

Anda menganggap enabled sukses.

Tetapi lagi-lagi:

```text
Full
```

tidak membuktikan charging control benar-benar enabled.

Lebih baik:

```text
control state == enabled
```

sebagai primary verification.

Battery status hanya secondary sanity check.

---

# 14. `online == None` bisa membuat daemon salah mengambil keputusan

Ini:

```rust
if snapshot.online == Some(false) {
    ...
}
```

Berarti:

```text
online = None
```

bukan dianggap offline.

Kemudian kalau:

```text
capacity = Some(...)
temp = Some(...)
online = None
```

engine dapat:

```text
Charging
→ ChargingEnabled
```

dan:

```rust
control::set_charging(true)
```

Padahal daemon belum tahu charger online atau tidak.

Untuk charging daemon, saya lebih suka:

```text
Some(false) → Offline
Some(true)  → normal
None        → Fault / Unknown
```

Daripada:

```text
None → anggap plugged
```

---

# 15. Ada masalah pada `Fault`

Sekarang:

```rust
if snapshot.temp_dc.is_none() || snapshot.capacity_pct.is_none() {
    ...
    self.policy = ChargePolicyState::Fault;
}
```

tetapi:

```rust
snapshot.online == None
```

tidak termasuk fault.

Saya akan pertimbangkan:

```text
capacity missing → fault
temperature missing → fault
online missing → degraded/fault
status missing → degraded/fault
```

tergantung kemampuan `CachedReader`.

Untuk hardware safety, **unknown jangan dianggap safe**.

---

# 16. `fault_recovery_reads` punya edge case

Misalnya:

```text
Fault
 ↓
read normal
 ↓
recovery 1
 ↓
read normal
 ↓
recovery 2
 ↓
read normal
 ↓
recovery 3
 ↓
normal
```

Ini oke.

Tetapi jika selama recovery:

```text
online = false
```

engine langsung:

```text
Offline
```

dan counter recovery tidak di-reset.

Ketika dicolok lagi, counter lama bisa masih tersimpan.

Lebih bersih kalau setiap transition keluar ke kondisi non-Fault:

```rust
self.fault_recovery_reads = 0;
```

---

# 17. `scheduler.observe()` masih memasukkan data ketika state `Unknown`

Ini:

```rust
if hardware.sync == SyncState::Synced || hardware.sync == SyncState::Unknown {
    scheduler.observe(&snapshot);
}
```

`Unknown` justru bisa berarti:

```text
hardware belum diketahui
```

atau:

```text
verification invalidated
```

Jadi Anda berpotensi memasukkan transient sample ke EMA.

Lebih aman:

```rust
if hardware.sync == SyncState::Synced {
    scheduler.observe(&snapshot);
}
```

Dan bahkan lebih bagus:

```text
only observe when:
- snapshot valid
- online == Some(true)
- hardware synchronized
- target stable
```

---

# 18. Netlink hanya mencari `ACTION=change`

Ini:

```rust
ACTION=change
```

umumnya bagus, tetapi event power supply bisa datang dalam bentuk yang lebih kompleks.

Anda juga sebaiknya tidak terlalu bergantung pada event netlink sebagai satu-satunya mekanisme correctness.

Model yang benar:

```text
Netlink
   ↓
wake up quickly

Periodic scheduler
   ↓
safety fallback
```

Dan kode Anda sebenarnya sudah menuju desain ini.

Ini bagus.

---

# 19. `poll()` timeout bisa menjadi 0

Ada:

```rust
(remaining.as_millis() as u64)
```

Kalau:

```text
remaining < 1ms
```

hasil:

```text
0
```

`poll(..., 0)` menjadi busy-ish evaluation.

Tidak terlalu serius karena scheduler minimum 2 detik, tetapi deadline verification bisa menyebabkan kondisi tersebut.

Bisa dibuat:

```rust
let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
```

---

# 20. `shutdown_restore()` sebaiknya diverifikasi

Sekarang:

```rust
control::set_charging(true)
```

berhasil → langsung dianggap restore berhasil.

Saya lebih suka:

```text
set original state
       ↓
read control state
       ↓
verify
       ↓
only then mark ownership released
```

Contoh:

```rust
set_charging(original)?;

let actual = control::get_charging()?;

if actual != original {
    return Err(...);
}
```

Ini penting karena `write()` sukses tidak selalu berarti hardware benar-benar mengikuti state tersebut.

---

# Arsitektur yang saya rekomendasikan

Saya akan ubah konsep `HardwareController` menjadi kira-kira:

```rust
pub struct HardwareController {
    desired: HardwareTarget,

    sync: SyncState,
    force_apply: bool,

    ownership: Ownership,

    generation: u64,
    verification: Option<Verification>,
    verification_failures: u8,
}

#[derive(Debug)]
enum Ownership {
    None,
    Owned {
        original_charging: bool,
    },
}
```

Kemudian lifecycle:

### Daemon start

```text
load persistent state
        ↓
stale ownership?
        ↓
restore original state
        ↓
clear stale state
        ↓
start
```

### First hardware modification

```text
read current charging control
        ↓
save original state persistently
        ↓
ownership = Owned(original)
        ↓
set charging
        ↓
verify
```

### Normal runtime

```text
Owned(original)
      │
      ├── enable
      ├── disable
      ├── enable
      └── disable
```

### Daemon disabled

```text
cfg.enabled = false
        ↓
restore original
        ↓
ownership = None
        ↓
Unmanaged
```

### Daemon shutdown

```text
restore original
        ↓
verify
        ↓
clear persistent state
        ↓
exit
```

### Charger unplugged

**Jangan release ownership.**

```text
Online
 ↓
Offline
 ↓
keep ownership
 ↓
wait
 ↓
Online
 ↓
continue policy
```

Ini penting sekali.

---

# State machine yang menurut saya lebih tepat

Daripada:

```text
Disabled → Unmanaged
Offline  → Unmanaged
```

lebih baik:

```text
                    ┌─────────────────┐
                    │   NotOwned      │
                    └────────┬────────┘
                             │
                       first control
                             │
                             ▼
                    ┌─────────────────┐
                    │     Owned       │
                    │ original=ON/OFF │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
              ▼              ▼              ▼
          Charging       Limited        ThermalFault
           enabled        disabled         disabled
              │              │              │
              └──────────────┴──────────────┘
                             │
                       daemon disabled
                             │
                             ▼
                    restore original
                             │
                             ▼
                       NotOwned
```

Sedangkan:

```text
Offline
```

bukan ownership state.

Offline hanya:

```text
charger presence = false
```

Jadi:

```text
Ownership ≠ ChargingState ≠ ChargerPresence ≠ PolicyState
```

Ini menurut saya **kunci desainnya**.

---

## Kesimpulan

Kode Anda sekarang sudah jauh lebih baik, tetapi saya **belum menganggapnya aman untuk release**.

Yang paling wajib diperbaiki:

| Prioritas | Masalah                                                                    |
| --------- | -------------------------------------------------------------------------- |
| 🔴 P0     | `shutdown_restore()` selalu `set_charging(true)`                           |
| 🔴 P0     | Tidak menyimpan `original_charging_state`                                  |
| 🔴 P0     | `Unmanaged` tidak restore state ketika daemon sebelumnya memegang hardware |
| 🔴 P0     | `cfg.enabled=false` bisa meninggalkan charging disabled                    |
| 🔴 P0     | Crash/SIGKILL tidak bisa melakukan restore                                 |
| 🔴 P0     | Tidak ada persistent recovery state                                        |
| 🟠 P1     | `Offline` sebaiknya tidak berarti `Unmanaged`                              |
| 🟠 P1     | `online=None` jangan dianggap charging normal                              |
| 🟠 P1     | Verification disabled terlalu bergantung pada `BatteryStatus`              |
| 🟠 P1     | Verification enabled juga tidak membuktikan control state                  |
| 🟠 P1     | `target` mencampur desired state dengan actual state                       |
| 🟡 P2     | Scheduler masih observe ketika `SyncState::Unknown`                        |
| 🟡 P2     | Fault recovery counter perlu reset lebih eksplisit                         |
| 🟡 P2     | poll timeout bisa 0 ms                                                     |

**Jadi jawaban singkatnya: `owns_hardware` yang Anda tambahkan adalah langkah yang benar, tetapi belum cukup.** Untuk requirement Anda — *"ketika daemon shutdown, sistem kembali ke keadaan sebelum daemon mengontrol sehingga ketika charger dicolok langsung normal"* — inti yang hilang adalah **capture + persist + restore `original charging control state`**, bukan sekadar `set_charging(true)`.

Kalau implementasi `charger_core::battery::control` Anda juga Anda kirim, terutama isi `set_charging()` dan method pembacaan control state-nya, saya bisa bedah bagian itu juga karena di situlah kita bisa menentukan **apa tepatnya yang harus di-snapshot dan bagaimana verification yang benar**.
