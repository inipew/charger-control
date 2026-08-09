Belum saya sebut **production-grade**. Fondasinya sudah jauh lebih matang—terutama dibanding versi sebelumnya—tetapi masih ada beberapa masalah yang menurut saya **P0/P1**, yaitu bisa menyebabkan daemon salah menganggap hardware aman, kehilangan ownership recovery, atau melakukan retry yang tidak sesuai.

Kalau saya beri penilaian:

| Area                             | Status                       |
| -------------------------------- | ---------------------------- |
| State machine target/sync        | 🟢 Bagus                     |
| Partial sysfs write handling     | 🟢 Bagus                     |
| Persistent ownership concept     | 🟡 Hampir                    |
| Stale ownership recovery         | 🟢 Konsep benar              |
| Verification                     | 🟡 Ada lubang penting        |
| Reader/cache                     | 🟡 Ada beberapa masalah      |
| Decision engine                  | 🟢 Cukup kuat                |
| Scheduler                        | 🟡 Belum production-grade    |
| Netlink                          | 🟢 Cukup baik                |
| Shutdown/crash safety            | 🟡 Belum lengkap             |
| Multi-instance safety            | 🔴 Belum ada                 |
| External hardware reconciliation | 🔴 Belum ada                 |
| Overall                          | **~80–85% production-ready** |

Yang paling penting:

---

# 1. 🔴 P0: `save_persistent_ownership()` gagal tetapi ownership tetap dianggap berhasil

Ini menurut saya **bug paling serius yang tersisa**.

Sekarang:

```rust
save_persistent_ownership(original);

self.ownership = Ownership::Owned {
    original_charging: original,
};
```

Padahal `save_persistent_ownership()` bisa gagal:

```rust
fs::create_dir_all(...)
fs::write(...)
rename(...)
```

dan hanya melakukan log error.

Akibatnya bisa terjadi:

```text
original charging = true

write ownership.state -> FAILED

self.ownership = Owned { original = true }

daemon crash
        ↓
restart
        ↓
ownership.state tidak ada
        ↓
daemon tidak tahu harus restore true
```

Ini bertentangan dengan tujuan utama persistent ownership.

### Harus diubah menjadi:

```rust
pub fn save_persistent_ownership(original: bool)
    -> Result<(), ChargerError>
```

dan:

```rust
match save_persistent_ownership(original) {
    Ok(()) => {
        self.ownership = Ownership::Owned {
            original_charging: original,
        };
    }

    Err(e) => {
        tracing::error!(
            "Cannot persist hardware ownership: {}",
            e
        );

        self.mark_apply_failed();
        return;
    }
}
```

**Jangan pernah mengambil hardware ownership kalau persistence ownership gagal.**

---

# 2. 🔴 `read_current_ma()` masih salah secara semantik

Ini bagian yang perlu diperbaiki meskipun Anda sudah menghilangkan heuristik unit.

Sekarang:

```rust
if value == 0 {
    continue;
}
```

dan akhirnya:

```rust
Ok(best_val.unwrap_or(0))
```

Ini bermasalah.

## Kasus:

Semua node membaca:

```text
current_now = 0
```

Itu **bisa merupakan pembacaan valid**.

Tetapi kode Anda mengabaikannya lalu menghasilkan:

```rust
0
```

secara indistinguishable dari:

> "current memang 0"

dan:

> "tidak berhasil membaca current sama sekali."

Lebih buruk lagi:

```rust
if self.current_fds.is_empty() {
    return Ok(0);
}
```

Jadi:

> tidak ada current node

dianggap:

> current = 0 mA.

Untuk verification charging disabled, ini berbahaya:

```rust
let current_safe = snapshot
    .current_ma
    .is_some_and(|current| current <= 100);
```

Jika reader gagal:

```text
read_current_ma() -> Ok(0)
```

maka:

```text
current_ma = Some(0)
```

dan:

```text
current_safe = true
```

Padahal **Anda sebenarnya tidak punya data current**.

Ini masih melanggar prinsip yang sebelumnya sudah kita tetapkan:

> `Unknown`/`None` tidak boleh dianggap sebagai kondisi aman.

---

# 3. 🔴 Current node priority juga masih punya bug

Misalnya:

```text
battery/current_now = 0
bms/current_now     = 120000
```

Kode sekarang:

```rust
battery = 0
→ continue

bms = 120000
→ pilih bms
```

