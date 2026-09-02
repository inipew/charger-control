# Charger Control — Master Remediation & Production Specification

> Status: authoritative remediation plan for the current `master` implementation.
>
> Goal: make the Android charging-control daemon deterministic, fail-safe, crash-recoverable, testable, and architecturally consistent.

## 1. Non-negotiable invariants

1. **Single hardware writer** — only `HardwareController` may mutate charging-control sysfs nodes.
2. **Policy/Execution separation** — `DecisionEngine` decides *what*; `Scheduler` decides *when*; `HardwareController` decides *how*; `HardwareIo` performs the actual I/O.
3. **Unknown is not Offline** — sensor failure, mixed hardware state, and unavailable hardware must never be silently converted into a safe-looking value.
4. **Crash must be recoverable** — if the daemon owns hardware, persistent ownership must survive process death and startup recovery must happen before normal policy execution.
5. **No unsafe enable on uncertainty** — invalid critical sensors, corrupt ownership, or ambiguous hardware state must never cause an automatic charging enable.
6. **One shutdown path** — IPC shutdown and OS signals must use the same graceful shutdown sequence.
7. **Scheduler is an optimization** — loss of netlink, prediction, EMA, or ETA must not weaken safety policy.

## 2. Critical P0 fixes

### P0.1 IPC shutdown must not call `process::exit()`

The IPC worker must send `ShutdownRequested` to the monitor/event loop and return. The monitor owns shutdown:

```text
IPC / SIGTERM / SIGINT
        ↓
ShutdownRequested
        ↓
stop new policy mutations
        ↓
restore owned hardware
        ↓
verify restoration
        ↓
clear ownership only after success
        ↓
close IPC
        ↓
exit process
```

There must be no `process::exit()` from an IPC worker thread.

### P0.2 Thermal switch must be authoritative

If `thermal_cutoff = false`, thermal policy is completely inactive. If enabled:

```text
T >= max_temp → ThermalCutoff
T <= max_temp - hysteresis → resume
```

Do not evaluate thermal cutoff merely because `max_temp_dc` exists.

### P0.3 Bypass must not bypass the controller

Direct sysfs writes from IPC bypass the ownership/verification lifecycle. Replace direct bypass writes with a `HardwareIntent::ManualBypass` (or equivalent) processed by `HardwareController`.

All hardware mutation must follow:

```text
Command → Event → Policy/Intent → HardwareController → HardwareIo → sysfs
```

### P0.4 Secure IPC

The daemon socket must not be world-writable. Target mode is `0660` with an appropriate privileged owner/group. Hardware-changing commands (`bypass`, `shutdown`, and future mutation commands) must be authorized separately from read-only status commands.

### P0.5 Never silently replace corrupt configuration with defaults

Configuration loading must distinguish:

```text
missing file  → create/use defaults
valid file    → use it
parse error   → reject, preserve file
I/O error     → reject
```

A corrupt configuration must never be silently overwritten by defaults.

## 3. Configuration architecture

Add one authoritative validation entry point:

```rust
impl Config {
    pub fn validate(&self) -> Result<(), ConfigError>;
    pub fn normalize(&mut self);
}
```

Validate both CLI-created configuration and configuration loaded by the daemon. At minimum:

- `50 <= charge_limit <= 100`
- `0 < resume_limit < charge_limit`
- positive thermal hysteresis
- thermal resume threshold below cutoff threshold
- valid/writable log path
- valid scheduler bounds
- valid retry/backoff values

Configuration writes must be atomic:

```text
write .tmp → flush/fsync → rename
```

Config reload must be an event, not an uncontrolled mutation from the IPC thread:

```text
read → parse → validate → normalize → ConfigChanged event → atomic replacement → immediate reconciliation
```

Changing policy-affecting settings must trigger an immediate policy re-evaluation and reset prediction state where appropriate.

## 4. Explicit domain state

Do not overload `StopReason` as actual hardware charging state.

Use separate concepts:

```rust
enum ChargerPresence {
    Online,
    Offline,
    Unknown,
}

enum ChargeState {
    Disabled,
    Offline,
    Charging,
    LimitReached,
    ThermalCutoff,
    ManualBypass,
    Fault,
}

enum HardwareState {
    Unmanaged,
    Applying,
    Enabled,
    Disabled,
    Mixed,
    Unknown,
    Failed,
}

enum OwnershipState {
    NotOwned,
    Acquiring,
    Owned,
    Releasing,
    Recovery,
    RecoveryFailed,
}
```

These state machines must remain separate. Do not collapse policy, hardware, and ownership into one enum.

## 5. Hardware intent

Introduce one normalized hardware intent:

```rust
enum HardwareIntent {
    NormalCharging,
    ChargeLimit,
    ThermalProtection,
    ManualBypass,
    Unmanaged,
}
```

Policy maps to intent. `HardwareController` applies intent and owns verification/retry/reconciliation.

## 6. HardwareController transaction model

Acquisition:

```text
read actual state
    ↓
validate state
    ↓
persist Acquiring
    ↓
write target
    ↓
read actual state
    ↓
verify
    ↓
persist Owned
```

Release:

```text
persist Releasing
    ↓
restore original state
    ↓
verify
    ↓
clear ownership
```

If restoration fails, do **not** clear ownership. Keep the recovery record for the next retry/startup.

## 7. Partial-write recovery

A multi-node write is not successful merely because some nodes succeeded.

After any partial failure:

```text
partial write
    ↓
read all control nodes
    ↓
classify Enabled / Disabled / Mixed / Unknown
    ↓
reconcile toward the safe target
    ↓
verify
```

Required and optional nodes must be explicit in the hardware profile. Failure of a required node fails the operation; optional-node failure is diagnostic unless the profile says otherwise.

## 8. Hardware state semantics

Do not map `Mixed` or `Unknown` to `NoChargingNodeFound`.

Use semantic errors such as:

```rust
enum HardwareStateError {
    NoNode,
    ReadFailed,
    InvalidValue,
    MixedState,
    UnknownState,
}
```

`NoNode`, `ReadFailed`, and `MixedState` require different recovery behaviour.

## 9. Verification model

Separate three dimensions:

```text
ControlState   = what the kernel control nodes report
ElectricalState = charging / idle / discharging behaviour
SensorState     = whether sensor readings are valid
```

Do not universally define `charging disabled` as `current <= 100mA`. Current thresholds are device/profile-specific diagnostics unless proven to be authoritative for that hardware.

Verification must be generation-aware so an old asynchronous verification cannot overwrite a newer intent.

## 10. Ownership persistence

Ownership state must be durable and atomic. Persist at least:

```text
format/version
operation_id
generation
phase
original_state
target_state
timestamp
```

Recommended journal lifecycle:

```text
Acquiring → Owned → Releasing → cleared
```

If the daemon crashes at any phase, startup recovery must inspect the record before normal policy begins.

If ownership storage is corrupt:

```text
do not delete automatically
do not take ownership
do not automatically enable charging
enter RecoveryCorrupt / safe degraded state
```

## 11. Startup order

The daemon must use this order:

```text
process lock
 ↓
load + validate config
 ↓
initialize logger
 ↓
initialize persistence/profile/I/O
 ↓
recover stale ownership
 ↓
probe hardware capabilities
 ↓
initialize reader/policy/scheduler
 ↓
start IPC/netlink
 ↓
start event loop
```

Normal policy execution must not start before ownership recovery succeeds or enters an explicit safe recovery-failed state.

## 12. Battery reader

Use one canonical `BatteryReader` abstraction. Remove duplicate reader paths with different semantics.

All sensor paths must come from `HardwareProfile`; generic reader code must not contain vendor-specific hard-coded paths.

Recommended snapshot:

```rust
struct SensorSnapshot {
    capacity_pct: Option<u8>,
    temp_dc: Option<i32>,
    current_ma: Option<i32>,
    online: Option<bool>,
    charging: Option<bool>,
    ts: Instant,
}
```

Critical sensor errors must not be converted to `0` using `unwrap_or(0)`. Preserve `None`/error semantics and let policy enter the appropriate fault/degraded state.

## 13. Current handling

Keep current unit explicit. Any `i64 → i32` conversion must be checked.

Separate:

```text
battery current
input current
```

Vendor current sign convention must be defined by the profile. Policy should consume semantic charging/discharging information rather than raw vendor sign assumptions.

## 14. Presence detection

Preferred resolution:

```text
authoritative online node
    ↓ if profile has none
current-based fallback
    ↓ if insufficient evidence
Unknown
```

If an authoritative node exists but cannot be read, report `Unknown`; do not hide the failure by falling back to a weaker signal.

Offline transitions should use debounce/hysteresis. Online transitions may be faster but must still avoid transient noise.

## 15. Policy state machine

Recommended transitions:

```text
Disabled → Offline → Charging → LimitReached → Charging
                         │
                         └→ ThermalCutoff → Charging
```

Any critical sensor/hardware uncertainty can enter `Fault`.

Fault recovery requires consecutive valid observations (for example three) before normal policy resumes.

Policy priority:

```text
Shutdown
  > Recovery
  > Disabled
  > Hardware fault
  > Sensor fault
  > Offline
  > Thermal protection
  > Manual bypass
  > Charge limit
  > Normal charging
```

The exact priority can be adjusted only with an explicit safety rationale and tests.

## 16. Charge-limit hysteresis

Invariant:

```text
resume_limit < charge_limit
```

Transitions:

```text
Charging + capacity >= charge_limit → LimitReached
LimitReached + capacity <= resume_limit → Charging
```

Boundary tests must cover exactly-equal values.

## 17. Scheduler

Scheduler answers only **when** to evaluate. It must never create a hardware decision.

Prediction/EMA is an optimization. If prediction is invalid, use a conservative fallback interval.

EMA must reject/reset on:

- invalid or impossible time delta
- implausible capacity jumps
- unplug/replug
- long sleep gaps
- relevant config changes

Capacity is an integer fuel-gauge estimate; do not interpret every 1% step as a continuous physical rate.

## 18. Netlink

Netlink is an acceleration mechanism, not a source of truth.

```text
uevent → invalidate/re-evaluate → read actual state
```

Polling fallback must remain safe if Netlink disappears.

Handle `poll()` correctly:

```text
> 0 → events
= 0 → timeout
< 0 → error / EINTR handling
```

Check `POLLERR`, `POLLHUP`, and `POLLNVAL` for sockets. Prefer RAII `OwnedFd` instead of manual `RawFd` ownership. Let the kernel assign Netlink port ID when appropriate.

## 19. Event architecture

Use one serialized event loop for state mutation:

```rust
enum DaemonEvent {
    SensorChanged,
    TimerExpired,
    HardwareVerificationDue,
    HardwareRetryDue,
    ConfigReload,
    ManualCommand,
    ShutdownRequested,
    NetlinkFailure,
}
```

IPC, signal handling, and Netlink adapters should produce events. They should not mutate hardware state directly.

## 20. IPC protocol

Move away from ambiguous raw commands toward a versioned request/response protocol containing at least:

```text
version
command
request_id
```

Response should contain:

```text
version
request_id
status
error_code
message
```

Bound connect/read/write timeouts are mandatory. Reject malformed, oversized, partial, or unknown-version requests safely.

## 21. CLI semantics

CLI flow:

```text
validate input
 ↓
atomically persist config
 ↓
send reload
 ↓
wait for acknowledgement
 ↓
report actual result
```

If daemon is unavailable, the CLI may report that configuration was persisted but must not claim the daemon reloaded it.

## 22. Shutdown and signals

IPC shutdown, SIGTERM, and SIGINT must converge on exactly one `shutdown_sequence()`.

Shutdown must:

1. stop new policy mutations;
2. stop/disable retries;
3. restore owned hardware;
4. verify restoration;
5. persist unresolved ownership if restoration fails;
6. close IPC/resources;
7. exit only from the main lifecycle owner.

## 23. Capabilities and profiles

Profiles should explicitly describe:

```text
charging_enable capability
input_suspend capability
required/optional control nodes
node priority
sensor paths
current sign/unit
verification strategy
```

Generic profile fallback must be conservative. Presence of a path alone is not sufficient proof that writing it is safe.

Startup probing should establish actual capabilities where feasible.

## 24. Observability

Expose an immutable `DaemonStatus` containing at least:

```text
daemon state
policy state
decision reason
charger presence
sensor snapshot
desired intent
applied intent
hardware state/sync
ownership state
verification state
retry/backoff
profile/capabilities
netlink health
```

Use operation/correlation IDs for hardware mutations and recovery attempts.

Normal polling belongs at DEBUG/TRACE; state transitions at INFO; hardware failures at WARN/ERROR.

## 25. Error model

Runtime boundaries must use semantic errors. Suggested categories:

```text
ConfigInvalid
ConfigRead
ConfigWrite
NoHardwareNode
HardwareRead
HardwareWrite
HardwareMixedState
HardwareUnknownState
OwnershipCorrupt
OwnershipRecoveryFailed
VerificationFailed
IpcPermissionDenied
IpcProtocolError
IpcTimeout
```

Avoid `unwrap()`/`expect()` on config, IPC, hardware, persistence, and runtime boundaries.

## 26. Testing requirements

### Policy tests

Cover:

- disabled
- offline
- unknown
- charging
- exact charge-limit boundary
- resume boundary
- thermal cutoff
- thermal hysteresis
- thermal disabled
- sensor fault
- fault recovery
- config changes

### Hardware tests

Cover:

- all-node success
- partial write
- total failure
- missing optional node
- missing required node
- mixed state
- external modification
- verification failure
- retry/backoff
- generation invalidation

