Ya. Dari kode terbaru ini, **akar masalahnya masih ada**, dan sekarang justru terlihat cukup jelas dari kombinasi kode + log sebelumnya.

Masalah utamanya bukan lagi sekadar `online=false`, tetapi **cara menentukan apakah `online=false` adalah unplug fisik atau efek samping dari target `ChargingDisabled`**.

### 1. Bug utama ada di sini

```rust
let self_induced_offline = hardware.is_owned()
    && (hardware.sync == SyncState::Synced || hardware.sync == SyncState::Pending)
    && hardware.desired_target == HardwareTarget::ChargingDisabled;
```

Ini terlalu ketat.

Log Anda sebelumnya menunjukkan:

```text
Hardware target changed: ChargingEnabled -> ChargingDisabled
Applying hardware target: ChargingDisabled (sync=Unknown, force=true)
```

Artinya setelah target `ChargingDisabled` diterapkan, `hardware.sync` **bisa menjadi `Unknown`**.

Akibatnya:

```text
hardware.is_owned() = true
hardware.desired_target = ChargingDisabled
hardware.sync = Unknown
```

maka:

```rust
self_induced_offline == false
```

Kemudian:

```rust
physically_offline = snapshot.online == Some(false)
```

menjadi `true`.

Lalu:

```rust
presence = ChargerPresence::Offline
```

dan `DecisionEngine` melakukan:

```rust
self.policy = ChargePolicyState::Offline;

HardwareTarget::Unmanaged
```

Persis bounce yang Anda alami.

Jadi siklusnya sekarang kemungkinan:

```text
ChargingEnabled
      ↓
capacity >= limit
      ↓
ChargingDisabled
      ↓
kernel online = 0
      ↓
hardware.sync = Unknown
      ↓
self_induced_offline = false
      ↓
presence = Offline
      ↓
Decision = Unmanaged
      ↓
restore charging
      ↓
kernel online = 1
      ↓
Decision = Charging
      ↓
...
```

**Jadi perubahan `ChargerPresence` belum menyelesaikan akar masalah.**

---

# 2. Perbaikan minimal

Untuk kasus ini, jangan gunakan `hardware.sync` sebagai syarat untuk menentukan self-induced disconnect.

Ganti:

```rust
let self_induced_offline = hardware.is_owned()
    && (hardware.sync == SyncState::Synced || hardware.sync == SyncState::Pending)
    && hardware.desired_target == charger_core::hardware::controller::HardwareTarget::ChargingDisabled;
```

menjadi minimal:

```rust
let self_induced_offline =
    hardware.is_owned()
    && hardware.desired_target
        == charger_core::hardware::controller::HardwareTarget::ChargingDisabled;
```

Dengan demikian:

```text
owned + desired disabled + online=false
```

dianggap:

```text
Unknown
```

bukan:

```text
Offline
```

### Tetapi saya belum menganggap ini desain final yang ideal.

Karena ada masalah kedua.

---

# 3. `desired_target` juga bukan bukti bahwa hardware benar-benar disabled

Ini penting.

Anda sebelumnya sudah mengalami:

```text
Charging control partially applied: 2/5 writes succeeded, 3 failed
```

Jadi:

```rust
hardware.desired_target == ChargingDisabled
```

belum tentu berarti:

```text
hardware benar-benar berhasil disabled
```

`desired_target` hanya berarti **daemon menginginkan** charging disabled.

Sementara:

```rust
hardware.is_owned()
```

berarti daemon memiliki ownership.

Keduanya belum membuktikan:

> "Saya sendiri yang menyebabkan VBUS menjadi invisible."

Jadi desain yang lebih kuat membutuhkan **last known/applied hardware state**.

---

# 4. Saya sarankan tambah `HardwareEffect`

