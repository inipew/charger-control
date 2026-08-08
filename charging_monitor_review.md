# Review Final Monitor Loop & Charging State Machine

## Ringkasan

Versi terbaru sudah **jauh lebih baik** dibanding versi sebelumnya. Perubahan penting yang sudah benar:

- ownership hardware sekarang eksplisit;
- state ownership dipersistenkan;
- shutdown berusaha mengembalikan state asli;
- stale ownership dipulihkan saat startup;
- `Offline` tidak lagi otomatis mematikan charging;
- `Disabled` menjadi `Unmanaged`;
- verification mengecek control state;
- scheduler hanya mengumpulkan data ketika hardware sudah `Synced`.

Namun masih ada beberapa kelemahan yang sebaiknya diperbaiki sebelum dianggap production-grade.

---

## 1. Jangan mengubah `target` sebelum apply berhasil

Saat ini:

```rust
pub fn apply_target(&mut self, target: HardwareTarget) {
    self.target = target;
    // ...
}
```

Jika `set_charging()` gagal, `target` sudah berubah walaupun hardware belum berubah.

Lebih aman mengubah `target` hanya setelah operasi berhasil:

```rust
match control::set_charging(enable) {
    Ok(()) => {
        self.target = target;
        self.mark_apply_success(target);
    }
    Err(e) => {
        tracing::error!("Failed to apply {:?}: {}", target, e);
        self.mark_apply_failed();
    }
}
```

Lebih ideal lagi pisahkan:

```rust
desired_target
applied_target
```

Dengan begitu jelas mana target yang diinginkan dan mana yang benar-benar sudah diterapkan.

---

## 2. Ownership diambil sebelum `set_charging()` berhasil

Kode:

```rust
if self.ownership == Ownership::NotOwned {
    let original = control::is_charging_enabled()?;
    save_persistent_ownership(original);
    self.ownership = Ownership::Owned {
        original_charging: original
    };
}
```

lalu baru:

```rust
control::set_charging(enable)
```

Jika write gagal, controller sudah menganggap dirinya owner.

Ini tidak selalu salah, karena write kernel bisa saja memiliki partial side effect. Tetapi state-nya perlu didefinisikan dengan jelas.

Untuk desain yang lebih kuat, pertimbangkan:

```rust
enum Ownership {
    NotOwned,
    Owned { original_charging: bool },
    RecoveryRequired { original_charging: bool },
}
```

---

## 3. Persistent ownership harus atomic

Sekarang:

```rust
std::fs::write(STATE_FILE, ...)
```

Untuk state recovery penting, sebaiknya gunakan:

```text
write temporary
    ↓
sync_all()
    ↓
rename()
```

Selain itu jangan abaikan error:

```rust
let _ = std::fs::write(...)
```

Lebih baik:

```rust
if let Err(e) = std::fs::write(...) {
    tracing::error!("Failed to persist ownership: {}", e);
}
```

Pastikan juga directory:

```text
/data/adb/charger-control/
```

sudah dibuat dengan:

```rust
std::fs::create_dir_all(...)
```

---

## 4. Tambahkan single-instance lock

Persistent ownership tidak mencegah dua daemon berjalan bersamaan.

Skenario:

```text
daemon A
    ↓
mengambil ownership
    ↓
daemon B juga start
    ↓
keduanya mengontrol charging
```

Sebaiknya gunakan lock seperti:

```text
/data/adb/charger-control/monitor.lock
```

dengan `flock()` atau mekanisme single-instance lain.

Untuk daemon Android/root, ini sangat disarankan.

---

## 5. Startup stale ownership sudah benar secara konsep

Bagian:

```rust
if let Some(original) = hardware::load_persistent_ownership() {
    set_charging(original)
    clear_persistent_ownership()
}
```

merupakan improvement yang bagus.

Namun recovery sebaiknya hanya menghapus state setelah restore berhasil.

Kode Anda sudah melakukan ini dengan benar:

```rust
if let Err(e) = set_charging(original) {
    // state tidak langsung dihapus
} else {
    clear_persistent_ownership();
}
```

