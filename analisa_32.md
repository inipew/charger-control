Ya — dengan data ini kita sekarang bisa membuat kesimpulan yang jauh lebih kuat. **Dan ada satu temuan penting: `typec_mode` bukan indikator presence yang bisa Anda pakai secara sederhana**, karena nilainya berubah ke mode yang tetap menunjukkan konfigurasi Type-C walaupun charger dicabut.

### Kondisi charger terpasang

```text
status             = Charging
charging_enabled   = 1
input_suspend      = 0
battery/current    ≈ -1.84 A
main/current       ≈ +1.93 A
main/input_current = +2.00 A
typec_mode         = Sink attached
```

### Charger dicabut

```text
status             = Discharging
charging_enabled   = 0
input_suspend      = 1
battery/current    ≈ +0.50 A
main/current       = 0
main/input_current = +2.00 A
typec_mode         = Powered cable w/ sink
```

## Kesimpulan utama

Untuk device Anda, **`main/current_now` adalah indikator presence yang paling kuat dari data yang kita punya saat ini.**

```text
charger terpasang:
main/current_now = 1930000 µA ≈ +1.93 A

charger dicabut:
main/current_now = 0 µA
```

Sedangkan:

```text
main/input_current_now = 2000000 µA
```

**tidak berubah**, jadi jangan gunakan itu sebagai presence detector.

Dan:

```text
typec_mode
```

berubah:

```text
Sink attached
        ↓
Powered cable w/ sink
```

tetapi keduanya menunjukkan keadaan Type-C, bukan secara langsung "charger sedang memasok daya". Jadi saya tidak akan menjadikannya source utama presence.

---

# Ada temuan penting mengenai `battery/current_now`

Ini juga mengonfirmasi bahwa kita **tidak boleh menggunakan tanda current secara naif**.

Saat charging:

```text
battery/current_now = -1839600 µA
```

≈ **-1.84 A**

Saat discharging:

```text
battery/current_now = +501700 µA
```

≈ **+0.50 A**

Jadi pada device Anda:

```text
battery/current_now < 0
    → charging

battery/current_now > 0
    → discharging
```

Ini kebalikan dari asumsi yang sering dipakai pada sebagian implementasi Android/Linux.

Tetapi jangan menjadikan tanda current sebagai satu-satunya indikator charging. `status` jauh lebih tepat untuk itu.

---

# Profile Anda sekarang sebaiknya diubah

Saya akan membuat profile device Anda seperti ini:

```rust
pub const DEVICE_PROFILE: HardwareProfile = HardwareProfile {
    name: "android-typec-main",

    control: ControlProfile {
        charging_nodes: &[
            "/sys/class/power_supply/battery/charging_enabled",
        ],

        suspend_nodes: &[
            "/sys/class/power_supply/battery/input_suspend",
        ],
    },

    sensor: SensorProfile {
        current_nodes: &[
            CurrentNodeConfig {
                path: "/sys/class/power_supply/battery/current_now",
                unit: CurrentUnit::MicroAmp,
                priority: 100,
            },

            CurrentNodeConfig {
                path: "/sys/class/power_supply/main/current_now",
                unit: CurrentUnit::MicroAmp,
                priority: 90,
            },
        ],

        online_nodes: &[
            OnlineNodeConfig {
                path: "/sys/class/power_supply/main/current_now",
                priority: 100,
            },
        ],

        capacity_path:
            "/sys/class/power_supply/battery/capacity",

        temperature_path:
            "/sys/class/power_supply/battery/temp",

        status_path:
            "/sys/class/power_supply/battery/status",
    },

    capabilities: CapabilityProfile {
        supports_charging_toggle: true,
        supports_input_suspend: true,
        supports_current_measurement: true,
        supports_temperature: true,
    },
};
```

**Tetapi ada catatan:** `online_nodes` secara desain saat ini hanya bisa membaca node yang bernilai `"1"`/`"0"`. `main/current_now` adalah angka mikroampere, sehingga **jangan langsung memasukkannya ke `OnlineNodeConfig` yang sekarang**.

