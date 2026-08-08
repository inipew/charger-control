Kalau **rewrite**, saya justru akan mengubahnya cukup besar — bukan karena kode sekarang jelek, tetapi karena sekarang sudah terlihat bahwa beberapa concern berbeda masih bercampur di `run_monitor_loop()`.

Target saya bukan "lebih sedikit baris", melainkan:

> **state machine deterministic + event loop sederhana + hardware controller terisolasi + scheduler murni + verification yang eksplisit.**

Saya akan mengarahkannya seperti ini.

---

# 1. Arsitektur rewrite yang saya rekomendasikan

Dari:

```text
run_monitor_loop()
 ├── config
 ├── sensor
 ├── state machine
 ├── scheduler
 ├── netlink
 ├── IPC
 ├── apply command
 ├── verification
 └── logging
```

menjadi:

```text
                    ┌──────────────┐
                    │ Config       │
                    └──────┬───────┘
                           │
                           ▼
┌──────────┐        ┌──────────────┐        ┌──────────────┐
│ Netlink  │───────►│ Event Loop   │◄───────│ IPC          │
└──────────┘        └──────┬───────┘        └──────────────┘
                           │
                           ▼
                    ┌──────────────┐
                    │ SensorReader │
                    └──────┬───────┘
                           │
                           ▼
                    ┌──────────────┐
                    │ Snapshot     │
                    └──────┬───────┘
                           │
              ┌────────────┴────────────┐
              ▼                         ▼
       ┌──────────────┐          ┌──────────────┐
       │ Decision     │          │ Scheduler    │
       │ Engine       │          │              │
       └──────┬───────┘          └──────────────┘
              │
              ▼
       ┌──────────────┐
       │ Controller   │
       └──────┬───────┘
              │
              ▼
       ┌──────────────┐
       │ Verification │
       └──────────────┘
```

Yang paling penting:

**event loop tidak mengambil keputusan.**

Event loop hanya:

```text
event
 ↓
collect snapshot
 ↓
engine.evaluate()
 ↓
apply
 ↓
schedule next wakeup
```

---

# 2. Saya akan pecah menjadi module

Misalnya:

```text
src/
├── monitor/
│   ├── mod.rs
│   ├── loop.rs
│   ├── state.rs
│   ├── decision.rs
│   ├── scheduler.rs
│   ├── snapshot.rs
│   └── verification.rs
│
├── power/
│   ├── mod.rs
│   ├── reader.rs
│   ├── controller.rs
│   └── netlink.rs
│
└── config/
```

Atau kalau ingin lebih domain-oriented:

```text
src/
└── charger/
    ├── domain/
    │   ├── state.rs
    │   ├── snapshot.rs
    │   ├── decision.rs
    │   └── policy.rs
    │
    ├── infrastructure/
    │   ├── sysfs.rs
    │   ├── controller.rs
    │   └── netlink.rs
    │
    └── runtime/
        ├── scheduler.rs
        ├── verification.rs
        └── monitor.rs
```

Saya **lebih menyukai struktur kedua** untuk proyek yang nantinya akan besar.

---

# 3. `SensorSnapshot` harus menjadi pusat data

Saya akan membuatnya seperti ini:

```rust
#[derive(Clone, Debug)]
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temperature_dc: Option<i32>,
    pub current_ma: Option<i32>,
    pub status: Option<BatteryStatus>,
    pub online: Option<bool>,
    pub timestamp: Instant,
}
```

Jangan:

```rust
current_ma: i32
```

karena:

```text
sensor error ≠ 0 mA
```

Begitu juga:

```text
status error ≠ NotCharging
```

---

# 4. Jangan simpan `charging: bool`

Saya malah **tidak akan menyimpan**:

```rust
charging: bool
```

karena itu derived data.

Gunakan:

```rust
impl SensorSnapshot {
    pub fn charging_state(&self) -> Option<bool> {
        match self.status {
            Some(BatteryStatus::Charging) => Some(true),

            Some(BatteryStatus::Discharging)
            | Some(BatteryStatus::NotCharging)
            | Some(BatteryStatus::Full) => Some(false),

            None => None,
        }
    }
}
```

Satu sumber kebenaran.

---

# 5. State machine jangan membawa data recovery

Saya tidak suka:

