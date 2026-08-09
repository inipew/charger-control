Ya. Setelah melihat kode lengkapnya, **analisis Anda tentang infinite bounce masuk akal, tetapi implementasi perbaikannya saat ini belum benar-benar menutup race/state transition tersebut**.

Ada satu masalah utama yang cukup jelas:

> `DecisionEngine` mencoba menentukan apakah `online=false` adalah "charger benar-benar dicabut" hanya berdasarkan `self.policy`.

Itu terlalu lemah karena `DecisionEngine` tidak mengetahui **intent hardware/controller**.

---

# 1. Bug utama ada di sini

Kode sekarang:

```rust
let mut is_offline = snapshot.online == Some(false);

if is_offline && (
    self.policy == ChargePolicyState::LimitReached || 
    self.policy == ChargePolicyState::ThermalCutoff || 
    self.policy == ChargePolicyState::Fault
) {
    is_offline = false;
}

if is_offline {
    self.policy = ChargePolicyState::Offline;

    return Self::decision(
        self.policy,
        HardwareTarget::Unmanaged,
        DecisionReason::ChargerOffline,
    );
}
```

Masalahnya adalah **`policy` bukan sumber kebenaran mengenai kondisi hardware**.

Contoh siklus:

```text
t0:
online=true
policy=Charging
target=ChargingEnabled

        ↓

capacity=90%

        ↓

Decision:
policy=LimitReached
target=ChargingDisabled

        ↓

HardwareController mematikan charging

        ↓

kernel:
online=false

        ↓

t1:
snapshot.online=false

policy memang seharusnya LimitReached
        ↓
online suppression aktif
        ↓
tetap ChargingDisabled
```

Untuk kondisi ideal, kode Anda memang sudah mencegah bounce.

**Tetapi ada beberapa kondisi di mana `policy` kembali menjadi `Charging`/`Offline`, sehingga perlindungan itu hilang.**

Yang paling berbahaya adalah startup/recovery.

---

# 2. `DecisionEngine` tidak seharusnya menebak intent hardware

Saat ini Anda memiliki tiga sumber state:

```text
SensorSnapshot
      ↓
DecisionEngine
      ↓
HardwareController
```

Tetapi `DecisionEngine` membuat keputusan:

```text
online=false → charger offline → Unmanaged
```

padahal:

```text
online=false
```

bisa berarti dua hal yang berbeda:

### Kasus A — benar-benar dicabut

```text
user cabut USB
      ↓
VBUS hilang
      ↓
online=false
```

Maka:

```text
Unmanaged
```

benar.

### Kasus B — daemon sendiri mematikan charging

```text
daemon
  ↓
input_suspend=1
  ↓
PMIC memutus jalur charging
  ↓
kernel kehilangan VBUS visibility
  ↓
online=false
```

Tetapi:

```text
charger fisik masih tertancap
```

Maka:

```text
Unmanaged
```

**salah.**

Ini adalah inti bug Anda.

---

# 3. Perbaikannya jangan hanya berdasarkan `policy`

Saya lebih menyarankan mengubah interface:

```rust
pub fn evaluate(
    &mut self,
    snapshot: &SensorSnapshot,
    cfg: &Config,
    hardware_target: HardwareTarget,
) -> Decision
```

atau lebih bagus lagi memberikan context khusus:

```rust
pub struct DecisionContext {
    pub hardware_target: HardwareTarget,
    pub hardware_owned: bool,
    pub sync_state: SyncState,
}
```

Kemudian:

```rust
pub fn evaluate(
    &mut self,
    snapshot: &SensorSnapshot,
    cfg: &Config,
    ctx: &DecisionContext,
) -> Decision
```

Dengan begitu decision engine tahu:

> "Apakah `online=false` terjadi ketika kita sendiri sedang memblokir charging?"

---

# 4. Logika yang lebih aman

Misalnya:

```rust
let physically_offline = snapshot.online == Some(false);

let self_induced_offline =
    physically_offline
    && ctx.hardware_owned
    && matches!(
        ctx.hardware_target,
        HardwareTarget::ChargingDisabled
    );
```

Kemudian:

```rust
if physically_offline && !self_induced_offline {
    self.policy = ChargePolicyState::Offline;

    return Self::decision(
        self.policy,
        HardwareTarget::Unmanaged,
        DecisionReason::ChargerOffline,
    );
}
```

Artinya:

```text
online=false
    │
    ├── kita tidak mengontrol charging
    │       └──> Offline → Unmanaged
    │
    └── kita sedang disable charging
            └──> jangan anggap charger dicabut
```

