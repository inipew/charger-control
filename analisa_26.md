Ya, **arah P3-nya sudah benar**, bahkan secara arsitektur sudah jauh lebih baik daripada sekadar menambahkan beberapa test. Tetapi ada beberapa hal yang saya sarankan diperbaiki sebelum implementasi, terutama supaya P3 tidak berubah menjadi refactor besar yang justru mengaburkan boundary antara **policy, hardware I/O, persistence, dan daemon runtime**.

Yang paling penting: **jangan menjadikan `HardwareProfile` sebagai tempat semua abstraksi hardware sekaligus**. Profile sebaiknya mendeskripsikan *apa yang tersedia*, bukan melakukan I/O atau menentukan policy.

### 1. Struktur yang saya sarankan

Struktur Anda:

```text
charger-core/
├── policy/
├── hardware/
├── battery/
├── persistence/
└── metrics.rs
```

sudah bagus. Saya akan sedikit ubah menjadi:

```text
charger-core/
├── policy/
│   ├── decision.rs
│   └── state.rs
│
├── hardware/
│   ├── controller.rs
│   ├── profile.rs
│   ├── verification.rs
│   └── error.rs
│
├── battery/
│   ├── reader.rs
│   ├── snapshot.rs
│   └── nodes.rs
│
├── persistence/
│   ├── ownership.rs
│   └── state.rs
│
├── metrics.rs
└── lib.rs
```

Sedangkan:

```text
charger-daemon/
└── monitor/
    ├── mod.rs
    ├── netlink.rs
    ├── runtime.rs
    └── signals.rs
```

Dengan boundary:

```text
                charger-daemon
                      │
        ┌─────────────┼─────────────┐
        │             │             │
      Netlink       Signals       IPC
        │             │             │
        └─────────────┼─────────────┘
                      │
                Runtime/Monitor
                      │
                      ▼
                charger-core
        ┌─────────────┼─────────────┐
        │             │             │
     Policy       Hardware      Persistence
        │             │             │
        │             │             │
        ▼             ▼             ▼
    Decision      Controller    Ownership
                      │
                      ▼
                 Sysfs/Kernel
```

**Intinya:** `charger-core` boleh tahu konsep hardware, tetapi sebaiknya tidak tahu tentang event loop daemon, signal Unix, atau netlink.

---

# P3 #19 — State-machine invariant tests

Ini menurut saya **sangat penting** dan rancangan test Anda sudah tepat.

Tetapi saya akan memperluas invariant-nya.

Misalnya:

### Ownership invariant

```text
IF ownership != Owned
THEN controller MUST NOT write hardware
```

Test:

```rust
#[test]
fn unowned_hardware_never_modified() {
    ...
}
```

### Partial-write invariant

Ini sangat relevan dengan perbaikan P1/P2 sebelumnya:

```text
IF apply() does not fully succeed
THEN state != Synced
```

atau:

```rust
#[test]
fn partial_write_never_becomes_synced() {
    ...
}
```

### Verification invariant

Yang lebih penting:

```text
Synced
    ↓
verification succeeds
    ↓
Synced
```

tetapi:

```text
Synced
    ↓
verification fails
    ↓
NeedsApply / Unknown
```

**Jangan pernah:**

```text
verification failed
        ↓
assume synced
```

### Retry invariant

```text
apply failed
    ↓
retry scheduled
    ↓
must not report stable/synced
```

Saya bahkan akan menambahkan:

```rust
#[test]
fn verification_failure_invalidates_synced_state() {}

#[test]
fn retry_never_bypasses_ownership_check() {}

#[test]
fn successful_verification_is_required_for_synced() {}

#[test]
fn failed_apply_cannot_publish_partial_state() {}

#[test]
fn stale_snapshot_cannot_prove_current_hardware_state() {}
```

Yang terakhir penting karena Anda sebelumnya sedang memperbaiki **snapshot/sensor verification**.

---

# P3 #20 — Fault injection