Padahal battery node priority 100 seharusnya tetap menjadi sumber utama.

Zero bukan berarti invalid.

Seharusnya:

```rust
let mut best_val = None;

for node {
    let value = read(...)?;

    let ma = convert(value);

    if higher_priority {
        best_val = Some(ma);
    }
}
```

Dengan kata lain:

```text
0 mA = valid measurement
read failure = unavailable
parse failure = invalid
```

Bukan:

```text
0 mA = skip
```

---

# 4. 🔴 `read_current_ma()` seharusnya `Err` ketika tidak ada valid measurement

Saya akan ubah semantik menjadi:

```rust
pub fn read_current_ma(&mut self) -> Result<i32, ChargerError>
```

dengan:

```text
node tidak ada              -> Err
semua read gagal            -> Err
semua parse gagal           -> Err
node valid membaca 0        -> Ok(0)
node valid membaca -500     -> Ok(-500)
node valid membaca +1500    -> Ok(1500)
```

Ini jauh lebih benar.

Kemudian:

```rust
current_ma: battery_reader.read_current_ma().ok(),
```

akan menghasilkan:

```text
Ok(0)    -> Some(0)
Err(...) -> None
```

sehingga verification benar-benar bisa membedakan keduanya.

---

# 5. 🟠 Verification charging disabled masih terlalu sederhana

Sekarang:

```rust
current <= 100
```

Saya tidak akan menjadikan itu satu-satunya bukti bahwa charging benar-benar berhenti.

Misalnya:

```text
charging_enabled = 0
current = +70 mA
```

mungkin benar-benar charging leakage.

Tetapi:

```text
current = -50 mA
```

berarti baterai sedang discharge.

Dan:

```text
current = 0
```

bisa berarti idle.

Lebih kuat kalau verification mempertimbangkan:

```text
control state
+
battery status
+
current
+
online
```

Contoh:

### Disabled

Idealnya:

```text
control = Disabled
AND
(
    status = Discharging
    OR
    status = NotCharging
    OR
    current <= threshold
)
```

Namun `current <= threshold` harus tetap memperhatikan **polaritas current vendor**.

Kalau kontrak Anda adalah:

```text
positive = charging
negative = discharging
```

maka:

```rust
current <= 100
```

masih masuk akal sebagai threshold.

Tetapi `current` unavailable harus:

```rust
None
```

dan bukan `0`.

---

# 6. 🔴 `release_ownership()` tidak menghormati retry backoff

Ini bug state-machine yang cukup penting.

Misalnya restore gagal:

```rust
self.sync = SyncState::Failed;
self.force_apply = true;
self.retry_at = Some(now + 30s);
```

Tetapi target sudah:

```rust
Unmanaged
```

Pada loop berikutnya:

```rust
if hardware.needs_apply(decision.target, now)
```

pertama:

```rust
if self.desired_target != target {
    return true;
}
```

dan ketika target sama, memang ada pengecekan `Failed`.

Tetapi dalam `apply_target()`:

```rust
HardwareTarget::Unmanaged => {
    self.release_ownership();
}
```

`release_ownership()` sendiri **tidak mengecek `retry_at`**.

Akibatnya restore bisa dicoba lagi setiap loop setelah target Unmanaged.

Ini membuat backoff:

```text
30s
60s
120s
300s
```

tidak benar-benar berlaku untuk release.

### Solusi

Pisahkan:

```rust
needs_apply()
```

dan pastikan **semua transition**, termasuk `Unmanaged`, tunduk pada retry gate.

Misalnya:

```rust
pub fn needs_apply(
    &self,
    target: HardwareTarget,
    now: Instant,
) -> bool {
    if self.sync == SyncState::Failed
        && self.retry_at.is_some_and(|t| now < t)
    {
        return false;
    }

    ...
}
```

Ini harus berlaku untuk:

```text
ChargingEnabled
ChargingDisabled
Unmanaged
```

---

# 7. 🔴 Tidak ada external-state reconciliation

Ini salah satu gap terbesar yang masih tersisa.

Setelah:

```text
HardwareTarget::ChargingDisabled
        ↓
set_charging(false)
        ↓
verification OK
        ↓
Synced
```

daemon kemudian percaya:

```rust
sync == Synced
```

Selama tidak ada decision baru, hardware dianggap tetap benar.