Ini jauh lebih kuat daripada:

```rust
self.policy == LimitReached
```

---

# 5. Bahkan `hardware_target` saja masih belum ideal

Ada detail penting.

Misalnya:

```text
desired_target = ChargingDisabled
```

tetapi:

```text
sync = Unknown
```

atau:

```text
sync = Failed
```

Kita belum tentu benar-benar berhasil mematikan charging.

Jadi lebih aman menggunakan:

```rust
hardware_owned
&& sync == SyncState::Synced
&& desired_target == ChargingDisabled
```

Contoh:

```rust
let self_induced_offline =
    snapshot.online == Some(false)
    && hardware_owned
    && sync_state == SyncState::Synced
    && desired_target == HardwareTarget::ChargingDisabled;
```

Ini penting untuk menghindari daemon menganggap dirinya berhasil mengontrol hardware padahal write sysfs sebenarnya gagal.

---

# 6. Ada masalah lebih besar pada startup/recovery Anda

Dari log sebelumnya:

```text
Found stale ownership state
(original charging=true, phase=Releasing)
```

lalu:

```text
Partial failure during stale ownership recovery
```

Kemudian:

```text
Applying hardware target: Unmanaged
(sync=Recovering, force=true)
```

Ini menunjukkan **recovery dan decision loop masih terlalu bercampur**.

Di kode Anda:

```rust
let decision = engine.evaluate(&snapshot, &cfg);
```

dilakukan **sebelum**:

```rust
if recovery.should_recover(now) {
    ...
}
```

Artinya DecisionEngine sudah membuat keputusan berdasarkan sensor sebelum recovery hardware selesai.

Saya akan membalik konsepnya menjadi:

```text
START
 │
 ├─ stale ownership?
 │      │
 │      ├─ YES → RECOVERY
 │      │          │
 │      │          ├─ success → normal operation
 │      │          └─ failed → remain recovery
 │      │
 │      └─ NO
 │
 ↓
READ SENSOR
 ↓
DECISION
 ↓
HARDWARE APPLY
 ↓
VERIFY
 ↓
WAIT
```

**Recovery harus menjadi fase state machine tersendiri**, bukan sekadar aktivitas tambahan di dalam loop.

---

# 7. Saya juga melihat potensi masalah pada `RecoveryState`

Anda punya:

```rust
enum RecoveryState {
    NotNeeded,
    Required,
    Failed { attempts: u32, next_retry: Instant },
    Done,
}
```

dan:

```rust
fn is_blocking(&self) -> bool {
    !matches!(
        self.state,
        RecoveryState::NotNeeded | RecoveryState::Done
    )
}
```

Ini bagus.

Tetapi setelah recovery sukses:

```rust
recovery.mark_done();

hardware.sync = SyncState::Unknown;
hardware.force_apply = true;
```

Kemudian decision yang sebelumnya sudah dihitung bisa saja:

```rust
HardwareTarget::Unmanaged
```

karena `online=false`.

Lalu:

```rust
hardware.apply_target(decision.target);
```

bisa menjadi:

```text
Unmanaged
```

padahal recovery baru saja mengembalikan hardware ke kondisi original.

Jadi setelah recovery sukses, **decision harus dihitung ulang**, bukan menggunakan decision lama.

Contoh sederhana:

```rust
if recovery.should_recover(now) {
    match recover(...) {
        Ok(Recovered) => {
            recovery.mark_done();
            hardware.sync = SyncState::Unknown;
            hardware.force_apply = true;

            // Jangan lanjut memakai decision lama.
            continue;
        }

        Ok(NotNeeded) => {
            recovery.mark_done();
            continue;
        }

        Err(...) => {
            recovery.mark_failed(now);
            continue;
        }
    }
}
```

Dengan demikian:

```text
recovery selesai
       ↓
sensor dibaca ulang
       ↓
decision baru
       ↓
hardware target baru
```

Ini jauh lebih deterministik.

---

# 8. Ada masalah lain: `Fault` juga digunakan sebagai "disable charging"

Ini:

```rust
if !sensors_valid {
    self.policy = ChargePolicyState::Fault;

    return Self::decision(
        self.policy,
        HardwareTarget::ChargingDisabled,
        reason,
    );
}
```

secara safety memang masuk akal:

```text
sensor invalid
     ↓
disable charging
```

Tetapi setelah itu:

```rust
online=false
```

Anda sengaja menganggap:

```rust
Fault + online=false
```

sebagai bukan offline.

Itu benar untuk self-induced disconnect.

Namun ada edge case:

```text
sensor fault
+
charger benar-benar dicabut
```

