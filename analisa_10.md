Ya, **versi sekarang sudah jauh lebih benar** untuk requirement:

> **`Unmanaged` (termasuk daemon disabled/offline) tidak boleh menyentuh charging control kernel.**

Perubahan terpenting sudah tepat: `HardwareTarget::Unmanaged` sekarang **tidak memanggil `set_charging(true)`**.

Namun saya masih melihat **2 bug state-machine penting** dan beberapa hal yang sebaiknya diperbaiki.

### 1. `Fault` recovery masih punya bug

Bagian ini:

```rust
if snapshot.temp_dc.is_none() || snapshot.capacity_pct.is_none() {
    self.fault_recovery_reads = 0;
    self.policy = ChargePolicyState::Fault;
    ...
}

if self.policy == ChargePolicyState::Fault {
    self.fault_recovery_reads += 1;
    ...
}
```

membuat `fault_recovery_reads` **selalu di-reset ke 0 ketika sensor masih invalid**, jadi counter recovery tidak pernah bisa berjalan.

Urutannya:

```text
sensor invalid
    ↓
fault_recovery_reads = 0
policy = Fault
    ↓
sensor valid
    ↓
fault_recovery_reads = 1
    ↓
evaluate berikutnya
fault_recovery_reads = 2
    ↓
evaluate berikutnya
fault_recovery_reads = 3
    ↓
recover
```

Jadi sebenarnya **bisa recovery**, tetapi logika lebih jelas kalau kondisi Fault ditangani **sebelum** invalid-sensor check, supaya state transition eksplisit.

Lebih penting lagi: saat Fault terjadi, Anda menghasilkan:

```rust
HardwareTarget::ChargingDisabled
```

Ini memang fail-safe, tetapi kalau tujuan Anda adalah **Unmanaged benar-benar tidak pernah mengubah kernel**, itu sudah konsisten.

---

### 2. Shutdown Anda masih secara eksplisit `set_charging(true)`

Ini bagian yang perlu diperhatikan:

```rust
if buf[0] == 2 {
    tracing::info!("Monitor loop shutting down via IPC. Restoring hardware control.");
    let _ = charger_core::battery::control::set_charging(true);
    return;
}
```

Secara requirement sebelumnya:

> Unmanaged, termasuk daemon disabled, tidak menyentuh kernel.

Shutdown ini **bukan Unmanaged secara state-machine**, tetapi secara semantik Anda sedang mengatakan:

> ketika daemon mati → restore charging

Kalau requirement Anda sekarang adalah:

**"Daemon disabled/offline/unmanaged harus mempertahankan keadaan hardware apa adanya"**

maka baris ini harus **dihapus**.

Menjadi:

```rust
if buf[0] == 2 {
    tracing::info!("Monitor loop shutting down; leaving charging hardware untouched.");
    return;
}
```

Ini lebih konsisten.

---

## 3. Ada masalah lebih besar pada `force_apply` ketika masuk Unmanaged

Sekarang:

```rust
pub fn apply_target(&mut self, target: HardwareTarget) {
    self.target = target;

    match target {
        ...
        HardwareTarget::Unmanaged => {
            self.force_apply = false;
            self.sync = SyncState::Synced;
            ...
        }
    }
}
```

Ini benar.

Misalnya:

```text
ChargingEnabled
      ↓
cfg.enabled = false
      ↓
Unmanaged
      ↓
apply_target(Unmanaged)
      ↓
TIDAK set_charging()
```

Bagus.

Tetapi ada satu hal penting:

```rust
if decision.target != old_target {
    hardware.invalidate_verification();
    hardware.force_apply = true;
}
```

Ketika:

```text
ChargingDisabled
      ↓
Unmanaged
```

`force_apply = true`, lalu `apply_target(Unmanaged)` hanya mengubah state internal.

**Tidak ada kernel write.**

Itu exactly yang kita inginkan.

---

# 4. Tapi ada masalah semantik `Offline`

Ini:

