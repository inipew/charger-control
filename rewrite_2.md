Kalau **rewrite sekarang**, saya sarankan jangan sekadar menambal kode yang ada. Jadikan `monitor loop` sebagai **state machine + desired hardware state + verification state** yang jelas.

Struktur yang saya sarankan:

```text
                     ┌──────────────────┐
                     │ Config / Sensor  │
                     └────────┬─────────┘
                              ↓
                    ┌───────────────────┐
                    │  Decision Engine  │
                    │   policy state    │
                    └────────┬──────────┘
                             ↓
                    Desired Hardware State
                             ↓
                    ┌───────────────────┐
                    │   Command Apply   │
                    └────────┬──────────┘
                             ↓
                    ┌───────────────────┐
                    │ Hardware Verify   │
                    └────────┬──────────┘
                       success│failure
                              │
                     ┌────────┴────────┐
                     ↓                 ↓
                  Synced          Retry/Failed
```

Yang paling penting: **jangan menjadikan `engine.state` sekaligus sebagai representasi policy dan status hardware.** Itu sumber kompleksitas terbesar di kode sekarang.

Saya akan rewrite menjadi kira-kira seperti ini.

# Arsitektur Rewrite `monitor.rs`

## 1. Pisahkan tiga jenis state

Gunakan tiga konsep berbeda:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChargePolicyState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    Fault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HardwareTarget {
    ChargingEnabled,
    ChargingDisabled,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncState {
    Unknown,
    Pending,
    Synced,
    Failed,
}
```

Dengan demikian:

```text
ChargePolicyState
    = apa yang seharusnya dilakukan policy

HardwareTarget
    = hardware seharusnya berada pada kondisi apa

SyncState
    = apakah hardware sudah terbukti mengikuti target
```

Contoh:

```text
Policy:
    LimitReached

Target:
    ChargingDisabled

Sync:
    Pending
```

Jauh lebih jelas daripada:

```text
ChargeState::LimitReached
```

yang sekaligus dianggap sebagai kondisi policy dan kondisi hardware.

---

# 2. Decision Engine hanya menentukan policy

Decision engine tidak melakukan command hardware.

```rust
struct Decision {
    state: ChargePolicyState,
    target: HardwareTarget,
    reason: DecisionReason,
}
```

Contoh:

```rust
ChargePolicyState::Charging
    -> HardwareTarget::ChargingEnabled

ChargePolicyState::LimitReached
    -> HardwareTarget::ChargingDisabled

ChargePolicyState::ThermalCutoff
    -> HardwareTarget::ChargingDisabled

ChargePolicyState::Disabled
    -> HardwareTarget::Unmanaged

ChargePolicyState::Offline
    -> HardwareTarget::Unmanaged
```

Jadi engine hanya berkata:

> "Menurut policy, hardware harus berada pada kondisi X."

Ia tidak peduli apakah command berhasil atau gagal.

---

# 3. Buat `HardwareController`

Semua interaksi dengan `control::set_charging()` dipusatkan.

```rust
struct HardwareController {
    target: HardwareTarget,
    sync: SyncState,
    force_apply: bool,

    verification_deadline: Option<Instant>,
    verification_failures: u8,
}
```

Kemudian:

```rust
impl HardwareController {
    fn new() -> Self {
        Self {
            target: HardwareTarget::Unmanaged,
            sync: SyncState::Unknown,
            force_apply: true,
            verification_deadline: None,
            verification_failures: 0,
        }
    }
}
```

Dengan `force_apply: true`, startup selalu melakukan sinkronisasi pertama.

---

# 4. Jangan clear `force_apply` sebelum command berhasil

Gunakan:

```rust
fn apply(&mut self, target: HardwareTarget) -> bool {
    match target {
        HardwareTarget::ChargingEnabled => {
            match control::set_charging(true) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!("Failed to enable charging: {}", e);
                    false
                }
            }
        }

        HardwareTarget::ChargingDisabled => {
            match control::set_charging(false) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!("Failed to disable charging: {}", e);
                    false
                }
            }
        }

        HardwareTarget::Unmanaged => {
            match control::set_charging(true) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!("Failed to restore charging: {}", e);
                    false
                }
            }
        }
    }
}
```

Kemudian:

```rust
if self.apply(target) {
    self.force_apply = false;
    self.sync = SyncState::Pending;
    self.verification_failures = 0;
    self.verification_deadline =
        Some(Instant::now() + VERIFY_DELAYS[0]);
} else {
    self.force_apply = true;
    self.sync = SyncState::Failed;
}
```

Dengan ini:

```text
command gagal
    ↓
