Ya. **Versi ini sudah jauh lebih matang dan secara arsitektur sudah sesuai dengan analisis sebelumnya.** Bahkan dibanding proposal awal, pemisahan `Presence`, `ChargingState`, `BatteryCurrent`, dan `InputCurrent` sekarang sudah benar.

Namun saya **belum akan menyebutnya 100% final**. Ada beberapa koreksi penting sebelum diimplementasikan.

## Yang sudah benar

### 1. Pemisahan empat domain — ✅ sangat tepat

```text
Presence
ChargingState
BatteryCurrent
InputCurrent
```

Ini adalah fondasi yang paling penting.

Pada device Anda:

```text
battery/current_now  = battery current
main/current_now     = actual input current
charging_enabled     = control state
input_suspend        = input control state
```

Jangan lagi mencoba menyimpulkan semuanya dari satu `current_now`.

Data nyata Anda mendukung ini dengan sangat kuat:

```text
colok:
battery/current_now = -1839600 µA
main/current_now    = 1930000 µA

cabut:
battery/current_now = +501700 µA
main/current_now    = 0 µA
```

Jadi pemisahan `Battery` dan `Input` memang diperlukan.

---

### 2. `main/input_current_now` tidak dipakai — ✅

Ini keputusan yang benar.

Anda sudah membuktikan:

```text
colok  = 2000000
cabut  = 2000000
```

Jadi node tersebut bukan actual input current. Jangan digunakan untuk presence.

---

### 3. Signed battery current tetap raw — ✅

Jangan lakukan:

```rust
current.abs()
```

atau:

```rust
if current < 0 { ... }
```

di layer reader hanya untuk "membetulkan" nilainya.

Reader sebaiknya menghasilkan:

```rust
battery_current_ma = -1839
```

dan consumer yang memahami semantik charging/discharging.

Komentar:

> Negatif pada device ini = charging; positif = discharging.

bagus sebagai dokumentasi profile/device, tetapi **jangan dijadikan asumsi global `charger-core`** karena device lain bisa mempunyai konvensi sign berbeda.

---

### 4. `None != Offline` — ✅ sangat penting

Ini sudah benar:

```text
Some(0)       -> kandidat Offline
Some(1930)    -> Online
None          -> Unknown
```

Jangan:

```rust
None => Offline
```

Karena:

```text
sensor rusak
file hilang
permission error
I/O error
driver bermasalah
```

semuanya bisa menghasilkan `None`, tetapi tidak berarti charger dicabut.

---

### 5. Asymmetric hysteresis — ✅

Untuk device Anda:

```text
OFFLINE -> ONLINE
```

boleh cepat.

Sedangkan:

```text
ONLINE -> OFFLINE
```

perlu debounce.

Misalnya:

```text
0 mA
0 mA
0 mA
```

selama ≥ 2 detik baru:

```text
Offline
```

Ini jauh lebih aman daripada langsung:

```rust
input_current == 0 => Offline
```

---

### 6. Partial write dianggap gagal — ✅ sangat penting

Perubahan:

```rust
res.succeeded > 0
```

menjadi:

```rust
res.all_succeeded()
```

adalah perbaikan yang benar.

Misalnya:

```text
charging_enabled       OK
input_suspend          FAILED
```

tidak boleh dianggap:

```text
ChargingDisabled = success
```

Karena hardware sebenarnya berada dalam state campuran.

Ini juga konsisten dengan keputusan sebelumnya untuk menjadikan `Mixed` sebagai state yang harus ditangani secara hati-hati.

---

### 7. Ownership persistence — ✅

Bagian ini sekarang jauh lebih aman:

```text
Acquiring
Owned
Releasing
```

dan semuanya dipulihkan pada boot.

Terutama:

```text
partial restore
        ↓
JANGAN clear persistent ownership
        ↓
Reboot
        ↓
recovery
```

Ini desain yang tepat untuk daemon yang mengubah hardware state.

---

# Tetapi ada 5 hal yang masih saya ubah

## 1. `PresenceReading` sebaiknya tidak memakai `Option<bool>` saja

Ini yang paling penting dari sisi desain.

Sekarang:

```rust
pub struct PresenceReading {
    pub input_current_ma: Option<i32>,
    pub online: Option<bool>,
}
```

