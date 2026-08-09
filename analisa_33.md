Ya, **arah besarnya sudah benar**, dan data sysfs yang Anda berikan memang mengubah kesimpulan desain secara cukup signifikan. Tetapi saya **tidak menyarankan proposal itu diterapkan persis seperti sekarang**. Ada beberapa koreksi penting.

### 1. Interpretasi data sysfs Anda benar

Dari data:

| Node                       | Charger terpasang |         Charger dicabut | Makna                                             |
| -------------------------- | ----------------: | ----------------------: | ------------------------------------------------- |
| `battery/status`           |        `Charging` |           `Discharging` | status baterai                                    |
| `battery/charging_enabled` |               `1` |                     `0` | control state                                     |
| `battery/input_suspend`    |               `0` |                     `1` | input charging suspended                          |
| `battery/current_now`      |         `-1.84 A` |               `+0.50 A` | **battery current**, sign vendor-specific         |
| `main/current_now`         |          `1.93 A` |                   `0 A` | **input current aktual**                          |
| `main/input_current_now`   |           `2.0 A` |                 `2.0 A` | kemungkinan **input-current limit**, bukan actual |
| `battery/typec_mode`       |   `Sink attached` | `Powered cable w/ sink` | Type-C state                                      |

Yang paling penting adalah:

```text
main/current_now
    plugged     ≈ 1.93 A
    unplugged   = 0 A
```

Ini jauh lebih cocok sebagai **indikator input-power presence** daripada `battery/current_now`.

Jadi pemisahan:

```text
battery_current_ma
input_current_ma
```

memang desain yang lebih sehat.

---

## 2. Tapi saya tidak akan menghapus `online` sebagai konsep

Yang sebaiknya dihapus adalah **ketergantungan terhadap sysfs `online`**, bukan konsep presence-nya.

Artinya:

```text
Sysfs online node
       ↓
       ┐
input current ──→ PresenceTracker ──→ ChargerPresence
       ┘
Type-C signal ───→ optional corroboration
```

`ChargerPresence` tetap merupakan **state hasil observasi**, bukan field mentah dari sysfs.

Jadi saya lebih menyarankan:

```rust
pub enum ChargerPresence {
    Online,
    Offline,
    Unknown,
}
```

dan:

```rust
PresenceTracker::update(input_current_ma)
```

menghasilkan state tersebut.

Dengan begitu nanti kalau ada device lain yang memiliki:

```text
usb/online
```

Anda masih bisa menambahkan backend presence tanpa mengubah controller.

---

# 3. `CurrentRole` adalah perubahan yang sangat bagus

Saya setuju dengan:

```rust
pub enum CurrentRole {
    Battery,
    Input,
}
```

dan:

```rust
pub struct CurrentNodeConfig {
    pub path: &'static str,
    pub unit: CurrentUnit,
    pub priority: u8,
    pub role: CurrentRole,
}
```

Kemudian profile Anda bisa menjadi kira-kira:

```rust
current_nodes: &[
    CurrentNodeConfig {
        path: "/sys/class/power_supply/battery/current_now",
        unit: CurrentUnit::MicroAmp,
        priority: 100,
        role: CurrentRole::Battery,
    },

    CurrentNodeConfig {
        path: "/sys/class/power_supply/main/current_now",
        unit: CurrentUnit::MicroAmp,
        priority: 100,
        role: CurrentRole::Input,
    },
],
```

Ini jauh lebih baik daripada sistem lama yang memasukkan semua `current_now` ke satu pool lalu memilih berdasarkan priority.

Karena:

```text
battery/current_now
```

dan:

```text
main/current_now
```

**bukan dua kandidat untuk sensor yang sama.**

Mereka mengukur hal yang berbeda.

---

# 4. Tetapi `input_current_ma >= 100` jangan langsung dianggap "charger terpasang"

Ini bagian yang paling perlu saya koreksi.

Proposal:

```text
>= 100 mA → Online
<= 50 mA  → Offline
```

masuk akal sebagai **signal heuristic**, tetapi belum cukup aman sebagai definisi presence.

Contoh:

```text
charger terpasang
main/current_now = 0 mA
```

bisa terjadi karena:

