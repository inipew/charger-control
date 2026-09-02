# ChargerControl — Analysis & Bugfix Roadmap

Folder ini adalah **engineering specification** untuk hardening ChargerControl menuju production-grade Android charging daemon.

Dokumen tidak hanya berisi TODO. Setiap requirement penting harus dapat ditelusuri:

```text
RISK/BUG
  -> REQUIREMENT
  -> INVARIANT
  -> DESIGN
  -> IMPLEMENTATION
  -> TEST
  -> RELEASE CRITERIA
```

## Urutan dokumen

| File | Dokumen | Fokus |
|---|---|---|
| `01.md` | Master Remediation Specification | keseluruhan remediation dan target architecture |
| `02.md` | Architecture Invariants | aturan/invariant yang tidak boleh dilanggar |
| `03.md` | Policy & State Machine | state, transition, priority, hysteresis |
| `04.md` | Hardware Control & Verification | controller, capability, partial write, verification |
| `05.md` | Battery Sensor & Reader | sensor semantics, cached FD, presence, validation |
| `06.md` | Ownership & Crash Recovery | journal, persistence, recovery |
| `07.md` | Runtime & IPC | event loop, IPC, signal, shutdown, Netlink boundary |
| `08.md` | Configuration & CLI | validation, atomic write, migration, reload |
| `09.md` | Scheduler & Netlink | EMA, deadlines, backoff, power efficiency |
| `10.md` | Testing & Release Gate | fault injection, invariant tests, release criteria |
| `11.md` | Android/Vendor Deployment | Magisk, SELinux, vendor kernel, external actors |
| `12.md` | Implementation Roadmap | urutan eksekusi dan Definition of Done |
| `13.md` | Power Supply & Kernel Semantics | property semantics, units, uevent, sensor truth |
| `14.md` | Hardware Capability & Vendor Profile | capability discovery, profile, vendor quirks |
| `15.md` | Desired vs Applied vs Actual State | reconciliation dan external mutation |
| `16.md` | Multi-Actor & Ownership Arbitration | Android/vendor/user/external writer arbitration |
| `17.md` | Suspend, Resume & Doze | sleep, resume reconciliation, wakeup limitations |
| `18.md` | Android & Magisk Lifecycle Contract | install, boot, restart, disable, update, rollback |
| `19.md` | Security Threat Model | IPC, filesystem, TOCTOU, authorization |
| `20.md` | Persistence & Filesystem Integrity | atomic state, journal, corruption, crash recovery |
| `21.md` | Error Taxonomy & Recovery Policy | typed errors, retry, Unknown/Offline/Fault |
| `22.md` | Temporal Correctness | monotonic time, stale events, generation, ordering |
| `23.md` | Configuration Semantics | schema, validation, normalization, transactional reload |
| `24.md` | CLI & IPC API Contract | protocol, idempotency, exit codes, authorization |
| `25.md` | Operations & Troubleshooting Runbook | diagnostics, status, logs, recovery |
| `26.md` | Android Compatibility Matrix | device/kernel/profile evidence dan support level |
| `27.md` | Performance & Power Budget | CPU, wakeups, sysfs I/O, event storm |
| `28.md` | Release Engineering | artifact, CI, upgrade, rollback, release gate |

## Architecture layers

```text
Android / Kernel / Vendor
          |
          v
Power Supply + Hardware Profile
          |
          v
Sensor Snapshot ---- Netlink Events
          |                 |
          +--------+--------+
                   v
             Decision Engine
                   |
             Desired State
                   |
                   v
          Hardware Controller
                   |
          Ownership / Journal
                   |
                   v
          Actual + Verification
                   |
                   +----> Reconciliation
```

## Core invariants

Beberapa invariant lintas dokumen yang wajib dipertahankan:

- **Policy** memutuskan *what*, scheduler memutuskan *when*, controller memutuskan *how*.
- Semua hardware mutation melewati satu controller/ownership boundary.
- Desired state bukan actual state; successful write bukan verification.
- `Unknown != Offline != Fault`.
- Netlink mempercepat reaction, tetapi polling/reconciliation menjaga correctness.
- Config invalid tidak boleh diam-diam menjadi default.
- Persistence harus crash-safe dan memiliki recovery semantics.
- IPC privileged harus least-privilege dan bounded.
- Kernel/hardware safety selalu lebih tinggi daripada user-level charging policy.
- Tidak ada infinite retry/write-fight loop.

## Dependency

```text
01 Master
  ↓
02 Invariants
  ↓
03 Policy
  ↓
04 Hardware
  ↓
05 Sensors
  ↓
06 Ownership
  ↓
07 Runtime/IPC
  ↓
08 Config
  ↓
09 Scheduler/Netlink
  ↓
10 Testing
  ↓
11 Android validation
  ↓
12 Implementation
  ↓
13–28 Deep hardening / production contracts
```

## Source of truth

- `01.md` = scope dan remediation master.
- `02.md` = registry invariant arsitektur.
- `03–09.md` = desain komponen inti.
- `10.md` = proving tests dan release gate.
- `11.md` = real-device/Android deployment constraints.
- `12.md` = urutan implementasi.
- `13–28.md` = hardening contracts yang memperjelas area yang sering menjadi sumber bug production.

Jika implementasi bertentangan dengan invariant, implementasi harus diperbaiki atau invariant diubah dengan alasan teknis yang terdokumentasi dan test yang membuktikan perubahan tersebut.