Di bagian ini saya justru menyarankan **jangan menggunakan permission filesystem sebagai mekanisme utama fault injection**.

Contohnya:

> simulate EIO by modifying permissions

Kurang ideal.

Karena test menjadi:

```text
test
 ↓
filesystem
 ↓
permission
 ↓
OS behavior
 ↓
actual error
```

Padahal yang ingin Anda test adalah:

```text
HardwareController
 ↓
I/O failed
 ↓
state transition
```

Lebih bagus membuat abstraction kecil.

Misalnya konsep:

```rust
trait HardwareIo {
    fn read(&self, node: &str) -> Result<String, HardwareError>;
    fn write(&self, node: &str, value: &str) -> Result<(), HardwareError>;
}
```

Production:

```text
SysfsIo
   ↓
real /sys/...
```

Test:

```text
MockHardwareIo
   ↓
in-memory nodes
   ↓
inject failure
```

Kemudian test bisa secara eksplisit mengatakan:

```text
write charging_enable -> OK
write input_suspend   -> EIO
```

dan memastikan:

```text
apply()
 ↓
first write OK
second write EIO
 ↓
NOT Synced
```

Ini jauh lebih deterministic.

---

# Bahkan lebih bagus: FaultPlan

Untuk testing kompleks, Anda bisa membuat sesuatu seperti:

```rust
enum Fault {
    ReadFailed,
    WriteFailed,
    VerificationFailed,
    NodeMissing,
    PermissionDenied,
}
```

atau test backend:

```rust
struct MockHardware {
    nodes: HashMap<NodeId, String>,
    failures: Vec<InjectedFailure>,
}
```

Sehingga bisa menguji:

```text
write #1 → success
write #2 → EIO
write #3 → success
```

Ini akan sangat berguna untuk memastikan **atomicity semantik** dari controller Anda.

---

# P3 #21 — Logging & Metrics

Konsep `Metrics` Anda bagus, tetapi saya sarankan **jangan membuat Metrics terlalu dekat dengan HardwareController**.

Misalnya jangan membuat controller selalu harus:

```rust
metrics.hardware_apply_success += 1;
```

Karena itu membuat business logic bergantung kepada instrumentation.

Lebih bersih:

```rust
HardwareController
        │
        ├── returns ApplyResult
        │
        ▼
Runtime
        │
        └── metrics.record(result)
```

Atau gunakan event:

```rust
enum ControllerEvent {
    ApplyStarted,
    ApplySucceeded,
    ApplyFailed,
    VerificationSucceeded,
    VerificationFailed,
    OwnershipLost,
}
```

Kemudian metrics/logging bisa mengonsumsi event tersebut.

---

## Metrics yang saya sarankan

Bukan hanya:

```text
hardware_apply_success
verification_failure
sensor_faults
```

tetapi kira-kira:

```text
apply_attempts
apply_successes
apply_failures

verification_attempts
verification_successes
verification_failures

retry_scheduled
retry_exhausted

ownership_acquire_success
ownership_acquire_failure
ownership_write_failure

partial_apply_detected

sensor_read_success
sensor_read_failure
sensor_invalid

netlink_events
netlink_reconnects

state_transitions
```

Dan kalau ingin lebih profesional:

```text
apply_duration_ms
verification_duration_ms
retry_delay_ms
```

Tetapi **jangan logging setiap polling loop**.

---

# Logging juga harus dibedakan dari metrics

Contoh:

```text
INFO:
ownership acquired

DEBUG:
verification passed

WARN:
verification failed, retry scheduled

WARN:
partial hardware apply detected

ERROR:
ownership persistence failed

ERROR:
hardware node unavailable
```

Sedangkan metrics:

```text
verification_failures = 3
partial_apply = 1
retry_scheduled = 3
```

Jadi log menjelaskan **kejadian**, metrics menunjukkan **pola**.

---

# P3 #22 — HardwareProfile

Ini bagian yang menurut saya perlu **sedikit diubah** dari proposal Anda.

Jangan membuat:

```rust
struct HardwareProfile {
    charging_nodes: ...,
    suspend_nodes: ...,
    capacity_path: ...,
    ...
}
```

menjadi tempat seluruh behavior.

Lebih baik:

```rust
pub struct HardwareProfile {
    pub name: &'static str,

    pub charging: ChargingNodes,
    pub battery: BatteryNodes,
    pub suspend: SuspendNodes,

    pub capabilities: HardwareCapabilities,
}
```

Misalnya:

```rust
pub struct HardwareCapabilities {
    pub supports_input_suspend: bool,
    pub supports_charging_enable: bool,
    pub supports_current_now: bool,
}
```

Dengan demikian controller bisa membedakan:

```text
node tidak ada
```

dari:

```text
hardware memang tidak mendukung feature
```

Ini penting untuk Android karena `/sys/class/power_supply/...` sangat vendor/kernel dependent.

---

# Jangan pakai `&'static HardwareProfile`

Saya juga tidak terlalu menyukai:

```rust
CachedReader::new(&'static HardwareProfile)
```

Tidak perlu dipaksa `'static`.

Lebih fleksibel:

```rust
CachedReader<'a> {
    profile: &'a HardwareProfile,
}
```

atau bahkan:

```rust
CachedReader {
    profile: Arc<HardwareProfile>,
}
```

tergantung ownership.

Untuk production, profile memang bisa berupa static constant:

```rust
pub static GENERIC_PROFILE: HardwareProfile = ...;
```

tetapi API-nya tidak harus memaksa static lifetime.

---

# Yang lebih penting: profile ≠ auto-detection

Saya setuju dengan rencana:

> `GENERIC_PROFILE`

tetapi saya **tidak menyarankan P3 langsung membuat auto-detection hardware**.

Lebih aman:

```text
configuration
     ↓
profile name
     ↓
HardwareProfile
```

misalnya:

```text
generic
mtk-generic
rosemary
sweet
```

daripada:

```text
detect random sysfs nodes
      ↓
guess hardware
      ↓
apply configuration
```

Auto-detection bisa menjadi pekerjaan berikutnya.

---

# Saya juga akan menambahkan Capability layer

Ini menurut saya penting untuk proyek Anda.

Misalnya profile:

```rust
pub struct HardwareCapabilities {
    pub charging_control: bool,
    pub input_suspend: bool,
    pub current_measurement: bool,
    pub temperature: bool,
}
```

Kemudian:

```text
HardwareProfile
       │
       ├── nodes
       └── capabilities
```

Jadi:

```text
current_now missing
```

tidak otomatis berarti:

```text
current = 0
```

tetapi:

```text
current = Unknown
capability.current_measurement = false
```

Ini konsisten dengan perbaikan P2 Anda tentang **sensor sanity validation** dan semantic verification.

---

# Satu hal yang belum ada: fake clock

Karena P2 Anda memiliki:

* retry
* deadline
* backoff
* verification deadline
* netlink backoff

maka P3 sebaiknya juga punya **Clock/Time abstraction**.

Jangan membuat unit test harus benar-benar:

```rust
sleep(Duration::from_secs(2));
```

Lebih bagus:

```rust
trait Clock {
    fn now(&self) -> Instant;
}
```

Production:

```text
SystemClock
```

Test:

```text
FakeClock
```

Kemudian:

```text
retry at t=1s
advance clock 1s
retry fires
```

tanpa menunggu waktu sebenarnya.

Ini akan sangat membantu untuk menguji P2 #15–17.

---

# Saya akan mengubah P3 menjadi seperti ini

## 🟢 P3 — Engineering Quality

### 19. State-machine invariant tests

Test invariant:

* ownership
* partial write
* verification
* retry
* state transitions
* stale snapshot
* failed persistence
* recovery

### 20. Deterministic fault-injection tests

Tambahkan:

```text
HardwareIo trait
PersistenceIo trait
FakeClock
MockHardwareIo
MockPersistence
```

Test:

* EIO
* ENODEV
* EACCES
* missing node
* partial write
* verification failure
* persistence failure
* crash/restart recovery

**Tidak bergantung pada permission filesystem sebagai mekanisme utama fault injection.**

### 21. Structured logging & metrics

Tambahkan:

```text
Metrics
ControllerEvent
structured tracing
```

Metrics:

```text
apply
verification
retry
ownership
sensor
netlink
state transition
```

Dan jangan membuat core terlalu tergantung pada logging implementation.

### 22. Hardware/vendor profile abstraction

```text
HardwareProfile
HardwareCapabilities
NodeConfig
```

Profile hanya mendeskripsikan:

```text
nodes
capabilities
limits/quirks
```

Bukan business logic.

Mulai dengan:

```text
GENERIC_PROFILE
```

dan jangan melakukan auto-detection dulu.

---

# Tambahan P3 yang menurut saya sangat layak

Saya justru akan menambahkan:

### 23. Fake Clock / deterministic time

Karena P2 sudah memperkenalkan retry/deadline/backoff.

```text
FakeClock
   ↓
advance()
   ↓
retry/deadline test
```

### 24. Property-based testing

Untuk state machine, ini sangat cocok.

Contohnya menghasilkan sequence:

```text
Acquire
Apply
WriteFail
Retry
VerifyFail
NetlinkEvent
OwnershipLost
Acquire
Apply
Verify
```

lalu memastikan invariant tetap benar.

Tidak wajib sekarang, tetapi **sangat bagus untuk charger controller**.

---

# Dan satu catatan arsitektur paling penting

Proposal Anda mengatakan:

> Move `Verification` logic into `charger-core`

Saya setuju **kalau Verification hanya berisi domain logic**.

Tetapi kalau sekarang `Verification` langsung melakukan:

```rust
std::fs::read("/sys/...")
```

jangan sekadar memindahkan file tersebut ke `charger-core`.

Lebih baik:

```text
                charger-core
                     │
             Verification
                     │
                HardwareIo
                     │
          ┌──────────┴──────────┐
          │                     │
      SysfsHardware         MockHardware
          │                     │
       Android               Unit test
```

Dengan begitu `charger-core` benar-benar bisa dites tanpa Android.

---

## Kesimpulan

**Ya, P3 yang Anda tulis sudah berada di jalur yang benar.** Saya akan memberi kira-kira **8.5/10**.

Perbaikan utamanya:

1. **Jangan terlalu cepat membuat refactor besar.**
2. Pisahkan **domain logic** dari **I/O**.
3. Gunakan `HardwareIo` untuk deterministic fault injection.
4. Tambahkan **FakeClock** karena P2 punya retry/deadline/backoff.
5. `HardwareProfile` hanya mendeskripsikan hardware + capability, bukan behavior.
6. Jangan paksa `&'static HardwareProfile`.
7. Metrics jangan terlalu mengotori `HardwareController`.
8. Jangan auto-detect vendor hardware dulu.
9. Tambahkan invariant yang secara eksplisit menjaga hasil perbaikan P1/P2.
10. Kalau memungkinkan, tambahkan property-based testing setelah unit/integration test stabil.

Dengan desain tersebut, arsitekturnya akan menjadi:

```text
                 charger-daemon
                       │
              runtime / netlink
                       │
                       ▼
                 charger-core
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
     Policy         Hardware      Persistence
        │              │              │
        │          Controller       Ownership
        │              │              │
        │         Verification       │
        │              │              │
        │         HardwareIo      PersistenceIo
        │              │              │
        └──────────────┼──────────────┘
                       │
                 deterministic
                     tests
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
       Mock I/O     Fake Clock   Fault Injection
```

**Ini menurut saya lebih tepat untuk target akhir proyek Anda daripada sekadar memindahkan file dari `charger-daemon` ke `charger-core`.** P3 seharusnya membuat sistem *provable/testable*, bukan hanya membuat struktur folder terlihat lebih bersih.