Tetapi Android/vendor daemon lain bisa mengubah:

```text
charging_enabled
input_suspend
```

tanpa memberitahu daemon Anda.

Contoh:

```text
charger-control
    ↓
charging disabled

vendor power HAL
    ↓
charging enabled

charger-control
    ↓
masih mengira Synced
```

Netlink membantu mendeteksi perubahan power-supply, tetapi event handling Anda sekarang hanya menyebabkan loop berjalan lagi. Belum ada mekanisme eksplisit:

```text
actual hardware state != desired_target
        ↓
re-apply
```

Ini perlu ditambahkan.

Misalnya setiap beberapa detik / setiap power-supply event:

```rust
match control::read_charging_state() {
    Ok(actual) => {
        if !hardware_matches_target(actual, decision.target) {
            hardware.force_apply = true;
            hardware.sync = SyncState::Unknown;
        }
    }
    Err(_) => {
        // jangan menganggap synced
    }
}
```

Jadi:

```text
Synced
```

berarti:

> terakhir diverifikasi cocok,

bukan:

> hardware pasti tidak berubah.

---

# 8. 🔴 Belum ada single-instance lock

Untuk daemon Android root, ini wajib.

Bayangkan:

```text
daemon A
daemon B
```

keduanya hidup.

A:

```text
original = true
set charging = false
```

B:

```text
original = false
set charging = true
```

Sekarang ownership state menjadi tidak dapat dipercaya.

Tambahkan lock eksklusif, misalnya:

```text
/data/adb/charger-control/daemon.lock
```

dengan `flock()`.

Startup:

```text
acquire lock
    ↓
berhasil → lanjut
gagal    → exit
```

Ini menurut saya **P1 wajib sebelum production**.

---

# 9. 🟠 Persistent state perlu durability lebih kuat

Sekarang:

```rust
fs::write(tmp)
rename(tmp, state)
```

Ini sudah jauh lebih baik daripada langsung:

```rust
write(state)
```

Tetapi untuk daemon yang mengontrol hardware, saya akan naikkan satu level:

```text
write temp
    ↓
fsync(temp)
    ↓
rename(temp, state)
    ↓
fsync(parent directory)
```

Karena `rename()` memberikan atomic namespace update, tetapi durability terhadap power loss/reboot tidak otomatis sama dengan:

> data pasti sudah durable di storage.

Untuk Android flash storage, crash biasa mungkin cukup tertangani, tetapi production-grade sebaiknya jelas.

---

# 10. 🟠 Stale ownership recovery: konsepnya sudah benar

Bagian ini sekarang saya nilai **bagus**:

```rust
Ok(res) if res.all_succeeded() => {
    clear_persistent_ownership();
}
```

dan:

```rust
Ok(res) => {
    // state tetap ada
}
```

Ini tepat.

Yang saya ubah adalah:

```rust
recover_stale_ownership()
```

sebaiknya mengembalikan status:

```rust
Result<RecoveryStatus, ChargerError>
```

Kemudian startup bisa memutuskan:

```text
recovery sukses
    ↓
normal

recovery gagal
    ↓
jangan langsung menjalankan policy normal
    ↓
masuk recovery/fault state
```

Ini menjawab salah satu masalah dari versi sebelumnya.

Saat ini:

```rust
hardware::recover_stale_ownership();

let mut battery_reader = ...
```

Jika recovery gagal, daemon **tetap lanjut**.

Itu masih kurang aman.

---

# 11. 🔴 Startup recovery gagal seharusnya punya state khusus

Bayangkan state:

```text
ownership.state = "1"
```

artinya:

```text
sebelum daemon mengambil kontrol:
charging = enabled
```

Daemon crash.

Restart.

Recovery:

```text
set_charging(true)
```

gagal.

Tetapi daemon lanjut dan policy memutuskan:

```text
capacity = 90
temperature = normal
target = ChargingDisabled
```

lalu mencoba:

```text
set_charging(false)
```

Padahal hardware recovery dari ownership lama belum selesai.

Lebih aman:

```text
STALE OWNERSHIP
      ↓
attempt restore
      ↓
success ──────────→ normal operation
      │
      └ failure
          ↓
      RecoveryFault
          ↓
      retry recovery
          ↓
      jangan ambil ownership baru
```

---

# 12. 🟠 Shutdown sekarang bagus, tapi belum benar-benar crash-safe

