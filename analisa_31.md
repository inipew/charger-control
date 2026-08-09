Ya. Dari kode terbaru + log terbaru, sekarang akar masalahnya **sudah jauh lebih jelas**.

Masalah utamanya **bukan lagi hysteresis, scheduler, netlink, atau `charge_limit`**. Ada race/semantic bug antara:

1. `snapshot.online`
2. `ChargerPresence`
3. `HardwareTarget`
4. partial write `2/5`
5. proses verification/reconcile.

Dan log menunjukkan bug itu terjadi **hanya ~145 ms setelah charging berhasil sebagian**.

---

# 1. Bukti paling kuat ada di timestamp

Perhatikan:

```text
11:08:00.517
Hardware target changed: Unmanaged -> ChargingEnabled

11:08:00.552
... 2/5 writes succeeded, 3 failed

11:08:00.725
Hardware target changed: ChargingEnabled -> ChargingDisabled
```

Jadi:

```text
ChargingEnabled
      ↓
set_charging(true)
      ↓
2/5 berhasil
      ↓
~173 ms kemudian
      ↓
ChargingDisabled
```

Padahal konfigurasi:

```toml
enabled = true
charge_limit = 100
thermal_cutoff = false
max_temp_dc = 420
thermal_resume_hysteresis_dc = 30
```

dan baterai:

```text
77%
34°C
```

Secara policy, **tidak ada alasan untuk masuk `ChargingDisabled`**.

Jadi hampir pasti `DecisionEngine` menerima sensor yang membuat:

```rust
presence == ChargerPresence::Offline
```

atau:

```rust
sensors_valid == false
```

atau ada status sensor lain yang tidak kita lihat di log.

---

# 2. Ada bug yang sangat mencurigakan di sini

Ini bagian paling penting:

```rust
let physically_offline = snapshot.online == Some(false);

let self_induced_offline = hardware.is_owned()
    && (hardware.sync == SyncState::Synced || hardware.sync == SyncState::Pending)
    && hardware.desired_target
        == charger_core::hardware::controller::HardwareTarget::ChargingDisabled;

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

Masalahnya:

> `snapshot.online == false` langsung dianggap charger dicabut.

Padahal `online` belum tentu berarti **physical charger presence**.

Pada Android/Qualcomm, node `online` dari power_supply dapat berubah akibat state charging/input-suspend/PMIC, dan bahkan bisa sementara tidak konsisten ketika beberapa node charging sedang diubah.

Lebih parah lagi, Anda baru saja melakukan:

```rust
control::set_charging(true, ...)
```

dan hasilnya:

```text
2/5 writes succeeded
3 failed
```

Artinya hardware berada dalam **partial state**.

Tetapi program kemudian melakukan:

```rust
snapshot.online
    ↓
ChargerPresence
    ↓