```rust
Fault { retry_count: u8 }
```

Rewrite:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    Fault,
}
```

Kemudian engine:

```rust
pub struct DecisionEngine {
    state: ChargeState,
    fault_recovery_reads: u8,
}
```

Ini memisahkan:

**state domain**

dari:

**runtime bookkeeping**.

---

# 6. `ChargeCommand` juga saya ubah

Saya akan menggunakan:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeCommand {
    Enable,
    Disable,
    RestoreAutomatic,
    Noop,
}
```

Bukan:

```text
ReleaseControl
```

kecuali backend memang benar-benar punya operasi release.

Semantik harus jelas:

```text
Enable
    → force charging ON

Disable
    → force charging OFF

RestoreAutomatic
    → daemon berhenti memaksa state
```

Kalau Android/kernel Anda **tidak memiliki automatic release API**, maka:

```rust
RestoreAutomatic
```

harus didefinisikan secara eksplisit sebagai fallback.

---

# 7. Decision harus pure

Ini salah satu perubahan terbesar.

Saya ingin:

```rust
pub struct Decision {
    pub next_state: ChargeState,
    pub command: ChargeCommand,
    pub reason: DecisionReason,
}
```

Dan:

```rust
pub fn evaluate(
    &mut self,
    snapshot: &SensorSnapshot,
    cfg: &Config,
) -> Decision
```

**tidak boleh melakukan:**

```text
sysfs write
sleep
poll
logging hardware
```

Decision engine hanya memutuskan.

---

# 8. Bahkan saya akan membuat policy function

Misalnya:

```rust
fn decide(
    state: ChargeState,
    snapshot: &SensorSnapshot,
    cfg: &Config,
) -> Decision
```

Dengan aturan:

```text
disabled
    → RestoreAutomatic

offline
    → Noop

temperature missing
    → Disable / Fault

temperature >= cutoff
    → Disable / ThermalCutoff

capacity >= limit
    → Disable / LimitReached

capacity <= resume
    → Enable / Charging

otherwise
    → Noop
```

Ini membuat state machine sangat mudah dites.

---

# 9. `DecisionEngine` menjadi kecil

Contohnya kira-kira:

```rust
pub struct DecisionEngine {
    state: ChargeState,
    fault_recovery_reads: u8,
}

impl DecisionEngine {
    pub fn evaluate(
        &mut self,
        snapshot: &SensorSnapshot,
        cfg: &Config,
    ) -> Decision {
        if !cfg.enabled {
            self.state = ChargeState::Disabled;

            return Decision::restore_automatic(
                DecisionReason::DaemonDisabled,
            );
        }

        if snapshot.online == Some(false) {
            self.state = ChargeState::Offline;

            return Decision::noop(
                ChargeState::Offline,
                DecisionReason::ChargerOffline,
            );
        }

        if snapshot.temperature_dc.is_none() {
            self.enter_fault();
            return Decision::disable(
                ChargeState::Fault,
                DecisionReason::SensorFault,
            );
        }

        self.evaluate_normal(snapshot, cfg)
    }
}
```

Tidak ada syscall sama sekali.

---

# 10. Controller harus terpisah

Buat:

```rust
pub trait ChargeController {
    fn enable(&mut self) -> Result<()>;
    fn disable(&mut self) -> Result<()>;
    fn restore_automatic(&mut self) -> Result<()>;

    fn verify(&mut self, expected: VerificationTarget)
        -> Result<VerificationResult>;
}
```

Implementasi Android:

```rust
pub struct AndroidChargeController {
    ...
}
```

Dengan begitu engine tidak tahu:

```text
sysfs
ioctl
property
vendor node
```

Ini **upgrade arsitektur paling penting**.

---

# 11. Verification menjadi object/state tersendiri

Jangan:

```rust
verification_deadline: Option<Instant>
pending_verification_state: Option<ChargeState>
```

tersebar di monitor loop.

Saya akan membuat:

```rust
struct PendingVerification {
    command: ChargeCommand,
    expected: VerificationTarget,
    deadline: Instant,
    attempts: u8,
}
```

Misalnya:

```rust
enum VerificationTarget {
    Charging,
    NotCharging,
    Any,
}
```

Kemudian:

```rust
struct VerificationManager {
    pending: Option<PendingVerification>,
}
```

API:

```rust
verification.start(command, now);
verification.is_due(now);
verification.complete(snapshot);
```

---

# 12. Scheduler harus PURE

Ini juga penting.

Scheduler jangan membaca hardware.

Jangan:

```rust
scheduler.read_temperature()
```

Jangan:

```rust
scheduler.read_capacity()
```

Tetap:

```rust
let timeout = scheduler.next_interval(&snapshot);
```

Dan scheduler hanya:

```text
Snapshot
 ↓
prediction
 ↓
timeout
```

Ini sangat mudah di-unit-test.

---

# 13. Saya akan membuang `history: VecDeque<SensorSnapshot>`

Untuk scheduler Anda sebenarnya tidak membutuhkan full snapshot.

Anda hanya membutuhkan data rate.

Buat:

```rust
struct RateEstimator {
    capacity_rate: f32,
    temperature_rate: f32,
}
```

Kemudian:

```rust
struct Scheduler {
    rate: RateEstimator,
    ...
}
```

Jika hanya menggunakan previous sample:

```rust
previous: Option<SamplePoint>
```

lebih sederhana daripada:

```rust
VecDeque<SensorSnapshot>
```

Sekarang Anda hanya menggunakan:

```text
previous sample
EMA
```

bukan seluruh lima snapshot untuk kalkulasi.

Jadi:

```rust
history: VecDeque<_>
```

sebenarnya bisa dihilangkan.

---

# 14. Saya akan buat `SamplePoint`

```rust
struct SamplePoint {
    capacity_pct: Option<u8>,
    temperature_dc: Option<i32>,
    charging: Option<bool>,
    timestamp: Instant,
}
```

Scheduler:

```rust
previous: Option<SamplePoint>,
capacity_rate: Ema,
temperature_rate: Ema,
```

Lebih hemat memory dan lebih jelas.

---

# 15. EMA jangan pakai magic arithmetic langsung

Daripada:

```rust
EMA_ALPHA * rate + (1.0 - EMA_ALPHA) * old
```

buat:

```rust
struct Ema {
    alpha: f32,
    value: Option<f32>,
}
```

API:

```rust
impl Ema {
    fn update(&mut self, sample: f32) -> f32 {
        ...
    }

    fn reset(&mut self) {
        self.value = None;
    }
}
```

Ini jauh lebih mudah dites.

---

# 16. Scheduler prediction saya akan ubah

Sekarang Anda punya:

```text
predicted = distance / rate * 0.5
```

Saya akan membuat explicit:

```rust
struct Prediction {
    eta: Option<Duration>,
    confidence: PredictionConfidence,
}
```

Misalnya:

```rust
enum PredictionConfidence {
    None,
    Low,
    Medium,
    High,
}
```

Tidak perlu langsung kompleks, tapi konsep ini penting.

Karena:

```text
EMA rate = 0.015 %/sec
```

belum tentu berarti prediction reliable.

---

# 17. Event loop menjadi sangat kecil

Target akhirnya:

```rust
pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    ipc: UnixDatagram,
) {
    let mut runtime = MonitorRuntime::new(config, ipc);

    loop {
        let timeout = runtime.next_timeout();

        let event = runtime.wait(timeout);

        match event {
            Event::Timeout => {
                runtime.evaluate();
            }

            Event::BatteryChanged => {
                runtime.evaluate();
            }

            Event::ConfigChanged => {
                runtime.reload_config();
                runtime.evaluate();
            }

            Event::Shutdown => {
                break;
            }

            Event::NetlinkError => {
                runtime.reconnect_netlink();
            }
        }
    }
}
```

**Ini yang saya kejar.**

Bukan 200+ baris di satu loop.

---

# 18. `MonitorRuntime` menjadi orchestrator

```rust
struct MonitorRuntime {
    config: Config,
    reader: CachedReader,
    controller: AndroidChargeController,
    engine: DecisionEngine,
    scheduler: AdaptiveScheduler,
    verification: VerificationManager,
    netlink: NetlinkWatcher,
}
```

Kemudian:

```rust
impl MonitorRuntime {
    fn evaluate(&mut self) {
        let snapshot = self.reader.snapshot();

        let decision =
            self.engine.evaluate(&snapshot, &self.config);

        self.apply(decision);

        self.scheduler.observe(&snapshot);
    }
}
```