`shutdown_restore()` sudah jauh lebih baik.

Tetapi:

```text
SIGTERM
SIGINT
SIGKILL
panic
OOM kill
kernel crash
```

tidak semuanya akan memanggil:

```rust
shutdown_restore()
```

Untuk:

```text
SIGTERM
SIGINT
```

Anda bisa menangani signal secara graceful.

Untuk:

```text
SIGKILL
kernel crash
```

mustahil menjamin cleanup langsung.

Karena itu persistent ownership recovery yang Anda sudah buat sebenarnya adalah **mekanisme utama crash safety**.

Jadi arsitekturnya harus:

```text
normal shutdown
    → restore langsung

SIGTERM
    → restore langsung

panic/crash
    → ownership.state tetap ada
    → restore pada startup berikutnya
```

Itu production-grade.

---

# 13. 🟠 `CachedReader` belum sepenuhnya cache-resilient

Anda sudah melakukan hal bagus dengan:

```rust
current_fds
online_fds
```

dan periodic rescan.

Tetapi:

```rust
capacity_fd
temp_fd
status_fd
```

dibuka sekali:

```rust
File::open(...).ok()
```

dan tidak pernah di-rescan.

Kalau vendor power supply directory/attribute berubah karena:

```text
charger reconnect
power HAL restart
power_supply device re-register
```

FD bisa menjadi stale.

Current dan online punya recovery:

```text
5 sec rescan
```

tetapi capacity/temp/status tidak.

Saya akan menyatukan mekanisme cached-node lifecycle untuk semua sensor.

---

# 14. 🔴 `is_plugged_in()` masih berpotensi false offline

Sekarang:

```rust
for entry in /sys/class/power_supply
```

lalu semua node selain:

```text
battery
bms
```

dengan:

```text
online
```

dianggap sumber charger.

Ini terlalu generik.

Misalnya:

```text
usb/online = 0
ac/online = 0
wireless/online = 0
```

→ offline.

Masih masuk akal.

Tetapi vendor Android bisa memiliki power supply virtual / auxiliary yang punya `online`, tetapi bukan representasi sederhana dari charger yang ingin Anda kontrol.

Lebih production-grade jika `nodes.rs` memiliki konfigurasi input:

```rust
ONLINE_NODES
```

dengan priority/role:

```text
usb
ac
mains
wireless
dc
```

daripada:

> setiap directory yang memiliki `online`.

---

# 15. 🟠 Sensor validation masih kurang

Sekarang:

```rust
snapshot.capacity_pct.is_some()
&& snapshot.temp_dc.is_some()
&& snapshot.online.is_some()
&& status_valid
```

Tetapi validitas hanya berarti:

> field berhasil dibaca.

Belum berarti:

> nilainya masuk akal.

Contoh capacity:

```rust
parse::<u8>()
```

menerima:

```text
101
200
255
```

padahal battery capacity seharusnya:

```text
0..=100
```

Harus ada:

```rust
if capacity > 100 {
    return Fault;
}
```

Temperature juga perlu sanity bound.

Misalnya konfigurasi/driver menghasilkan:

```text
-32768
9999
```

jangan sampai decision engine menganggap itu temperature valid.

---

# 16. 🟢 Decision engine sekarang sudah cukup bagus

Ini salah satu bagian terkuat.

Terutama:

```rust
snapshot.online == Some(false)
```

→ `Unmanaged`

dan:

```rust
online == None
```

tidak dianggap offline.

Kemudian:

```rust
!sensors_valid
```

→

```rust
ChargingDisabled
```

Ini conservative.

Hysteresis juga sudah benar secara konsep:

```text
limit reached
    ↓
disable
    ↓
harus turun ke resume
    ↓
enable
```

dan thermal:

```text
temp >= max
    ↓
disable
    ↓
harus turun <= resume
    ↓
enable
```

Saya tidak melihat masalah arsitektur besar di sini.

---

# 17. 🟠 Tetapi `ChargingDisabled` karena sensor fault perlu dibedakan dari limit

Sekarang:

```rust
SensorFault
    → ChargingDisabled
```

Ini aman, tetapi secara state machine hardware:

```text
Fault
```

dan:

```text
LimitReached
```

dua-duanya menghasilkan:

```text
ChargingDisabled
```

Itu tidak masalah untuk hardware safety, tetapi sebaiknya controller mengetahui alasan berbeda:

```text
Policy target = disabled
Reason = SensorFault
```

dan mungkin memberikan retry/alert yang berbeda.

Anda sudah memiliki `DecisionReason`, jadi fondasinya sudah ada.

---

# 18. 🟡 Scheduler terlalu pintar untuk sesuatu yang safety-critical

Bagian EMA/ETA ini menarik:

```rust
cap_rate_ema
temp_rate_ema
```

tetapi saya tidak akan membiarkan prediction menjadi dasar safety.

Untungnya saat ini decision sebenarnya tetap berasal dari:

```rust
DecisionEngine
```

sedangkan scheduler hanya menentukan kapan membaca lagi.

Itu bagus.

Pertahankan prinsip:

```text
Scheduler = kapan mengecek
DecisionEngine = apa yang harus dilakukan
HardwareController = bagaimana menerapkannya
```

Jangan pernah membuat:

```text
ETA prediction says safe
→ don't check
```

secara mutlak.

---

# 19. 🟢 Netlink cukup bagus, tapi perlu satu perbaikan kecil

Arsitektur:

```text
netlink
   ↓
debounce 250 ms
   ↓
wake monitor
   ↓
read sensors
   ↓
decision
```

sudah tepat.

Saya suka bahwa socket menggunakan:

```rust
OwnedFd
```

daripada raw FD manual.

Dan:

```rust
MSG_DONTWAIT
```

menghindari daemon tersangkut di `recv()`.

Tetapi ketika reconnect karena `recv()` error:

```rust
self.reconnect_at = Some(now + self.backoff);
```

Anda tidak menaikkan `backoff` di jalur tersebut.

Sedangkan `schedule_reconnect()` menaikkannya.

Sebaiknya semua failure path memakai satu fungsi reconnect/backoff agar behavior konsisten.

---

# 20. 🟠 `poll()` loop punya polling 2 detik saat Failed

Ini:

```rust
if hardware.sync == SyncState::Failed {
    next_wake = next_wake.min(
        loop_now + Duration::from_secs(2)
    );
}
```

membuat daemon bangun setiap 2 detik meskipun:

```text
retry_at = 30 detik
```

Padahal tidak ada yang perlu dilakukan.

Lebih bagus `HardwareController` expose:

```rust
pub fn next_deadline(&self) -> Option<Instant>
```

yang mengembalikan:

```text
verification deadline
atau
retry_at
```

sehingga:

```text
failure
retry_at = now + 30s
```

→ daemon benar-benar tidur 30 detik, kecuali:

```text
IPC
netlink
```

membangunkannya.

Ini lebih hemat baterai dan lebih bersih.

---

# 21. 🟠 `next_deadline()` sekarang misleading

Saat ini:

```rust
pub fn next_deadline(&self) -> Option<Instant> {
    self.verification.as_ref().map(|v| v.deadline)
}
```

namanya `next_deadline`, tetapi tidak memasukkan:

```rust
retry_at
```

Jadi caller harus tahu internal implementation:

```rust
if hardware.sync == Failed {
    ...
}
```

Saya sarankan:

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

Tetapi caller juga harus tahu deadline itu untuk apa atau cukup bangun dan evaluate.

---

# 22. 🔴 Belum ada invariant testing untuk state machine

Sebelum disebut production-grade, saya sangat menyarankan test invariant.

Minimal test:

### Ownership

```text
acquire
→ state persisted
→ crash
→ recover
→ state cleared
```

### Persistence failure

```text
persist fails
→ MUST NOT become Owned
```

### Partial write

```text
2 nodes
1 success
1 failure
→ NOT Synced
→ retry
```

### Verification

```text
target disabled
control disabled
current unavailable
→ MUST NOT Synced
```

Ini sangat penting.

### External modification

```text
target disabled
actual disabled
→ Synced

external actor enables
→ next reconciliation
→ Pending/Apply
→ disabled again
```

### Crash

```text
Owned
→ process dies
→ state remains
→ next process restores
```

---

# 23. Ada satu desain yang saya sarankan: pisahkan `ownership` dari `sync`

Saat ini sudah cukup bagus, tetapi untuk production saya akan memformalkan invariant:

```text
Ownership:
    NotOwned
    Owned(original)

Sync:
    Unknown
    Pending
    Synced
    Failed
```

dan buat invariant:

### `NotOwned`