Daemon bisa salah menganggap:

```text
online=false
```

sebagai akibat disable charging.

Jadi `Fault` **tidak cukup sebagai bukti self-induced disconnect**.

Sekali lagi, bukti terbaik adalah:

```text
Ownership + verified hardware target
```

bukan policy.

---

# 9. Saya sarankan tambahkan `ChargingDisabled` context

Misalnya:

```rust
#[derive(Debug, Clone, Copy)]
pub struct HardwareContext {
    pub owned: bool,
    pub target: HardwareTarget,
    pub sync: SyncState,
}
```

Kemudian:

```rust
fn is_self_induced_offline(
    snapshot: &SensorSnapshot,
    ctx: HardwareContext,
) -> bool {
    snapshot.online == Some(false)
        && ctx.owned
        && ctx.sync == SyncState::Synced
        && ctx.target == HardwareTarget::ChargingDisabled
}
```

Lalu decision:

```rust
let offline = snapshot.online == Some(false);

if offline && !is_self_induced_offline(snapshot, ctx) {
    self.policy = ChargePolicyState::Offline;

    return Self::decision(
        self.policy,
        HardwareTarget::Unmanaged,
        DecisionReason::ChargerOffline,
    );
}
```

Ini secara arsitektur jauh lebih benar.

---

# 10. Tetapi saya akan menambahkan satu state lagi

Saya sebenarnya lebih suka tidak langsung menganggap:

```text
online=false + ChargingDisabled
```

sebagai "charger online".

Lebih tepat:

```text
ChargerPresence
```

memiliki tiga kondisi:

```rust
pub enum ChargerPresence {
    Online,
    Offline,
    Unknown,
}
```

Misalnya:

```text
online=false
+
self-induced disable
       ↓
Unknown
```

bukan:

```text
Online
```

Kenapa?

Karena daemon **memang tidak tahu secara fisik apakah charger masih tertancap**.

Kernel sudah kehilangan informasi.

Jadi:

```text
online=false
```

tidak boleh dipaksa menjadi:

```text
online=true
```

Kita hanya tahu:

> `online=false` tidak dapat dipercaya dalam kondisi ini.

Itu perbedaan penting.

---

# 11. Ini juga akan membuat DecisionEngine lebih bersih

Modelnya menjadi:

```text
SensorSnapshot
       │
       ├── capacity
       ├── temperature
       ├── status
       └── online
              │
              ↓
       Presence Resolver
              │
              ↓
     Online / Offline / Unknown
              │
              ↓
       DecisionEngine
              │
              ↓
       HardwareController
```

Jadi DecisionEngine tidak perlu mengetahui detail:

```text
/sys/class/power_supply/usb/online
```

atau:

```text
input_suspend
```

---

# 12. Ada satu hal lagi yang saya anggap penting: jangan gunakan `online` sebagai satu-satunya charger presence

Untuk Android vendor tertentu, lebih robust jika presence ditentukan dari beberapa indikator:

```text
online
status
current
USB state
AC/Mains/Wireless online
```

Misalnya:

```text
online=false
status=NotCharging
current≈0
```

belum tentu charger dicabut.

Sedangkan:

```text
online=false
status=Discharging
current<0
```

lebih kuat mengarah ke benar-benar tidak charging.

Dan:

```text
online=false
hardware_owned=true
target=ChargingDisabled
sync=Synced
```

jelas harus diperlakukan sebagai:

```text
Unknown / self-induced
```

Ini sejalan dengan perbaikan P2 Anda tentang **sensor sanity validation**.

---

# 13. Scheduler Anda juga terkena efek yang sama

Ini:

```rust
if snapshot.online == Some(false) {
    self.last_interval =
        if netlink_alive {
            UNPLUGGED_HEARTBEAT
        } else {
            UNPLUGGED_HEARTBEAT_NO_NETLINK
        };

    return self.last_interval;
}
```

Kalau:

```text
online=false
```

adalah self-induced:

```text
scheduler → 600 detik
```

Ini bisa membuat sistem terlalu lama tidak mengevaluasi kondisi baterai.

Lebih bagus scheduler menerima:

```rust
charger_presence
```

daripada raw:

```rust
snapshot.online
```

Misalnya:

```rust
match presence {
    ChargerPresence::Offline => {
        Duration::from_secs(600)
    }

    ChargerPresence::Unknown => {
        Duration::from_secs(5)
    }

    ChargerPresence::Online => {
        // adaptive scheduling
    }
}
```

**Unknown harus heartbeat pendek**, bukan 600 detik.

---

