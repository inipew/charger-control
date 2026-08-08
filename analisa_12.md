Ya, **arahnya sudah benar**, tetapi ada satu hal penting: `shutdown_restore()` yang memanggil `control::set_charging(true)` memang akan membuat daemon melepas kontrol dan mengaktifkan charging normal sebelum keluar.

Namun saya akan ubah sedikit supaya semantik **shutdown benar-benar aman dan konsisten**.

### 1. `shutdown_restore()` sebaiknya hanya restore jika daemon memang pernah mengambil kontrol

Saat ini:

```rust
pub fn shutdown_restore(&mut self) {
    tracing::info!("Daemon shutting down; restoring normal charging");

    match control::set_charging(true) {
```

Artinya **setiap shutdown daemon selalu menulis `charging=true`**, bahkan kalau daemon selama hidupnya belum pernah melakukan intervensi.

Lebih baik tambahkan flag:

```rust
pub struct HardwareController {
    pub target: HardwareTarget,
    pub sync: SyncState,
    pub force_apply: bool,

    generation: u64,
    verification: Option<Verification>,
    verification_failures: u8,

    owns_hardware: bool,
}
```

`new()`:

```rust
Self {
    target: HardwareTarget::Unmanaged,
    sync: SyncState::Unknown,
    force_apply: true,
    generation: 0,
    verification: None,
    verification_failures: 0,
    owns_hardware: false,
}
```

Kemudian ketika daemon benar-benar mengubah charging:

```rust
fn mark_apply_success(&mut self, target: HardwareTarget) {
    self.force_apply = false;
    self.sync = SyncState::Pending;
    self.verification_failures = 0;

    self.owns_hardware = true;

    self.generation += 1;

    self.verification = Some(Verification {
        generation: self.generation,
        target,
        deadline: Instant::now() + VERIFY_DELAYS[0],
    });
}
```

Dan `Unmanaged`:

```rust
HardwareTarget::Unmanaged => {
    tracing::debug!(
        "Entering Unmanaged state; charging hardware left untouched"
    );

    self.force_apply = false;
    self.sync = SyncState::Synced;
    self.verification = None;
    self.verification_failures = 0;
}
```

### 2. Shutdown restore

Saya lebih menyarankan:

```rust
pub fn shutdown_restore(&mut self) {
    if !self.owns_hardware {
        tracing::info!(
            "Daemon shutting down without hardware ownership; leaving charging untouched"
        );
        return;
    }

    tracing::info!(
        "Daemon shutting down; restoring normal charging"
    );

    match control::set_charging(true) {
        Ok(()) => {
            tracing::info!(
                "Charging control restored; daemon relinquishing hardware ownership"
            );

            self.target = HardwareTarget::Unmanaged;
            self.sync = SyncState::Synced;
            self.force_apply = false;
            self.verification = None;
            self.verification_failures = 0;
            self.owns_hardware = false;
        }

        Err(e) => {
            tracing::error!(
                "Failed to restore normal charging during shutdown: {}",
                e
            );

            self.sync = SyncState::Failed;
            self.force_apply = true;
        }
    }
}
```

Dengan ini lifecycle-nya menjadi:

```text
Daemon start
    │
    ▼
Unmanaged
    │
    │ policy membutuhkan kontrol
    ▼
ChargingEnabled / ChargingDisabled
    │
    │ daemon owns charging control
    ▼
Daemon shutdown
    │
    ▼
set_charging(true)
    │
    ▼
Unmanaged
    │
    ▼
exit
```

Jadi setelah daemon mati:

```text
charger dicolok
      ↓
kernel/charger driver
      ↓
normal charging
```

**bukan**:

```text
daemon mati
      ↓
charging tetap disabled
      ↓
colok charger → tidak ngecas
```

---

### 3. Ada satu masalah yang lebih penting di kode kamu

Bagian ini:

```rust
if ipc_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
    tracing::error!("IPC socket error/hangup. Exiting monitor loop.");
    return;
}
```

akan keluar **tanpa `shutdown_restore()`**.

Jadi sekarang kamu punya dua jalur:

```text
IPC shutdown command
        ↓
shutdown_restore()
        ↓
return
```

tetapi:

```text
IPC error/HUP
        ↓
return
        ↓
TIDAK restore
```

Ini berbahaya kalau daemon memang sedang berada pada:

```rust
HardwareTarget::ChargingDisabled
```

Saya akan ubah menjadi:

```rust
if ipc_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
    tracing::error!(
        "IPC socket error/hangup. Restoring charging before exit."
    );

    hardware.shutdown_restore();
    return;
}
```