* charger belum benar-benar negotiated,
* USB-PD sedang transisi,
* PMIC sedang suspend,
* charging sengaja dibypass,
* thermal/current management,
* charger sedang idle,
* kabel/adapter bermasalah.

Dan yang paling penting untuk project Anda:

### bypass/charging control dapat membuat `main/current_now == 0`

padahal kabel sebenarnya **masih terpasang**.

Jadi kalau:

```rust
input_current_ma <= 50
```

berarti:

```rust
ChargerPresence::Offline
```

maka ketika daemon sengaja melakukan:

```text
charging_enabled = 0
```

presence bisa berubah menjadi Offline.

Itu justru menghidupkan kembali masalah coupling yang sebelumnya ingin kita hilangkan.

---

# 5. Presence dan charging effect harus benar-benar dipisahkan

Saya akan menggunakan model:

```text
                  ┌────────────────────┐
battery current ─→│                    │
                  │ Hardware Snapshot  │
input current ───→│                    │
                  │                    │
status ──────────→│                    │
                  └─────────┬──────────┘
                            │
                  ┌─────────▼──────────┐
                  │  PresenceTracker   │
                  └─────────┬──────────┘
                            │
                    ChargerPresence
                            │
              ┌─────────────▼─────────────┐
              │       Decision Engine     │
              └─────────────┬─────────────┘
                            │
                     HardwareTarget
                            │
              ┌─────────────▼─────────────┐
              │   HardwareController      │
              └────────────────────────────┘
```

Dengan demikian:

**Presence:**

> "Apakah input power kemungkinan tersedia?"

sedangkan:

**Charging state:**

> "Apakah hardware charging controller saat ini enabled/disabled?"

dan:

**Battery current:**

> "Apa yang sedang terjadi pada baterai?"

Ketiganya tidak boleh disamakan.

---

# 6. Bahkan `battery/status` bisa menjadi corroborating signal

Data Anda sangat bagus:

### Plugged

```text
status = Charging
main/current_now = 1930000
typec_mode = Sink attached
```

### Unplugged

```text
status = Discharging
main/current_now = 0
typec_mode = Powered cable w/ sink
```

Jadi sebenarnya Anda memiliki beberapa sinyal:

```text
S1 = main/current_now
S2 = battery/status
S3 = battery/typec_mode
```

Saya tidak akan menjadikan semuanya sebagai syarat wajib.

Tetapi bisa digunakan sebagai **evidence**.

Misalnya:

```text
main/current_now > threshold
        ↓
strong online evidence
```

sedangkan:

```text
main/current_now == 0
```

hanya:

```text
offline candidate
```

Kemudian `PresenceTracker` melakukan debounce.

---

# 7. PresenceTracker asimetris tetap saya setujui

Ini bagian proposal Anda yang menurut saya tepat.

Misalnya:

```rust
const ONLINE_THRESHOLD_MA: i32 = 100;
const OFFLINE_THRESHOLD_MA: i32 = 50;

const OFFLINE_CONFIRMATIONS: u8 = 3;
const OFFLINE_CONFIRM_DURATION: Duration =
    Duration::from_secs(2);
```

Kemudian:

```text
Offline
   │
   │ input >= 100 mA
   ▼
Online
```

langsung.

Sebaliknya:

```text
Online
   │
   │ input <= 50
   ▼
OfflineCandidate
   │
   ├── signal kembali > 50 → Online
   │
   └── >= 3 samples + >= 2 sec
                  ↓
              Offline
```

Ini jauh lebih aman daripada:

```rust
if current == 0 {
    offline
}
```

---

# 8. Tetapi tambahkan `Unknown`

Ini penting.

Jangan:

```rust
Option<i32>
```

lalu:

```text
None → Offline
```

Karena:

```text
None
```

berarti:

> sensor tidak tersedia / gagal dibaca

bukan:

> charger tidak ada.

Saya akan membuat:

```rust
pub enum PresenceSignal {
    Online,
    OfflineCandidate,
    Unknown,
}
```

atau langsung mempertahankan:

```rust
ChargerPresence {
    Online,
    Offline,
    Unknown,
}
```

Dengan aturan:

```text
input_current = Some(1930)
        ↓
Online

input_current = Some(0)
        ↓
Offline candidate

input_current = None
        ↓
Unknown
```

---