Pertahankan pola ini.

---

## 6. Verification `ChargingEnabled` masih agak lemah

Sekarang:

```rust
snapshot.online == Some(true)
    && control::is_charging_enabled().unwrap_or(false)
```

Ini memverifikasi bahwa control charging aktif, tetapi belum memastikan battery benar-benar sedang charging.

Contoh:

```text
online = true
control enabled = true
status = NotCharging
```

Verification tetap sukses.

Perlu dibedakan:

```text
charging control enabled
```

dengan:

```text
battery actively charging
```

Untuk hardware state, `is_charging_enabled()` dapat menjadi authority. Untuk battery activity, gunakan `BatteryStatus` dan/atau current.

---

## 7. Verification `ChargingDisabled` perlu definisi yang sama

Sekarang:

```rust
control::is_charging_enabled().unwrap_or(true) == false
```

Ini cukup aman karena error dianggap gagal.

Namun idealnya dapat digabung dengan battery state:

```rust
control state = disabled
AND
battery tidak actively charging
```

Tetap pertahankan retry karena status battery Android bisa terlambat berubah.

---

## 8. Retry verification memiliki off-by-one

Sekarang:

```rust
if self.verification_failures > MAX_VERIFICATION_RETRIES
```

Dengan:

```rust
MAX_VERIFICATION_RETRIES = 3
```

permanent failure baru terjadi pada failure ke-4.

Jika maksudnya maksimal tiga attempt, gunakan:

```rust
if self.verification_failures >= MAX_VERIFICATION_RETRIES
```

Jika maksudnya satu initial attempt + tiga retry, implementasikan counter secara eksplisit agar tidak ambigu.

---

## 9. Netlink debounce sekarang adalah leading-edge debounce

Kode:

```rust
if found {
    if self.debounce_target.is_none() {
        self.debounce_target = Some(now + NETLINK_DEBOUNCE);
    }
}
```

Event kedua tidak menggeser deadline.

Jadi:

```text
event A ────────────────┐
                        │ 250 ms
event B ──────┐         │
              │         ▼
              └──────── wake
```

Jika yang diinginkan adalah "250 ms setelah event terakhir", gunakan:

```rust
if found {
    self.debounce_target = Some(now + NETLINK_DEBOUNCE);
}
```

Untuk burst uevent `power_supply`, trailing debounce biasanya lebih cocok.

---

## 10. Netlink `recv()` belum membedakan error

Sekarang:

```rust
if n <= 0 {
    break;
}
```

Ini menyamakan banyak kondisi:

- `EAGAIN`
- `EWOULDBLOCK`
- `EINTR`
- error socket
- EOF

Lebih baik:

```rust
if n < 0 {
    let err = std::io::Error::last_os_error();

    match err.kind() {
        std::io::ErrorKind::WouldBlock |
        std::io::ErrorKind::Interrupted => break,

        _ => {
            tracing::error!("Netlink recv failed: {}", err);
            self.disconnect();
            self.schedule_reconnect(now);
            return;
        }
    }
}

if n == 0 {
    break;
}
```

Dengan demikian socket error benar-benar memicu reconnect.

---

## 11. `online == None` belum memiliki policy eksplisit

Saat:

```rust
snapshot.online == None
```

kode tidak masuk `Offline` dan tidak masuk `Fault`.

Akibatnya policy dapat tetap:

```text
Charging
```

atau:

```text
LimitReached
```

Ini harus menjadi keputusan desain eksplisit.

Pilihan:

### Conservative

```text
online == None
→ Fault
→ ChargingDisabled
```

### Availability-oriented

```text
online == None
→ gunakan sensor lain
```

### Ownership-oriented

```text
online == None
→ pertahankan target sebelumnya
```

Untuk daemon charging-control, jangan biarkan perilaku ini terjadi secara implisit.

---

## 12. `status == None` juga belum dianggap fault

Fault saat ini hanya jika:

```rust
temp_dc.is_none()
|| capacity_pct.is_none()
```

Padahal `charging_state()` bergantung pada:

```rust
status
```

