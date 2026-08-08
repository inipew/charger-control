Ya, **versi ini sudah jauh lebih matang**. Dibanding versi sebelumnya, struktur `AdaptiveScheduler → DecisionEngine → hardware control → verification → Netlink/poll` sekarang sudah cukup jelas dan masuk akal untuk daemon charging Android.

Tetapi saya **belum menganggapnya final/production-ready**. Ada beberapa masalah penting yang masih saya ubah sebelum dipakai sebagai daemon utama.

### Yang sudah bagus

* `OwnedFd` untuk Netlink → jauh lebih aman daripada raw FD manual.
* Ada **adaptive polling** dengan EMA.
* Ada **hysteresis charge limit** (`limit` vs `resume`).
* Ada **thermal hysteresis**.
* Sensor temperature diperlakukan sebagai **safety-critical**.
* Capacity missing tidak langsung mematikan charging.
* Ada **fault recovery** berdasarkan beberapa pembacaan berturut-turut.
* Ada **Netlink event + debounce**.
* Ada **Netlink reconnect + exponential backoff**.
* Ada verification setelah `set_charging()`.
* Config reload tidak perlu restart daemon.
* Saat charger dicabut, scheduler tidak melakukan polling agresif dan memakai heartbeat 10 menit.

Jadi secara arsitektur, ini sudah berada di level yang cukup bagus.

---

# 1. Bug paling penting: verification belum benar-benar melakukan recovery

Bagian ini:

```rust
if verification_mismatch {
    verification_failures = verification_failures.saturating_add(1);

    if verification_failures < MAX_VERIFICATION_FAILURES {
        verification_deadline = Some(Instant::now() + VERIFY_DELAY);
    } else {
        tracing::error!(
            "Verification failed after {} attempts for state {:?}",
            verification_failures,
            state
        );
        verification_failures = 0;
        pending_verification_state = None;
    }
}
```

hanya melakukan:

> "hardware tidak sesuai → coba baca lagi"

Tetapi setelah 3 kali gagal, **tidak ada tindakan terhadap hardware**.

Misalnya:

```text
Daemon:
Charging → LimitReached
set_charging(false)

Hardware:
masih Charging
```

Verification:

```text
500 ms → masih Charging
500 ms → masih Charging
500 ms → masih Charging
```

Kemudian:

```text
Verification failed after 3 attempts
```

dan selesai.

Daemon tetap menganggap:

```text
state = LimitReached
```

padahal hardware sebenarnya:

```text
Charging
```

Ini berbahaya untuk charging limiter.

### Saya sarankan

Setelah verification gagal, jangan langsung dianggap selesai.

Minimal:

```text
command
   ↓
verify
   ↓
mismatch
   ↓
retry
   ↓
mismatch
   ↓
retry
   ↓
mismatch
   ↓
hardware_sync_failed
   ↓
re-issue command
```

Lebih bagus lagi memakai state khusus:

```rust
HardwareSyncFailed
```

atau counter terpisah:

```rust
verification_failures
```

dan melakukan retry command dengan backoff.

---

# 2. `VERIFY_DELAY = 500ms` kemungkinan terlalu agresif untuk Android

Ini:

```rust
const VERIFY_DELAY: Duration = Duration::from_millis(500);
```

tidak selalu cukup untuk memastikan perubahan charging benar-benar terlihat di:

```text
/sys/class/power_supply/*
```

terutama pada Android/vendor kernel.

Bisa terjadi:

```text
set_charging(false)
       ↓
kernel driver
       ↓
charger IC
       ↓
power_supply
       ↓
uevent/status
```

yang tidak selesai dalam 500 ms.

Akibatnya:

```text
command berhasil
↓
500ms
↓
status masih lama
↓
verification mismatch
```

Padahal sebenarnya hardware sedang dalam proses transisi.

Saya lebih suka:

```rust
const VERIFY_DELAY: Duration = Duration::from_secs(1);
```

atau menggunakan beberapa tahap:

```text
500ms
1s
2s
```

