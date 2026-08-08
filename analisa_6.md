Ya, **versi ini sudah jauh lebih baik**, dan sebagian besar 8 perbaikan yang Anda sebutkan memang sudah masuk. Tetapi saya **belum menganggapnya bug-free**. Ada beberapa bug logika yang masih cukup penting, terutama di `force_apply`, verifikasi hardware, dan reconnect Netlink.

### Status 8 poin Anda

| Poin                           | Status          | Catatan                                   |
| ------------------------------ | --------------- | ----------------------------------------- |
| 1. `force_apply`               | ⚠️ Hampir benar | Ada bug jika `set_charging()` gagal       |
| 2–3. Netlink reconnect/backoff | ✅               | Sudah jauh lebih benar                    |
| 4. Batalkan verification stale | ⚠️ Belum 100%   | Ada celah pada `reconfigure()`            |
| 5. `HardwareSyncFailed`        | ⚠️ Ada          | Tetapi mekanismenya masih bisa diperbaiki |
| 6. `CapacityUnavailable`       | ✅               | Benar                                     |
| 7. Reset EMA                   | ✅               | Benar                                     |
| 8. Unplugged fallback 30 detik | ✅               | Benar                                     |

## 1. Bug paling penting: `force_apply` hilang walaupun command gagal

Ini:

```rust
if let Err(e) = control::set_charging(true) {
    tracing::error!("Failed to enable charging: {}", e);
}
verification_failures = 0;
verification_deadline = Some(...);
pending_verification_state = Some(decision.state);
engine.force_apply = false;
```

Masalahnya adalah:

```text
set_charging(true) -> ERROR
        ↓
force_apply = false
        ↓
daemon menganggap command sudah diterapkan
        ↓
tidak mencoba lagi sampai terjadi state transition
```

Padahal hardware belum tentu berubah.

### Seharusnya

`force_apply` **hanya di-clear kalau command berhasil**.

Misalnya:

```rust
match control::set_charging(true) {
    Ok(()) => {
        engine.force_apply = false;
        verification_failures = 0;
        verification_deadline = Some(Instant::now() + VERIFY_DELAYS[0]);
        pending_verification_state = Some(decision.state);
    }
    Err(e) => {
        tracing::error!("Failed to enable charging: {}", e);
        engine.force_apply = true;
    }
}
```

Hal yang sama untuk:

```rust
set_charging(false)
```

dan `RestoreCharging`.

**Ini saya anggap bug nyata.**

---

# 2. `HardwareSyncFailed` sebenarnya belum sepenuhnya ideal

Sekarang:

```rust
ChargeState::HardwareSyncFailed => {
    self.state = ChargeState::Charging;
    let mut dec = self.evaluate(snapshot, cfg);
    dec.reason = DecisionReason::HardwareSyncFailed;
    dec
}
```

Ini memang membuat command diulang.

Tetapi secara semantik agak aneh:

```text
HardwareSyncFailed
       ↓
Charging
       ↓
evaluate()
       ↓
Enable / Disable
```

Artinya `HardwareSyncFailed` hanya menjadi **transient state**, bukan state recovery yang benar-benar menyimpan informasi kegagalan.

Lebih penting lagi, setelah hardware gagal sinkron, Anda langsung mengubah state menjadi `Charging` sebelum command baru berhasil.

Saya lebih suka pola:

```text
HardwareSyncFailed
       │
       ├── retry policy
       │
       ↓
desired policy state
       │
       ↓
apply command
       │
       ├── success → normal state
       │
       └── failure → HardwareSyncFailed
```

Namun untuk desain daemon Anda sekarang, implementasi yang ada masih **fungsional**.

---

# 3. Jumlah verification attempt tidak sesuai nama constant

Anda punya:

```rust
const MAX_VERIFICATION_FAILURES: u8 = 3;
```

dan:

```rust
if verification_failures < MAX_VERIFICATION_FAILURES {
    let delay = VERIFY_DELAYS[verification_failures as usize];
    verification_deadline = Some(Instant::now() + delay);
    verification_failures = verification_failures.saturating_add(1);
} else {
    ...
}
```

Urutannya:

```text
command
 ↓
verification #1 → failure_count 0 → delay 500ms
 ↓
verification #2 → failure_count 1 → delay 1s
 ↓
verification #3 → failure_count 2 → delay 2s
 ↓
verification #4 → failure_count 3 → HardwareSyncFailed
```