### Ownership tests

Cover crashes during:

- acquiring
- owned
- releasing
- recovery

Also cover corrupt state, restore failure, and repeated recovery.

### IPC tests

Cover:

- authorization
- malformed input
- oversized input
- timeout
- concurrent clients
- shutdown
- bypass
- protocol versioning

### Integration tests

A mandatory shutdown integration test must prove:

```text
daemon owns hardware
→ shutdown request
→ monitor receives request
→ hardware restored
→ restoration verified
→ ownership cleared
→ daemon exits
```

## 27. Recommended module boundaries

```text
crates/charger-core/
├── battery/
│   ├── reader.rs
│   ├── snapshot.rs
│   └── control.rs
├── config/
│   ├── schema.rs
│   ├── validation.rs
│   └── persistence.rs
├── hardware/
│   ├── controller.rs
│   ├── io.rs
│   ├── profile.rs
│   ├── capability.rs
│   └── verification.rs
├── ownership/
│   ├── state.rs
│   ├── persistence.rs
│   └── recovery.rs
├── policy/
│   ├── engine.rs
│   └── state.rs
├── error.rs
└── time.rs

crates/charger-daemon/
├── event.rs
├── ipc.rs
├── signals.rs
├── monitor.rs
├── scheduler.rs
├── runtime.rs
└── main.rs

crates/charger-ctl/
├── commands/
├── ipc_client.rs
├── config.rs
└── main.rs
```

## 28. Implementation order

### Phase 1 — Safety blockers

1. Fix IPC shutdown lifecycle.
2. Unify signal/IPC shutdown.
3. Enforce thermal cutoff switch.
4. Move bypass into `HardwareController`.
5. Secure IPC permissions/authorization.
6. Stop corrupt-config fallback/overwrite.
7. Add authoritative config validation.

### Phase 2 — Hardware correctness

1. Semantic hardware errors.
2. Required/optional nodes.
3. Partial-write reconciliation.
4. Checked current conversions.
5. Profile-driven paths/capabilities.
6. Canonical battery reader.
7. Verification redesign.

### Phase 3 — Ownership/recovery

1. Atomic persistence.
2. Operation IDs and generations.
3. Explicit recovery states.
4. Corruption handling.
5. Crash tests.

### Phase 4 — Policy/event architecture

1. Separate presence/charge/hardware/ownership state.
2. Introduce normalized hardware intent.
3. Serialize all mutations through the event loop.
4. Make config reload an event.

### Phase 5 — Scheduler/observability

1. Correct Netlink error handling.
2. Conservative polling fallback.
3. EMA reset/validation.
4. Status snapshot.
5. Structured operation logging.

### Phase 6 — Release gate

Run formatting, Clippy, unit tests, integration tests, failure injection, and Android-device validation before declaring a production release.

## 29. Definition of done

The implementation is production-ready only when all of the following are true:

```text
[ ] Single hardware writer
[ ] No worker-thread process::exit
[ ] Unified graceful shutdown
[ ] Shutdown restores owned hardware
[ ] Ownership survives crashes
[ ] Recovery precedes normal policy
[ ] Corrupt ownership is fail-safe
[ ] Config validation is authoritative
[ ] Corrupt config is preserved
[ ] Config writes are atomic
[ ] thermal_cutoff is actually honored
[ ] Unknown != Offline
[ ] Mixed hardware state is explicit
[ ] Partial writes reconcile
[ ] Current conversions are checked
[ ] Profile is source of hardware paths
[ ] Duplicate reader paths removed
[ ] IPC is not world-writable
[ ] Hardware-changing IPC is authorized
[ ] IPC has bounded timeouts
[ ] Netlink is only an accelerator
[ ] Scheduler cannot bypass policy
[ ] Fault mode is fail-safe
[ ] Status exposes real state
[ ] Structured operation IDs exist
[ ] Runtime unwrap/expect hazards removed
[ ] Policy boundary tests pass
[ ] Hardware failure tests pass
[ ] Ownership crash/recovery tests pass
[ ] Shutdown integration test passes
[ ] Android-device validation passes
```

## 30. Final invariant

The architecture must converge to exactly one control path:

```text
PolicyEngine
    │
    │ HardwareIntent
    ▼
HardwareController
    │
    │ ownership + transaction + verification
    ▼
HardwareIo
    │
    ▼
Android sysfs
```

And one lifecycle path:

```text
IPC / Signal / Runtime event
          ↓
      Event Loop
          ↓
      State Machine
          ↓
      Hardware Intent
          ↓
      Controller
          ↓
      Verified State
```

Any future feature that bypasses either path is an architectural regression.