Daripada menebak dari `sync`, buat status eksplisit:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareEffect {
    None,
    ChargingEnabled,
    ChargingDisabled,
    Unknown,
}
```

Kemudian `HardwareController` menyimpan:

```rust
pub last_effect: HardwareEffect,
```

Ketika berhasil menerapkan:

```rust
ChargingDisabled
```

set:

```rust
self.last_effect = HardwareEffect::ChargingDisabled;
```

Tetapi karena sistem Anda punya partial write, **jangan langsung menganggap sukses jika hanya sebagian node berhasil**.

Idealnya:

```rust
if result.all_succeeded() {
    self.last_effect = HardwareEffect::ChargingDisabled;
} else {
    self.last_effect = HardwareEffect::Unknown;
}
```

Hal yang sama untuk enable.

---

# 5. Bahkan lebih bagus: bedakan "intent" dan "effect"

Anda sekarang punya:

```rust
desired_target
```

Saya sarankan controller mempunyai tiga konsep:

```rust
desired_target
applied_target
hardware_effect
```

Contohnya:

```text
desired_target = ChargingDisabled
applied_target = ChargingDisabled
hardware_effect = ChargingDisabled
```

berarti:

> daemon meminta disable dan berhasil menerapkannya.

Sedangkan:

```text
desired_target = ChargingDisabled
applied_target = Unknown
hardware_effect = Unknown
```

berarti:

> daemon ingin disable tetapi belum terbukti berhasil.

Ini sangat penting untuk kasus Anda karena log menunjukkan:

```text
2/5 succeeded
3 failed
```

---

# 6. `ChargerPresence::Unknown` Anda sudah benar secara konsep

Bagian ini:

```rust
let presence = if physically_offline {
    if self_induced_offline {
        ChargerPresence::Unknown
    } else {
        ChargerPresence::Offline
    }
} else {
    ChargerPresence::Online
};
```

sudah merupakan arah desain yang benar.

Masalahnya adalah **deteksi `self_induced_offline` masih salah**.

Setelah diperbaiki, alurnya akan menjadi:

```text
capacity = 90
online = 1
        │
        ▼
LimitReached
        │
        ▼
ChargingDisabled
        │
        ▼
hardware disables charging
        │
        ▼
kernel online = 0
        │
        ▼
owned + disabled effect
        │
        ▼
ChargerPresence::Unknown
        │
        ▼
DecisionEngine tidak menghasilkan Offline/Unmanaged
        │
        ▼
tetap ChargingDisabled
```

Itu yang Anda inginkan.

---

# 7. Tetapi ada masalah lain di `DecisionEngine`

Saat `presence == Unknown`, kode Anda sekarang **tidak punya state khusus**.

Anda melakukan:

```rust
if presence == ChargerPresence::Offline {
    ...
}
```

Tetapi:

```rust
ChargerPresence::Unknown
```

langsung masuk ke:

```rust
status_valid
capacity_valid
temp_valid
...
```

dan kemudian ke:

```rust
limit_reached
```

Ini sebenarnya bagus untuk kasus limit:

```text
Unknown
+
capacity >= limit
→ LimitReached
→ ChargingDisabled
```

Tetapi ada satu edge case.

Misalnya:

```text
capacity = 88
resume = 85
policy = LimitReached
online = 0
presence = Unknown
```

Kode:

```rust
let limit_reached = if self.policy == ChargePolicyState::LimitReached {
    capacity > resume
} else {
    capacity >= limit
};
```

menghasilkan:

```text
88 > 85 = true
```

sehingga tetap:

```text
ChargingDisabled
```

**Bagus.**

Tetapi ketika:

```text
capacity = 84
presence = Unknown
```

maka:

```text
limit_reached = false
```

dan akhirnya:

```rust
self.policy = ChargePolicyState::Charging;

HardwareTarget::ChargingEnabled
```

Ini bisa menjadi masalah.

Karena `Unknown` berarti:

> kita belum tahu apakah kabel benar-benar dicabut atau `online=0` akibat intervensi hardware.

Anda **tidak boleh langsung meng-enable charging hanya karena capacity turun di bawah resume**.

---

# 8. `Unknown` seharusnya menjadi state konservatif

Untuk kasus charger-control, prinsipnya:

> **Unknown ≠ Online**

dan juga:

> **Unknown ≠ Offline**

Unknown harus mempertahankan state terakhir yang aman.

Misalnya:

```rust
match presence {
    ChargerPresence::Offline => {
        ...
    }

    ChargerPresence::Unknown => {
        // jangan mengubah ownership/charging state hanya
        // berdasarkan online=false
    }

    ChargerPresence::Online => {
        // normal policy evaluation
    }
}
```

Namun cara paling bagus adalah menjadikan **policy state sebagai sumber kebenaran**, bukan `online`.

Contoh:

```text
LimitReached + Unknown
→ ChargingDisabled