Jadi:

> `MAX_VERIFICATION_FAILURES = 3`

sebenarnya menghasilkan **4 pemeriksaan hardware**.

Kalau maksud Anda **3 verification attempts total**, ubah desain counter-nya.

Kalau maksud Anda:

> initial check + 3 retries

maka kode sekarang masuk akal, tetapi namanya lebih jelas kalau:

```rust
const MAX_VERIFICATION_RETRIES: u8 = 3;
```

dan log:

```text
after 3 retries
```

bukan:

```text
after 3 attempts
```

---

# 4. Masih ada masalah pada stale verification

Anda sudah punya:

```rust
if prev_state != decision.state {
    verification_deadline = None;
    pending_verification_state = None;
    verification_failures = 0;
}
```

Ini bagus.

Tetapi perhatikan urutannya:

```rust
let reconfigured = engine.reconfigure(...);

if reconfigured || scheduler_changed {
    scheduler.reset_prediction();
}

if reconfigured {
    engine.force_apply = true;
}

let prev_state = engine.state;
let decision = engine.evaluate(...);
```

`reconfigure()` **sudah dapat mengubah `engine.state` sebelum `prev_state` diambil**.

Contoh:

```text
pending verification:
    LimitReached

config berubah

reconfigure():
    LimitReached → Charging

prev_state = Charging

evaluate():
    Charging → Charging

prev_state == decision.state
```

Maka blok:

```rust
if prev_state != decision.state
```

tidak berjalan.

Artinya verification lama bisa masih ada.

Memang kemudian command baru biasanya akan menimpa:

```rust
pending_verification_state = Some(decision.state);
```

tetapi secara desain lebih bersih jika **reconfigure langsung membatalkan verification pending ketika state berubah**.

Contohnya:

```rust
let old_state = engine.state;
let reconfigured = engine.reconfigure(&cfg, Some(&snapshot));

if reconfigured {
    verification_deadline = None;
    pending_verification_state = None;
    verification_failures = 0;
    engine.force_apply = true;

    if old_state != engine.state {
        tracing::info!(
            "State changed during reconfigure: {:?} -> {:?}",
            old_state,
            engine.state
        );
    }
}
```

Ini lebih deterministic.

---

# 5. Ada masalah kecil pada `reconfigure()`

Anda punya:

```rust
let thermal_max = cfg.max_temp_dc;
let safe_hysteresis = cfg
    .thermal_resume_hysteresis_dc
    .clamp(1, thermal_max.saturating_sub(1).max(1));
```

Jika `thermal_max` sangat kecil, logikanya menjadi agak tidak intuitif.

Untuk konfigurasi normal Android, kemungkinan tidak bermasalah, tetapi secara robustness saya akan melakukan validasi konfigurasi **sebelum** masuk ke DecisionEngine.

---

# 6. Bug desain lain: `Disabled` tetap menggunakan `RestoreCharging`

Ini:

```rust
if !cfg.enabled {
    self.state = ChargeState::Disabled;

    Decision {
        command: ChargeCommand::RestoreCharging,
        state: ChargeState::Disabled,
        ...
    }
}
```

Secara policy ini masuk akal:

> daemon disabled → lepaskan kontrol charging → biarkan hardware charging normal.

Tetapi ada konsekuensi:

```text
daemon disabled
      ↓
RestoreCharging
      ↓
force_apply = false
```

Jika `set_charging(true)` gagal, lagi-lagi Anda kehilangan kesempatan retry.

Jadi kembali ke poin #1.

---

# 7. Netlink reconnect sekarang sudah jauh lebih bagus

Bagian ini:

```rust
Err(e) => {
    tracing::warn!(
        "Netlink reconnect failed ({}); scheduling backoff.",
        e
    );

    next_netlink_reconnect = Some(now + netlink_reconnect_backoff);

    netlink_reconnect_backoff =
        (netlink_reconnect_backoff * 2).min(NETLINK_RECONNECT_MAX_BACKOFF);
}
```

sudah memperbaiki bug fatal sebelumnya.

Dan ketika berhasil:

```rust
next_netlink_reconnect = None;
netlink_reconnect_backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;
```

juga benar.

### Tetapi ada satu masalah minor

Pada `POLLERR`, Anda langsung mencoba reconnect:

```rust
match create_netlink_socket()
```

kemudian jika gagal menjadwalkan backoff.

Itu tidak salah.

