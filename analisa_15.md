Nah, setelah melihat `control` + `reader` ini, akar masalahnya jadi jauh lebih jelas. **Ada bug konkret di `read_status()` dan ada desain verification yang membuat loop.**

## 1. Bug paling jelas: parsing `Not Charging` salah

Kode kamu:

```rust
if s_lower.contains("discharging") {
    Ok(BatteryStatus::Discharging)
} else if s_lower.contains("charging") && !s_lower.contains("not charging") {
    Ok(BatteryStatus::Charging)
} else if s_lower.contains("full") {
    Ok(BatteryStatus::Full)
} else if s_lower.contains("not charging") {
    Ok(BatteryStatus::NotCharging)
}
```

Untuk sysfs:

```text
Not Charging
```

hasil:

```rust
s_lower.contains("charging") == true
s_lower.contains("not charging") == true
```

jadi kondisi kedua false, lalu baru masuk:

```rust
NotCharging
```

**Yang ini sebenarnya masih benar.**

Tetapi parsing-nya lebih baik dibalik urutannya karena sekarang parser bergantung pada pengecualian substring.

Saya akan ubah menjadi:

```rust
pub fn read_status(&mut self) -> Result<BatteryStatus, ChargerError> {
    let s = Self::read_fd_to_str(&mut self.status_fd, &mut self.buf, "status")?;

    match s.trim().to_ascii_lowercase().as_str() {
        "charging" => Ok(BatteryStatus::Charging),
        "discharging" => Ok(BatteryStatus::Discharging),
        "not charging" => Ok(BatteryStatus::NotCharging),
        "full" => Ok(BatteryStatus::Full),
        _ => Ok(BatteryStatus::Unknown),
    }
}
```

Lebih aman dan tidak ambigu.

---

# 2. Tapi ini bukan penyebab utama loop

Dengan kode `control` yang kamu kirim, kita sekarang tahu:

```rust
set_charging(false)
```

melakukan:

```text
CHARGING_NODES → "0"
SUSPEND_NODES  → "1"
```

Kemudian:

```rust
is_charging_enabled()
```

membaca:

```text
CHARGING_NODES
```

terlebih dahulu.

Artinya jika node pertama berhasil ditulis:

```text
charging_enabled = 0
```

maka:

```rust
is_charging_enabled()
```

harus mengembalikan:

```rust
false
```

**jika node tersebut memang read/write control node yang benar.**

Jadi verification:

```rust
control_disabled = false
```

kemungkinan kecil kalau sysfs node benar.

Yang jauh lebih mencurigakan adalah:

```rust
battery_safe = matches!(
    snapshot.charging_state(),
    ChargingState::NotCharging | ChargingState::Full
);
```

---

# 3. Ada kemungkinan besar kondisi kamu seperti ini

Setelah:

```rust
set_charging(false)
```

hardware:

```text
charging_enabled = 0
input_suspend    = 1
```

tetapi:

```text
/sys/class/power_supply/battery/status
```

masih:

```text
Charging
```

atau status berubah terlambat.

Maka:

```text
control_disabled = true
battery_safe     = false
```

dan:

```rust
true && false
```

→ verification gagal.

Padahal **charging control sebenarnya sudah berhasil dimatikan.**

Ini sangat mungkin menjelaskan log kamu.

---

# 4. Lebih parah lagi: `is_charging_enabled()` hanya membaca node pertama

Ini desain yang berbahaya:

```rust
for node in CHARGING_NODES {
    let path = Path::new(node);

    if path.exists() {
        if let Ok(content) = fs::read_to_string(path) {
            return Ok(content.trim() == "1");
        }
    }
}
```

Misalnya:

```text
CHARGING_NODES:
    battery/charging_enabled
    main/charging_enabled
    usb/charging_enabled
```

dan:

```text
battery/charging_enabled = 0
main/charging_enabled    = 1
```

fungsi langsung berhenti di node pertama.

Begitu juga sebaliknya.

Padahal `set_charging()`:

```rust
for node in CHARGING_NODES {
    ...
    write_sysfs(...)
}
```

menulis **semua node yang tersedia**.

Jadi semantics write dan read tidak simetris.

### Write:

```text
node A ← 0
node B ← 0
node C ← 0
```

### Read:

```text
baca node A
STOP
```

Itu bisa menghasilkan verification palsu.

---

# 5. Masalah yang lebih besar: `set_charging()` menganggap satu node sukses = semuanya sukses

Ini:

```rust
if path.exists() && write_sysfs(path, charge_val).is_ok() {
    any_written = true;
}
```

Kalau:

```text
A → sukses
B → gagal
C → gagal
```

fungsi tetap:

```rust
Ok(())
```

Karena:

```rust
any_written == true
```

Ini mungkin memang sengaja karena device punya banyak variasi node.

Tetapi untuk verification, kamu kemudian harus tahu:

> node mana yang sebenarnya authoritative?

Sekarang tidak ada konsep itu.

