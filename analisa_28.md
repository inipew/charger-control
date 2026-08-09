Ya. Dari log ini terlihat **P3 sudah menangkap failure dengan benar**, tetapi sekarang muncul **bug baru pada orchestration/recovery loop**. Ini bukan sekadar masalah permission.

Ada **dua masalah berbeda**:

1. **Root cause hardware:** beberapa sysfs node memang menolak write (`EACCES`).
2. **Bug arsitektur daemon:** stale ownership recovery dipanggil berulang-ulang sehingga daemon masuk **recovery loop**.

---

# 1. Apa yang sebenarnya terjadi

Urutannya sangat jelas dari log:

```text
10:18:33.258
Found stale ownership state
original charging=true
phase=Releasing
```

Daemon menemukan:

```text
ownership.state
```

yang masih tersisa dari proses sebelumnya.

Itu **benar**. Karena recovery state memang seharusnya dipertahankan kalau recovery sebelumnya gagal.

Kemudian daemon mencoba:

```text
charging_enabled = 1
battery_charging_enabled = 1
input_suspend = 0
```

dan mendapatkan:

```text
Permission denied (os error 13)
```

Hasilnya:

```text
2/5 writes succeeded
3 failed
```

Kemudian:

```text
Partial failure during stale ownership recovery
```

**Ini juga benar.**

Jangan menghapus ownership state karena recovery belum berhasil.

---

# 2. Masalah sebenarnya ada setelah itu

Sekitar 100 ms kemudian:

```text
Applying hardware target: Unmanaged (sync=Recovering, force=true)
```

lalu:

```text
Found stale ownership state
```

lagi.

Kemudian recovery dijalankan lagi:

```text
2/5 writes succeeded
3 failed
```

Jadi sekarang pola Anda adalah:

```text
daemon start
    ↓
detect stale ownership
    ↓
recovery
    ↓
recovery gagal
    ↓
ownership tetap ada   ← BENAR
    ↓
monitor apply Unmanaged
    ↓
recovery dipanggil lagi ← SALAH
    ↓
recovery gagal
    ↓
ownership tetap ada
    ↓
apply Unmanaged
    ↓
recovery lagi
```

Ini yang perlu diperbaiki.

---

# 3. `ownership.state` JANGAN dihapus

Ini penting.

Jangan memperbaiki masalah ini dengan:

```rust
if recovery_failed {
    clear_ownership();
}
```

Itu justru merusak invariant P3.

Karena sekarang situasinya:

```text
original charging = true

recovery:
    node A = success
    node B = success
    node C = FAILED
    node D = FAILED
    node E = FAILED
```

Hardware **belum terbukti kembali ke kondisi original**.

Jadi:

```text
ownership.state harus tetap ada
```

Ini adalah behavior yang benar.

---

# 4. Yang harus diperbaiki adalah state machine recovery

Saat startup:

```text
Unowned
   │
   │ ownership.state ditemukan
   ▼
RecoveryRequired
   │
   ├── success ─────→ Recovered → Unowned
   │
   └── failure ─────→ RecoveryBlocked
```

Bukan:

```text
RecoveryRequired
   ↓
failure
   ↓
RecoveryRequired
   ↓
failure
   ↓
RecoveryRequired
```

Tambahkan state seperti:

```rust
pub enum RecoveryState {
    NotNeeded,
    Required,
    InProgress,
    Failed {
        retry_at: Instant,
        attempts: u32,
    },
    Recovered,
}
```

Atau kalau ingin lebih sederhana:

```rust
pub enum SyncState {
    Synced,
    Dirty,
    Failed,
    Recovering,
    RecoveryFailed,
}
```

---

# 5. Recovery harus dilakukan satu kali pada startup

Saya sangat menyarankan pola:

```text
daemon startup
      │
      ▼
recover_stale_ownership()
      │
      ├── success
      │      ↓
      │   clear ownership
      │      ↓
      │   normal operation
      │
      └── failure
             ↓
       mark RecoveryFailed
             ↓
       normal monitor tetap hidup
             ↓
       retry berdasarkan deadline
```

Jadi `apply_target(Unmanaged)` **tidak boleh otomatis memanggil stale recovery lagi**.

Ini dua operasi berbeda:

```text
recover_stale_ownership()
```

vs

```text
apply_target(Unmanaged)
```

---

# 6. Ini juga menjelaskan `sync=Recovering`

Log:

```text
Applying hardware target: Unmanaged (sync=Recovering, force=true)
```

menunjukkan controller kemungkinan sedang berada pada:

```rust
SyncState::Recovering
```

lalu `apply_target()` memutuskan:

> karena recovering, saya perlu menjalankan recovery.

Ini coupling yang sebaiknya dihilangkan.

Lebih baik:

```text
StartupCoordinator
        │
        └── OwnershipManager::recover()
```

dan setelah itu:

```text
Controller
        │
        ├── apply_target()
        └── verify()
```

Controller **tidak mengetahui lifecycle startup recovery**.

---