# 9. Ada satu masalah besar lain: sign `battery/current_now`

Anda sudah menemukan fakta penting:

```text
Charging:
battery/current_now = -1839600

Discharging:
battery/current_now = +501700
```

Jangan memperbaiki ini dengan heuristik global seperti:

```rust
abs(value)
```

atau:

```rust
if value < 0 {
    charging
}
```

Karena sign tersebut **vendor/kernel-specific**.

`CurrentUnit` hanya menangani:

```text
µA → mA
```

bukan semantik sign.

Saya malah akan menambahkan:

```rust
pub enum CurrentRole {
    Battery,
    Input,
}
```

dan membiarkan `battery_current_ma` tetap **raw signed current**.

Jadi:

```text
battery_current_ma = -1839
```

tetap -1839.

Kemudian interpretasi charging/discharging dilakukan oleh:

```text
battery/status
+
charging control state
+
battery current
```

bukan oleh reader.

Itu separation of concerns yang lebih bagus.

---

# 10. `main/input_current_now` memang jangan digunakan untuk presence

Saya setuju penuh.

Data Anda:

```text
plugged:
main/input_current_now = 2000000

unplugged:
main/input_current_now = 2000000
```

jelas menunjukkan bahwa node itu **tidak merepresentasikan actual input current** pada device ini.

Maka jangan masukkan:

```rust
main/input_current_now
```

ke:

```rust
CurrentRole::Input
```

Yang benar:

```rust
main/current_now → Input
```

sedangkan:

```rust
main/input_current_now
```

kalau ingin dipertahankan, sebaiknya diberi role berbeda, misalnya:

```rust
CurrentRole::InputLimit
```

atau bahkan tidak perlu dibaca sama sekali untuk controller sekarang.

---

# 11. `OnlineNodeConfig` tidak perlu dihapus dari seluruh arsitektur

Saya akan mengubahnya menjadi lebih generik daripada menghapus total.

Misalnya profile:

```rust
pub struct PresenceProfile {
    pub input_current_threshold_ma: Option<i32>,
    pub online_nodes: &'static [&'static str],
}
```

Tetapi untuk device Anda:

```rust
PresenceProfile {
    input_current_threshold_ma: Some(100),
    online_nodes: &[],
}
```

Kemudian future device bisa:

```text
device A:
input current

device B:
usb/online

device C:
ac/online

device D:
input current + usb/online
```

Ini membuat arsitektur Anda benar-benar **hardware-profile driven**, bukan **Redmi-specific driven**.

---

# 12. Ada satu koreksi terhadap `reader.rs`

Proposal Anda mengatakan:

> Pisahkan logika caching file descriptor untuk arus baterai dan arus input.

Saya setuju secara semantik, tetapi tidak harus membuat dua implementasi caching yang berbeda.

Lebih bagus:

```rust
current_fds: Vec<CurrentFd>
```

tetap satu collection, tetapi:

```rust
CurrentFd {
    config: CurrentNodeConfig,
    file: File,
}
```

dan filter berdasarkan role:

```rust
read_current_by_role(CurrentRole::Battery)
read_current_by_role(CurrentRole::Input)
```

Contoh API:

```rust
pub fn read_battery_current_ma(&mut self)
    -> Result<i32, ChargerError>

pub fn read_input_current_ma(&mut self)
    -> Result<i32, ChargerError>
```

internalnya bisa satu helper:

```rust
fn read_current_role(
    &mut self,
    role: CurrentRole,
) -> Result<i32, ChargerError>
```

Jadi tidak ada duplikasi.

---

# 13. Controller proposal Anda sebelumnya juga perlu satu koreksi

Saya setuju dengan:

```rust
res.all_succeeded()
```

untuk `apply_charging`.

Tetapi ada konsekuensi penting.

Misalnya:

```text
battery/charging_enabled = 0   SUCCESS
main/charging_enabled    = 0   FAIL
```

maka:

```text
partial failure
```

harus menjadi:

```text
SyncState::Failed
force_apply = true
```

**tetapi ownership tetap `Owned`.**

Itu benar.

Namun pada retry berikutnya, jangan selalu melakukan acquisition lagi.

Controller harus memahami:

```rust
ownership == Owned
```

berarti:

> hardware sudah menjadi tanggung jawab kita; ulangi desired operation.