---

# 6. Saya sarankan pisahkan "control state" dan "actual charging state"

Ini menurut saya perubahan arsitektur terpenting.

Jangan menganggap:

```text
charging_enabled == false
```

harus langsung menghasilkan:

```text
BatteryStatus::NotCharging
```

Keduanya adalah dua state berbeda:

```text
                ┌──────────────────┐
                │ Charging Control │
                └────────┬─────────┘
                         │
                  enabled / disabled
                         │
                         ▼
                ┌──────────────────┐
                │ Charging Engine  │
                │ / Kernel / HAL   │
                └────────┬─────────┘
                         │
                         ▼
                ┌──────────────────┐
                │ Battery Status   │
                └──────────────────┘
```

Jadi:

```rust
is_charging_enabled()
```

adalah **control state**.

Sedangkan:

```rust
BatteryStatus
```

adalah **reported battery state**.

---

# 7. Verification `ChargingDisabled` seharusnya jangan terlalu ketat

Saat ini:

```rust
ChargingDisabled => {
    let control_disabled =
        control::is_charging_enabled().unwrap_or(true) == false;

    let battery_safe =
        matches!(
            snapshot.charging_state(),
            ChargingState::NotCharging | ChargingState::Full
        );

    control_disabled && battery_safe
}
```

Saya justru akan membuat:

```rust
ChargingDisabled => {
    match control::is_charging_enabled() {
        Ok(false) => true,
        Ok(true) => false,
        Err(e) => {
            tracing::warn!(
                "Unable to verify charging control state: {}",
                e
            );
            false
        }
    }
}
```

Karena tujuan command:

```rust
set_charging(false)
```

adalah memastikan **charging control disabled**.

Sedangkan `BatteryStatus` bisa digunakan sebagai **secondary safety observation**, bukan syarat utama synchronization.

---

# 8. Tetapi ada satu caveat penting

Kalau tujuan aplikasi kamu adalah **charge limiter**, bukan sekadar toggle charging, saya malah menyarankan verification:

```text
control disabled
        AND
actual current <= threshold
```

bukan:

```text
control disabled
        AND
status == NotCharging
```

Karena `status` Android bisa lag / tidak konsisten.

Kamu sudah punya:

```rust
current_ma
```

Jadi bisa digunakan.

Misalnya:

```rust
fn charging_actually_stopped(snapshot: &SensorSnapshot) -> bool {
    match snapshot.current_ma {
        Some(current) => current <= 100,
        None => false,
    }
}
```

Tetapi threshold `100 mA` harus disesuaikan dengan device/driver karena current measurement bisa noisy.

---

# 9. Ada bug penting pada `read_current_ma()`

Ini juga perlu diperhatikan.

Kamu punya:

```rust
if val != 0 {
    let mut ua = val as f32;

    if ua.abs() > 10_000.0 {
        ua /= 1000.0;
    }

    return Ok(ua);
}
```

Ini mengasumsikan:

```text
> 10000 → µA
<= 10000 → mA
```

Masalahnya node `current_now` Android biasanya bisa berupa:

```text
-150000
```

→ -150 mA

tetapi ada driver yang memakai:

```text
-150
```

→ -150 mA.

Heuristik ini memang praktis, tetapi bukan sumber loop verification.

---

# 10. `is_plugged_in()` juga punya safety fallback yang berpotensi salah

Ini:

```rust
Ok(true)
```

ketika tidak menemukan `online`.

Artinya:

> kalau tidak tahu charger terpasang atau tidak, anggap charger terpasang.

Untuk safety charging, itu masuk akal.

Tetapi:

```rust
CachedReader::is_plugged_in()
```

berbeda:

```rust
if self.online_fds.is_empty() {
    return is_plugged_in();
}
...
Ok(false)
```

Jadi ada dua semantics:

### Non-cached

```text
tidak menemukan online → true
```

### Cached

```text
FD ada tetapi semuanya 0 → false
```

Ini sebenarnya baik, karena FD memang berhasil dibuka. Tapi jika semua charger `online=0`, false.

---

# 11. Ada masalah lifetime dengan `CachedReader`

Kamu menyimpan:

```rust
online_fds: Vec<File>
```

dan membuka daftar power supply **hanya sekali**:

```rust
CachedReader::new()
```

Kalau Android:

* USB charger berubah
* power supply muncul/hilang
* USB PD berubah
* driver reload

daftar FD bisa menjadi stale.

Misalnya daemon mulai ketika:

```text
usb/online
```

belum tersedia.

Maka:

```rust
online_fds = []
```

dan selanjutnya menggunakan fallback.

Ini bukan penyebab log verification saat ini, tetapi perlu diperhatikan.

---

# 12. Sekarang kembali ke loop

Ini tetap bug terbesar:

```rust
pub fn needs_apply(&self, new_target: HardwareTarget) -> bool {
    self.applied_target != new_target
        || self.force_apply
        || self.sync == SyncState::Failed
}
```