force_apply tetap true
    ↓
command akan dicoba lagi
```

Ini memperbaiki bug utama versi sekarang.

---

# 5. Verification harus berdasarkan target, bukan ChargeState

Jangan:

```rust
match state {
    ChargeState::Charging => ...
    ChargeState::LimitReached => ...
}
```

Gunakan:

```rust
fn verify(
    target: HardwareTarget,
    snapshot: &SensorSnapshot,
) -> bool {
    match target {
        HardwareTarget::ChargingEnabled => {
            snapshot.online == Some(true)
                && snapshot.charging_state() == ChargingState::Charging
        }

        HardwareTarget::ChargingDisabled => {
            snapshot.charging_state() != ChargingState::Charging
        }

        HardwareTarget::Unmanaged => {
            true
        }
    }
}
```

Ini jauh lebih bersih.

Policy bisa berubah:

```text
LimitReached
→ ThermalCutoff
```

tetapi target tetap:

```text
ChargingDisabled
```

Tidak perlu menganggap verification lama sebagai state tertentu.

---

# 6. Verification harus memiliki generation/token

Ini penting untuk benar-benar menghilangkan stale verification.

Tambahkan:

```rust
struct Verification {
    generation: u64,
    target: HardwareTarget,
    deadline: Instant,
}
```

Dan controller:

```rust
struct HardwareController {
    target: HardwareTarget,
    sync: SyncState,
    force_apply: bool,

    generation: u64,
    verification: Option<Verification>,
    verification_failures: u8,
}
```

Setiap command baru:

```rust
self.generation += 1;

self.verification = Some(Verification {
    generation: self.generation,
    target,
    deadline: Instant::now() + VERIFY_DELAYS[0],
});
```

Ketika verification dijalankan:

```rust
let Some(v) = &self.verification else {
    return;
};

if v.generation != self.generation {
    return;
}
```

Dengan ini verification lama secara eksplisit tidak mungkin diterapkan terhadap command baru.

---

# 7. Jangan menggunakan `HardwareSyncFailed` sebagai policy state

Saya justru menyarankan menghapus:

```rust
ChargeState::HardwareSyncFailed
```

dari `ChargeState`.

Gunakan:

```rust
SyncState::Failed
```

Karena:

```text
LimitReached
```

adalah policy state.

Sedangkan:

```text
HardwareSyncFailed
```

adalah synchronization state.

Keduanya berbeda domain.

Contoh:

```text
PolicyState:
    LimitReached

Target:
    ChargingDisabled

SyncState:
    Failed
```

Ini jauh lebih informatif.

---

# 8. Retry hardware

Buat satu fungsi:

```rust
fn verification_failed(&mut self) {
    self.verification_failures =
        self.verification_failures.saturating_add(1);

    if self.verification_failures > MAX_VERIFICATION_RETRIES {
        tracing::error!(
            "Hardware synchronization failed after {} retries",
            MAX_VERIFICATION_RETRIES
        );

        self.sync = SyncState::Failed;
        self.verification = None;
        self.force_apply = true;
        return;
    }

    let index = (self.verification_failures as usize)
        .min(VERIFY_DELAYS.len() - 1);

    self.verification = Some(Verification {
        generation: self.generation,
        target: self.target,
        deadline: Instant::now() + VERIFY_DELAYS[index],
    });
}
```

Dengan demikian:

```text
apply
 ↓
verify
 ├── success → Synced
 │
 └── failure
      ↓
    retry 1
      ↓
    retry 2
      ↓
    retry 3
      ↓
    Failed
      ↓
    force_apply
```

---

# 9. Reconfiguration

Ketika config berubah:

```rust
let old_target = controller.target;

let decision = engine.evaluate(&snapshot, &cfg);