Ini berarti arsitektur Anda perlu sedikit diperbaiki.

---

# Presence bukan sebenarnya `online node`

Ini justru menunjukkan kenapa saya sebelumnya menyarankan `PresenceTracker` terpisah.

Anda memiliki tiga jenis signal:

### 1. Charging control

```text
battery/charging_enabled
battery/input_suspend
```

### 2. Charging state

```text
battery/status
battery/current_now
```

### 3. Input presence

```text
main/current_now
```

Jadi jangan paksa semuanya masuk ke:

```rust
OnlineNodeConfig
```

karena semantic-nya berbeda.

---

# Saya sarankan `PresenceTracker` menerima `Option<bool>`

Decision engine bisa menentukan presence berdasarkan device profile terlebih dahulu:

```rust
pub enum PresenceSignal {
    Online,
    Offline,
    Unknown,
}
```

Misalnya:

```rust
fn read_presence(...) -> PresenceSignal {
    match read_main_current() {
        Ok(current) if current > PRESENCE_CURRENT_THRESHOLD_UA => {
            PresenceSignal::Online
        }

        Ok(_) => {
            PresenceSignal::Offline
        }

        Err(_) => {
            PresenceSignal::Unknown
        }
    }
}
```

Kemudian:

```text
raw signal
    │
    ▼
PresenceTracker
    │
    ├── Online
    ├── Offline
    └── Unknown
```

Ini lebih bersih.

---

# Tapi jangan pakai `current > 0` saja

Saya sangat menyarankan **threshold + hysteresis**.

Misalnya:

```rust
const ONLINE_THRESHOLD_UA: i64 = 100_000;
const OFFLINE_THRESHOLD_UA: i64 = 50_000;
```

Jadi:

```text
main/current_now >= 100mA
        ↓
       ONLINE
```

dan:

```text
main/current_now <= 50mA
        ↓
candidate OFFLINE
```

Sedangkan:

```text
50mA < current < 100mA
```

dianggap:

```text
UNCHANGED
```

Ini bahkan lebih bagus daripada hanya debounce boolean.

---

# PresenceTracker ideal untuk device Anda

Saya akan menggunakan kombinasi:

```text
                main/current_now
                       │
                       ▼
              ┌─────────────────┐
              │ Current classifier│
              └────────┬────────┘
                       │
                 raw presence
                       │
                       ▼
              ┌─────────────────┐
              │ PresenceTracker │
              │                 │
              │ hysteresis      │
              │ + debounce      │
              └────────┬────────┘
                       │
                 stable presence
```

Contohnya:

```text
ONLINE threshold = 100mA
OFFLINE threshold = 50mA
OFFLINE confirmation = 3 samples
minimum duration = 2 sec
```

---

# Kenapa threshold ini penting?

Bayangkan saat charger dicabut:

```text
1,930,000
1,500,000
800,000
300,000
80,000
0
```

Dengan threshold:

```text
ONLINE
ONLINE
ONLINE
ONLINE
ONLINE
candidate offline
```

baru setelah `0` bertahan beberapa polling:

```text
ONLINE
 ↓
candidate OFFLINE
 ↓
candidate OFFLINE
 ↓
candidate OFFLINE
 ↓
OFFLINE
```

Tidak ada false disconnect karena transisi PMIC.

---

# Dan ada fakta menarik tentang `input_current_now`

Anda mendapatkan:

```text
charger ON:
main/input_current_now = 2000000

charger OFF:
main/input_current_now = 2000000
```

Ini sangat jelas menunjukkan bahwa:

> **jangan gunakan `input_current_now` untuk menentukan presence pada device ini.**

Bahkan ketika charger dicabut, nilainya tetap `2A`.

Kemungkinan besar itu adalah **configured/requested input-current limit**, bukan actual measured current.

Ini juga contoh bagus kenapa kode awal Anda:

> "sysfs current jangan main tebak unit dan heuristik pembacaan membabi buta"

memang perlu diperbaiki.

---

# `main/current_now` juga jangan dijadikan battery current

Ini juga penting.

Anda sekarang punya:

```text
battery/current_now = -1.84A
main/current_now    = +1.93A
```

Jangan memilih salah satunya hanya berdasarkan:

```text
priority
```

lalu menganggap hasilnya sebagai:

```text
battery current
```

Karena secara semantic berbeda.

Lebih tepat:

```text
battery/current_now
    → battery current

main/current_now
    → input/charger current
```

Jadi saya sebenarnya akan **memisahkan konfigurasi sensor**:

```rust
pub struct CurrentNodeConfig {
    pub path: &'static str,
    pub unit: CurrentUnit,
    pub priority: u8,
    pub role: CurrentRole,
}

pub enum CurrentRole {
    Battery,
    Input,
}
```

Lalu:

```rust
CurrentNodeConfig {
    path: "/sys/class/power_supply/battery/current_now",
    unit: CurrentUnit::MicroAmp,
    priority: 100,
    role: CurrentRole::Battery,
}

CurrentNodeConfig {
    path: "/sys/class/power_supply/main/current_now",
    unit: CurrentUnit::MicroAmp,
    priority: 100,
    role: CurrentRole::Input,
}
```

Ini jauh lebih aman.

---

# Ini juga mengubah rekomendasi `read_current_ma()`

Sebelumnya fungsi:

```rust
read_current_ma()
```

mengembalikan satu nilai dari beberapa node.

Untuk device Anda, saya lebih suka:

```rust
read_battery_current_ma()
read_input_current_ma()
```

atau:

```rust
pub struct CurrentReadings {
    pub battery_ma: Option<i32>,
    pub input_ma: Option<i32>,
}
```

Kemudian snapshot:

```rust
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temp_dc: Option<i32>,
    pub battery_current_ma: Option<i32>,
    pub input_current_ma: Option<i32>,
    pub ...
}
```

Dengan begitu tidak ada lagi ambiguity:

```text
current_ma = ? battery atau charger?
```

---

# Untuk controller Anda

Dengan data device ini, saya akan mempertahankan:

```text
verify ChargingEnabled
    ↓
ChargingState::Enabled
```

dan:

```text
verify ChargingDisabled
    ↓
ChargingState::Disabled
```

**Jangan gunakan `main/current_now` sebagai verification utama.**

`main/current_now` lebih cocok untuk:

```text
PresenceTracker
```

Sedangkan:

```text
battery/status
battery/charging_enabled
battery/input_suspend
```

untuk:

```text
HardwareController
```

---

## Jadi pembagian finalnya

| Sysfs                      | Fungsi                                               |
| -------------------------- | ---------------------------------------------------- |
| `battery/charging_enabled` | **Hardware control**                                 |
| `battery/input_suspend`    | **Hardware control**                                 |
| `battery/status`           | Charging state                                       |
| `battery/current_now`      | Battery current                                      |
| `battery/current_avg`      | Battery current smoothing/diagnostic                 |
| `battery/voltage_now`      | Battery voltage                                      |
| `battery/temp`             | Temperature                                          |
| `main/current_now`         | **Input/charger actual current + presence evidence** |
| `main/input_current_now`   | Configured/input limit — **jangan presence**         |
| `main/voltage_max`         | Input/charger capability/limit                       |
| `battery/typec_mode`       | Type-C state/context — **bukan primary presence**    |

### Dan yang paling penting:

**Proposal perubahan Anda sebelumnya tetap benar.** Bahkan data ini semakin memperkuat desain tersebut:

```text
PresenceTracker
       │
       │ main/current_now
       ▼
stable charger presence

HardwareController
       │
       ├── charging_enabled
       ├── input_suspend
       └── read_charging_state()
```

Jangan lagi mencoba membuat satu `current_ma` atau satu `online` signal menangani kedua domain tersebut.

Dengan data konkret device Anda ini, saya akan **mengubah desain `HardwareProfile` sedikit sebelum mengimplementasikan Proposed Changes**, terutama memisahkan **Battery Current vs Input Current** dan membuat **presence source berbasis `main/current_now`**. Itu akan membuat implementasi jauh lebih robust daripada profile generic saat ini.