Tetapi arsitekturnya sekarang punya **dua jalur reconnect**:

1. reconnect langsung pada `POLLERR`
2. reconnect melalui `next_netlink_reconnect`

Ini masih aman, tetapi lebih sederhana kalau `POLLERR` hanya melakukan:

```rust
_nl_sock = None;
num_fds = 1;
pfds[1].fd = -1;

next_netlink_reconnect = Some(now + netlink_reconnect_backoff);
```

Kemudian **satu-satunya tempat reconnect adalah scheduler reconnect**.

Itu mengurangi kompleksitas state machine.

---

# 8. Fallback 30 detik sudah benar

Ini bagus:

```rust
fn next_interval(
    &mut self,
    s: &SensorSnapshot,
    has_netlink: bool
) -> Duration {
    if s.online == Some(false) {
        self.last_interval =
            if has_netlink {
                UNPLUGGED_HEARTBEAT
            } else {
                UNPLUGGED_HEARTBEAT_NO_NETLINK
            };
```

Dengan:

```rust
UNPLUGGED_HEARTBEAT = 600s
UNPLUGGED_HEARTBEAT_NO_NETLINK = 30s
```

Maka:

```text
Netlink sehat + unplugged
        ↓
10 menit

Netlink mati + unplugged
        ↓
30 detik
```

Ini jauh lebih aman daripada 10 menit ketika event mechanism rusak.

---

# 9. Reset EMA sudah benar

Ini:

```rust
if prev.charging_state() != s.charging_state() {
    self.ema_cap_rate = 0.0;
    self.ema_temp_rate = 0.0;
}
```

sudah sesuai dengan masalah yang sebelumnya kita identifikasi.

Bahkan Anda menambahkan:

```rust
fn reset_prediction(&mut self) {
    self.history.clear();
    self.ema_cap_rate = 0.0;
    self.ema_temp_rate = 0.0;
    self.last_interval = MIN_INTERVAL;
}
```

Ini bagus ketika konfigurasi berubah.

---

# 10. Tetapi `scheduler_changed` bisa dibuat lebih sederhana

Sekarang:

```rust
let mut scheduler_changed = false;

if (...) {
    scheduler_changed = true;
}
```

Bisa langsung:

```rust
let scheduler_changed =
    (scheduler.limit - limit as f32).abs() > f32::EPSILON
    || (scheduler.resume_limit - resume as f32).abs() > f32::EPSILON
    || (scheduler.thermal_cutoff - cfg.max_temp_dc as f32 / 10.0).abs()
        > f32::EPSILON;
```

Bukan bug, hanya cleanup.

---

# Kesimpulan

Saya akan menilai versi ini:

**sekitar 85–90% sudah benar**, tetapi belum saya sebut final.

Yang **wajib diperbaiki sebelum production** adalah:

### P0 — wajib

**`force_apply` jangan di-clear jika `set_charging()` gagal.**

Ini yang paling penting.

### P1 — sangat disarankan

Perbaiki lifecycle:

```text
verification pending
       ↓
config/state berubah
       ↓
batalkan verification lama
       ↓
apply command baru
       ↓
verification baru
```

Jangan mengandalkan overwrite `pending_verification_state` secara tidak langsung.

### P1

Ubah:

```rust
MAX_VERIFICATION_FAILURES
```

menjadi semantik yang jelas:

```rust
MAX_VERIFICATION_RETRIES
```

jika memang yang diinginkan adalah:

```text
initial verification
+ 3 retries
```

### P2

Satukan mekanisme reconnect Netlink agar tidak ada dua jalur reconnect yang berjalan.

---

**Yang paling penting:** konsep arsitekturnya sekarang sudah jauh lebih solid. Anda sudah punya:

```text
Sensor
  ↓
Snapshot
  ↓
Adaptive Scheduler
  ↓
Decision Engine
  ↓
Hardware Command
  ↓
Delayed Verification
  ↓
HardwareSyncFailed
  ↓
Retry
```

Ditambah:

```text
Netlink event
     ↓
debounce
     ↓
immediate evaluation
```

dan:

```text
Netlink failure
     ↓
30s fallback
     +
exponential reconnect
```

Jadi saya **tidak akan kembali ke versi kode sebelumnya**. Versi yang Anda paste ini adalah basis yang lebih baik; tinggal dibereskan terutama pada **error handling `force_apply` dan lifecycle verification**.