# 14. Netlink Anda relatif sudah baik

Bagian ini:

```rust
const DEBOUNCE: Duration = Duration::from_millis(250);
```

dan:

```rust
self.debounce_target = Some(now + DEBOUNCE);
```

sudah cukup masuk akal.

Backoff juga:

```text
1s
2s
4s
8s
...
60s
```

sudah bagus.

Saya hanya akan memisahkan:

```text
socket reconnect
```

dari:

```text
power-supply event debounce
```

secara konseptual, tetapi implementasi sekarang tidak menjadi sumber utama bounce.

---

# 15. Urutan loop yang saya rekomendasikan

Saat ini:

```text
read sensor
↓
verify
↓
decision
↓
recovery
↓
apply
↓
netlink
↓
sleep
```

Saya akan ubah menjadi:

```text
┌──────────────────────────────┐
│          MONITOR LOOP        │
└──────────────┬───────────────┘
               ↓
        Process IPC/events
               ↓
        Recovery required?
          /           \
        YES            NO
         ↓              ↓
     Recovery      Read sensors
         ↓              ↓
      success?     Resolve presence
       /   \             ↓
     no    yes      DecisionEngine
     ↓      ↓            ↓
   retry  continue   HardwareController
                        ↓
                     Verify
                        ↓
                     Scheduler
                        ↓
                       poll
```

Recovery yang sukses sebaiknya **memulai iterasi baru**.

---

# 16. Tes yang sekarang wajib ditambahkan

Untuk bug ini, saya akan menambahkan minimal 5 test.

### Test 1 — self-induced disconnect

```text
online=true
policy=Charging
target=ChargingEnabled

→ capacity reaches limit

decision:
target=ChargingDisabled

→ next sensor:
online=false

EXPECT:
policy != Offline
target == ChargingDisabled
```

### Test 2 — physical unplug

```text
online=true
target=ChargingEnabled

→ user unplugs

online=false

EXPECT:
policy == Offline
target == Unmanaged
```

### Test 3 — disable target but write failed

```text
target=ChargingDisabled
sync=Failed
online=false

EXPECT:
Offline
```

Ini penting.

Karena daemon **tidak boleh menganggap dirinya berhasil memutus charging jika write sysfs gagal**.

### Test 4 — recovery

```text
stale ownership
online=false

→ recovery succeeds

EXPECT:
decision evaluated again
```

dan **tidak menggunakan decision lama**.

### Test 5 — no infinite bounce

Simulasikan:

```text
90%
online=true
        ↓
disable
        ↓
online=false
        ↓
online=false
        ↓
online=false
```

EXPECT:

```text
target sequence:

ChargingEnabled
ChargingDisabled
ChargingDisabled
ChargingDisabled
```

dan **tidak pernah**:

```text
ChargingDisabled
→ Unmanaged
→ ChargingEnabled
→ ChargingDisabled
```

Ini menurut saya adalah test P3 yang paling berharga untuk bug Anda.

---

# 17. Kesimpulan review kode

**Ya, arah perbaikannya benar, tetapi saya belum akan menganggap bug bounce selesai.**

Bagian ini:

```rust
if is_offline && (
    self.policy == ChargePolicyState::LimitReached || 
    self.policy == ChargePolicyState::ThermalCutoff || 
    self.policy == ChargePolicyState::Fault
)
```

adalah **patch heuristik**, bukan solusi arsitektural.

Solusi yang lebih tepat:

```text
              ┌────────────────────┐
              │ Sensor online=false│
              └─────────┬──────────┘
                        ↓
              apakah kita memiliki
              hardware ownership?
                   /          \
                 NO            YES
                 ↓              ↓
              Offline     target disabled?
                              /      \
                            NO        YES
                            ↓          ↓
                         Offline    Unknown
                                      │
                              jangan release ownership
```

Dan yang paling penting:

> **`DecisionEngine` harus diberi context dari `HardwareController`; jangan mencoba menyimpulkan ownership/intent hardware dari `ChargePolicyState`.**

Selain itu, **recovery sukses harus `continue` ke iterasi berikutnya**, supaya keputusan lama yang mungkin masih `Unmanaged` tidak langsung diterapkan.

Kalau saya yang melanjutkan refactor ini, saya akan menjadikan **`HardwareTarget + Ownership + SyncState` sebagai source of truth untuk menentukan apakah `online=false` boleh dianggap sebagai unplug**, lalu membuat `ChargerPresence::Online/Offline/Unknown`. Itu akan menyelesaikan akar masalah bounce sekaligus membuat desain P2/P3 jauh lebih konsisten.