**Hapus `self.sync == SyncState::Failed` dari sini.**

Misalnya:

```rust
pub fn needs_apply(&self, new_target: HardwareTarget) -> bool {
    self.applied_target != new_target || self.force_apply
}
```

Kemudian setelah verification habis:

```rust
self.sync = SyncState::Failed;
self.verification = None;
self.force_apply = false;
```

Jangan:

```rust
self.force_apply = true;
```

---

# 13. Kalau ingin recovery, gunakan retry timer

Jangan:

```text
Failed → apply lagi di tick berikutnya
```

gunakan:

```text
Failed
  ↓
cooldown
  ↓
retry
```

Contohnya tambahkan:

```rust
retry_at: Option<Instant>,
```

kemudian:

```rust
Failed
    ↓
retry_at = now + 30s
```

dan `needs_apply()`:

```rust
pub fn needs_apply(&self, target: HardwareTarget, now: Instant) -> bool {
    if self.applied_target != target {
        return true;
    }

    if self.force_apply {
        return true;
    }

    if self.sync == SyncState::Failed {
        return self.retry_at.is_some_and(|t| now >= t);
    }

    false
}
```

Ini menghilangkan:

```text
2 detik → write
2 detik → write
2 detik → write
```

---

# 14. Saya juga akan mengubah `set_charging()` supaya hasilnya lebih informatif

Sekarang:

```rust
Result<(), ChargerError>
```

terlalu sederhana.

Karena kamu sebenarnya punya:

```text
berapa node ditemukan
berapa node berhasil ditulis
berapa node gagal
```

Idealnya:

```rust
pub struct ChargingWriteResult {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}
```

Kemudian:

```rust
pub fn set_charging(enable: bool) -> Result<ChargingWriteResult, ChargerError>
```

Dengan begitu log bisa mengatakan:

```text
Charging disable:
attempted=4 succeeded=4 failed=0
```

atau:

```text
attempted=4 succeeded=1 failed=3
```

Ini jauh lebih berguna untuk debugging device Android yang punya banyak node.

---

# 15. Ada satu hal yang saya ingin kamu cek sekarang

Karena kita sudah sampai ke level ini, **yang paling penting berikutnya adalah isi `nodes.rs`**:

```rust
CHARGING_NODES
SUSPEND_NODES
CURRENT_NODES
```

Terutama saya ingin melihat apakah misalnya:

```rust
CHARGING_NODES
```

berisi:

```text
/sys/class/power_supply/battery/charging_enabled
/sys/class/power_supply/main/charging_enabled
```

atau node MediaTek/MTK lainnya.

Karena ada kemungkinan lain:

```text
set_charging(false)
```

berhasil menulis:

```text
battery/charging_enabled = 0
```

tetapi:

```text
main/charging_enabled = 1
```

atau `input_suspend` masih berbeda.

---

## Diagnosis saya sekarang

Setelah melihat seluruh kode yang kamu kirim, saya akan rank seperti ini:

### 🔴 #1 — penyebab loop

```rust
self.sync == SyncState::Failed
```

di:

```rust
needs_apply()
```

ditambah:

```rust
self.force_apply = true
```

di:

```rust
verification_failed()
```

Ini **pasti memungkinkan infinite re-apply loop**.

---

### 🔴 #2 — verification terlalu ketat

```rust
control_disabled && battery_safe
```

`battery_safe` berdasarkan:

```rust
BatteryStatus::NotCharging | Full
```

padahal `BatteryStatus` tidak identik dengan charging control.

---

### 🟠 #3 — read/write node tidak simetris

`set_charging()` menulis semua node:

```text
A
B
C
```

tetapi `is_charging_enabled()` hanya membaca node pertama yang berhasil:

```text
A → return
```

Ini bisa menghasilkan false verification.

---

### 🟠 #4 — error disamarkan

```rust
unwrap_or(true)
```

membuat kegagalan read terlihat seperti:

```text
charging enabled
```

tanpa tahu kenapa.

---

### 🟡 #5 — `set_charging()` menganggap partial success sebagai success

```rust
any_written = true
```

padahal sebagian node mungkin gagal.

---

### 🟡 #6 — `CachedReader` menyimpan online FD terlalu lama

Ini bukan penyebab log yang kamu tunjukkan, tetapi bisa menjadi bug runtime lain.

---

**Jadi saya belum menyarankan membongkar seluruh arsitektur.** Fondasinya sebenarnya sudah cukup bagus. Yang perlu diperbaiki adalah kontrak antara `HardwareController` ↔ `control` ↔ `SensorSnapshot`.

Kalau kamu kirim `nodes.rs`, saya bisa lanjutkan satu langkah lagi dan **trace persis node mana yang ditulis, node mana yang dibaca untuk verification, dan apakah ada konflik antara `charging_enabled` dan `input_suspend`**. Dari situ kita bisa menentukan patch final yang paling aman tanpa sekadar "mematikan loop" secara paksa.