ThermalCutoff + Unknown
→ ChargingDisabled

Charging + Unknown
→ jangan otomatis Unmanaged
```

---

# 9. Saya juga melihat masalah pada `Fault`

Ini bagian:

```rust
if !sensors_valid {
    self.policy = ChargePolicyState::Fault;
    self.fault_recovery_reads = 0;

    return Self::decision(
        self.policy,
        HardwareTarget::ChargingDisabled,
        reason,
    );
}
```

Kalau `online` hanya transient `false`, Anda sekarang **sudah tidak langsung masuk sini** karena `presence` dipisahkan.

Itu bagus.

Tetapi:

```rust
snapshot.online.is_some()
```

masih membuat `online=None` dianggap sensor fault.

Padahal untuk desain Anda, `online` sekarang sudah bukan sensor yang selalu reliable.

Saya justru akan memisahkan:

```rust
capacity_valid
temp_valid
status_valid
```

dari:

```rust
presence
```

dan **tidak menjadikan `snapshot.online` sebagai syarat sensor battery fault**.

Jadi:

```rust
let sensors_valid =
    capacity_valid
    && temp_valid
    && status_valid;
```

lebih masuk akal.

`online` sudah diproses menjadi:

```rust
ChargerPresence
```

di layer monitor.

---

# 10. Scheduler juga punya potensi masalah

Anda sudah melakukan:

```rust
match presence {
    ChargerPresence::Offline => 600s,
    ChargerPresence::Unknown => calculate_eta(...).min(15s),
    ChargerPresence::Online => calculate_eta(...),
}
```

Ini jauh lebih baik daripada versi sebelumnya.

Tetapi ada bug kecil:

```rust
self.calculate_eta(snapshot).min(Duration::from_secs(15))
```

`calculate_eta()` selalu di-clamp:

```rust
interval.clamp(MIN_INTERVAL, MAX_INTERVAL)
```

jadi Unknown maksimal 15 detik memang benar.

Namun ketika:

```text
Unknown + ChargingDisabled + capacity=90
```

`calculate_eta()` bisa menghasilkan `MIN_INTERVAL` karena capacity sudah dekat target.

Jadi daemon bisa polling:

```text
2s
2s
2s
...
```

Tidak salah, tetapi tidak perlu terlalu agresif.

Untuk `Unknown` saya lebih suka:

```rust
ChargerPresence::Unknown => {
    self.last_interval = Duration::from_secs(5);
    self.last_interval
}
```

atau 5–10 detik.

Karena tujuan Unknown bukan menghitung ETA charging, melainkan **memastikan apakah presence bisa dikonfirmasi kembali**.

---

# 11. Ada masalah lebih besar: partial write

Ini justru menurut saya perlu Anda perbaiki sebelum mengejar scheduler.

Log:

```text
Charging control partially applied: 2/5 writes succeeded, 3 failed
```

tetapi controller kemudian mencatat:

```text
Hardware charging set to false: 2/5 nodes succeeded (3 failed)
```

Kalau dua node yang berhasil itu termasuk node yang benar-benar mengontrol charging, sementara tiga lainnya gagal, sistem berada dalam kondisi:

```text
Hardware = partially modified
```

Tetapi decision layer bisa menganggap:

```text
ChargingDisabled
```

secara penuh.

Ini berbahaya.

Anda perlu tiga hasil:

```rust
pub enum ApplyResult {
    Applied,
    Partial,
    Failed,
}
```

dan state:

```rust
SyncState::Synced
SyncState::Pending
SyncState::Unknown
```

Misalnya:

```text
5/5 → Applied
2/5 → Partial
0/5 → Failed
```

Untuk:

```text
Partial
```

jangan anggap:

```text
HardwareEffect::ChargingDisabled
```

melainkan:

```text
HardwareEffect::Unknown
```

sampai verification membuktikan kondisi akhir.

---

# 12. Dan log Anda mengungkap masalah permission yang berbeda

Ini:

```text
Permission denied (os error 13)
```

terjadi pada:

```text
/sys/class/power_supply/main/charging_enabled
/sys/class/power_supply/battery/battery_charging_enabled
/sys/class/power_supply/usb/input_suspend
```

Kalau daemon benar-benar berjalan sebagai root, maka **ini bukan masalah Rust decision engine**.

Anda perlu cek dari proses daemon itu sendiri:

```sh
id
```

dan:

```sh
cat /proc/$(pidof charger_daemon)/status | grep -E 'Uid|Gid|Cap'
```

serta:

```sh
ls -l /sys/class/power_supply/main/charging_enabled
ls -l /sys/class/power_supply/battery/battery_charging_enabled
ls -l /sys/class/power_supply/usb/input_suspend
```

Karena kalau shell root bisa:

```sh
echo 0 > ...
```

tetapi daemon mendapat:

```text
EACCES
```

maka kita perlu melihat **SELinux/domain/capability/context proses daemon**, bukan sekadar UID.

---

# 13. Kesimpulan terhadap kode terbaru

**Arsitekturnya sudah jauh lebih benar**, khususnya:

```rust
ChargerPresence {
    Online,
    Offline,
    Unknown,
}
```

dan pemisahan:

```rust
presence
```

dari:

```rust
snapshot.online
```

adalah langkah yang tepat.

Tetapi ada **3 bug penting yang masih tersisa**:

### Bug #1 — paling langsung menyebabkan bounce

Ini:

```rust
hardware.sync == SyncState::Synced
    || hardware.sync == SyncState::Pending