Jika `status = None`, state menjadi:

```rust
ChargingState::Unknown
```

Tentukan apakah `status` wajib atau hanya supplementary.

---

## 13. Scheduler dapat tidur terlalu lama setelah hardware failure

Ini salah satu masalah yang paling penting.

Jika:

```text
apply_target()
    ↓
FAILED
    ↓
sync = Failed
    ↓
force_apply = true
```

scheduler masih dapat menghasilkan:

```text
hingga 90 detik
```

Daemon kemudian baru mencoba lagi setelah interval tersebut.

Lebih aman jika:

```rust
if hardware.sync == SyncState::Failed {
    timeout = timeout.min(Duration::from_secs(2));
}
```

Atau buat retry deadline khusus di `HardwareController`.

---

## 14. Thermal safety jangan hanya bergantung pada ETA

DecisionEngine sudah benar karena thermal cutoff diperiksa langsung:

```rust
temp >= thermal_max
```

Tetapi scheduler bisa saja tidur lama ketika temperature belum mencapai cutoff.

Contoh:

```text
42°C
thermal limit 45°C
scheduler 90 detik
```

Temperature bisa melewati cutoff sebelum daemon bangun.

Sebaiknya dekat thermal limit ada hard upper bound.

Contoh:

```rust
if temp >= thermal_cutoff_dc - 30 {
    interval = interval.min(Duration::from_secs(5));
}
```

Jika unit adalah deci-degree, `30` berarti 3°C.

Contoh kebijakan:

```text
< cutoff - 5°C  → adaptive
cutoff - 5°C    → max 15 s
cutoff - 3°C    → max 5 s
```

---

## 15. Charge-limit juga sebaiknya punya polling upper bound

Misalnya:

```text
limit = 80%
capacity = 79%
```

Jangan biarkan scheduler memilih interval terlalu panjang.

Bisa dibuat:

```text
within 5% of limit → max 15 s
within 2% of limit → max 5 s
```

Scheduler tetap hanya optimasi wake-up; DecisionEngine tetap menjadi safety authority.

---

## 16. Satuan temperature scheduler membingungkan

`temp_dc` adalah deci-degree:

```text
450 = 45.0°C
```

Tetapi:

```rust
thermal_cutoff: f32
```

disimpan sebagai:

```text
45.0
```

lalu dikali lagi:

```rust
self.thermal_cutoff * 10.0
```

Secara numerik benar, tetapi mudah menimbulkan bug maintenance.

Lebih baik gunakan satu unit konsisten, misalnya:

```rust
thermal_cutoff_dc: i32
```

atau semua dalam Celsius:

```rust
temp_c: f32
thermal_cutoff_c: f32
```

---

## 17. Scheduler tidak mengetahui policy

Scheduler hanya tahu:

```rust
SensorSnapshot
netlink_alive
```

Padahal kondisi seperti:

```text
Fault
ThermalCutoff
LimitReached
Charging
```

mempengaruhi kebutuhan polling.

Pertimbangkan:

```rust
pub struct SchedulerContext {
    pub policy: ChargePolicyState,
    pub hardware_sync: SyncState,
    pub netlink_alive: bool,
}
```

Lalu:

```rust
scheduler.next_interval(&snapshot, &context)
```

---

## 18. Scheduler harus tetap menjadi optimasi, bukan safety authority

Arsitektur yang benar:

```text
             Battery Sensors
                    │
                    ▼
             Decision Engine
              SAFETY AUTHORITY
                    │
                    ▼
            Hardware Controller
                    │
                    ▼
             Kernel / Driver
```

Sedangkan:

```text
Scheduler
```

hanya menjawab:

```text
"Kapan kita perlu bangun dan membaca lagi?"
```

Jangan sampai scheduler menentukan apakah charging aman.

---

## 19. `current_ma` belum digunakan

Field:

```rust
current_ma: Option<i32>
```

masih unused.

Ini tidak masalah.

Nantinya current dapat digunakan untuk:

- memastikan charging benar-benar aktif;
- mendeteksi charger terhubung tetapi tidak mengisi;
- mendeteksi charging taper;
- memperbaiki estimasi charging rate.