---

### 4. Lebih bagus lagi: jangan hanya restore pada `buf[0] == 2`

Idealnya **semua exit normal dari `run_monitor_loop()` melewati satu fungsi shutdown**.

Misalnya:

```rust
fn shutdown(mut hardware: HardwareController) {
    tracing::info!("Monitor loop shutting down");
    hardware.shutdown_restore();
}
```

Kemudian:

```rust
if buf[0] == 2 {
    shutdown(hardware);
    return;
}
```

dan:

```rust
if ipc_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
    tracing::error!("IPC socket error/hangup");
    shutdown(hardware);
    return;
}
```

Ini mengurangi kemungkinan ada jalur exit yang lupa restore.

---

## 5. Tapi ada satu kasus yang tidak bisa ditangani dengan `shutdown_restore()`

Ini penting untuk Android daemon.

Kalau proses mati karena:

```text
SIGTERM
SIGINT
```

kamu masih bisa menangani dengan signal handling.

Tetapi kalau:

```text
SIGKILL
kill -9
kernel panic
battery disconnect
power loss
OOM killer dalam kondisi tertentu
crash native
```

Rust tidak bisa menjalankan cleanup setelah proses benar-benar dibunuh oleh `SIGKILL`.

Jadi jangan menganggap:

```rust
shutdown_restore()
```

sebagai **jaminan absolut**.

Untuk charger daemon yang memodifikasi kernel control, desain yang lebih aman adalah:

### Fail-safe ownership

Ketika daemon mengambil kontrol:

```text
daemon
  │
  ├── charging enabled
  │
  └── charging disabled
```

kemudian daemon shutdown:

```text
daemon
  ↓
restore charging = enabled
  ↓
release ownership
  ↓
exit
```

Tetapi kalau daemon crash:

```text
daemon
  ↓
CRASH
  ↓
???
```

Kalau driver/sysfs control yang kamu gunakan **persisten sampai ditulis kembali**, maka charging bisa tetap disabled.

Karena itu, untuk implementasi production, saya justru menyarankan **watchdog/recovery mechanism** di luar proses monitor, atau parent/supervisor yang bisa melakukan recovery.

---

## 6. Satu koreksi kecil pada `DecisionEngine`

Ini:

```rust
ChargePolicyState::Disabled | ChargePolicyState::Offline
    => HardwareTarget::Unmanaged,
```

sudah benar **kalau maksudmu daemon tidak boleh mengubah hardware ketika config disabled atau charger offline**.

Tetapi hati-hati dengan transisi:

```text
ChargingDisabled
      ↓
config disabled
      ↓
Unmanaged
```

`apply_target(Unmanaged)` **tidak mengaktifkan charging**.

Itu memang sesuai komentar kamu:

```rust
// Do NOT touch the kernel charging control.
```

Artinya:

```text
charging disabled oleh daemon
        ↓
user disable daemon
        ↓
charging tetap disabled
```

**sampai daemon shutdown**.

Kalau `cfg.enabled = false` dimaksudkan sebagai **"daemon berhenti mengontrol dan sistem kembali normal"**, maka behavior ini belum sesuai.

Ada dua konsep yang perlu dibedakan:

```text
Unmanaged
    = jangan ubah hardware sekarang

Restore
    = kembalikan hardware ke charging normal
```

Itu bukan hal yang sama.

---

### Jadi saya rekomendasikan state seperti ini

| Kondisi           | Aksi                                                       |
| ----------------- | ---------------------------------------------------------- |
| `Charging`        | `set_charging(true)`                                       |
| `LimitReached`    | `set_charging(false)`                                      |
| `ThermalCutoff`   | `set_charging(false)`                                      |
| `Fault`           | `set_charging(false)`                                      |
| `Offline`         | **jangan sentuh hardware**                                 |
| `Daemon disabled` | **tergantung definisi**, tapi kalau ingin normal → restore |
| Shutdown          | **restore charging**                                       |
| Crash/SIGKILL     | perlu mekanisme eksternal                                  |

**Kesimpulan:** kode yang kamu tambahkan sudah benar untuk kasus `buf[0] == 2`, tetapi saya sangat menyarankan menambahkan `owns_hardware` dan memanggil `shutdown_restore()` juga pada jalur `POLLHUP/POLLERR/POLLNVAL`. Itu membuat ownership hardware jauh lebih jelas dan mencegah daemon meninggalkan charging dalam keadaan disabled ketika keluar secara abnormal melalui jalur IPC.