# 7. Saya akan membuat ownership API seperti ini

Misalnya:

```rust
pub enum RecoveryResult {
    NotNeeded,
    Recovered,
    Failed {
        succeeded: usize,
        failed: usize,
    },
}
```

Kemudian:

```rust
match ownership.recover(...) {
    RecoveryResult::NotNeeded => {
        controller.start_normal_operation();
    }

    RecoveryResult::Recovered => {
        controller.start_normal_operation();
    }

    RecoveryResult::Failed { .. } => {
        controller.mark_recovery_failed();
    }
}
```

Setelah `Failed`, **jangan langsung memanggil recovery lagi dari `apply_target()`**.

---

# 8. Tetapi tetap perlu retry recovery

Karena permission error bisa saja sementara, misalnya:

* service belum mendapatkan privilege,
* SELinux context belum siap,
* power-supply driver belum siap,
* node belum fully initialized,
* kernel/vendor driver berubah state.

Jadi recovery tetap perlu retry.

Tetapi harus seperti P2 #16/#17:

```text
attempt 1
    ↓
fail
    ↓
1s
    ↓
attempt 2
    ↓
fail
    ↓
2s
    ↓
attempt 3
    ↓
fail
    ↓
4s
    ↓
...
```

Bukan:

```text
event
 ↓
recover
 ↓
event
 ↓
recover
 ↓
event
 ↓
recover
```

---

# 9. Dan ini seharusnya memakai deadline yang sudah Anda rancang di P2

Misalnya:

```rust
struct RecoveryState {
    attempts: u32,
    next_retry_at: Instant,
}
```

Ketika gagal:

```rust
self.attempts += 1;

self.next_retry_at =
    clock.now() + backoff(self.attempts);
```

Kemudian monitor:

```rust
if recovery.is_due(clock.now()) {
    recovery.try_recover();
}
```

Jadi **tidak perlu polling 2 detik**.

Ini sekaligus menyelesaikan P2 #15/#16/#17 yang sebelumnya kita bahas.

---

# 10. Ada masalah kedua: `Permission denied`

Sekarang kita masuk ke root cause hardware.

Tiga node gagal:

```text
/sys/class/power_supply/main/charging_enabled
/sys/class/power_supply/battery/battery_charging_enabled
/sys/class/power_supply/usb/input_suspend
```

Tetapi:

```text
2/5 writes succeeded
```

Artinya ada node lain yang masih bisa ditulis.

Jadi ini **bukan sekadar "sysfs semuanya read-only"**.

Ada kemungkinan:

### A. permission Unix

Misalnya:

```text
-r--r--r--
```

### B. SELinux

Node terlihat writable secara Unix permission, tetapi domain daemon tetap ditolak.

### C. Android/vendor driver

Kernel attribute memang ada tetapi `store()` handler menolak operasi.

### D. daemon tidak berjalan dengan privilege/context yang sama

Misalnya shell:

```bash
su
echo 1 > node
```

berhasil,

tetapi daemon:

```text
charger_daemon
```

berjalan dalam context berbeda.

---

# 11. Jangan langsung menyimpulkan `root != permission`

Di Android:

```text
uid=0
```

tidak otomatis berarti:

```text
boleh menulis semua sysfs
```

SELinux tetap bisa memblokir.

Karena itu saya ingin Anda cek **dari environment yang sama dengan daemon**.

Jalankan:

```sh
id
```

dan:

```sh
getenforce
```

Kemudian:

```sh
ls -l /sys/class/power_supply/main/charging_enabled
ls -l /sys/class/power_supply/battery/battery_charging_enabled
ls -l /sys/class/power_supply/usb/input_suspend
```

Lalu cek context:

```sh
ls -Z /sys/class/power_supply/main/charging_enabled
ls -Z /sys/class/power_supply/battery/battery_charging_enabled
ls -Z /sys/class/power_supply/usb/input_suspend
```

Dan yang sangat penting:

```sh
dmesg | grep -i -E 'avc|denied|charging|power_supply'
```

Kalau `dmesg` tidak bisa diakses:

```sh
logcat -b all -d | grep -i -E 'avc|denied|charging|power_supply'
```

---

# 12. Tes manual node juga sangat penting

Dari shell root:

```sh
echo 1 > /sys/class/power_supply/main/charging_enabled
echo 1 > /sys/class/power_supply/battery/battery_charging_enabled
echo 0 > /sys/class/power_supply/usb/input_suspend
```

Kemudian:

```sh
echo $?
```

Kalau manual root berhasil tetapi daemon gagal:

```text
Root shell       → SUCCESS
charger-daemon   → EACCES
```

maka fokusnya:

```text
daemon UID/GID
SELinux domain
Magisk service context
```

bukan `HardwareController`.

Kalau manual root juga gagal:

```text
Root shell       → EACCES
charger-daemon   → EACCES
```

maka masalahnya kemungkinan besar ada pada:

```text
kernel/vendor driver/sysfs
```

---

# 13. Jangan lupa: partial write recovery adalah kondisi yang berbahaya