Ini sangat jauh lebih mudah dipelihara.

---

# 19. Event juga sebaiknya enum

Daripada event logic tersebar:

```rust
enum MonitorEvent {
    Timeout,
    BatteryChanged,
    ConfigChanged,
    Shutdown,
    NetlinkError,
}
```

Maka `poll()` hanya bertugas menghasilkan:

```rust
Option<MonitorEvent>
```

---

# 20. Netlink watcher sendiri

```rust
struct NetlinkWatcher {
    fd: OwnedFd,
    reconnect_backoff: Duration,
}
```

API:

```rust
impl NetlinkWatcher {
    fn poll_fd(&self) -> RawFd;

    fn drain_events(&mut self) -> NetlinkEvent;

    fn reconnect(&mut self) -> io::Result<()>;
}
```

Jadi `run_monitor_loop()` tidak perlu tahu:

```rust
recv()
MSG_DONTWAIT
SUBSYSTEM=power_supply
ACTION=change
```

Semua itu masuk ke `netlink.rs`.

---

# 21. IPC watcher juga dipisahkan

Bahkan:

```rust
UnixDatagram
```

jangan langsung diproses di monitor.

Buat:

```rust
enum IpcCommand {
    ReloadConfig,
    Shutdown,
}
```

Parser:

```rust
fn receive_command(&self) -> io::Result<IpcCommand>
```

Dengan demikian:

```text
byte 1
byte 2
```

tidak menyebar ke business logic.

---

# 22. Config harus divalidasi sekali

Saya akan membuat:

```rust
struct ValidatedConfig {
    enabled: bool,
    charge_limit: u8,
    resume_limit: u8,
    max_temp_dc: i32,
    thermal_cutoff: bool,
    thermal_resume_hysteresis_dc: i32,
}
```

Kemudian:

```rust
Config
   ↓ validate()
ValidatedConfig
```

Validasi:

```text
resume < limit
limit <= 100
max_temp > 0
hysteresis > 0
hysteresis < max_temp
```

Setelah itu engine tidak perlu terus-menerus defensif terhadap config invalid.

---

# 23. Saya akan menghilangkan `RwLock` dari dalam loop jika memungkinkan

Sekarang setiap iterasi:

```rust
let cfg = config.read()...
```

Kalau config hanya berubah melalui IPC, lebih bagus:

```text
Config manager
     ↓
reload
     ↓
runtime.config = validated_config
```

Jadi hot path menggunakan:

```rust
runtime.config
```

langsung.

Kalau arsitektur program mengharuskan shared config, tetap boleh `Arc<RwLock<_>>`, tetapi jangan membuat decision engine bergantung langsung pada lock.

---

# 24. Testing menjadi jauh lebih kuat

Setelah rewrite, saya bisa menulis:

```rust
#[test]
fn reaches_limit() {}

#[test]
fn resumes_below_limit() {}

#[test]
fn thermal_cutoff() {}

#[test]
fn thermal_hysteresis() {}

#[test]
fn unplugged_is_noop() {}

#[test]
fn disabled_restores_automatic() {}

#[test]
fn sensor_failure_enters_fault() {}

#[test]
fn fault_requires_three_valid_reads() {}

#[test]
fn increasing_limit_releases_limit_state() {}

#[test]
fn decreasing_limit_enters_limit_state() {}
```

Dan scheduler:

```rust
#[test]
fn charging_near_limit_wakes_soon() {}

#[test]
fn stable_battery_sleeps_longer() {}

#[test]
fn unplugged_uses_heartbeat() {}

#[test]
fn unknown_temperature_is_conservative() {}
```

**Ini salah satu alasan utama saya merekomendasikan rewrite.**

---

# 25. Saya akan menggunakan model state transition yang lebih formal

Bahkan bisa dibuat:

```text
                   ┌────────────┐
                   │  Disabled  │
                   └─────┬──────┘
                         │ enabled
                         ▼
                   ┌────────────┐
              ┌────│  Charging  │────┐
              │    └─────┬──────┘    │
              │          │            │
        resume│       limit      thermal
              │          │            │
              │          ▼            ▼
              │   ┌────────────┐ ┌──────────────┐
              └───│LimitReached│ │ThermalCutoff│
                  └────────────┘ └──────────────┘

                   Charging
                      │
                 unplugged
                      ▼
                  Offline

             sensor failure
                      ▼
                   Fault
```