Misalnya:

```rust
const VERIFY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
```

Ini lebih cocok dengan hardware Android yang vendor-dependent.

---

# 3. Ada masalah konseptual pada `BatteryStatus::Charging`

Bagian:

```rust
fn is_charging(&self) -> bool {
    matches!(self.status, Some(BatteryStatus::Charging))
}
```

dan:

```rust
fn charging_state(&self) -> Option<bool> {
    self.status
        .map(|status| matches!(status, BatteryStatus::Charging))
}
```

berarti hanya:

```text
Charging = true
```

sedangkan:

```text
Full
NotCharging
Unknown
```

semuanya dianggap:

```text
false
```

Padahal pada Android:

```text
status = NotCharging
```

belum tentu berarti:

> charger tidak terhubung.

Bisa saja:

```text
USB connected
online = true
battery status = NotCharging
```

karena charging memang sedang disabled oleh driver.

Ini penting sekali untuk scheduler.

### Lebih baik

Pisahkan:

```rust
enum ChargingState {
    Charging,
    NotCharging,
    Full,
    Unknown,
}
```

atau minimal:

```rust
status: Option<BatteryStatus>
```

jangan langsung mengubah semua non-`Charging` menjadi `false`.

---

# 4. `online` sebenarnya sudah bagus, tetapi policy-nya bisa dibuat lebih eksplisit

Sekarang:

```rust
if snapshot.online == Some(false) {
    ...
}
```

Ini benar untuk:

```text
charger benar-benar dicabut
```

Tetapi:

```rust
online == None
```

tidak dianggap offline.

Itu bagus untuk safety karena daemon tidak langsung mengambil keputusan berdasarkan sensor yang tidak diketahui.

Namun saya akan menetapkan policy eksplisit:

```text
online = Some(false)
    → Offline

online = Some(true)
    → charger connected

online = None
    → unknown, hold state
```

Jangan biarkan `None` secara implisit bercampur dengan status charging.

---

# 5. Bug kecil tetapi nyata: `debounce_target` tidak dipertahankan antar iterasi

Ini:

```rust
let mut debounce_target: Option<Instant> = None;
```

dibuat ulang setiap outer loop.

Dalam kondisi normal memang tidak menjadi masalah karena ketika deadline tercapai:

```rust
should_evaluate = true;
break;
```

sehingga langsung masuk evaluasi.

Jadi ini **bukan bug fatal**.

Tetapi desainnya agak membingungkan karena event state sementara berada di inner loop.

Saya lebih suka event handling dibuat sebagai:

```text
Netlink event
    ↓
schedule evaluation
    ↓
debounce deadline
    ↓
evaluate
```

dengan `next_event_deadline` yang lifetime-nya jelas.

---

# 6. Netlink reconnect masih bisa diperbaiki

Sekarang pada error:

```rust
match create_netlink_socket() {
    Ok(new_sock) => { ... }
    Err(e) => {
        tracing::warn!("Netlink reconnect failed...")
    }
}
```

Anda mencoba reconnect **langsung sekali**.

Kemudian nanti:

```rust
next_netlink_reconnect
```

baru mengambil alih.

Ini sebenarnya sudah cukup aman, tetapi agak tidak konsisten dengan sistem exponential backoff yang sudah Anda buat.

Saya akan menyederhanakan:

```text
socket error
    ↓
drop socket
    ↓
schedule reconnect
    ↓
1s
    ↓
2s
    ↓
4s
    ↓
8s
...
    ↓
60s
```

Jangan langsung mencoba reconnect di handler error.

Dengan begitu logic reconnect hanya memiliki **satu jalur**.

---

# 7. Masalah penting: perubahan konfigurasi tidak mereset scheduler history

Ketika config berubah:

```rust
scheduler.limit = limit as f32;
scheduler.resume_limit = resume as f32;
scheduler.thermal_cutoff = ...
```

tetapi:

```rust
history
ema_cap_rate
ema_temp_rate
last_interval
```

tetap menggunakan data konfigurasi sebelumnya.