Tetapi jangan menambahkan logic tersebut sebelum kontrak sensor Android-nya jelas.

---

## 20. Config reload masih melakukan force re-apply

Sekarang:

```rust
hardware.invalidate_verification();
hardware.force_apply = true;
scheduler.reset_prediction();
```

Ini aman, tetapi bisa menyebabkan write hardware berulang walaupun target tidak berubah.

Lebih efisien:

```text
config reload
    ↓
reset prediction
    ↓
DecisionEngine evaluate
    ↓
apply hanya jika desired target berubah
```

Jika ingin re-sync, cukup invalidate verification dan lakukan read-back.

---

## 21. Kombinasi state mulai kompleks

Sekarang controller memiliki:

```text
target
force_apply
sync
ownership
generation
verification
verification_failures
```

Kombinasi ini semakin banyak.

Contoh:

```text
target = ChargingDisabled
force_apply = true
sync = Failed
verification = None
ownership = Owned
```

Semua mungkin valid, tetapi sulit dibuktikan secara formal.

Untuk versi production-grade, pertimbangkan explicit state machine:

```rust
enum HardwareState {
    Unmanaged,

    AcquiringOwnership,

    Applying {
        target: HardwareTarget,
        attempt: u8,
    },

    Verifying {
        target: HardwareTarget,
        attempt: u8,
        deadline: Instant,
    },

    Synced {
        target: HardwareTarget,
    },

    Failed {
        target: HardwareTarget,
        retry_at: Instant,
    },
}
```

Ownership dapat tetap menjadi property terpisah.

---

## 22. `generation` sebenarnya sudah merupakan desain yang bagus

Bagian ini jangan dihilangkan sembarangan.

Contoh:

```text
apply A
generation = 5

apply B
generation = 6

verification A datang terlambat
```

Verification A ditolak:

```rust
if v.generation != self.generation {
    return;
}
```

Ini bagus untuk mencegah stale verification.

Jika state machine diperluas, `generation` dapat dinamai lebih jelas:

```rust
operation_id
```

---

# Rekomendasi Prioritas

## P0 — Wajib

1. Jangan mengubah `target` sebelum hardware berhasil.
2. Buat persistent ownership atomic.
3. Jangan abaikan error persistensi.
4. Pastikan directory state tersedia.
5. Tambahkan single-instance lock.
6. Jangan biarkan hardware failure menunggu sampai 90 detik.
7. Beri thermal polling hard upper bound.

## P1 — Sangat disarankan

8. Perjelas verification semantics.
9. Perbaiki retry counting.
10. Ubah netlink debounce menjadi trailing debounce jika itu yang diinginkan.
11. Bedakan error `recv()`.
12. Tentukan semantic `online == None`.
13. Tentukan semantic `status == None`.

## P2 — Arsitektur

14. Pisahkan `desired_target` dan `applied_target`.
15. Pertimbangkan explicit hardware state machine.
16. Gunakan unit temperature yang konsisten.
17. Berikan policy context kepada scheduler.
18. Pertahankan generation/operation ID untuk mencegah stale verification.

---

# Kesimpulan

Versi ini sudah memiliki fondasi arsitektur yang jauh lebih kuat:

```text
DecisionEngine
      ↓
HardwareController
      ↓
Charging Control
      ↓
Verification
```

Ditambah:

```text
Netlink
   ↓
wake-up

Scheduler
   ↓
poll optimization

Persistent Ownership
   ↓
crash recovery
```

Ini jauh lebih aman daripada implementasi sederhana:

```text
read battery
→ if >= limit disable
→ sleep
```

Namun sebelum disebut **production-ready**, minimal selesaikan:

```text
1. target/applied-state semantics
2. atomic ownership persistence
3. single-instance protection
4. retry/failure wake-up
5. thermal polling upper bound
6. verification semantics
7. netlink recv error handling
8. unknown sensor semantics
```

Setelah itu, state machine ini sudah memiliki fondasi yang sangat baik untuk daemon charging-control Android/root.