Masalahnya:

```rust
online == None
```

bisa berarti dua hal:

```text
A. online node memang tidak tersedia
B. online node tersedia tetapi gagal dibaca
```

Padahal keduanya berbeda.

Saya lebih menyarankan:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceSignal<T> {
    Unavailable,
    Value(T),
    Error,
}
```

atau lebih sederhana:

```rust
pub struct PresenceReading {
    pub input_current_ma: Option<i32>,
    pub online: Option<bool>,
    pub online_available: bool,
}
```

Tetapi saya lebih suka desain enum:

```rust
pub enum OnlineReading {
    Unavailable,
    Online,
    Offline,
    Error,
}
```

Sehingga:

```text
Unavailable
    ↓
boleh fallback ke input_current

Error
    ↓
sensor bermasalah
    ↓
jangan pura-pura menganggap node tidak ada
```

Ini penting untuk generic profile.

---

## 2. Prioritas `online_nodes` perlu didefinisikan lebih tegas

Anda menulis:

> online_node mendapat prioritas lebih tinggi dari input_current.

Saya setuju.

Tetapi implementasinya harus:

```text
online node tersedia + valid
        ↓
gunakan online node

online node tidak tersedia
        ↓
fallback input current

online node error
        ↓
Unknown / fallback sesuai kebijakan
```

Dan **jangan** melakukan:

```text
online = false
input_current = 1930
       ↓
Online
```

kalau `online` node memang authoritative.

Kalau ada:

```text
usb/online = 0
main/current_now = 1930mA
```

kita harus tahu apakah `usb/online` memang authoritative untuk platform tersebut.

Karena itu `PresenceProfile` sebaiknya mendokumentasikan bahwa `online_nodes` adalah **authoritative primary signal**, bukan sekadar node tambahan.

---

# 3. `self_induced_offline` jangan diterapkan secara membabi buta

Bagian ini:

```rust
if self_induced_offline {
    self.current_presence = ChargerPresence::Unknown;
}
```

**benar untuk fallback `input_current`**, karena Anda sudah tahu:

```text
charging_enabled=0
        ↓
main/current_now=0
```

dan itu tidak membuktikan kabel dicabut.

Tetapi untuk:

```text
online node = false
```

saya **tidak akan mengubahnya menjadi Unknown hanya karena daemon sedang menonaktifkan charging**.

Karena online node adalah sinyal berbeda.

Jadi:

```text
online node
    ↓
authoritative
    ↓
tetap Offline jika false
```

sedangkan:

```text
input_current
    ↓
dipengaruhi charging control
    ↓
self_induced_offline => Unknown
```

Ini konsisten dengan prinsip:

> Presence ≠ ChargingState ≠ InputCurrent.

---

# 4. `read_current_role()` harus benar-benar menghormati priority

Anda punya:

```rust
CurrentNodeConfig {
    path: "...",
    priority: 100,
    role: CurrentRole::Input,
}
```

Maka `read_current_role()` jangan sekadar:

```rust
for fd in &self.current_fds {
    if fd.config.role == role {
        return read(fd);
    }
}
```

Harus mempertimbangkan:

```text
Input:
    priority 100
    priority 90
    priority 80

        ↓

pilih highest-priority valid reading
```

Tetapi ada satu keputusan penting:

### Jangan fallback ke node priority rendah jika node priority tinggi menghasilkan nilai valid 0.

Misalnya:

```text
main/current_now = 0       ← valid
usb/current_now  = 1500    ← stale / berbeda domain
```

jangan mengambil `1500`.

`0` adalah pembacaan valid.

Jadi:

```text
read error / unavailable
        ↓
boleh fallback

valid 0
        ↓