Ini sudah cukup dekat dengan implementasi Anda sekarang.

---

# 14. Restore juga benar-benar harus strict

Proposal Anda:

> partial restore → jangan clear persistent ownership.

Saya setuju **100%**.

Misalnya:

```text
original = charging enabled

restore:
battery/charging_enabled = 1 → SUCCESS
main/charging_enabled    = 1 → FAIL
```

Anda **tidak boleh**:

```rust
clear_persistent_ownership()
```

karena secara persisten Anda belum tahu hardware benar-benar kembali ke keadaan semula.

Lebih aman:

```text
OwnershipPhase::Releasing
        ↓
restore partial/failed
        ↓
persistent record tetap
        ↓
boot recovery
```

Ini justru salah satu perubahan paling penting dari desain awal.

---

# Kesimpulan

Jadi saya akan menilai proposal Anda:

| Perubahan                                         | Penilaian                                                  |
| ------------------------------------------------- | ---------------------------------------------------------- |
| `CurrentRole::{Battery, Input}`                   | ✅ Sangat tepat                                             |
| `battery_current_ma` + `input_current_ma`         | ✅ Sangat tepat                                             |
| `main/current_now` sebagai input current          | ✅ Tepat untuk device ini                                   |
| `main/input_current_now` sebagai actual current   | ❌ Jangan                                                   |
| Menghapus ketergantungan sysfs `online`           | ✅ Tepat                                                    |
| Menghapus konsep `ChargerPresence`                | ❌ Jangan                                                   |
| PresenceTracker                                   | ✅ Sangat tepat                                             |
| asymmetric hysteresis                             | ✅ Tepat                                                    |
| `>=100 mA = online`                               | ⚠️ Bagus sebagai signal, jangan dianggap kebenaran absolut |
| `<=50 mA = offline`                               | ⚠️ Harus debounce                                          |
| `None = offline`                                  | ❌ Harus `Unknown`                                          |
| `battery/current_now` sign heuristic              | ❌ Jangan                                                   |
| strict partial write                              | ✅ Tepat                                                    |
| strict restore                                    | ✅ Sangat tepat                                             |
| `Mixed` langsung external modification            | ❌ Jangan                                                   |
| `Mixed` → uncertain/debounce                      | ✅ Tepat                                                    |
| `snapshot.online` untuk hardware verification     | ❌ Hapus                                                    |
| `ChargingEnabled` diverifikasi dari control state | ✅ Tepat                                                    |

### Jadi versi arsitektur yang saya rekomendasikan:

```text
                   SYSFS
                     │
       ┌─────────────┼──────────────┐
       │             │              │
 battery/current   main/current   battery/status
       │             │              │
       ▼             ▼              ▼
battery_current  input_current   BatteryStatus
       │             │
       └─────────────┤
                     ▼
              PresenceTracker
                     │
            ChargerPresence
          Online / Offline / Unknown
                     │
                     ▼
             Decision Engine
                     │
                     ▼
              HardwareTarget
                     │
                     ▼
           HardwareController
                     │
          ┌──────────┴──────────┐
          ▼                     ▼
    charging_enabled      input_suspend
          │                     │
          └──────────┬──────────┘
                     ▼
             ChargingState
        Enabled / Disabled / Mixed
                     │
                     ▼
              Verification
```

**Dengan data konkret device Anda, saya akan memilih `main/current_now` sebagai primary presence signal, `battery/status` sebagai corroborating/diagnostic signal, dan sama sekali tidak menggunakan `main/input_current_now` untuk menentukan presence.**

Dan satu hal lagi: karena `typec_mode` Anda menunjukkan `"Sink attached"` saat terpasang dan `"Powered cable w/ sink"` saat dicabut, **jangan menjadikan `typec_mode` sebagai primary presence signal** tanpa pemetaan state yang tervalidasi. Nilainya jelas berubah, tetapi semantiknya vendor/Type-C-stack-specific.

Jadi: **proposal Anda sudah berada di jalur yang benar, tetapi saya akan menambahkan `Unknown` dan menjaga `ChargerPresence` sebagai state turunan, bukan menghapus konsep presence.** Itu akan membuat desain jauh lebih tahan terhadap transisi PMIC dan vendor kernel.