if decision.target != old_target {
    controller.invalidate_verification();
    controller.force_apply = true;
}
```

Fungsi:

```rust
fn invalidate_verification(&mut self) {
    self.generation += 1;
    self.verification = None;
    self.verification_failures = 0;
    self.sync = SyncState::Unknown;
}
```

Dengan ini tidak perlu:

```rust
verification_deadline = None;
pending_verification_state = None;
verification_failures = 0;
```

tersebar di banyak tempat.

---

# 10. Scheduler tetap dipisahkan

`AdaptiveScheduler` yang sekarang pada dasarnya sudah bagus.

Tetap gunakan:

```rust
struct AdaptiveScheduler {
    limit: f32,
    resume_limit: f32,
    thermal_cutoff: f32,

    history: VecDeque<SensorSnapshot>,

    ema_cap_rate: f32,
    ema_temp_rate: f32,

    last_interval: Duration,
}
```

Tetapi scheduler hanya menentukan:

```rust
Duration
```

Ia tidak melakukan:

```rust
control::set_charging()
```

dan tidak mengetahui `ChargePolicyState`.

---

# 11. Netlink juga dipisahkan

Buat:

```rust
struct NetlinkMonitor {
    socket: Option<OwnedFd>,
    reconnect_at: Option<Instant>,
    backoff: Duration,
}
```

Dengan fungsi:

```rust
impl NetlinkMonitor {
    fn disconnect(&mut self);

    fn schedule_reconnect(&mut self, now: Instant);

    fn try_reconnect(&mut self, now: Instant) -> bool;

    fn handle_events(&mut self) -> bool;
}
```

Sehingga `run_monitor_loop()` tidak dipenuhi detail:

```rust
create_netlink_socket()
pfds[1]
num_fds
next_netlink_reconnect
netlink_reconnect_backoff
```

---

# 12. Satu jalur reconnect saja

Jangan melakukan reconnect langsung di `POLLERR`.

Pada error:

```rust
netlink.disconnect();
netlink.schedule_reconnect(now);
```

Kemudian hanya satu tempat yang melakukan:

```rust
if netlink.should_reconnect(now) {
    netlink.try_reconnect(now);
}
```

Flow:

```text
POLLERR
  ↓
disconnect
  ↓
schedule reconnect
  ↓
1s
  ↓
failed → 2s
  ↓
failed → 4s
  ↓
failed → 8s
  ↓
...
  ↓
60s
```

Jika berhasil:

```text
success
 ↓
backoff = 1s
reconnect_at = None
```

---

# 13. Monitor loop akhirnya menjadi sederhana

Target akhirnya kira-kira:

```rust
pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    rx: UnixDatagram,
) {
    let mut battery = CachedReader::new();
    let mut engine = DecisionEngine::new();
    let mut scheduler = AdaptiveScheduler::new(...);
    let mut hardware = HardwareController::new();
    let mut netlink = NetlinkMonitor::new();

    loop {
        let cfg = read_config(&config);
        let snapshot = read_snapshot(&mut battery);

        scheduler.observe(&snapshot);

        // 1. Handle verification
        if hardware.verification_due() {
            hardware.verify(&snapshot);
        }

        // 2. Calculate desired policy
        let decision = engine.evaluate(&snapshot, &cfg);

        // 3. Apply hardware target if necessary
        if hardware.needs_apply(decision.target) {
            hardware.apply_target(decision.target);
        }

        // 4. Determine next wakeup
        let timeout = scheduler.next_interval(
            &snapshot,
            netlink.is_connected(),
        );

        // 5. Wait for:
        //    - IPC
        //    - Netlink
        //    - verification deadline
        //    - reconnect deadline
        //    - scheduler timeout

        wait_for_events(...);
    }
}
```

Ini jauh lebih mudah diaudit.

---

## 14. State machine akhirnya menjadi jelas

Contoh startup:

```text
                 START
                   │
                   ▼
             force_apply=true
                   │
                   ▼
              read sensor
                   │
                   ▼
             DecisionEngine
                   │
                   ▼
          desired target = ENABLE
                   │
                   ▼
             set_charging()
              /          \
           success       failure
             │              │
             ▼              ▼
          Pending        force_apply
             │
             ▼
          verify
          /     \
      success   failure
        │          │
        ▼          ▼
     Synced      Retry
                    │
                    ▼
                  Failed
                    │
                    ▼
               force_apply
