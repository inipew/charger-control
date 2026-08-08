Ya. Kalau maksudmu **saat daemon masih hidup `Unmanaged` tidak menyentuh kernel**, tetapi **ketika daemon benar-benar shutdown harus melepaskan kontrol dan mengembalikan charging ke mode normal sistem**, maka sebaiknya `Unmanaged` dan `shutdown cleanup` **dibedakan**.

Jangan membuat:

```rust
Unmanaged => set_charging(true)
```

karena itu akan membuat setiap transisi `Disabled/Offline → Unmanaged` mengubah hardware.

Yang benar:

```text
Daemon aktif
    │
    ├── enabled
    │     ├── Charging      → set_charging(true)
    │     ├── LimitReached  → set_charging(false)
    │     └── ThermalCutoff → set_charging(false)
    │
    └── disabled
          ↓
       Unmanaged
          ↓
   tidak menyentuh kernel


Daemon shutdown
       ↓
   cleanup/restore
       ↓
 set_charging(true)
       ↓
 kernel kembali menerima charging normal
```

### Implementasi

Tambahkan method khusus:

```rust
impl HardwareController {
    pub fn shutdown_restore(&mut self) {
        tracing::info!("Daemon shutting down; restoring normal charging");

        match control::set_charging(true) {
            Ok(()) => {
                tracing::info!("Charging restored successfully");
                self.target = HardwareTarget::Unmanaged;
                self.sync = SyncState::Synced;
                self.force_apply = false;
                self.verification = None;
                self.verification_failures = 0;
            }

            Err(e) => {
                tracing::error!(
                    "Failed to restore normal charging during shutdown: {}",
                    e
                );

                // Jangan menganggap hardware sudah restored.
                self.sync = SyncState::Failed;
                self.force_apply = true;
            }
        }
    }
}
```

Kemudian bagian shutdown:

```rust
if buf[0] == 2 {
    tracing::info!("Monitor loop shutting down");

    hardware.shutdown_restore();

    return;
}
```

Jadi **jangan** lagi:

```rust
if buf[0] == 2 {
    tracing::info!(
        "Monitor loop shutting down; leaving charging hardware untouched."
    );
    return;
}
```

---

## Tapi ada satu detail penting

Kalau `set_charging(true)` pada device kamu berarti:

> enable charging control

maka ini cukup untuk mengembalikan charging.

Tetapi kalau driver/kernel memiliki konsep **charging limit** atau sysfs tertentu, `set_charging(true)` belum tentu sama dengan:

> "hapus semua override daemon dan kembalikan konfigurasi default Android/kernel".

Misalnya implementasi `control::set_charging()` ternyata menulis:

```text
/sys/class/power_supply/battery/input_suspend
```

atau:

```text
/sys/class/power_supply/battery/charging_enabled
```

maka `true` mungkin memang tepat.

Namun kalau `charger_core::battery::control` menyimpan state/override lain, cleanup harus mengembalikan **semua state yang diubah daemon**, bukan sekadar `true`.

---

## Saya juga menyarankan RAII-style cleanup

Untuk daemon seperti ini, jangan hanya mengandalkan IPC shutdown.

Ada beberapa cara daemon bisa mati:

```text
normal shutdown
SIGTERM
SIGINT
panic
crash
kill -9
power loss
```

Untuk normal shutdown/SIGTERM, lakukan:

```text
signal
  ↓
shutdown flag
  ↓
monitor loop berhenti
  ↓
shutdown_restore()
  ↓
exit
```

Tetapi perlu diingat:

**`SIGKILL (kill -9)` tidak bisa ditangani.**

Jadi kalau daemon sedang:

```text
charging disabled
```

dan seseorang melakukan:

```bash
kill -9 <pid>
```

daemon tidak punya kesempatan menjalankan:

```rust
set_charging(true)
```

Kalau requirement kamu adalah **charging harus selalu kembali normal bahkan setelah crash/kill**, arsitekturnya perlu sedikit berbeda.

---

# Desain yang paling aman

Saya justru menyarankan membedakan tiga konsep:

```rust
pub enum HardwareTarget {
    ChargingEnabled,
    ChargingDisabled,
    Unmanaged,
}
```

dengan:

```rust
pub enum ShutdownAction {
    RestoreCharging,
}
```

`Unmanaged` **bukan berarti restore**.

Artinya:

> "daemon tidak mengontrol hardware."

Sedangkan:

```rust
shutdown_restore()
```

berarti:

> "daemon akan keluar, jadi kembalikan charging ke default/normal."

Dengan demikian tidak ada ambiguitas.

### State transition

```text
                    ┌─────────────────┐
                    │    Unmanaged    │
                    │ no kernel write  │
                    └────────┬────────┘
                             │
                   daemon enabled
                             │
                             ▼
                    ┌─────────────────┐
                    │    Charging     │
                    │ set_charging(1) │
                    └────────┬────────┘
                             │
                   limit / thermal
                             │
                             ▼
                    ┌─────────────────┐
                    │     Halted      │
                    │ set_charging(0) │
                    └─────────────────┘


Shutdown dari STATE APAPUN
             │
             ▼
    shutdown_restore()
             │
             ▼
      set_charging(true)
             │
             ▼
     daemon process exit
```

Ini menurut saya paling sesuai dengan requirement kamu:

> **Selama daemon hidup dan disabled → jangan mengubah charging. Saat daemon benar-benar shutdown → restore charging supaya setelah kabel dicas, sistem Android/kernel bisa charging normal lagi.**

Satu catatan: kalau yang kamu maksud dengan **"dikembalikan ke sistem"** adalah bukan sekadar `set_charging(true)`, tetapi **mengembalikan nilai sysfs persis seperti sebelum daemon mengambil kontrol** (misalnya sebelum daemon hidup charging sudah disabled karena sistem/OEM), maka jangan hard-code `true`; kita perlu lihat implementasi `charger_core::battery::control::set_charging()` dan state yang sebenarnya diubahnya.