```text
desired = Unmanaged
applied = Unmanaged
```

### `Owned`

```text
persistent ownership state MUST exist
```

### `Synced`

```text
actual hardware == desired target
```

### `Failed`

```text
actual hardware != known desired
OR
verification unavailable
```

Jangan pernah:

```text
SyncState::Synced
```

hanya karena `set_charging()` berhasil.

Anda sudah hampir melakukan ini dengan verification. Tinggal konsisten di semua path.

---

# 24. Urutan lifecycle production yang saya inginkan

Secara arsitektur, daemon Anda seharusnya menjadi:

```text
                ┌─────────────────┐
                │ acquire lock    │
                └────────┬────────┘
                         ↓
                ┌─────────────────┐
                │ recover stale   │
                │ ownership       │
                └────────┬────────┘
                         │
                    failure?
                    /       \
                  yes        no
                   ↓          ↓
             RecoveryFault   normal
                   │
                   └──── retry


                    NORMAL
                       │
                       ↓
               read sensor snapshot
                       │
                       ↓
                validate snapshot
                       │
                 ┌─────┴─────┐
                 ↓           ↓
              invalid       valid
                 ↓           ↓
               Fault      Decision
                 │           │
                 └─────┬─────┘
                       ↓
                 desired target
                       │
                       ↓
              external reconciliation
                       │
                       ↓
                HardwareController
                       │
                       ↓
                sysfs write
                       │
                       ↓
                  verification
                       │
                ┌──────┴──────┐
                ↓             ↓
             success        failure
                ↓             ↓
             Synced         retry
```

Kemudian:

```text
SIGTERM / IPC shutdown
        ↓
restore ownership
        ↓
verify restore
        ↓
clear state
        ↓
exit
```

---

# Prioritas perbaikan

Kalau Anda ingin benar-benar menyelesaikan ini, saya akan mengurutkannya seperti ini:

### 🔴 P0 — wajib

1. **`save_persistent_ownership()` harus `Result`**
2. **Jangan `Owned` jika persistence gagal**
3. **`read_current_ma()` jangan mengubah unavailable menjadi `0`**
4. **`0 mA` harus valid measurement**
5. **Tambah validation `capacity <= 100`**
6. **Startup stale recovery failure jangan langsung masuk normal mode**

### 🔴 P1 — sangat disarankan sebelum production

7. **Single-instance `flock`**
8. **External hardware reconciliation**
9. **Retry gate harus berlaku juga untuk `Unmanaged`/release**
10. **Persistent state durability (`fsync`)**
11. **Recovery state khusus**
12. **Signal handling untuk SIGTERM/SIGINT**

### 🟠 P2

13. Rescan capacity/temp/status FD
14. Perbaiki online-node discovery menjadi explicit/configurable
15. Satukan deadline verification + retry
16. Hilangkan polling 2 detik ketika hanya menunggu retry
17. Rapikan netlink backoff
18. Tambahkan sensor sanity validation

### 🟢 P3

19. State-machine invariant tests
20. Fault-injection tests
21. Logging/metrics
22. Hardware/vendor profile abstraction

---

## Kesimpulan

**Ya, kode ini sudah jauh lebih dekat ke daemon production-grade.** Perubahan yang Anda lakukan sebelumnya—khususnya `all_succeeded()`, tidak lagi menebak unit `current_now`, pemisahan `Unknown/Mixed`, ownership persistence, verification generation, dan cached FD—sudah tepat.

Tetapi saya **belum akan memasukkannya ke Magisk production release dalam bentuk sekarang**.

Yang paling menghalangi bukan scheduler atau netlink, melainkan empat hal:

```text
1. persistence failure masih bisa menghasilkan Owned
2. current unavailable masih bisa berubah menjadi 0 mA
3. stale recovery failure masih membolehkan normal operation
4. belum ada external-state reconciliation + single-instance lock
```

Kalau empat hal itu diperbaiki, ditambah retry-release dan sensor validation, saya akan menilainya sekitar **90–95% production-ready**. Setelah fault-injection/state-machine testing, baru layak saya sebut **production-grade** untuk daemon charger-control Android root.

Dan satu hal penting: **`control.rs` Anda sekarang sudah berada di arah yang benar.** `ChargingWriteResult` + `all_succeeded()` adalah fondasi yang tepat; saya tidak akan kembali ke model `bool` sederhana.