STOP
```

Ini penting sekali.

---

# 5. `all_succeeded()` harus didefinisikan dengan benar

Pastikan implementasinya bukan sekadar:

```rust
self.failed == 0
```

karena:

```text
attempted = 0
succeeded = 0
failed = 0
```

bisa secara matematis menghasilkan:

```text
true
```

padahal tidak ada node yang berhasil ditulis.

Lebih aman:

```rust
pub fn all_succeeded(&self) -> bool {
    self.attempted > 0
        && self.failed == 0
        && self.succeeded == self.attempted
}
```

Ini sangat cocok dengan kebutuhan controller Anda.

---

# Ada satu hal lagi: `reconcile()`

Proposal sebelumnya mengatakan `Mixed` jangan langsung dianggap external modification.

Saya setuju, tetapi jangan sampai implementasinya hanya:

```rust
Mixed => ignore
```

karena `Mixed` bisa bertahan selamanya akibat:

```text
partial write
driver failure
external modification
race dengan Android PMIC
```

Lebih bagus:

```text
Enabled
   ↓
Mixed
   ↓
re-read
   ↓
Mixed
   ↓
re-read
   ↓
Mixed
   ↓
uncertain
   ↓
reconciliation
```

Misalnya:

```rust
reconcile_mixed_count
```

atau state:

```rust
ReconciliationState {
    candidate_since: Instant,
    consecutive_mixed: u8,
}
```

Dengan begitu:

```text
Mixed sekali
    -> abaikan

Mixed 2-3 kali dalam window
    -> uncertain

Mixed persistent
    -> ExternalModificationDetected
```

Ini lebih baik daripada agresif maupun terlalu permisif.

---

# Ownership recovery juga saya setujui

Bagian ini:

```rust
match record.phase {
    OwnershipPhase::Acquiring |
    OwnershipPhase::Releasing |
    OwnershipPhase::Owned
        => record.original_charging,
}
```

secara prinsip benar.

Yang lebih penting justru invariant-nya:

```text
persistent ownership exists
        ↓
daemon wajib menganggap hardware masih berpotensi dimiliki
        ↓
attempt restore
        ↓
ALL nodes berhasil
        ↓
clear persistence
```

Sedangkan:

```text
partial
total failure
I/O error
        ↓
JANGAN clear
```

Itu sudah tepat.

---

# Saya juga akan mengubah sedikit arsitektur `Presence`

Menurut saya diagram final yang paling bersih adalah:

```text
                    SYSFS
                      │
        ┌─────────────┼─────────────┐
        │             │             │
 battery/current   main/current   online node
        │             │             │
        ▼             ▼             ▼
 BatteryCurrent   InputCurrent   OnlineSignal
        │             │             │
        └─────────────┴─────────────┘
                      │
                      ▼
              PresenceResolver
                      │
          ┌───────────┴───────────┐
          │                       │
   authoritative online      input activity
       available?              fallback
          │                       │
          └───────────┬───────────┘
                      ▼
               PresenceTracker
               hysteresis/debounce
                      │
          ┌───────────┼───────────┐
          ▼           ▼           ▼
       Online       Offline     Unknown
          │           │           │
          └───────────┼───────────┘
                      ▼
               Decision Engine
                      │
                      ▼
                HardwareTarget
                      │
                      ▼
             HardwareController
                      │
             ┌────────┴────────┐
             ▼                 ▼
      charging_enabled   input_suspend
             │                 │
             └────────┬────────┘
                      ▼
               ChargingState
        Enabled / Disabled / Mixed
                      │
                      ▼
                 Verification
```

Dengan satu catatan konseptual:

> Pada `DEVICE_PROFILE` Anda, karena tidak ada authoritative `online` node, `ChargerPresence::Online` sebenarnya berarti **InputActivity::Active**, bukan bukti kabel secara fisik terpasang.

Itu menurut saya adalah wording yang paling aman.

---

## Kesimpulan

**Ya, rencana terakhir ini sudah berada di jalur yang benar dan saya setujui sebagai baseline implementasi.**

Saya hanya akan melakukan **5 koreksi sebelum coding**:

1. Bedakan `online node unavailable` vs `online node read error`.
2. Tegaskan `online_nodes` sebagai authoritative signal.
3. `self_induced_offline` hanya mempengaruhi interpretasi **input current**, bukan authoritative online node.
4. Pastikan `read_current_role()` memilih **highest-priority valid node** dan `0` dianggap valid.
5. Pastikan `all_succeeded()` mensyaratkan `attempted > 0 && succeeded == attempted && failed == 0`.

Setelah lima poin itu dibereskan, arsitektur ini sudah cukup kuat untuk masuk tahap implementasi dan testing di device Anda.