```rust
if snapshot.online == Some(false) {
    self.policy = ChargePolicyState::Offline;
    return self.build_decision(DecisionReason::ChargerOffline);
}
```

menghasilkan:

```rust
HardwareTarget::Unmanaged
```

Ini bagus jika definisi Anda:

> charger dicabut → daemon menyerahkan kontrol hardware.

Dan ketika charger dicolok kembali:

```text
Offline / Unmanaged
        ↓
online = true
        ↓
Charging / LimitReached / ThermalCutoff
        ↓
hardware control aktif lagi
```

Maka daemon akan kembali mengambil alih.

Ini desain yang masuk akal.

---

# 5. Ada bug pada `ChargingEnabled` verification

Anda sekarang menggunakan:

```rust
matches!(
    snapshot.charging_state(),
    ChargingState::Charging | ChargingState::Full
)
```

Ini bagus dibanding versi sebelumnya karena `Full` tidak dianggap gagal.

Tetapi ada kasus:

```text
capacity = 80
status = NotCharging
online = true

set_charging(true)
```

Lalu kernel membutuhkan waktu untuk berubah.

Verification pertama:

```text
500ms → NotCharging
```

gagal.

Kemudian:

```text
1s → NotCharging
```

gagal.

Kemudian:

```text
2s → NotCharging
```

gagal.

Setelah itu:

```rust
verification_failures > MAX_VERIFICATION_RETRIES
```

menjadi `Failed`.

Itu sebenarnya masih oke, karena `needs_apply()` akan mencoba lagi.

Tetapi Anda perlu sadar bahwa retry Anda sekarang:

```text
apply
 ↓
500ms
 ↓
verify
 ↓
1s
 ↓
verify
 ↓
2s
 ↓
verify
 ↓
Failed
 ↓
scheduler interval
 ↓
apply lagi
```

Kalau kernel Android Anda memang lambat mengubah status battery, ini bisa menghasilkan loop apply/verify yang agak agresif.

Saya akan mempertahankan mekanisme ini, tetapi menambahkan **backoff setelah failure** jika nantinya terlihat di log.

---

# 6. `NetlinkMonitor` sekarang lebih bagus

Perubahan:

```rust
pub fn handle_events(&mut self, now: Instant)
```

tidak lagi mengembalikan `bool`.

Ini lebih bersih karena netlink sekarang hanya bertugas:

```text
receive event
    ↓
debounce
    ↓
next_deadline()
```

Sedangkan monitor loop yang menentukan:

```text
harus evaluate atau tidak
```

Ini pemisahan tanggung jawab yang bagus.

Namun ada satu detail:

```rust
if let Some(nd) = netlink.next_deadline() {
    if loop_now >= nd {
        if netlink.debounce_due(loop_now) {
            should_evaluate = true;
            break;
        }

        if netlink.should_reconnect(loop_now) {
            netlink.try_reconnect(loop_now);
        }
    }
```

Jika `nd` berasal dari `reconnect_at`, `try_reconnect()` bisa berhasil dan:

```rust
next_deadline()
```

menjadi `None`.

Anda sudah memperbaikinya dengan:

```rust
if let Some(new_nd) = netlink.next_deadline() {
    if new_nd > loop_now {
        next_wake = next_wake.min(new_nd);
    }
}
```

Jadi bagian ini **sudah jauh lebih aman**.

---

# 7. Ada satu hal yang justru saya sarankan diubah: `shutdown`

Saya akan membuat shutdown sepenuhnya konsisten dengan konsep ownership.

### Sekarang

```rust
if buf[0] == 2 {
    tracing::info!(
        "Monitor loop shutting down via IPC. Restoring hardware control."
    );
    let _ = charger_core::battery::control::set_charging(true);
    return;
}
```

### Saya sarankan

```rust
if buf[0] == 2 {
    tracing::info!(
        "Monitor loop shutting down; leaving charging hardware untouched."
    );
    return;
}
```

Dengan begitu:

```text
Daemon running
      │
      ├── ChargingEnabled → kernel ON
      │
      ├── ChargingDisabled → kernel OFF
      │
      └── Unmanaged → kernel UNTOUCHED
                       │
                       ├── disabled
                       ├── offline
                       └── shutdown
```

Ini jauh lebih mudah dipahami.

---

# 8. Tetapi ada satu pertanyaan desain yang sangat penting

Anda harus menentukan apakah:

> **Daemon disabled = Unmanaged**

atau:

> **Daemon disabled = ChargingEnabled**

Dari kode Anda sekarang, Anda memilih:

```rust
if !cfg.enabled {
    self.policy = ChargePolicyState::Disabled;
}
```

kemudian:

```rust
Disabled => Unmanaged
```

Artinya:

```text
config.enabled = false
        ↓
Unmanaged
        ↓
TIDAK set_charging(true)
        ↓
TIDAK set_charging(false)
```

Ini menurut saya **pilihan yang paling aman** untuk sebuah daemon charging controller.

Karena disabled berarti:

> "daemon tidak berhak mengontrol charging."

Bukan:

> "daemon memerintahkan charging ON."

---

# 9. Saya juga akan mengubah komentar `policy_to_target`

Supaya kontrak arsitekturnya sangat jelas:

```rust
fn policy_to_target(&self, policy: ChargePolicyState) -> HardwareTarget {
    match policy {
        // Daemon relinquishes hardware ownership.
        // HardwareController MUST NOT call set_charging()
        // for this target.
        ChargePolicyState::Disabled
        | ChargePolicyState::Offline => HardwareTarget::Unmanaged,

        ChargePolicyState::Charging => HardwareTarget::ChargingEnabled,

        ChargePolicyState::LimitReached
        | ChargePolicyState::ThermalCutoff
        | ChargePolicyState::Fault => HardwareTarget::ChargingDisabled,
    }
}
```

Ini lebih jelas daripada enam branch terpisah.

---

# 10. Ada satu hal lagi: `Fault` vs `Unmanaged`

Sekarang Anda punya:

```text
Fault → ChargingDisabled
```

Ini berarti ketika sensor rusak:

```rust
set_charging(false)
```

dipanggil.

**Itu memang fail-safe**, dan saya justru menyarankan tetap seperti itu.

Jangan ubah Fault menjadi Unmanaged kalau tujuan Anda adalah proteksi.

Karena:

```text
Unmanaged:
    jangan sentuh hardware

Fault:
    daemon tahu ada kondisi abnormal
    → fail-safe charging OFF
```

Dua state ini memiliki semantic yang berbeda.

---

## Kesimpulan

**Versi terbaru sudah benar secara fundamental.**

Yang paling penting sekarang:

| Kondisi          | Target                | Kernel                |
| ---------------- | --------------------- | --------------------- |
| Enabled + normal | `ChargingEnabled`     | `set_charging(true)`  |
| Limit tercapai   | `ChargingDisabled`    | `set_charging(false)` |
| Thermal cutoff   | `ChargingDisabled`    | `set_charging(false)` |
| Sensor fault     | `ChargingDisabled`    | `set_charging(false)` |
| Daemon disabled  | `Unmanaged`           | **tidak disentuh**    |
| Charger offline  | `Unmanaged`           | **tidak disentuh**    |
| Daemon shutdown  | sebaiknya `Unmanaged` | **tidak disentuh**    |

**Satu perubahan yang paling saya rekomendasikan sekarang adalah menghapus `set_charging(true)` dari shutdown.**

Dengan itu, kontrak arsitekturnya menjadi sangat bersih:

> **`HardwareController` hanya menulis ke kernel ketika target-nya `ChargingEnabled` atau `ChargingDisabled`. `Unmanaged` adalah true relinquish-control state dan tidak pernah melakukan write ke charging backend.**

Dan saya akan mempertahankan `Fault → ChargingDisabled`, karena itu adalah jalur fail-safe, bukan relinquish-control.