Contoh:

```text
limit lama = 80
limit baru = 90
```

EMA masih berdasarkan trajectory lama.

Biasanya tidak fatal, tetapi bisa menghasilkan interval yang aneh sesaat setelah reload.

Saya akan menambahkan:

```rust
fn reset_prediction(&mut self) {
    self.history.clear();
    self.ema_cap_rate = 0.0;
    self.ema_temp_rate = 0.0;
    self.last_interval = MIN_INTERVAL;
}
```

dan memanggilnya ketika parameter scheduler berubah secara signifikan.

---

# 8. `scheduler.push_sample()` dilakukan sebelum verification selesai

Sekarang:

```rust
scheduler.push_sample(snapshot.clone());
```

dilakukan sebelum verification processing.

Artinya snapshot yang diambil ketika hardware sedang dalam transisi setelah command tetap masuk ke EMA.

Contoh:

```text
80%
Charging
↓
set_charging(false)
↓
500ms
↓
status transition
↓
snapshot
↓
EMA
```

Ini dapat menghasilkan rate yang tidak merepresentasikan kondisi steady-state.

Saya justru akan mempertahankan prinsip:

> **verification samples jangan digunakan untuk prediction.**

Misalnya snapshot biasa:

```rust
if verification_deadline.is_none() {
    scheduler.push_sample(snapshot.clone());
}
```

atau tandai sample sebagai transitional.

---

# 9. Prediksi capacity masih sangat kasar

Ini:

```rust
(cap as f32 - prev_cap as f32) / dt
```

menggunakan resolusi capacity integer:

```text
79
80
81
```

Dengan polling beberapa detik, rate-nya sangat noisy.

Untungnya Anda sudah memakai:

```rust
EMA_ALPHA = 0.3
```

dan:

```rust
EMA_HISTORY_LEN = 5
```

jadi efeknya dikurangi.

Tetapi untuk charging controller Android, saya lebih percaya pada:

```text
current_ma
capacity
elapsed time
```

daripada capacity delta saja.

Anda sudah membaca:

```rust
current_ma
```

tetapi belum digunakan scheduler.

Ini bisa menjadi peningkatan besar berikutnya.

---

# 10. `current_ma` saat ini praktis belum berguna

Anda punya:

```rust
current_ma: Option<i32>,
```

dan:

```rust
battery_reader.read_current_ma()
```

tetapi tidak dipakai dalam decision.

Kalau memang belum digunakan, tidak masalah secara correctness, tetapi secara desain:

```rust
current_ma
```

bisa digunakan untuk mendeteksi:

```text
charger connected
+
online=true
+
status=NotCharging
+
current ≈ 0
```

versus:

```text
charger connected
+
current positive
```

tergantung sign convention driver.

Namun **jangan mengasumsikan tanda current Android universal**. Itu harus diverifikasi dari `BatteryStatus`/driver device target.

---

# 11. Ada satu hal yang sangat saya suka: state machine-nya sekarang cukup bersih

Ini sudah bagus:

```text
                 ┌─────────────┐
                 │   Charging  │
                 └──────┬──────┘
                        │
             cap >= limit
                        │
                        ▼
               ┌────────────────┐
               │ LimitReached   │
               └───────┬────────┘
                       │
                 cap <= resume
                       │
                       ▼
                 ┌───────────┐
                 │ Charging  │
                 └───────────┘
```

dan thermal:

```text
Charging
   │
   │ temp >= max
   ▼
ThermalCutoff
   │
   │ temp <= max - hysteresis
   ▼
Charging
```

Ini jauh lebih aman daripada implementasi charging limiter yang hanya:

```rust
if capacity >= limit {
    disable();
}

if capacity < limit {
    enable();
}
```

karena implementasi sederhana akan mudah mengalami oscillation.

---

# 12. Satu hal yang saya akan ubah: `DecisionReason::SensorFault`

Ini digunakan untuk:

```rust
if snapshot.capacity_pct.is_none()
```

padahal komentar Anda sendiri mengatakan:

> Missing capacity is non-critical.

Jadi:

```rust
DecisionReason::SensorFault
```

kurang tepat.

Lebih baik:

```rust
CapacityUnavailable
```

Misalnya:

```rust
enum DecisionReason {
    ...
    CapacityUnavailable,
    TemperatureUnavailable,
}
```

Ini akan membuat logging jauh lebih jelas.

---

# 13. `DecisionEngine::reconfigure()` masih bisa lebih lengkap

Sekarang:

```rust
match self.state {
    ChargeState::ThermalCutoff ...
    ChargeState::LimitReached ...
    _ => {}
}
```

Misalnya config berubah dari:

```text
charge_limit = 80
```

ke:

```text
charge_limit = 90
```

Anda sudah menangani:

```rust
cap < cfg.charge_limit
```

bagus.

Tetapi perubahan:

```text
thermal_cutoff
thermal_resume_hysteresis
resume_limit
```

belum semuanya diperlakukan sebagai state transition policy.

Saya akan membuat `reconfigure()` benar-benar menjadi:

```text
configuration changed
       ↓
validate config
       ↓
reconcile state
       ↓
reset scheduler prediction jika perlu
       ↓
evaluate immediately
```

---

# 14. Potensi masalah paling besar justru bukan di polling

Secara keseluruhan, bagian:

```rust
poll()
+
Netlink
+
adaptive timer
```

sudah cukup bagus.

Yang sekarang paling penting adalah **hardware control semantics**:

```rust
control::set_charging(true)
control::set_charging(false)
```

Karena di Android vendor/kernel berbeda-beda, `set_charging(false)` belum tentu berarti:

> benar-benar memutus charging.

Bisa saja backend menggunakan:

```text
charging_enabled
input_suspend
charge_disable
current_limit
constant_charge_current
```

atau node vendor tertentu.

Jadi verification yang sekarang:

```rust
snapshot.is_charging()
```

sebenarnya sangat penting.

Tetapi saya akan menjadikannya **hardware synchronization layer**, bukan sekadar logging.

---

# Penilaian saya sekarang

| Komponen                   | Kondisi                            |
| -------------------------- | ---------------------------------- |
| State machine              | 🟢 Bagus                           |
| Charge limit hysteresis    | 🟢 Bagus                           |
| Thermal cutoff             | 🟢 Bagus                           |
| Sensor fault handling      | 🟢 Bagus                           |
| Adaptive scheduler         | 🟢 Bagus                           |
| Netlink                    | 🟢 Bagus                           |
| Netlink reconnect          | 🟢 Cukup matang                    |
| IPC                        | 🟢 Bagus                           |
| Config reload              | 🟢 Bagus                           |
| FD ownership               | 🟢 Bagus                           |
| Verification               | 🟡 Perlu diperkuat                 |
| Battery status abstraction | 🟡 Perlu diperbaiki                |
| Prediction                 | 🟡 Masih sederhana                 |
| Hardware synchronization   | 🟠 Ini yang paling perlu diperkuat |

### Kesimpulan

**Ya, sekarang jauh lebih bagus dan secara arsitektur sudah layak dijadikan basis final.** Saya tidak akan melakukan rewrite lagi.

Namun sebelum saya menyebutnya **production-ready**, saya akan memperbaiki **4 hal utama**:

1. **Verification failure harus melakukan recovery/retry command**, bukan hanya berhenti setelah 3 kegagalan.
2. **Pisahkan `BatteryStatus::Charging` dari `NotCharging/Full/Unknown`**, jangan semuanya dianggap `false`.
3. **Jangan masukkan snapshot saat hardware sedang verification/transisi ke EMA prediction.**
4. **Perkuat hardware-sync layer**, karena keberhasilan `set_charging()` harus dibuktikan dari state hardware, bukan hanya dari state internal daemon.

Setelah empat hal itu dibereskan, desain ini sudah jauh lebih dekat ke **charging controller daemon Android yang robust**, bukan sekadar polling script yang dibungkus Rust.