```

---

## 15. Policy state

Tetap gunakan:

```rust
enum ChargePolicyState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    Fault,
}
```

Policy:

```text
Disabled
    → Unmanaged

Offline
    → Unmanaged

Charging
    → ChargingEnabled

LimitReached
    → ChargingDisabled

ThermalCutoff
    → ChargingDisabled

Fault
    → ChargingDisabled
```

Perhatikan bahwa **Fault sebaiknya tidak otomatis dianggap `Unmanaged`**.

Sensor temperature hilang adalah kondisi safety-critical. Untuk kasus ini:

```text
temperature unavailable
        ↓
Fault
        ↓
ChargingDisabled
```

kemudian setelah sensor valid selama `FAULT_RECOVERY_READS`:

```text
Fault
  ↓
recovered
  ↓
normal policy evaluation
```

---

# 16. Capacity unavailable

Tetap:

```rust
DecisionReason::CapacityUnavailable
```

dan:

```text
temperature valid
capacity invalid
       ↓
do not change charging state
       ↓
Noop
       ↓
short monitoring interval
```

Ini sudah merupakan keputusan yang bagus dari versi Anda.

---

# 17. EMA

Pertahankan perbaikan Anda:

```rust
if prev.charging_state() != current.charging_state() {
    self.ema_cap_rate = 0.0;
    self.ema_temp_rate = 0.0;
}
```

Dan ketika config berubah:

```rust
scheduler.reset_prediction();
```

Ini sudah benar.

---

# 18. Unplugged fallback

Pertahankan:

```rust
const UNPLUGGED_HEARTBEAT: Duration =
    Duration::from_secs(600);

const UNPLUGGED_HEARTBEAT_NO_NETLINK: Duration =
    Duration::from_secs(30);
```

Logic:

```rust
if snapshot.online == Some(false) {
    return if netlink_connected {
        UNPLUGGED_HEARTBEAT
    } else {
        UNPLUGGED_HEARTBEAT_NO_NETLINK
    };
}
```

Ini bagus.

---

# 19. Target akhir struktur file

Saya bahkan akan memecahnya menjadi:

```text
monitor/
├── mod.rs
├── decision.rs
├── hardware.rs
├── scheduler.rs
├── netlink.rs
├── snapshot.rs
└── tests.rs
```

Dengan tanggung jawab:

```text
snapshot.rs
    └── baca battery

decision.rs
    └── policy

hardware.rs
    └── set_charging + verification

scheduler.rs
    └── EMA + adaptive interval

netlink.rs
    └── uevent + reconnect

monitor/mod.rs
    └── orchestration

tests.rs
    └── state-machine tests
```

Ini jauh lebih maintainable daripada satu `run_monitor_loop()` yang mengurus semuanya.

---

# 20. Yang saya anggap sebagai rewrite final

Jadi kalau kita rewrite, saya **tidak menyarankan sekadar mengubah kode terakhir Anda**.

Saya akan melakukan perubahan fundamental:

```text
VERSI SEKARANG

ChargeState
    +
force_apply
    +
verification_deadline
    +
pending_verification_state
    +
HardwareSyncFailed
    +
Netlink reconnect
    +
scheduler
        ↓
satu monitor loop besar
```

menjadi:

```text
                    ┌───────────────┐
                    │ SensorReader  │
                    └───────┬───────┘
                            │
                            ▼
                    ┌───────────────┐
                    │ DecisionEngine│
                    └───────┬───────┘
                            │
                     Decision/Target
                            │
             ┌──────────────┴─────────────┐
             ▼                            ▼
      HardwareController            AdaptiveScheduler
             │                            │
       Apply + Verify                 Next wake
             │
             ▼
        SyncState
```

**Ini yang menurut saya paling tepat untuk rewrite final.**

Dan satu perubahan paling penting dibanding kode Anda sekarang adalah:

> **`HardwareSyncFailed` jangan menjadi bagian dari `ChargeState`; jadikan ia bagian dari `SyncState`.**

Dengan pemisahan itu, bug stale verification, retry, `force_apply`, config reload, dan hardware desync menjadi jauh lebih mudah dikendalikan dan dites.