DecisionEngine
```

seolah-olah hardware sudah stabil.

---

# 3. Partial write masih dianggap "ApplySuccess"

Ini juga bug penting.

Sekarang:

```rust
Ok(res) if res.succeeded > 0 => {
```

berarti:

```text
2/5
```

dianggap:

```rust
ApplySuccess
```

dan kemudian:

```rust
self.mark_apply_success(target, is_partial);
```

memang Anda sudah membuat:

```rust
effect = HardwareEffect::Unknown
```

untuk partial.

Itu bagus.

**Tetapi `sync` tetap menjadi `Pending`, lalu verification berjalan.**

```rust
self.sync = SyncState::Pending;
```

Dan yang lebih penting, monitor tetap membiarkan `DecisionEngine` mengevaluasi snapshot seolah-olah hardware normal.

Jadi secara konseptual Anda sekarang punya:

```text
Hardware = PARTIALLY APPLIED
Controller = Pending
Decision engine = bebas mengubah target
```

Ini tidak aman.

---

# 4. Ada masalah yang lebih besar: `Unmanaged` menyebabkan oscillation

Lihat sequence dari log:

```text
ChargingEnabled
    ↓
ChargingDisabled
    ↓
Unmanaged
    ↓
ChargingDisabled
```

Ini sangat informatif.

Kenapa `ChargingEnabled -> ChargingDisabled`?

Kemungkinan besar:

```rust
snapshot.online == Some(false)
```

sehingga:

```rust
presence = Offline
```

dan:

```rust
DecisionEngine
    → HardwareTarget::Unmanaged
```

Sebenarnya log menunjukkan `ChargingDisabled`, bukan `Unmanaged`, jadi setelah itu snapshot berikutnya kemungkinan berubah lagi dan decision engine masuk state lain.

Artinya sistem Anda sedang **berosilasi karena sensor `online` berubah**.

---

# 5. Bug fatal lainnya: `Unmanaged` saat charger offline tidak perlu menulis hardware

Di decision:

```rust
if presence == ChargerPresence::Offline {
    self.policy = ChargePolicyState::Offline;

    return Self::decision(
        self.policy,
        HardwareTarget::Unmanaged,
        DecisionReason::ChargerOffline,
    );
}
```

Ini masuk akal secara konsep.

Tetapi:

```rust
HardwareTarget::Unmanaged
```

di controller berarti:

> Lepaskan ownership dan restore original charging state.

Karena:

```rust
HardwareTarget::Unmanaged => {
    self.release_ownership()
}
```

Jadi `Unmanaged` bukan sekadar:

> "jangan kontrol hardware"

tetapi:

> "restore hardware ke keadaan original sekarang."

Itu dua semantik yang berbeda.

Ketika charger dianggap offline karena `snapshot.online` false secara transient, Anda langsung:

```text
Offline
 ↓
Unmanaged
 ↓
release_ownership()
 ↓
restore original charging
```

Padahal charger mungkin **tidak benar-benar dicabut**.

Ini bisa menyebabkan feedback loop.

---

# 6. Saya sarankan pisahkan `presence` dari `online`

Ini perubahan arsitektur yang menurut saya sekarang paling penting.

Jangan gunakan:

```rust
snapshot.online
```

langsung sebagai:

```rust
ChargerPresence
```

Minimal buat:

```rust
pub enum ChargerPresence {
    Online,
    Offline,
    Unknown,
}
```

tetapi tentukan berdasarkan **stable observation**, bukan satu read.

Contohnya:

```rust
struct PresenceTracker {
    stable: ChargerPresence,
    candidate: ChargerPresence,
    consecutive: u8,
}
```

Kemudian:

```text
online=false satu kali
       ↓
candidate Offline
       ↓
jangan langsung unplug
       ↓
read lagi
       ↓
false
       ↓
read lagi
       ↓
false
       ↓
Offline
```

Misalnya membutuhkan 2–3 pembacaan konsisten.

Karena sekarang satu:

```rust
snapshot.online == Some(false)
```

langsung mengubah policy.

---

# 7. Tetapi ada masalah yang lebih mendasar: `online` kemungkinan bukan indikator yang tepat

Saya justru ingin melihat implementasi:

```rust
CachedReader::is_plugged_in()
```

Karena dari gejala ini saya sangat curiga.

Anda perlu kirim kode:

```rust
impl CachedReader {
    pub fn is_plugged_in(...)
}
```

dan fungsi yang dipanggilnya sampai akhirnya membaca sysfs.

Misalnya apakah dia membaca:

```text
/sys/class/power_supply/usb/online
```

atau:

```text
/sys/class/power_supply/ac/online
```

atau:

```text
/sys/class/power_supply/main/online
```

atau kombinasi beberapa node.

Ini sangat penting.

---

# 8. Saya juga ingin melihat `read_charging_state()`

Karena controller Anda sekarang melakukan:

```rust
control::read_charging_state(...)
```

dan:

```rust
control::set_charging(...)
```

Tetapi log menunjukkan:

```text
2/5 succeeded
3 failed
```

Kita belum tahu **dua node mana yang berhasil**.

Itu sangat penting.

Tambahkan log sementara di `set_charging()`:

```rust
tracing::info!(
    "Charging control node result: path={} value={} result={:?}",
    path,
    value,
    result
);
```

atau paling tidak:

```text
SUCCESS /sys/.../xxx = 1
FAILED  /sys/.../yyy = 1
```

Saya ingin tahu apakah dua node yang berhasil adalah misalnya:

```text
main/charging_enabled
battery/battery_charging_enabled
```

sementara:

```text
usb/input_suspend
...
```

gagal.

Kalau begitu kita bisa menentukan **node mana yang benar-benar mengontrol charging pada perangkat Anda**.

---

# 9. Ada bug di verification yang perlu diperbaiki

Sekarang:

```rust
HardwareTarget::ChargingEnabled => {
    match control::read_charging_state(...) {
        Ok(ChargingState::Enabled) => {
            snapshot.online == Some(true)
        }
```

Ini terlalu ketat.

Anda sedang memverifikasi dua hal sekaligus:

```text
charging state == Enabled
AND
online == true
```

Padahal `read_charging_state()` dan `snapshot.online` adalah dua domain berbeda.

Verification hardware seharusnya:

```rust
Ok(ChargingState::Enabled) => true,
```

Sedangkan physical presence ditangani oleh `DecisionEngine`.

Jadi:

```rust
HardwareTarget::ChargingEnabled => {
    matches!(
        control::read_charging_state(...),
        Ok(control::ChargingState::Enabled)
    )
}
```

Jangan:

```rust
&& snapshot.online == Some(true)
```

Kalau tidak, Anda mencampur:

```text
hardware control verification
```

dengan:

```text
charger presence detection
```

---

# 10. `ChargingDisabled` verification juga masih mencampur dua hal

Sekarang:

```rust
let current_safe = snapshot.current_ma
    .is_some_and(|current| current <= 100);
```

kemudian:

```rust
Ok(control::ChargingState::Disabled) => current_safe,
```

Ini lebih masuk akal daripada sebelumnya, tetapi tetap ada potensi masalah unit `current_ma`.

Kalau `read_current_ma()` sebenarnya menghasilkan nilai yang tidak benar-benar mA, maka:

```rust
current <= 100
```

bisa salah.

Untuk sementara, saya sarankan verification hardware **hanya memverifikasi state charging node**:

```rust
HardwareTarget::ChargingDisabled => {
    matches!(
        control::read_charging_state(...),
        Ok(control::ChargingState::Disabled)
    )
}
```

Kemudian current digunakan sebagai telemetry/safety signal di decision engine, bukan sebagai syarat utama bahwa hardware write berhasil.

---

# 11. Ada bug kecil pada recovery verification

Ini:

```rust
if self.verification_failures >= MAX_VERIFICATION_RETRIES {
```

dengan:

```rust
VERIFY_DELAYS = [
    500ms,
    1s,
    2s,
];
```

berarti sequence:

```text
apply
 ↓
500ms verify #1
 ↓ fail
1s verify #2
 ↓ fail
2s verify #3
 ↓ fail
Failed
```

Itu sebenarnya oke.

Tetapi ketika `Failed`:

```rust
self.retry_at = Some(...);
```

dan monitor melakukan:

```rust
if hardware.retry_due(now) {
    hardware.force_apply = true;
}
```

Kemudian:

```rust
hardware.apply_target(decision.target);
```

Itu juga bisa mengulang partial write yang sama.

Jadi kalau akar masalahnya adalah:

```text
3/5 node Permission denied
```

retry 30s kemudian hanya akan menghasilkan:

```text
permission denied
permission denied
permission denied
...
```

Tidak akan memperbaiki masalah.

---

# 12. Hal paling mencurigakan justru `Permission denied`

Ini:

```text
/sys/class/power_supply/main/charging_enabled
Permission denied
```

dan:

```text
/sys/class/power_supply/battery/battery_charging_enabled
Permission denied
```

dan:

```text
/sys/class/power_supply/usb/input_suspend
Permission denied
```

**Padahal daemon Anda berjalan sebagai root.**

Ini bukan normal jika node tersebut memang writable oleh root.

Ada beberapa kemungkinan:

### A. SELinux

Root UID tidak otomatis berarti SELinux mengizinkan write.

Cek:

```sh
getenforce
```

dan:

```sh
dmesg | grep -i 'avc.*denied'
```

atau:

```sh
logcat -b all | grep -i 'avc.*denied'
```

### B. Node memang `0444`

Cek:

```sh
ls -l \
/sys/class/power_supply/main/charging_enabled \
/sys/class/power_supply/battery/battery_charging_enabled \
/sys/class/power_supply/usb/input_suspend
```

### C. Sysfs attribute menerima write tetapi kernel driver menolak

Coba manual sebagai root:

```sh
echo 1 > /sys/class/power_supply/main/charging_enabled
echo 1 > /sys/class/power_supply/battery/battery_charging_enabled
echo 0 > /sys/class/power_supply/usb/input_suspend
```

Kemudian:

```sh
echo $?
```

**Ini test yang sangat penting.**

Kalau manual juga:

```text
Permission denied
```

maka Rust controller Anda **bukan akar masalahnya**.

---

# 13. Bahkan saya ingin Anda melakukan satu eksperimen sederhana

Dengan charger terpasang dan baterai 77%, jalankan sebagai root:

```sh
for f in \
/sys/class/power_supply/main/charging_enabled \
/sys/class/power_supply/battery/battery_charging_enabled \
/sys/class/power_supply/usb/input_suspend
do
    echo "===== $f ====="
    ls -l "$f"
    cat "$f"
done
```

Kemudian:

```sh
echo 1 > /sys/class/power_supply/main/charging_enabled
echo $?

echo 1 > /sys/class/power_supply/battery/battery_charging_enabled
echo $?

echo 0 > /sys/class/power_supply/usb/input_suspend
echo $?
```

Lalu langsung:

```sh
for f in \
/sys/class/power_supply/main/charging_enabled \
/sys/class/power_supply/battery/battery_charging_enabled \
/sys/class/power_supply/usb/input_suspend
do
    echo "===== $f ====="
    cat "$f"
done
```

Kalau hasilnya misalnya:

```text
main/charging_enabled        -> Permission denied
battery/...                   -> Permission denied
usb/input_suspend             -> Permission denied
```

kita sudah tahu masalahnya berada **di layer kernel/SELinux/sysfs permission**, bukan Rust.

---

# 14. Tetapi dari kode controller, saya tetap akan mengubah 3 hal

## Perubahan 1 — partial write jangan dianggap synced candidate

Sekarang:

```rust
self.mark_apply_success(target, is_partial);
```

ubah semantik menjadi:

```rust
if res.failed == 0 {
    self.mark_apply_success(target, false);
    events.push(ControllerEvent::ApplySuccess(target));
} else {
    tracing::error!(
        "Partial hardware application: {}/{} succeeded",
        res.succeeded,
        res.attempted
    );

    self.mark_apply_failed();

    events.push(ControllerEvent::ApplyFailed);
}
```

Dengan kata lain:

> **2/5 bukan success.**

Karena charging control adalah operation yang seharusnya atomic secara logical.

Kalau sebagian node berhasil, hasil akhirnya tidak deterministic.

`HardwareEffect::Unknown` bagus untuk observability, tetapi **jangan treat partial write sebagai ApplySuccess**.

---

# 15. Perubahan 2 — verification charging jangan bergantung pada `online`

Ubah:

```rust
Ok(control::ChargingState::Enabled) => {
    snapshot.online == Some(true)
}
```

menjadi:

```rust
Ok(control::ChargingState::Enabled) => true,
```

dan disabled:

```rust
Ok(control::ChargingState::Disabled) => true,
```

Untuk sementara.

---

# 16. Perubahan 3 — jangan langsung `Offline` karena satu pembacaan

Sekarang:

```rust
let physically_offline = snapshot.online == Some(false);
```

Ini terlalu agresif.

Minimal:

```rust
let physically_offline = matches!(
    snapshot.online,
    Some(false)
);
```

memang sama secara logic, jadi **bukan sekadar ganti sintaks**.

Yang diperlukan adalah state tracker:

```text
Online
  ↓
online=false sekali
  ↓
Unknown
  ↓
online=false lagi
  ↓
Unknown
  ↓
online=false lagi
  ↓
Offline
```

Dan:

```text
Online
  ↓
online=false
  ↓
online=true
  ↓
Online
```

Tidak pernah masuk Offline.

---

# 17. Ada satu perubahan arsitektur yang saya sangat rekomendasikan

Pisahkan tiga konsep ini:

```text
ChargerPresence
```

```text
HardwareEffect
```

```text
HardwareSync
```

Karena saat ini mereka masih saling mempengaruhi.

Idealnya:

```text
                   ┌──────────────────┐
                   │ Battery Sensors  │
                   └────────┬─────────┘
                            │
             ┌──────────────┴──────────────┐
             ↓                             ↓
       Charger Presence              Battery Policy
             │                             │
             └──────────────┬──────────────┘
                            ↓
                     Desired Target
                            │
                            ↓
                    HardwareController
                            │
                 ┌──────────┴─────────┐
                 ↓                    ↓
             Apply result         Verify result
                 │                    │
                 └──────────┬─────────┘
                            ↓
                       Sync State
```

Jangan:

```text
hardware write
     ↓
online
     ↓
presence
     ↓
decision
     ↓
hardware target
     ↓
hardware write
```

karena itu feedback loop.

---

# 18. Untuk kasus Anda sekarang

Dengan:

```text
battery = 77%
temperature = 34°C
limit = 100%
thermal cutoff = false
```

**DecisionEngine seharusnya menghasilkan:**

```text
policy  = Charging
target  = ChargingEnabled
reason  = normal_charging
```

selama:

```text
enabled = true
charger benar-benar terpasang
sensor valid
```

Jadi kalau log Anda menunjukkan:

```text
ChargingEnabled -> ChargingDisabled
```

maka **jangan utak-atik scheduler dulu**.

Scheduler bahkan tidak menentukan target. Dia hanya menentukan kapan loop berikutnya berjalan.

Fokus sekarang adalah mencari kenapa:

```rust
engine.evaluate(...)
```

tidak lagi menghasilkan:

```rust
ChargingEnabled
```

---

## Yang paling saya butuhkan berikutnya

Kirim **dua kode ini**:

1. implementasi:

```rust
CachedReader::is_plugged_in()
```

termasuk fungsi sysfs yang dipanggilnya.

2. seluruh implementasi:

```rust
control::set_charging()
control::read_charging_state()
control::is_charging_enabled()
```

Terutama bagian yang menghasilkan daftar **5 write nodes**.

Dari log sekarang saya sudah cukup yakin ada **dua masalah berbeda**:

```text
A. `online=false` sedang diperlakukan sebagai physical unplug terlalu cepat
B. 2/5 partial sysfs write masih dianggap ApplySuccess
```

dan kemungkinan besar **A adalah penyebab langsung `ChargingEnabled -> ChargingDisabled`**, sementara **B adalah masalah hardware-control yang membuat state charging tidak stabil sejak awal**.

Yang juga sangat penting: **jangan ubah `DecisionEngine` lagi sebelum kita lihat `is_plugged_in()`**. Konfigurasi 77%/34°C Anda sendiri sudah jelas tidak akan memicu `LimitReached` atau `ThermalCutoff`.


Ya. Dari tiga bagian kode yang sekarang kamu kirim, **ada beberapa masalah yang cukup kuat untuk menjelaskan gejala “charger aktif beberapa detik lalu dianggap terputus”**, meskipun konfigurasi kamu sebenarnya tidak meminta cutoff.

Dengan kondisi:

```ini
enabled = true
charge_limit = 100
thermal_cutoff = false
max_temp_dc = 420
thermal_resume_hysteresis_dc = 30
```

dan baterai **77%, 34°C**, secara logika target seharusnya:

```text
HardwareTarget::ChargingEnabled
```

dan **tidak boleh ada alasan dari charge limit maupun thermal untuk mematikan charging**.

Yang paling mencurigakan justru ada di jalur **apply → verify → reconcile**.

---

# 1. Masalah paling besar: `reconcile()` terlalu agresif

Ini bagian yang sangat mencurigakan:

```rust
pub fn reconcile(&mut self) -> Vec<ControllerEvent> {
    let mut events = Vec::new();
    if self.sync != SyncState::Synced {
        return events;
    }

    match self.applied_target {
        HardwareTarget::ChargingEnabled => {
            if let Ok(control::ChargingState::Disabled)
                | Ok(control::ChargingState::Mixed)
                = control::read_charging_state(...)
            {
                self.sync = SyncState::Unknown;
                self.force_apply = true;
                events.push(...);
            }
        }
```

Artinya:

```text
charging enabled
       ↓
read_charging_state()
       ↓
Disabled / Mixed
       ↓
anggap external modification
       ↓
force_apply = true
```

Masalahnya adalah `read_charging_state()` **menggabungkan node yang berbeda semantik**:

```text
charging_enabled
input_suspend
```

Keduanya tidak selalu merupakan indikator yang identik terhadap **actual charging state**.

---

# 2. Kamu sedang mencampur "charging control state" dengan "actual charging state"

Ini desain yang perlu dibedakan.

Misalnya:

```text
battery/charging_enabled = 1
battery/input_suspend    = 0
```

Itu memang:

```text
charging control = enabled
input not suspended
```

Tetapi belum tentu:

```text
battery benar-benar sedang menerima arus
```

Sebaliknya:

```text
charging_enabled = 1
input_suspend    = 0
current_now      = 0
```

bisa terjadi ketika:

* battery sudah full
* charger sedang renegosiasi
* PMIC sedang transition
* thermal/power management sementara menghentikan charging
* USB PD/QC berubah state
* vendor kernel melakukan automatic regulation

Jadi **`current_now == 0` atau perubahan sementara pada node control tidak boleh langsung dianggap external modification.**

Ini sangat penting untuk `reconcile()`.

---

# 3. Lebih parah lagi: `Mixed` langsung dianggap external modification

Kamu punya:

```rust
if let Ok(control::ChargingState::Disabled)
    | Ok(control::ChargingState::Mixed)
    = control::read_charging_state(...)
```

Jadi:

```text
Mixed
```

langsung dianggap:

```text
orang lain mematikan charger
```

Padahal `Mixed` pada desain kamu bisa muncul hanya karena:

```text
battery/charging_enabled = 1
main/charging_enabled    = 0
```

atau node suspend berbeda.

Padahal belum tentu kontrol sebenarnya berubah.

Dan fungsi `read_charging_state()` sendiri mengatakan:

```rust
// Consensus
// Priority
// Mixed
```

Tetapi `reconcile()` memperlakukan `Mixed` sebagai bukti kuat external modification.

**Ini terlalu agresif.**

---

# 4. Ada masalah penting di `set_charging()`

Ini:

```rust
Ok(res) if res.succeeded > 0 => {
    ...
    let is_partial = res.failed > 0;
    self.mark_apply_success(target, is_partial);
    events.push(ControllerEvent::ApplySuccess(target));
}
```

berarti:

```text
1 sukses
3 gagal
```

tetap menghasilkan:

```text
ApplySuccess
```

walaupun kamu sudah membuat:

```rust
partial_failure()
```

di `ChargingWriteResult`.

Jadi misalnya:

```text
battery/charging_enabled = sukses
main/charging_enabled    = gagal
battery/input_suspend    = sukses
some_vendor_node         = gagal
```

hasil:

```text
succeeded = 2
failed    = 2
```

tetap:

```rust
ApplySuccess
```

dan kemudian:

```rust
sync = Pending
verification = ...
```

Padahal hardware sebenarnya **belum sinkron sepenuhnya**.

Memang kamu set:

```rust
HardwareEffect::Unknown
```

untuk partial failure, tetapi state machine tetap menganggap operasi tersebut sebagai apply success.

Ini inkonsisten.

---

# 5. Saya akan ubah ini menjadi `partial != success`

Bagian:

```rust
Ok(res) if res.succeeded > 0 => {
```

sebaiknya **jangan** berarti success.

Lebih aman:

```rust
Ok(res) if res.all_succeeded() => {
    tracing::info!(
        "Hardware charging set to {}: {}/{} nodes succeeded",
        enable,
        res.succeeded,
        res.attempted
    );

    // persist ownership...

    self.mark_apply_success(target, false);
    events.push(ControllerEvent::ApplySuccess(target));
}
```

Kemudian:

```rust
Ok(res) if res.partial_failure() => {
    tracing::error!(
        "Hardware charging partially applied: {}/{} succeeded, {} failed",
        res.succeeded,
        res.attempted,
        res.failed
    );

    self.mark_apply_failed();

    events.push(ControllerEvent::ApplyFailed);
}
```

dan:

```rust
Ok(res) if res.all_failed() => {
    ...
}
```

Walaupun `all_failed()` sekarang secara desain sebenarnya dikembalikan sebagai `Err`, struktur ini jauh lebih jelas.

---

# 6. Bug lain: ownership sudah dianggap Owned sebelum apply berhasil

Ini juga penting:

```rust
self.ownership = Ownership::Owned {
    original_charging: original,
};
```

dilakukan **sebelum**:

```rust
control::set_charging(enable, ...)
```

Jadi sequence-nya:

```text
read original
    ↓
persist Acquiring
    ↓
ownership = Owned
    ↓
set charging
    ↓
FAILED
```

Sekarang controller tetap:

```text
Ownership::Owned
```

walaupun target tidak pernah berhasil diterapkan.

Ini tidak langsung menjelaskan charger putus setelah beberapa detik, tetapi state machine menjadi rancu.

Lebih aman ownership mempunyai fase:

```text
NotOwned
Acquiring
Owned
Releasing
```

dan `Ownership::Owned` baru diberikan setelah **all_succeeded()**.

---

# 7. `verify()` untuk ChargingEnabled juga terlalu ketat

Sekarang:

```rust
HardwareTarget::ChargingEnabled => {
    match control::read_charging_state(...) {
        Ok(control::ChargingState::Enabled) => {
            snapshot.online == Some(true)
        }

        Ok(control::ChargingState::Disabled) => false,

        Ok(control::ChargingState::Mixed) => false,

        ...
    }
}
```

Jadi verification success membutuhkan **dua hal**:

```text
control state == Enabled
AND
snapshot.online == Some(true)
```

Ini sebenarnya masuk akal, tetapi masalahnya adalah `snapshot.online` kemungkinan dibaca dari:

```rust
is_plugged_in()
```

yang hanya mencari:

```text
*/online == 1
```

Jika vendor Android kamu memiliki beberapa power-supply node dan node yang dianggap "online" berubah ketika PMIC melakukan transition, maka:

```text
charging_enabled = 1
online = 0
```

selama sesaat akan menyebabkan verification gagal.

Kemudian:

```text
500 ms
1 sec
2 sec
```

gagal tiga kali:

```rust
sync = SyncState::Failed;
```

dan controller masuk:

```rust
retry_at = now + 30 sec
```

---

# 8. Tetapi ada hal yang sangat penting: verification failure TIDAK mematikan charger

Ini perlu dipahami.

Kode kamu:

```rust
self.sync = SyncState::Failed;
self.verification = None;
self.force_apply = true;
self.retry_at = Some(...);
```

**tidak melakukan:**

```rust
set_charging(false)
```

Jadi kalau charger secara fisik benar-benar berhenti setelah beberapa detik, penyebab langsungnya kemungkinan **bukan `verification_failed()`**.

Yang harus dicari adalah siapa yang menghasilkan:

```rust
HardwareTarget::ChargingDisabled
```

karena controller ini sendiri tidak mengubah target menjadi disabled akibat verification failure.

---

# 9. Saya justru sangat curiga terhadap `DecisionEngine`

Dengan konfigurasi:

```ini
charge_limit = 100
```

dan battery:

```text
77%
```

decision seharusnya:

```text
ChargingEnabled
```

Tetapi kamu perlu memastikan hysteresis di `DecisionEngine` tidak salah interpretasi.

Misalnya kalau ada log:

```text
capacity=77
limit=100
resume_limit=?
decision=ChargingDisabled
```

maka masalah **bukan HardwareController**.

HardwareController hanya melakukan:

```rust
set_desired_target(ChargingDisabled)
```

lalu:

```rust
set_charging(false)
```

dan charger benar-benar mati.

---

# 10. Ada bug lain di `read_current_ma()`

Ini:

```rust
let ma = match current_fd.config.unit {
    CurrentUnit::MicroAmp => (value / 1000) as i32,
    CurrentUnit::MilliAmp => value as i32,
};
```

dan kemudian:

```rust
let better = highest_prio
    .map(|p| current_fd.config.priority > p)
    .unwrap_or(true);
```

Kamu memilih node **berdasarkan priority saja**, bukan berdasarkan validitas nilai.

Contoh:

```text
battery/current_now = 0      priority 100
bms/current_now     = 1250   priority 90
```

hasilnya:

```text
current_ma = 0
```

karena priority 100 menang.

Ini berbeda dengan versi lama kamu yang melakukan:

```rust
if value == 0 {
    continue;
}
```

Memang keputusan menghapus "zero heuristic" itu benar secara prinsip, karena `0 mA` bisa valid.

Tetapi sekarang kamu membutuhkan **semantik pemilihan sensor yang lebih benar**, bukan sekadar priority.

Kalau scheduler menggunakan current untuk menentukan charging state, ini bisa menyebabkan keputusan salah.

---

# 11. Bahkan `read_current_ma()` bisa menghasilkan nilai yang salah karena node aktif

Kamu memiliki:

```text
battery/current_now
bms/current_now
main/current_now
usb/current_now
```

dan semua dibaca.

Tetapi current node-node tersebut **tidak selalu mengukur hal yang sama**.

Misalnya:

```text
battery/current_now = -200
usb/current_now     = 1500
```

Tidak berarti kamu boleh memilih salah satunya hanya karena priority.

Yang harus ditentukan oleh profile adalah:

```text
sensor ini mengukur battery current
sensor ini mengukur input current
sensor ini fallback
```

Kalau tujuanmu menentukan apakah **battery sedang charging**, sensor paling penting adalah battery-side current atau status charging, bukan sembarang `current_now`.

---

# 12. `CachedReader` juga punya masalah lifecycle FD

Ini:

```rust
if stale_fd {
    tracing::trace!(
        "One or more current FDs became stale; \
         waiting for scheduled rescan."
    );
}
```

kemudian kamu **tidak memaksa rescan**.

Misalnya:

```text
t=0    open current_now
t=1    PMIC/vendor node berubah
t=1    FD stale
t=2    read gagal
t=3    read gagal
t=4    read gagal
t=5    baru rescan
```

Jadi selama 5 detik data sensor bisa hilang.

Untuk Android power_supply, saya justru lebih nyaman dengan pendekatan:

```text
normal polling:
    cached FD

read failure:
    close FD
    mark stale

next polling:
    immediate rescan
```

daripada selalu menunggu 5 detik.

Misalnya:

```rust
if stale_fd {
    self.current_fds.clear();
    self.next_current_rescan = self.clock.now();
}
```

dan online:

```rust
if stale_fd {
    self.online_fds.clear();
    self.next_online_rescan = self.clock.now();
}
```

Ini tidak perlu `File::open()` langsung di polling; cukup menjadwalkan rescan segera.

---

# 13. Yang paling saya curigai untuk kasusmu

Dengan kondisi:

```text
77%
34°C
charge_limit=100
thermal_cutoff=false
```

saya urutkan kemungkinan:

### Kemungkinan #1 — DecisionEngine menghasilkan `ChargingDisabled`

Ini yang **pertama harus diperiksa**.

Karena kalau terjadi:

```text
DecisionEngine
    ↓
ChargingDisabled
    ↓
HardwareController.apply_target()
    ↓
set_charging(false)
```

maka gejalanya persis:

> charger jalan beberapa detik lalu terputus.

---

### Kemungkinan #2 — `reconcile()` mendeteksi `Mixed`

Misalnya:

```text
charging_enabled = 1
main/charging_enabled = 0
```

atau node lain berbeda.

Kemudian:

```text
Mixed
 ↓
ExternalModificationDetected
 ↓
force_apply
```

Ini menyebabkan controller masuk siklus re-sync.

---

### Kemungkinan #3 — `snapshot.online` berubah false

Verification:

```rust
ChargingEnabled
```

membutuhkan:

```rust
snapshot.online == Some(true)
```

Jika `online` sesaat false, verification gagal.

---

### Kemungkinan #4 — current sensor salah

Scheduler bisa melihat:

```text
current = 0
```

atau nilai yang salah karena priority node.

Kemudian DecisionEngine dapat mengambil keputusan yang tidak seharusnya.

---

# 14. Ada satu perubahan desain yang saya sangat rekomendasikan

Jangan gunakan:

```text
ChargingState
```

untuk sekaligus merepresentasikan:

```text
hardware control state
```

dan:

```text
actual charging behavior
```

Pisahkan.

Misalnya:

```rust
pub enum ControlState {
    Enabled,
    Disabled,
    Mixed,
    Unknown,
}

pub enum ChargingBehavior {
    Charging,
    NotCharging,
    Discharging,
    Full,
    Unknown,
}
```

Kemudian:

```text
ControlState
    ↓
charging_enabled / input_suspend
```

sedangkan:

```text
ChargingBehavior
    ↓
BatteryStatus
current_ma
online
```

Dengan begitu:

```text
ControlState::Enabled
ChargingBehavior::NotCharging
```

**tidak otomatis dianggap error.**

Contoh valid:

```text
charging_enabled = 1
input_suspend = 0
battery status = NotCharging
current = 0
```

bisa saja terjadi karena baterai sudah penuh atau PMIC sedang tidak mengambil arus.

---

# 15. Untuk konfigurasi kamu, expected state harus seperti ini

Saat daemon mulai:

```text
capacity       = 77%
temperature    = 34.0°C
thermal        = disabled
charge_limit   = 100%
```

Decision:

```text
ChargingEnabled
```

Hardware:

```text
charging_enabled = 1
input_suspend    = 0
```

Ownership:

```text
Owned
```

Verification:

```text
control state = Enabled
online        = true
```

Sync:

```text
Synced
```

Kemudian scheduler/reconcile:

```text
77%
34°C
target = ChargingEnabled

NO ACTION
```

**Tidak boleh ada `set_charging(false)` sama sekali.**

---

# 16. Yang perlu kamu cari di log sekarang

Ini yang paling penting. Jalankan daemon dengan logging maksimal dan cari urutan seperti:

```text
desired target
Apply
Verification
ExternalModification
Decision
```

Saya terutama ingin melihat apakah ada log semacam:

```text
Hardware desired target: ChargingEnabled -> ChargingDisabled
```

atau:

```text
External hardware modification detected
```

atau:

```text
Hardware verification failed
```

atau:

```text
capacity=77 ... decision=ChargingDisabled
```

Kalau ada:

```text
Hardware desired target: ChargingEnabled -> ChargingDisabled
```

**kita sudah tahu masalahnya bukan `control.rs`, melainkan DecisionEngine/scheduler yang mengubah target.**

Kalau ada:

```text
External hardware modification detected
```

maka fokus ke `reconcile()`.

Kalau ada:

```text
Verification failed
```

tetapi **tidak ada `ChargingDisabled`**, berarti charger berhenti bukan karena controller mematikannya dan kita perlu melihat siapa di luar daemon yang mengubah sysfs.

---

## Kesimpulan

Dari kode yang kamu kirim, saya **belum melihat bug yang secara langsung mengatakan "setelah beberapa detik matikan charger"**. Justru itu petunjuk penting.

Yang paling bermasalah saat ini adalah:

1. **`DecisionEngine` perlu diperiksa**, karena hanya dia yang kemungkinan menghasilkan `ChargingDisabled` berdasarkan 77%/34°C.
2. `reconcile()` **terlalu agresif memperlakukan `Mixed` sebagai external modification**.
3. `ChargingWriteResult` partial failure masih diperlakukan sebagai `ApplySuccess`.
4. Ownership ditandai `Owned` sebelum apply benar-benar berhasil.
5. `ChargingState` mencampur **control state** dengan **actual charging behavior**.
6. `CachedReader` menunggu sampai 5 detik ketika FD stale.
7. Pemilihan `current_now` masih bisa salah karena priority mengalahkan konteks sensor.
8. `verify(ChargingEnabled)` terlalu bergantung pada `online == true` sebagai syarat absolut.

**Namun untuk kasus spesifikmu (77%, 34°C, thermal cutoff false, limit 100), saya paling ingin melihat `decision.rs` + scheduler yang memanggil `set_desired_target()`**, karena di situlah kemungkinan target berubah menjadi `ChargingDisabled`.

Kirim `decision.rs` dan bagian scheduler/main loop yang memanggil `needs_apply()`, `apply_target()`, `verify()`, dan `reconcile()`. Dari empat file itu kita bisa telusuri **urutan persis kenapa charger hidup beberapa detik lalu dimatikan**, bukan sekadar menebak dari hardware layer.


Ya, dari tiga file ini sudah terlihat beberapa masalah yang **bisa menjelaskan charger hidup beberapa detik lalu mati**. Dengan konfigurasi:

```ini
enabled = true
charge_limit = 100
thermal_cutoff = false
max_temp_dc = 420
thermal_resume_hysteresis_dc = 30
```

dan kondisi:

* battery = **77%**
* temperature = **34°C**
* thermal cutoff = **false**

maka secara policy **seharusnya target tetap `ChargingEnabled`**. Jadi kalau charger benar-benar diputus beberapa detik setelah start, saya justru curiga pada **hardware controller / verification / reconcile / pembacaan online**, bukan pada limit 100% atau thermal.

## 1. Ada bug penting di `apply_charging()`

Bagian ini:

```rust
Ok(res) if res.succeeded > 0 => {
    ...
    let is_partial = res.failed > 0;
    self.mark_apply_success(target, is_partial);
    events.push(ControllerEvent::ApplySuccess(target));
}
```

Masalahnya: **partial write dianggap `ApplySuccess`**.

Misalnya profile Anda punya:

```text
battery/charging_enabled
main/charging_enabled
battery/input_suspend
```

dan hasilnya:

```text
3 attempted
2 succeeded
1 failed
```

Anda tetap melakukan:

```rust
mark_apply_success(...)
```

dan controller menganggap hardware sudah diterapkan.

Memang `effect` dibuat `Unknown`, tetapi:

```rust
self.sync = SyncState::Pending;
```

lalu verification berjalan.

Ini sendiri belum tentu penyebab charger mati, tetapi state machine-nya masih terlalu permisif.

### Saya sarankan ubah menjadi:

```rust
match control::set_charging(enable, &self.profile, &*self.hw_io) {
    Ok(res) if res.all_succeeded() => {
        tracing::info!(
            "Hardware charging set to {}: {}/{} nodes succeeded",
            enable,
            res.succeeded,
            res.attempted
        );

        if let Ownership::Owned { original_charging } = self.ownership {
            let record = crate::persistence::ownership::OwnershipRecord {
                version: 1,
                generation: self.generation,
                original_charging,
                target_charging: enable,
                phase: crate::persistence::ownership::OwnershipPhase::Owned,
            };

            if let Err(e) =
                crate::persistence::ownership::save_persistent_ownership(
                    &record,
                    &*self.pers_io,
                )
            {
                tracing::error!(
                    "Failed to persist hardware ownership phase Owned: {}",
                    e
                );
            }
        }

        self.mark_apply_success(target, false);
        events.push(ControllerEvent::ApplySuccess(target));
    }

    Ok(res) => {
        tracing::error!(
            "Charging control partially/fully failed: \
             {}/{} succeeded, {} failed",
            res.succeeded,
            res.attempted,
            res.failed
        );

        self.mark_apply_failed();
        events.push(ControllerEvent::ApplyFailed);
    }

    Err(e) => {
        tracing::error!(
            "Failed to set charging={} : {}",
            enable,
            e
        );

        self.mark_apply_failed();
        events.push(ControllerEvent::ApplyFailed);
    }
}
```

**Partial write harus dianggap gagal**, bukan sukses dengan `Unknown`.

---

# 2. Tetapi ada masalah yang lebih mencurigakan: `online`

Ini bagian paling penting:

```rust
HardwareTarget::ChargingEnabled => {
    match control::read_charging_state(...) {
        Ok(control::ChargingState::Enabled) => {
            snapshot.online == Some(true)
        }
```

Artinya:

> Charging node menunjukkan enabled **belum cukup**. `snapshot.online` juga HARUS `Some(true)`.

Jadi kalau:

```text
charging_enabled = 1
input_suspend = 0
```

tetapi:

```text
online = false
```

maka verification dianggap gagal.

Setelah tiga kali:

```text
500 ms
1 sec
2 sec
```

controller masuk:

```rust
SyncState::Failed
```

dan:

```rust
self.force_apply = true;
self.retry_at = now + 30s;
```

**Ini sangat cocok dengan gejala "jalan beberapa detik lalu terputus".**

Timeline-nya kira-kira:

```text
t=0
daemon apply charging=1
        ↓
charging hidup
        ↓
t=0.5s verification
        ↓
online != true
        ↓
FAILED

t≈1.5s verification
        ↓
FAILED

t≈3.5s verification
        ↓
FAILED
        ↓
SyncState::Failed
```

Setelah itu controller tidak menganggap hardware synchronized.

---

# 3. Lebih parah lagi: `is_plugged_in()` Anda punya asumsi yang mungkin salah di Android

Anda menggunakan:

```rust
for config in self.profile.sensor.online_nodes {
    match File::open(config.path) {
        ...
    }
}
```

kemudian:

```rust
if value.trim() == "1" {
    return Ok(true);
}
```

Secara teori bagus.

Tetapi pada Android/vendor kernel, `online` **tidak selalu merepresentasikan "charger sedang memasok baterai"** dengan cara yang konsisten.

Contohnya bisa ada:

```text
usb/online = 1
ac/online = 1
battery/online = ...
main/online = ...
```

dan setelah Anda mengubah:

```text
charging_enabled
input_suspend
```

status power_supply dapat mengalami transisi sementara.

Akibatnya:

```text
charging_enabled = 1
online = 0
```

bisa terjadi sementara, padahal charger sebenarnya masih terpasang.

Jadi verification Anda terlalu ketat.

---

# 4. `ChargingEnabled` jangan diverifikasi berdasarkan `online`

Saya akan ubah:

```rust
Ok(control::ChargingState::Enabled) => {
    snapshot.online == Some(true)
}
```

menjadi:

```rust
Ok(control::ChargingState::Enabled) => true,
```

atau lebih baik:

```rust
Ok(control::ChargingState::Enabled) => {
    match snapshot.online {
        Some(true) => true,
        Some(false) => {
            tracing::warn!(
                "Charging nodes enabled but power_supply reports offline"
            );
            false
        }
        None => {
            tracing::warn!(
                "Charging nodes enabled but online state is unknown"
            );
            true
        }
    }
}
```

Tetapi untuk kasus Anda saya malah lebih memilih **memisahkan hardware state dengan charger presence**.

### Verification hardware:

```rust
Ok(control::ChargingState::Enabled) => true,
```

### Presence:

`online` dipakai oleh **decision engine**, bukan sebagai syarat bahwa write `charging_enabled=1` berhasil.

Ini penting secara arsitektur:

```text
                 ┌──────────────────┐
                 │ Hardware control │
                 └────────┬─────────┘
                          │
                    charging_enabled
                          │
                          ▼
                   verification
                          │
                     ENABLED
```

sedangkan:

```text
USB/AC online
      │
      ▼
 charger presence
      │
      ▼
 decision engine
```

Jangan campur kedua konsep itu.

---

# 5. Ada bug lain di `CachedReader::read_current_ma()`

Versi lama Anda melakukan:

```rust
if value == 0 {
    continue;
}
```

Versi baru **tidak lagi melakukan itu**.

Sekarang:

```rust
let ma = match current_fd.config.unit {
    CurrentUnit::MicroAmp => (value / 1000) as i32,
    CurrentUnit::MilliAmp => value as i32,
};
```

Jadi node dengan priority tinggi yang nilainya:

```text
0
```

tetap dipilih.

Contoh:

```text
battery/current_now = 0       priority 100
bms/current_now     = 1500    priority 90
```

hasil Anda:

```text
current = 0
```

padahal node priority 100 mungkin tidak aktif/valid.

Ini bisa menyebabkan decision engine membaca:

```text
current = 0 mA
```

dan kemudian salah mengambil keputusan.

Namun **jangan kembali ke `value == 0 { continue; }` secara buta**. Nol adalah nilai valid ketika:

* baterai penuh,
* charging berhenti,
* bypass,
* idle.

Yang benar adalah memilih node berdasarkan **validity + availability + priority**, bukan menganggap zero invalid.

---

# 6. Masalah lebih besar di `read_charging_state()`

Saya kurang suka algoritma ini:

```rust
let all_enabled = observations
    .iter()
    .all(|n| n.state == ChargingNodeState::Enabled);

if all_enabled {
    return Ok(ChargingState::Enabled);
}
```

Karena Anda menggabungkan:

```text
charging_enabled
```

dan:

```text
input_suspend
```

sebagai satu jenis state.

Misalnya:

```text
battery/charging_enabled = 1
main/charging_enabled    = 1
battery/input_suspend    = 0
```

memang hasilnya Enabled.

Tetapi:

```text
battery/charging_enabled = 1
main/charging_enabled    = 0
battery/input_suspend    = 0
```

Anda menganggap:

```text
Mixed
```

atau mencoba resolve dengan priority.

Padahal pada vendor Android, node-node itu bisa mempunyai **fungsi berbeda**, bukan redundant control yang harus selalu identik.

Saya lebih menyarankan profile mendefinisikan:

```rust
charging_nodes
suspend_nodes
```

dan verification menentukan state dari **primary control node**, bukan consensus seluruh node.

---

# 7. `set_charging()` juga berpotensi menjadi sumber masalah hardware

Ini menurut saya perlu diperiksa paling serius.

Anda melakukan:

```rust
for node in profile.control.charging_nodes {
    write(node, charge_val)
}

for node in profile.control.suspend_nodes {
    write(node, suspend_val)
}
```

Ketika enable:

```text
charging_enabled = 1
input_suspend = 0
```

Secara konsep masuk akal.

Tetapi pada kernel/vendor tertentu, **tidak semua node boleh dikendalikan bersamaan**.

Contohnya:

```text
battery/charging_enabled
```

mungkin merupakan node utama.

Sedangkan:

```text
main/charging_enabled
```

bisa merupakan alias/representasi lain.

Dan:

```text
input_suspend
```

bisa punya mekanisme berbeda.

Jadi Anda harus tahu **profile Redmi/S8/etc yang sedang dites ini sebenarnya node apa saja**.

---

# 8. Saya justru curiga ada target yang berubah ke `ChargingDisabled`

Karena konfigurasi Anda:

```ini
charge_limit = 100
```

dan:

```text
capacity = 77
```

maka seharusnya:

```text
77 < 100
```

→ `ChargingEnabled`.

Suhu:

```text
34°C = 340 dc
```

sedangkan:

```text
max_temp_dc = 420
```

→ belum thermal cutoff.

Dan:

```ini
thermal_cutoff = false
```

→ thermal policy tidak boleh mematikan charging.

Jadi kalau charger benar-benar **mati**, saya ingin melihat apakah controller sebenarnya mendapat:

```text
HardwareTarget::ChargingDisabled
```

setelah beberapa detik.

Tambahkan log **sebelum** `apply_target()`:

```rust
tracing::warn!(
    "HARDWARE APPLY: target={:?}, desired={:?}, applied={:?}, \
     sync={:?}, force_apply={}, ownership={:?}",
    target,
    self.desired_target,
    self.applied_target,
    self.sync,
    self.force_apply,
    self.ownership,
);
```

dan terutama di:

```rust
set_desired_target()
```

Anda sudah punya:

```rust
tracing::debug!(
    "Hardware desired target: {:?} -> {:?}",
    self.desired_target,
    target
);
```

ubah sementara menjadi `info`:

```rust
tracing::info!(
    "TARGET CHANGE: {:?} -> {:?}",
    self.desired_target,
    target
);
```

Kalau log menunjukkan:

```text
TARGET CHANGE: ChargingEnabled -> ChargingDisabled
```

berarti **controller hardware bukan akar masalah**. Decision engine/scheduler yang harus dibedah.

---

# 9. Ada satu bug ownership yang juga harus diperbaiki

Saat:

```rust
self.ownership == Ownership::NotOwned
```

Anda membaca:

```rust
let original = control::is_charging_enabled(...)
```

kemudian:

```rust
self.ownership = Ownership::Owned {
    original_charging: original,
};
```

Baru kemudian:

```rust
set_charging(enable)
```

Ini benar secara umum.

Tetapi kalau:

```text
original = false
target = true
```

dan kemudian terjadi:

```text
partial write
```

Anda sudah menganggap ownership berhasil diambil:

```rust
self.ownership = Owned
```

padahal hardware belum benar-benar berada pada target.

Ini alasan lain mengapa partial write harus **langsung gagal**.

---

# 10. `release_ownership()` juga bermasalah

Ini:

```rust
self.invalidate_verification();
```

di awal `release_ownership()` menaikkan:

```rust
generation
```

kemudian Anda membuat record:

```rust
generation: self.generation,
```

Tidak fatal.

Tetapi kalau `release_ownership()` dipanggil karena decision engine mengeluarkan:

```text
Unmanaged
```

maka:

```rust
control::set_charging(original_charging)
```

akan langsung mengembalikan hardware ke state sebelum daemon mengambil ownership.

Kalau original state ternyata:

```text
false
```

maka gejalanya persis:

```text
daemon start
↓
charger ON
↓
controller mendapat Unmanaged
↓
release_ownership()
↓
set_charging(false)
↓
charger OFF
```

Jadi sekali lagi, **log target sangat penting**.

---

# 11. Ada masalah pada `reconcile()`

Anda melakukan:

```rust
if self.sync != SyncState::Synced {
    return events;
}
```

Kemudian:

```rust
if let Ok(ChargingState::Disabled | Mixed) = read_charging_state(...)
```

langsung:

```rust
self.sync = SyncState::Unknown;
self.force_apply = true;
```

Ini berarti external modification dianggap terjadi bila node terbaca:

```text
Mixed
```

Padahal `Mixed` bisa saja **normal untuk konfigurasi vendor tertentu**, terutama karena Anda memasukkan beberapa node yang semantik/fungsinya berbeda.

Saya sarankan untuk sementara **jangan anggap `Mixed` sebagai external modification**.

Gunakan:

```rust
if let Ok(control::ChargingState::Disabled) =
    control::read_charging_state(...)
{
    ...
}
```

Untuk `Mixed`, log saja:

```rust
tracing::warn!(
    "Charging state is mixed; not treating as external modification"
);
```

sampai hierarchy node sudah benar-benar tervalidasi.

---

# 12. Saya akan memperbaiki verification menjadi seperti ini

Untuk `ChargingEnabled`:

```rust
HardwareTarget::ChargingEnabled => {
    match control::read_charging_state(
        &self.profile,
        &*self.hw_io,
    ) {
        Ok(control::ChargingState::Enabled) => true,

        Ok(control::ChargingState::Disabled) => {
            tracing::warn!(
                "Verification: charging control is disabled"
            );
            false
        }

        Ok(control::ChargingState::Mixed) => {
            tracing::warn!(
                "Verification: charging control state is mixed"
            );
            false
        }

        Ok(control::ChargingState::Unknown) | Err(_) => {
            tracing::warn!(
                "Verification: charging state unavailable"
            );
            false
        }
    }
}
```

**Tidak menggunakan `snapshot.online`.**

Lalu `online` tetap dipakai untuk menentukan apakah charger sedang terpasang.

---

# 13. Untuk konfigurasi Anda, state yang saya harapkan

Dengan:

```text
capacity = 77%
temperature = 340 dc
charge_limit = 100
thermal_cutoff = false
```

expected:

```text
Sensor
 ├─ capacity      = 77
 ├─ temperature   = 340
 └─ online        = true

Decision
 └─ target        = ChargingEnabled

Hardware
 ├─ charging      = enabled
 └─ sync          = Pending

Verification
 └─ charging      = Enabled

Controller
 └─ sync          = Synced
```

Setelah itu:

```text
reconcile()
```

tidak boleh menghasilkan:

```text
ExternalModificationDetected
```

dan target **tidak boleh berubah ke `ChargingDisabled`**.

---

## Yang paling saya curigai sekarang

Urutan kecurigaan saya:

**① `snapshot.online` menyebabkan verification gagal**
→ sangat mungkin karena Anda menjadikannya syarat `ChargingEnabled`.

**② Decision engine mengubah target ke `ChargingDisabled`**
→ perlu dilihat log target transition.

**③ `reconcile()` membaca `Mixed` lalu memaksa resync**
→ sangat mungkin jika profile mempunyai beberapa control node.

**④ Profile menulis terlalu banyak control node sekaligus**
→ terutama kombinasi `charging_enabled` + `input_suspend`.

**⑤ CachedReader current/online memilih node yang salah**
→ perlu diperbaiki, tetapi kemungkinan bukan penyebab langsung charger mati beberapa detik setelah start.

---

### Yang saya perlukan untuk memastikan akar masalah

Jalankan daemon dengan logging debug/trace dan kirim **log dari sekitar 10 detik sejak daemon start sampai charger mati**. Terutama cari baris:

```text
TARGET CHANGE
HARDWARE APPLY
Hardware charging set
Hardware verification
Verification failed
External hardware modification
Charging nodes
online
current
```

Dan yang paling penting, kirim juga **HardwareProfile yang sedang digunakan**, khusus bagian:

```rust
profile.control.charging_nodes
profile.control.suspend_nodes
profile.sensor.online_nodes
profile.sensor.current_nodes
```

Dari kode yang Anda kirim sekarang, saya **belum akan menyalahkan konfigurasi `charge_limit=100` atau thermal**. Secara matematis kondisi 77% / 34°C memang seharusnya tetap `ChargingEnabled`. Yang perlu kita buktikan sekarang adalah **siapa yang mematikan charger: decision engine, verification/reconcile, atau write ke sysfs yang memang menyebabkan kernel memutus charging**.


Ya. Dengan `profile.rs` ini, arsitektur yang Anda bangun sudah jauh lebih konsisten, tetapi setelah melihat **`reader.rs` + `control.rs` + `controller.rs` + `profile.rs` secara bersamaan**, masih ada beberapa masalah penting yang menurut saya perlu diperbaiki sebelum dianggap robust.

Yang paling penting justru bukan `profile.rs`-nya, melainkan **semantik ownership + partial write + restore** di `controller.rs`.

## 1. Masalah paling serius: partial restore dianggap sukses

Di `release_ownership()`:

```rust
match control::set_charging(original_charging, &self.profile, &*self.hw_io) {
    Ok(res) if res.succeeded > 0 => {
        ...
        clear_persistent_ownership(&*self.pers_io);

        self.ownership = Ownership::NotOwned;
        ...
    }
```

Ini masih salah.

`set_charging()` sudah dengan benar membedakan:

* `all_succeeded()`
* `partial_failure()`
* `all_failed()`

Tetapi controller kemudian mengabaikan perbedaan itu:

```rust
Ok(res) if res.succeeded > 0
```

Artinya:

```text
3 attempted
2 succeeded
1 failed
```

tetap dianggap:

> ownership berhasil dilepas

Lalu:

```rust
clear_persistent_ownership(...)
self.ownership = Ownership::NotOwned
self.sync = SyncState::Synced
```

Padahal hardware bisa masih:

```text
battery/charging_enabled = 1
main/charging_enabled    = 0
```

atau kombinasi lain.

### Seharusnya

Untuk operasi **restore**, hanya:

```rust
res.all_succeeded()
```

yang boleh dianggap berhasil.

Jadi:

```rust
match control::set_charging(original_charging, &self.profile, &*self.hw_io) {
    Ok(res) if res.all_succeeded() => {
        // benar-benar restored
    }

    Ok(res) => {
        // partial atau all failed
        // ownership TETAP dipertahankan
        // persistent ownership TETAP ada
    }

    Err(e) => {
        ...
    }
}
```

Ini sangat penting karena `release_ownership()` adalah operasi keselamatan.

---

# 2. `shutdown_restore()` punya bug yang sama

Sekarang:

```rust
match control::set_charging(original_charging, &self.profile, &*self.hw_io) {
    Ok(res) if res.succeeded > 0 => {
        ...
        clear_persistent_ownership(&*self.pers_io);
    }
```

Masalahnya sama.

Misalnya:

```text
attempted = 3
succeeded = 1
failed = 2
```

daemon shutdown akan:

```text
anggap restore berhasil
↓
clear ownership
↓
process mati
↓
hardware sebenarnya mixed
```

Ini justru kondisi yang paling tidak boleh kehilangan ownership record.

### Harus:

```rust
Ok(res) if res.all_succeeded() => {
    ...
}
Ok(res) => {
    // jangan clear ownership
}
```

Kalau daemon benar-benar akan mati, persistent state `Releasing` bahkan sangat berguna untuk proses berikutnya mengetahui bahwa restore sebelumnya belum selesai.

---

# 3. `apply_charging()` juga sebaiknya tidak menganggap partial sebagai ApplySuccess biasa

Sekarang:

```rust
Ok(res) if res.succeeded > 0 => {
    ...
    let is_partial = res.failed > 0;
    self.mark_apply_success(target, is_partial);
    events.push(ControllerEvent::ApplySuccess(target));
}
```

Ini masih agak ambigu.

Anda memang sudah membuat:

```rust
HardwareEffect::Unknown
```

untuk partial:

```rust
self.effect = if partial {
    HardwareEffect::Unknown
}
```

Itu bagus.

Tetapi event:

```rust
ApplySuccess(target)
```

mengatakan seolah-olah operasi sukses.

Padahal:

```text
partial write
↓
hardware state unknown
↓
verification pending
```

Lebih bersih jika event dipisahkan:

```rust
ApplySuccess(target)
ApplyPartial(target)
ApplyFailed
```

atau minimal:

```rust
ControllerEvent::ApplySuccess {
    target,
    partial: bool,
}
```

Dengan begitu scheduler/event loop tidak perlu menebak dari state internal.

---

# 4. Ownership persistence setelah partial apply juga perlu dipikirkan

Bagian ini:

```rust
let record = OwnershipRecord {
    ...
    phase: OwnershipPhase::Owned,
};
```

dipanggil setelah:

```rust
res.succeeded > 0
```

Jadi partial write:

```text
charging node A = berhasil
charging node B = gagal
```

tetap menghasilkan:

```text
OwnershipPhase::Owned
```

Ini sebenarnya **bisa diterima**, asalkan ownership record berarti:

> "daemon memiliki ownership atas hardware"

bukan:

> "hardware sudah berhasil mencapai target"

Dan desain Anda memang sudah memisahkan ownership dari synchronization.

Jadi saya justru menyarankan **tetap Owned**, tetapi target hardware harus dianggap `Unknown/Pending`, bukan sukses.

---

# 5. Ada masalah serius pada `read_current_ma()`: integer cast bisa overflow

Di `reader.rs`:

```rust
let ma = match current_fd.config.unit {
    CurrentUnit::MicroAmp => (value / 1000) as i32,
    CurrentUnit::MilliAmp => value as i32,
};
```

`value` adalah:

```rust
i64
```

Kemudian langsung:

```rust
as i32
```

Kalau vendor/kernel memberikan nilai abnormal:

```text
999999999999
```

hasil cast bisa wrap/truncate.

Kemudian Anda baru melakukan:

```rust
if !(-20000..=20000).contains(&val)
```

Masalahnya nilai sudah dikonversi ke `i32`.

Lebih aman:

```rust
let ma = match current_fd.config.unit {
    CurrentUnit::MicroAmp => value
        .checked_div(1000)
        .and_then(|v| i32::try_from(v).ok()),

    CurrentUnit::MilliAmp => i32::try_from(value).ok(),
};

let Some(ma) = ma else {
    continue;
};

if !(-20_000..=20_000).contains(&ma) {
    continue;
}
```

Atau jadikan overflow sebagai `ParseError`.

---

# 6. `CurrentNodeConfig` sudah jauh lebih bagus daripada heuristik unit

Ini bagian yang saya setujui:

```rust
CurrentNodeConfig {
    path: ".../current_now",
    unit: CurrentUnit::MicroAmp,
    priority: 100,
}
```

Daripada:

```text
kalau angka > X berarti µA
kalau angka < X berarti mA
```

**Profile harus menentukan unit.**

Itu jauh lebih deterministic.

Tetapi ada satu hal penting:

### Jangan memilih berdasarkan nilai non-zero.

Versi lama Anda:

```rust
if value == 0 {
    continue;
}
```

sudah Anda hilangkan di `CachedReader`.

Ini benar.

Karena:

```text
current_now = 0
```

adalah nilai valid.

Contohnya:

```text
battery/current_now = 0
bms/current_now     = 120000
```

Kalau `battery` priority 100, maka `0` seharusnya tetap bisa menjadi pembacaan authoritative **jika profile memang menyatakan battery/current_now adalah sensor utama**.

Jadi perubahan ini bagus.

---

# 7. Tetapi priority current node punya semantik yang perlu diperjelas

Sekarang:

```rust
battery/current_now  priority 100
bms/current_now      priority 90
main/current_now     priority 80
usb/current_now      priority 60
```

Dan algoritmanya:

```rust
for current_fd ...
    if priority lebih tinggi {
        best_val = ...
    }
```

Ini berarti:

> selama node priority 100 berhasil dibaca, semua node lain diabaikan.

Itu bagus kalau priority berarti:

> authoritative sensor.

Tetapi kalau tujuan Anda sebenarnya:

> fallback sensor

maka sebaiknya pemilihannya adalah:

```text
highest priority AVAILABLE
```

yang sekarang memang sudah dilakukan.

Jadi desain ini benar **asal dokumentasi profile menyatakan priority adalah authority/fallback priority**, bukan "nilai arus terbesar".

---

# 8. `OnlineNodeConfig.priority` sekarang tidak digunakan

Anda punya:

```rust
pub struct OnlineNodeConfig {
    pub path: &'static str,
    pub priority: u8,
}
```

tetapi:

```rust
pub fn is_plugged_in(&mut self) -> Result<bool, ChargerError>
```

hanya:

```rust
for online_fd in &mut self.online_fds {
    ...
    if value.trim() == "1" {
        return Ok(true);
    }
}
```

Jadi:

```text
USB priority 100
AC priority 90
wireless priority 80
```

priority sama sekali tidak berpengaruh.

Ini bukan bug fatal, tetapi API/profile-nya misleading.

Anda punya dua pilihan.

### Pilihan A — online memang OR semantics

Kalau:

```text
usb = 0
ac = 1
wireless = 0
```

→ plugged.

Maka hapus `priority` dari `OnlineNodeConfig`.

```rust
pub struct OnlineNodeConfig {
    pub path: &'static str,
}
```

Menurut saya ini paling tepat.

### Pilihan B — priority memang diperlukan

Kalau priority harus menentukan authoritative source, implementasikan seperti current.

Tetapi untuk `online`, menurut saya **OR semantics lebih masuk akal**:

```text
any online == 1
```

karena perangkat bisa punya USB/AC/wireless input.

---

# 9. Generic profile terlalu agresif dalam mengklaim capability

Anda punya:

```rust
capabilities: CapabilityProfile {
    supports_charging_toggle: true,
    supports_input_suspend: true,
    supports_current_measurement: true,
    supports_temperature: true,
},
```

untuk:

```rust
GENERIC_PROFILE
```

Padahal generic profile hanya mendefinisikan kemungkinan node.

Misalnya device tidak memiliki:

```text
charging_enabled
input_suspend
```

maka:

```rust
supports_charging_toggle: true
```

secara semantik berarti:

> hardware mendukung fitur ini.

Padahal belum tentu.

Lebih baik capability ditentukan berdasarkan **discovery**, bukan hardcoded generic capability.

Misalnya:

```rust
CapabilityProfile {
    supports_charging_toggle: !profile.control.charging_nodes.is_empty(),
    supports_input_suspend: !profile.control.suspend_nodes.is_empty(),
    supports_current_measurement: !profile.sensor.current_nodes.is_empty(),
    supports_temperature: !profile.sensor.temperature_path.is_empty(),
}
```

Tetapi bahkan ini masih belum menjamin node benar-benar ada.

Lebih ideal:

```text
Profile
   ↓
Discovery
   ↓
RuntimeCapabilities
```

---

# 10. `supports_input_suspend` vs `supports_charging_toggle` juga jangan dianggap setara

Contoh:

```text
charging_enabled = 0
```

bisa berarti:

> charger tidak mengisi baterai.

Sedangkan:

```text
input_suspend = 1
```

bisa berarti:

> input charger diputus dari charger path.

Efek hardware-nya bisa berbeda.

Untuk mode:

```text
ChargingDisabled
```

Anda sekarang melakukan keduanya:

```rust
charging_nodes -> 0
suspend_nodes -> 1
```

Ini berpotensi terlalu agresif pada beberapa vendor.

Profile sebaiknya akhirnya bisa menentukan strategy:

```rust
pub enum ChargingControlStrategy {
    ChargingEnabledOnly,
    InputSuspendOnly,
    ChargingEnabledAndInputSuspend,
}
```

atau lebih fleksibel:

```rust
pub struct ControlProfile {
    pub charging_nodes: ...,
    pub suspend_nodes: ...,
    pub disable_strategy: DisableStrategy,
}
```

Karena pada Android vendor kernel, `charging_enabled` dan `input_suspend` tidak selalu interchangeable.

---

# 11. `enter_bypass_mode()` dan `exit_bypass_mode()` sebaiknya menggunakan profile

Sekarang hardcoded:

```rust
let nodes = [
    ("/sys/class/power_supply/battery/input_suspend", "1"),
    ("/sys/class/power_supply/battery/charging_enabled", "0"),
    ("/sys/class/power_supply/main/charging_enabled", "0"),
];
```

Padahal Anda sudah punya:

```rust
HardwareProfile
```

Ini menciptakan dua sumber kebenaran.

Misalnya profile:

```rust
charging_nodes = [...]
suspend_nodes = [...]
```

tetapi bypass menggunakan path lain.

Akhirnya:

```text
normal charging control → profile
bypass → hardcoded
verification → profile
```

Saya sarankan bypass juga diprofilkan.

---

# 12. `read_charging_state()` punya satu semantik yang masih bisa diperdebatkan

Anda melakukan:

```rust
let all_enabled = observations
    .iter()
    .all(|n| n.state == ChargingNodeState::Enabled);

if all_enabled {
    return Ok(ChargingState::Enabled);
}
```

dan:

```rust
let all_disabled = ...
```

Ini berarti:

```text
battery/charging_enabled = 1  priority 100
main/charging_enabled    = 1  priority 90
input_suspend            = 1  priority 80
```

akan menjadi:

```text
Mixed
```

karena input_suspend disabled-state = `Disabled`.

Padahal mungkin:

```text
charging_enabled=1
input_suspend=1
```

memang secara hardware berarti charging disabled.

Jadi ada masalah konseptual:

> Anda mencampur **control mechanism** yang berbeda ke dalam satu consensus pool.

`charging_enabled=1` dan `input_suspend=0` memang equivalent secara semantic.

Tetapi ketika keduanya tidak konsisten, `Mixed` masuk akal.

Yang perlu diperjelas adalah **priority**.

Saat ini:

```text
battery charging_enabled = 1 priority 100
input_suspend = 1          priority 80
```

hasilnya:

```text
Enabled
```

karena highest priority menang.

Itu bagus.

Tetapi:

```text
battery charging_enabled = 1 priority 100
main charging_enabled    = 0 priority 90
```

juga:

```text
Enabled
```

yang mungkin benar jika battery node memang authoritative.

---

# 13. Verification `ChargingDisabled` masih terlalu bergantung pada current

Ini:

```rust
let current_safe = snapshot.current_ma
    .is_some_and(|current| current <= 100);
```

dan:

```rust
ChargingState::Disabled => current_safe
```

Ada dua masalah.

### Pertama: `None`

Kalau current tidak tersedia:

```rust
current_ma = None
```

maka:

```rust
false
```

dan verification gagal.

Ini **aman**, tetapi mungkin terlalu strict untuk device yang memang tidak memiliki reliable current sensor.

### Kedua: nilai negatif

Misalnya:

```text
current = -5000 mA
```

maka:

```rust
-5000 <= 100
```

→ `true`.

Secara semantik ini berarti baterai sedang discharge, jadi memang aman dari charging.

Itu sebenarnya bisa diterima.

Saya hanya akan membuat namanya lebih jelas:

```rust
fn current_is_not_charging(current_ma: Option<i32>) -> bool {
    current_ma.is_some_and(|ma| ma <= 100)
}
```

Supaya maksudnya tidak tersembunyi.

---

# 14. Verification ChargingEnabled sudah jauh lebih aman

Ini bagus:

```rust
Ok(control::ChargingState::Enabled) => {
    snapshot.online == Some(true)
}
```

Daripada:

```rust
charging_enabled == 1
```

langsung dianggap sukses.

Karena:

```text
charging_enabled=1
```

belum tentu charger benar-benar terhubung.

Namun ada satu hal:

```rust
snapshot.online == Some(true)
```

mengharuskan online sensor tersedia.

Jadi pada hardware yang tidak punya online node:

```text
charging_enabled = 1
online = None
```

→ verification gagal terus.

Kalau generic profile memang mewajibkan online detection, tidak masalah. Tetapi capability seharusnya menentukan apakah online adalah **required verification signal**.

---

# 15. `is_charging_enabled()` menggunakan error yang kurang tepat untuk Mixed

Sekarang:

```rust
ChargingState::Mixed | ChargingState::Unknown => {
    Err(ChargerError::NoChargingNodeFound)
}
```

Ini secara semantic tidak benar.

`Mixed` bukan:

> NoChargingNodeFound.

Node ditemukan, dibaca, dan justru memberikan konflik.

Saya sangat menyarankan error baru:

```rust
ChargerError::ChargingStateUnknown
```

dan kalau ingin lebih presisi:

```rust
ChargerError::ChargingStateMixed
```

Misalnya:

```rust
match read_charging_state(...) {
    Ok(ChargingState::Enabled) => Ok(true),
    Ok(ChargingState::Disabled) => Ok(false),
    Ok(ChargingState::Mixed) => {
        Err(ChargerError::ChargingStateMixed)
    }
    Ok(ChargingState::Unknown) => {
        Err(ChargerError::ChargingStateUnknown)
    }
    Err(e) => Err(e),
}
```

Ini akan sangat membantu debugging dan recovery.

---

# 16. `read_charging_state()` sebenarnya tidak pernah mengembalikan `Unknown`

Anda mendefinisikan:

```rust
pub enum ChargingState {
    Enabled,
    Disabled,
    Mixed,
    Unknown,
}
```

tetapi implementasinya:

```rust
if observations.is_empty() {
    return Err(ChargerError::NoChargingNodeFound);
}
```

dan setelah itu hanya:

```text
Enabled
Disabled
Mixed
```

Jadi:

```rust
ChargingState::Unknown
```

praktis unreachable dari function tersebut.

Pilih salah satu:

### Opsi A

Hapus:

```rust
Unknown
```

### Opsi B

Gunakan `Unknown` ketika node ada tetapi seluruh pembacaan invalid/unreadable.

Menurut saya **B lebih cocok** dengan arsitektur Anda.

Misalnya:

```text
node exists
read failed
↓
Unknown
```

sedangkan:

```text
no configured/available node
↓
NoChargingNodeFound
```

Itu lebih informatif.

---

# 17. `OnlineNodeConfig.priority` adalah kandidat kuat untuk dihapus

Saya pribadi akan ubah:

```rust
#[derive(Debug, Clone, Copy)]
pub struct OnlineNodeConfig {
    pub path: &'static str,
    pub priority: u8,
}
```

menjadi:

```rust
#[derive(Debug, Clone, Copy)]
pub struct OnlineNodeConfig {
    pub path: &'static str,
}
```

Kemudian:

```rust
online_nodes: &[
    OnlineNodeConfig {
        path: "/sys/class/power_supply/usb/online",
    },
    OnlineNodeConfig {
        path: "/sys/class/power_supply/ac/online",
    },
    OnlineNodeConfig {
        path: "/sys/class/power_supply/wireless/online",
    },
    OnlineNodeConfig {
        path: "/sys/class/power_supply/dc/online",
    },
],
```

Karena `online` lebih natural sebagai:

```text
ANY online node == 1
```

bukan highest priority.

---

# 18. Ada satu optimisasi penting di `CachedReader`

Anda menggunakan:

```rust
maybe_rescan_nodes();
```

di:

```rust
read_current_ma()
is_plugged_in()
```

Bagus.

Tetapi `read_capacity()` dan `read_temperature_dc()` tidak memanggil:

```rust
maybe_rescan_nodes()
```

Jika FD menjadi stale:

```rust
self.capacity.file = None;
```

maka pembacaan berikutnya langsung:

```rust
FD not open
```

dan rescan baru terjadi kalau ada pemanggilan lain yang menjalankan `maybe_rescan_nodes()`.

Ini sebaiknya diperbaiki.

Misalnya setiap public sensor read:

```rust
pub fn read_capacity(&mut self) -> Result<u8, ChargerError> {
    self.maybe_rescan_nodes();
    ...
}
```

Begitu juga:

```rust
read_temperature_dc()
read_status()
```

---

# 19. `read_status()` juga perlu invalidation/rescan

Sekarang kalau:

```rust
read_file(...)
```

gagal:

```rust
self.status.file = None;
return Err(e);
```

Tetapi tidak ada mekanisme rescan langsung selain `maybe_rescan_nodes()` dari function lain.

Jadi pola idealnya:

```rust
pub fn read_status(&mut self) -> Result<BatteryStatus, ChargerError> {
    self.maybe_rescan_nodes();

    let file = ...
```

---

# 20. Profile Anda secara keseluruhan sudah berada di arah yang benar

Saya akan mempertahankan struktur:

```text
HardwareProfile
├── ControlProfile
│   ├── charging_nodes
│   └── suspend_nodes
│
├── SensorProfile
│   ├── current_nodes[]
│   ├── online_nodes[]
│   ├── capacity_path
│   ├── temperature_path
│   └── status_path
│
└── CapabilityProfile
```

Ini jauh lebih baik daripada menyebarkan path Android vendor ke seluruh codebase.

Tetapi saya akan membuat satu pemisahan lagi:

```text
Static HardwareProfile
        │
        ▼
Hardware Discovery
        │
        ▼
RuntimeHardwareCapabilities
        │
        ▼
CachedReader / Controller
```

Dengan begitu `GENERIC_PROFILE` hanya mengatakan:

> "node-node ini mungkin tersedia."

Discovery kemudian menentukan:

> "node mana yang benar-benar tersedia."

---

# Prioritas perbaikan saya

Kalau ini codebase yang sedang Anda finalisasi, saya akan mengurutkan pekerjaan seperti ini:

### 🔴 P0 — wajib

1. **`release_ownership()` → hanya `all_succeeded()` yang boleh clear ownership**
2. **`shutdown_restore()` → hanya `all_succeeded()` yang boleh clear ownership**
3. Jangan kehilangan persistent ownership pada partial restore.
4. Perbaiki cast `i64 → i32` pada current.
5. Jangan memetakan `Mixed` menjadi `NoChargingNodeFound`.

### 🟠 P1 — sangat disarankan

6. `OnlineNodeConfig.priority` dihapus atau benar-benar digunakan.
7. `CachedReader` selalu menjalankan `maybe_rescan_nodes()` pada semua public sensor reads.
8. Pisahkan `Unknown` dan `NoChargingNodeFound`.
9. Bypass menggunakan `HardwareProfile`, bukan hardcoded nodes.
10. Capability jangan mengklaim fitur hanya karena profile generic memiliki daftar path.

### 🟡 P2 — desain lanjutan

11. Pisahkan `ApplySuccess` vs `ApplyPartial`.
12. Tambahkan control strategy per profile.
13. Pisahkan static profile dan runtime discovery.
14. Tambahkan verification policy per capability.

---

## Yang paling penting

Dengan perubahan yang sudah Anda lakukan di `control.rs`, fondasinya **sudah benar**:

```text
write all nodes
       │
       ├── all succeeded ──────► success
       │
       ├── partial ─────────────► hardware UNKNOWN
       │
       └── all failed ──────────► failure
```

Tetapi `controller.rs` **belum sepenuhnya mengikuti kontrak tersebut**, khususnya pada jalur restore:

```text
control.rs
partial = NOT success
       ↓
controller.rs
succeeded > 0 = success   ← BUG
```

Jadi kalau saya harus memilih **satu hal yang harus diperbaiki sekarang**, itu adalah:

> **Semua operasi restore/release (`release_ownership()` dan `shutdown_restore()`) harus menggunakan `res.all_succeeded()`, bukan `res.succeeded > 0`.**

Itu yang paling berpengaruh terhadap integritas ownership dan mencegah daemon meninggalkan hardware dalam **mixed state tanpa persistent recovery record**.