Ini lebih mudah diverifikasi daripada nested `match` + recursion.

---

# 26. Jangan membuat `DecisionEngine` terlalu pintar

Ini jebakan yang perlu dihindari.

Jangan sampai akhirnya:

```rust
DecisionEngine
 ├── read sysfs
 ├── control charging
 ├── netlink
 ├── timer
 ├── config reload
 └── logging
```

Engine harus tetap **pure-ish**.

Ideal:

```text
Input:
    State
    Snapshot
    Config

Output:
    Decision
```

Tidak lebih.

---

# 27. Target akhir saya

Saya ingin file utama kira-kira hanya:

```rust
pub fn run_monitor_loop(
    config: Arc<RwLock<Config>>,
    ipc: UnixDatagram,
) {
    let mut runtime =
        MonitorRuntime::new(config, ipc);

    loop {
        match runtime.wait_for_event() {
            Ok(Event::Timeout) |
            Ok(Event::BatteryChanged) |
            Ok(Event::ConfigChanged) => {
                runtime.evaluate();
            }

            Ok(Event::Shutdown) => break,

            Ok(Event::NetlinkError) => {
                runtime.reconnect_netlink();
            }

            Err(e) => {
                tracing::error!(%e, "Monitor event loop failure");
            }
        }
    }
}
```

**Itu saja.**

Kalau file utama masih 500 baris setelah rewrite, berarti pemisahannya belum cukup bagus.

---

# Urutan rewrite yang saya sarankan

Jangan rewrite sekaligus.

### Phase 1 — Domain

Buat dulu:

```text
snapshot.rs
state.rs
decision.rs
```

dan pastikan semua unit test lulus.

### Phase 2 — Controller

Pindahkan:

```text
set_charging()
verification
```

ke:

```text
controller.rs
verification.rs
```

### Phase 3 — Scheduler

Pindahkan seluruh:

```text
EMA
prediction
adaptive timeout
```

ke:

```text
scheduler.rs
```

dan buat pure.

### Phase 4 — Event sources

Pindahkan:

```text
Netlink
IPC
poll()
```

ke masing-masing module.

### Phase 5 — Runtime

Baru gabungkan:

```text
MonitorRuntime
```

### Phase 6 — Testing

Target minimal:

```text
DecisionEngine      > 90% branch coverage
Scheduler           > 90%
Verification        > 90%
Netlink             integration tests
Controller          Android/device integration tests
```

---

## Kesimpulan

Kalau hanya **memperbaiki kode sekarang**, saya akan mempertahankan ~80–90% strukturnya.

Tetapi kalau pertanyaannya:

> **"Kalau sekalian rewrite supaya fondasinya benar-benar profesional, bagaimana?"**

Saya akan **rewrite arsitektur, bukan rewrite algoritma**.

Algoritma dasarnya tetap:

```text
Sensor
 → State Machine
 → Command
 → Hardware
 → Verification
 → Scheduler
```

Yang berubah adalah boundary-nya:

```text
┌───────────────────────────────────────────────┐
│                 DOMAIN                        │
│ Snapshot → Policy → Decision → State         │
│                 ↑                             │
│              PURE / TESTABLE                  │
└───────────────────────┬───────────────────────┘
                        │
┌───────────────────────▼───────────────────────┐
│               RUNTIME                         │
│ Scheduler → Verification → Event Loop         │
└───────────────────────┬───────────────────────┘
                        │
┌───────────────────────▼───────────────────────┐
│             ANDROID I/O                       │
│ Sysfs / Netlink / Unix IPC / Controller      │
└───────────────────────────────────────────────┘
```

**Menurut saya ini versi yang paling tepat kalau `charger_core` nantinya akan menjadi daemon serius di Android**, bukan sekadar script wrapper. Dengan desain ini, nanti Anda juga jauh lebih mudah menambahkan **battery health policy, charging schedule, overnight charging, temperature curve, vendor-specific controller, logging/metrics, dan simulation mode** tanpa kembali mengotori monitor loop.