Log:

```text
2/5 writes succeeded, 3 failed
```

harus dianggap:

```text
HardwareState = Unknown / PartiallyApplied
```

bukan:

```text
HardwareState = Original
```

Ini penting sekali.

Saya bahkan akan menyarankan result yang lebih eksplisit:

```rust
pub enum ApplyResult {
    Applied,
    Partial {
        succeeded: usize,
        failed: usize,
    },
    Failed {
        succeeded: usize,
        failed: usize,
    },
}
```

Kemudian:

```rust
match result {
    ApplyResult::Applied => SyncState::Synced,

    ApplyResult::Partial { .. } |
    ApplyResult::Failed { .. } => SyncState::Failed,
}
```

**Tidak boleh ada jalan menuju `Synced` dari partial write.**

---

# 14. Recovery `phase=Releasing` juga memberi informasi penting

Ini:

```text
original charging=true
phase=Releasing
```

menunjukkan daemon sebelumnya kemungkinan mati ketika berada pada fase:

```text
Owned
  ↓
Releasing
  ↓
restore original hardware
  ↓
CLEAR ownership
```

dan crash terjadi sebelum selesai.

Itu **justru membuktikan ownership persistence Anda bekerja**.

Sebelum P3:

```text
crash
 ↓
bypass tetap aktif
 ↓
tidak ada informasi
```

Sekarang:

```text
crash
 ↓
ownership.state tetap ada
 ↓
startup mendeteksi
 ↓
recovery mencoba
 ↓
recovery gagal
 ↓
state tetap dipertahankan
```

Jadi bagian ini sebenarnya **success dari desain P3**.

Yang gagal adalah **scheduler/orchestrator recovery setelah failure**.

---

# 15. Perbaikan konkret yang saya rekomendasikan

Saya akan mengubah alurnya menjadi:

```text
                     STARTUP
                        │
                        ▼
              load ownership.state
                        │
              ┌─────────┴─────────┐
              │                   │
           absent              present
              │                   │
              ▼                   ▼
        Normal operation    Recovery attempt
                                  │
                         ┌────────┴────────┐
                         │                 │
                      SUCCESS           FAILURE
                         │                 │
                         ▼                 ▼
                 clear ownership    keep ownership
                         │                 │
                         ▼                 ▼
                  Normal operation   RecoveryFailed
                                           │
                                           ▼
                                    schedule retry
                                           │
                                    deadline reached?
                                           │
                                           ▼
                                    recovery attempt
```

Dan **tidak ada**:

```text
apply_target()
    ↓
recover_stale_ownership()
```

---

# 16. Bahkan lebih baik: pisahkan tiga hal

Saya akan membuat:

```text
OwnershipManager
```

bertanggung jawab:

```text
acquire
persist
recover
release
```

`HardwareController` bertanggung jawab:

```text
apply
verify
sync state
```

`Monitor` bertanggung jawab:

```text
event
deadline
retry scheduling
```

Jadi:

```text
OwnershipManager
       │
       │ recovery result
       ▼
     Monitor
       │
       │ decide when retry
       ▼
OwnershipManager::recover()
```

bukan:

```text
HardwareController
       ↓
OwnershipManager
       ↓
Monitor
       ↓
HardwareController
```

yang berpotensi membentuk siklus seperti log Anda sekarang.

---

## Kesimpulan

**Ini bukan kegagalan total P3. Justru log tersebut menunjukkan beberapa invariant P3 bekerja dengan benar.**

Yang terbukti bekerja:

* ✅ stale ownership terdeteksi
* ✅ original state berhasil dibaca
* ✅ partial write terdeteksi
* ✅ `2/5` tidak dianggap sukses
* ✅ ownership tidak langsung dihapus setelah recovery gagal
* ✅ recovery error tercatat

Yang **masih bug**:

* ❌ recovery dipanggil lagi oleh `apply_target(Unmanaged)`
* ❌ tidak ada `RecoveryFailed` state yang memblokir repeated recovery
* ❌ retry belum sepenuhnya deadline/backoff driven
* ❌ recovery dan normal hardware apply masih terlalu coupled

Dan **masalah hardware yang terpisah**:

* ⚠️ 3 sysfs node menghasilkan `EACCES`, yang harus dibedakan antara Unix permission, SELinux, dan driver/kernel rejection.

Jadi saya akan memberi status:

> **P3 core invariant: PASS**
> **P3 recovery orchestration: FAIL — perlu diperbaiki**
> **Android sysfs access: UNKNOWN — perlu diagnosis permission/SELinux**
> **Jangan hapus `ownership.state` untuk menghilangkan error.**

Kalau Anda kirimkan implementasi **`recover_stale_ownership()`, `apply_target()`, `SyncState`, dan bagian monitor yang menghasilkan `Applying hardware target: Unmanaged (sync=Recovering, force=true)`**, saya bisa tunjukkan **tepat di mana recovery loop terjadi dan bentuk patch arsitektural yang seharusnya**, tanpa perlu refactor P3 dari awal.
