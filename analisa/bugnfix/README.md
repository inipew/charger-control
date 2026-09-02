# ChargerControl — Analysis & Bugfix Roadmap

Dokumen di folder ini adalah **master reference** untuk hardening dan refactor ChargerControl menuju production-grade Android charging daemon.

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

## Dependency

Implementasi sebaiknya mengikuti:

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
12 Release gate
```

## Prinsip

Dokumen ini adalah spesifikasi engineering, bukan sekadar daftar TODO. Jika implementasi bertentangan dengan invariant, implementasi harus diperbaiki atau invariant harus diubah dengan alasan teknis yang terdokumentasi dan test baru.

## Source of truth

Untuk detail paling luas mulai dari `01.md`. Untuk aturan yang harus selalu dipertahankan lihat `02.md`. Untuk urutan pengerjaan gunakan `12.md`.