```

membuat `self_induced_offline` gagal ketika `sync == Unknown`.

**Perbaiki.**

---

### Bug #2 — `desired_target` bukan bukti hardware berhasil berubah

```rust
desired_target == ChargingDisabled
```

hanya berarti intent.

Anda membutuhkan:

```rust
applied_target
```

atau lebih bagus:

```rust
HardwareEffect
```

yang hanya berubah setelah verification sukses.

---

### Bug #3 — `Unknown` belum benar-benar diperlakukan sebagai unknown

Sekarang:

```rust
Unknown
```

tetap bisa berakhir ke:

```rust
ChargingEnabled
```

ketika capacity turun di bawah resume.

Untuk state `Unknown`, seharusnya **jangan mengambil keputusan hardware baru hanya berdasarkan `online=false`**.

---

## Perbaikan paling penting sekarang

Kalau Anda ingin menguji apakah diagnosis ini benar **tanpa refactor besar**, ubah dulu:

```rust
let self_induced_offline = hardware.is_owned()
    && (hardware.sync == SyncState::Synced || hardware.sync == SyncState::Pending)
    && hardware.desired_target == HardwareTarget::ChargingDisabled;
```

menjadi:

```rust
let self_induced_offline =
    hardware.is_owned()
    && hardware.desired_target
        == charger_core::hardware::controller::HardwareTarget::ChargingDisabled;
```

Lalu tambahkan log:

```rust
tracing::debug!(
    "Presence: online={:?}, owned={}, sync={:?}, desired={:?}, self_induced={}, presence={:?}",
    snapshot.online,
    hardware.is_owned(),
    hardware.sync,
    hardware.desired_target,
    self_induced_offline,
    presence,
);
```

Saya **sangat menyarankan menjalankan satu siklus lagi dengan log ini**.

Kalau diagnosis saya benar, tepat setelah:

```text
ChargingDisabled
```

Anda akan melihat kira-kira:

```text
online=Some(false)
owned=true
sync=Unknown
desired=ChargingDisabled
self_induced=true
presence=Unknown
```

dan **tidak lagi**:

```text
presence=Offline
target=Unmanaged
```

Itu akan membuktikan bahwa bounce yang tersisa memang berasal dari `SyncState::Unknown` yang saat ini salah dipakai sebagai syarat deteksi self-induced disconnect.
