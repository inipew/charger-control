# ChargerControl — Analysis & Bugfix Roadmap

Folder ini adalah **engineering specification** untuk hardening ChargerControl menuju production-grade Android charging daemon.

## 1. Dokumen ini harus actionable

Dokumen 01–28 bukan sekadar style/template atau daftar TODO. Setiap dokumen harus menjelaskan behaviour secara teknis sampai dapat langsung dipakai sebagai dasar implementasi dan review code.

Minimum depth setiap domain:

```text
CURRENT BEHAVIOUR
    ↓
CONCRETE BUG/RISK
    ↓
WHY IT IS WRONG
    ↓
EXECUTION FLOW / FAILURE FLOW
    ↓
TARGET SEMANTICS
    ↓
INVARIANTS
    ↓
IMPLEMENTATION CONTRACT
    ↓
RACE / CRASH / EDGE CASES
    ↓
TEST + FAULT INJECTION
    ↓
ACCEPTANCE CRITERIA
    ↓
DEFINITION OF DONE
```

Dokumen `analisa_10.md` di root adalah salah satu baseline reasoning: pembahasan harus mampu menunjuk state/flow yang salah, menjelaskan konsekuensinya, dan menunjukkan bentuk behaviour/code yang benar. Folder ini memperluas kedalaman tersebut ke seluruh domain.

## 2. Traceability

Setiap requirement penting harus dapat ditelusuri:

```text
RISK/BUG
  → REQUIREMENT
  → INVARIANT
  → DESIGN
  → IMPLEMENTATION
  → TEST
  → RELEASE CRITERIA
```

## 3. Urutan dokumen

| File | Dokumen | Fokus |
|---|---|---|
| `01.md` | Master Remediation Specification | keseluruhan remediation dan target architecture |
| `02.md` | Architecture Invariants | invariant + enforcement + traceability |
| `03.md` | Policy & State Machine | state, precedence, transition, hysteresis, fault |
| `04.md` | Hardware Control & Verification | transaction, ownership, partial write, readback |
| `05.md` | Battery Sensor & Reader | raw sensor, unit/sign, cache, freshness |
| `06.md` | Ownership & Crash Recovery | journal, atomicity, crash points, recovery |
| `07.md` | Runtime & IPC | event loop, authorization, shutdown, concurrency |
| `08.md` | Configuration & CLI | parse, validation, normalization, reload, API |
| `09.md` | Scheduler & Netlink | debounce, reconnect, EMA, deadlines, wakeups |
| `10.md` | Testing & Release Gate | matrix, fuzz, fault injection, real device |
| `11.md` | Android/Vendor Deployment | Magisk, SELinux, vendor actors, boot |
| `12.md` | Implementation Roadmap | phased implementation dan DoD |
| `13.md` | Power Supply & Kernel Semantics | property availability, units, presence, status |
| `14.md` | Hardware Capability & Vendor Profile | capability probing, profile, quirks |
| `15.md` | Desired vs Applied vs Actual State | state model dan reconciliation |
| `16.md` | Multi-Actor & Ownership Arbitration | external writers dan bounded convergence |
| `17.md` | Suspend, Resume & Doze | missed deadlines, freshness, wakeup limitations |
| `18.md` | Android & Magisk Lifecycle Contract | install, boot, crash, update, rollback |
| `19.md` | Security Threat Model | IPC, filesystem, path safety, DoS |
| `20.md` | Persistence & Filesystem Integrity | atomic state, journal, corruption |
| `21.md` | Error Taxonomy & Recovery Policy | typed errors dan retry semantics |
| `22.md` | Temporal Correctness | monotonic time, ordering, stale result |
| `23.md` | Configuration Semantics | schema, default, migration, reload |
| `24.md` | CLI & IPC API Contract | protocol, idempotency, timeout, exit codes |
| `25.md` | Operations & Troubleshooting Runbook | diagnostics dan recovery procedure |
| `26.md` | Android Compatibility Matrix | device/kernel/profile evidence |
| `27.md` | Performance & Power Budget | CPU, memory, wakeup, I/O, log budget |
| `28.md` | Release Engineering | CI, artifact, upgrade, rollback, release gate |

## 4. Architecture layers

```text
Android / Kernel / Vendor
          |
          v
Power Supply + Hardware Profile
          |
          v
Sensor Snapshot ← Netlink Events
          |
          v
             Decision Engine
                   |
                   v
              Desired Intent
                   |
                   v
          Hardware Controller
             |          |
        Ownership    Verification
             |          |
             +----+-----+
                  v
             Actual State
                  |
                  v
             Reconciliation
```

## 5. Core invariants

- **Policy** memutuskan *what*, scheduler memutuskan *when*, controller memutuskan *how*.
- Semua hardware mutation melewati satu controller/ownership boundary.
- `Desired != Applied != Actual != Verified`.
- `Unknown != Offline != Fault`.
- `Unmanaged` berarti no charging write.
- `Fault` tetap dapat menghasilkan protective disable; jangan menyamakannya dengan Unmanaged.
- Netlink mempercepat reaction, polling/reconciliation menjaga correctness.
- Config invalid tidak boleh diam-diam menjadi default.
- Persistence harus crash-safe dan recovery-aware.
- IPC privileged harus least-privilege dan resource-bounded.
- External writer adalah kemungkinan normal; reconciliation harus bounded.
- Kernel/hardware safety selalu lebih tinggi daripada userspace policy.
- Tidak ada infinite retry/write-fight loop.

## 6. Source of truth

- `01.md` = master scope dan global remediation.
- `02.md` = invariant registry.
- `03–09.md` = core runtime/control design.
- `10.md` = proving test dan release gate.
- `11.md` = Android/vendor deployment contract.
- `12.md` = implementation sequence.
- `13–28.md` = deep hardening contracts.
- `analisa_10.md` = baseline contoh audit state-machine yang konkret; bukan pengganti 01–28.

Jika implementasi bertentangan dengan invariant, implementasi harus diperbaiki atau invariant diubah dengan alasan teknis yang terdokumentasi dan test yang membuktikan perubahan tersebut.
