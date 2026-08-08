Ya. **Kode yang kamu paste masih punya beberapa bug/logical flaw**, dan ada **1 bug yang cukup serius pada startup/inisialisasi state** serta **1 bug pada reconnect Netlink**.

Saya review berdasarkan kode yang kamu paste sekarang, bukan versi sebelumnya.

### 1. Bug paling penting: state awal `Charging`

```rust
fn new() -> Self {
    Self {
        state: ChargeState::Charging,
        fault_recovery_reads: 0,
    }
}
```

Ini bermasalah jika saat daemon start:

* charger terpasang
* baterai 50%
* hardware sebenarnya **tidak sedang charging**

`evaluate()` menghasilkan:

```rust
ChargeState::Charging
ChargeCommand::Enable
```

Tetapi di bawah:

```rust
if prev_state != decision.state {
    control::set_charging(true)
}
```

`prev_state` sudah `Charging`, `decision.state` juga `Charging`.

Jadi:

```text
prev_state == decision.state
       ↓
set_charging(true) TIDAK dipanggil
       ↓
hardware tetap tidak charging
```

Ini bisa membuat daemon menganggap charging aktif padahal hardware tidak.

**Solusi:** jangan mulai `DecisionEngine` dalam state `Charging`. Gunakan state awal yang belum committed, atau lakukan initial synchronization.

Misalnya lebih aman:

```rust
struct DecisionEngine {
    state: ChargeState,
    initialized: bool,
    fault_recovery_reads: u8,
}
```

dan saat pertama evaluasi, command tetap dieksekusi walaupun state tidak berubah.

Atau paling sederhana:

```rust
struct DecisionEngine {
    state: ChargeState,
    fault_recovery_reads: u8,
    force_apply: bool,
}

fn new() -> Self {
    Self {
        state: ChargeState::Charging,
        fault_recovery_reads: 0,
        force_apply: true,
    }
}
```

Kemudian:

```rust
let should_apply = prev_state != decision.state || engine.force_apply;

match decision.command {
    ChargeCommand::Enable => {
        if should_apply {
            if let Err(e) = control::set_charging(true) {
                tracing::error!("Failed to enable charging: {}", e);
            }
            engine.force_apply = false;
        }
    }
    ...
}
```

Namun saya lebih menyarankan **initial hardware synchronization** daripada `force_apply`.

---

### 2. Bug Netlink reconnect yang nyata

Bagian ini:

```rust
if nl_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
    tracing::error!("Netlink socket error. Reconnecting...");
    _nl_sock = None;
    num_fds = 1;
    pfds[1].fd = -1;

    match create_netlink_socket() {
        Ok(new_sock) => {
            ...
            _nl_sock = Some(new_sock);
        }
        Err(e) => {
            tracing::warn!("Netlink reconnect failed ({}).", e);
        }
    }
}
```

Kalau reconnect gagal:

```text
Netlink error
     ↓
create_netlink_socket()
     ↓
FAILED
     ↓
hanya log warning
     ↓
next_netlink_reconnect tetap None
     ↓
TIDAK ADA RETRY
```

Padahal kamu sudah membuat mekanisme:

```rust
NETLINK_RECONNECT_INITIAL_BACKOFF
NETLINK_RECONNECT_MAX_BACKOFF
next_netlink_reconnect
netlink_reconnect_backoff
```

Tetapi mekanisme tersebut hanya bekerja untuk kondisi Netlink **sudah tidak ada sejak awal**, bukan ketika socket yang aktif kemudian mati.

Harusnya ketika reconnect gagal:

```rust
next_netlink_reconnect = Some(
    now + netlink_reconnect_backoff
);

netlink_reconnect_backoff =
    (netlink_reconnect_backoff * 2)
        .min(NETLINK_RECONNECT_MAX_BACKOFF);
```

Dan ketika berhasil:

```rust
next_netlink_reconnect = None;
netlink_reconnect_backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;
```

Ini menurut saya **bug yang harus diperbaiki**.

---

### 3. Reconnect berhasil dari error tidak reset backoff

Pada jalur ini:

```rust
Ok(new_sock) => {
    ...
    _nl_sock = Some(new_sock);
}
```

kamu tidak melakukan:

```rust
netlink_reconnect_backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;
next_netlink_reconnect = None;
```

Jadi state backoff bisa tertinggal dari percobaan sebelumnya.

Tidak selalu menyebabkan failure, tetapi state machine reconnect menjadi tidak konsisten.

---

### 4. `verification_failures` bisa tertinggal pada perubahan state

Misalnya:

```text
Disable charging
   ↓
verification pending
   ↓
state berubah karena event/config
   ↓
pending verification masih menunjuk state lama
```

`pending_verification_state` hanya dibersihkan setelah verification selesai.

Ada potensi verification terhadap state lama ketika state sudah berubah.

Lebih aman ketika state berubah, batalkan verification lama:

```rust
if prev_state != decision.state {
    verification_deadline = None;
    pending_verification_state = None;
    verification_failures = 0;
}
```

Lalu buat verification baru hanya jika command benar-benar diterapkan.

---

### 5. Verification `Charging` belum sepenuhnya valid

Saat:

```rust
ChargeState::Charging
```

kamu memeriksa:

```rust
if !snapshot.is_charging() && snapshot.online == Some(true)
```

Masalahnya status battery Linux/Android tidak selalu berubah seketika setelah:

```rust
set_charging(true)
```

Kamu memang sudah memberikan:

```rust
VERIFY_DELAY = 500ms
```

tetapi 500 ms belum tentu cukup di semua kernel/device.

Yang lebih penting: **verification failure sekarang hanya logging**.

Setelah:

```rust
MAX_VERIFICATION_FAILURES = 3
```

kamu melakukan:

```rust
tracing::error!(
    "Verification failed after {} attempts...",
    verification_failures,
);
```

tetapi tidak melakukan recovery action.

Jadi:

```text
set_charging(true)
     ↓
hardware gagal enable
     ↓
verification gagal
     ↓
3x gagal
     ↓
log error
     ↓
selesai
```

Daemon tidak mencoba memperbaiki hardware.

Untuk daemon charger, saya akan membuat:

```text
verification failure
        ↓
retry control
        ↓
re-read
        ↓
retry control
        ↓
persistent failure
        ↓
Fault
```

---

### 6. `SensorFault` untuk capacity missing agak misleading

Ini:

```rust
if snapshot.capacity_pct.is_none() {
    return Decision {
        command: ChargeCommand::Noop,
        state: self.state,
        reason: DecisionReason::SensorFault,
    };
}
```

Padahal comment kamu mengatakan:

> Capacity is policy-critical; if missing, daemon holds state and takes no action.

Secara logic memang hold state.

Tetapi `DecisionReason::SensorFault` terdengar seperti **fault state**, padahal:

```rust
state: self.state
```

bisa tetap:

```rust
ChargeState::Charging
```

Saya lebih suka:

```rust
DecisionReason::CapacityUnavailable
```

dan tambahkan enum:

```rust
CapacityUnavailable,
```

Ini bukan bug fungsional, tetapi penting untuk observability/debugging.

---

### 7. Scheduler bisa terlalu cepat setelah state berubah

`next_interval()` menggunakan:

```rust
ema_cap_rate
ema_temp_rate
```

Tetapi ketika charging state berubah, kamu hanya reset:

```rust
self.ema_cap_rate = 0.0;
```

Tidak reset:

```rust
ema_temp_rate
```

Padahal perubahan charging bisa mengubah thermal trajectory secara signifikan.

Lebih aman:

```rust
if state_changed {
    self.ema_cap_rate = 0.0;
    self.ema_temp_rate = 0.0;
}
```

---

### 8. Bug kecil pada `next_interval()` saat offline

Ini:

```rust
if s.online == Some(false) {
    self.last_interval = UNPLUGGED_HEARTBEAT;
    return self.last_interval;
}
```

600 detik = 10 menit.

Secara battery daemon masuk akal untuk menghemat wakeup.

Tetapi ketika charger dicolokkan, kamu **bergantung sepenuhnya pada Netlink** untuk membangunkan daemon.

Kalau Netlink mati:

```text
unplugged
   ↓
sleep 10 menit
   ↓
charger masuk
   ↓
Netlink tidak bekerja
   ↓
daemon baru sadar sampai 10 menit kemudian
```

Jadi heartbeat 600 detik sebaiknya menjadi fallback, tetapi **bukan satu-satunya mekanisme deteksi**.

Kalau Netlink reconnect gagal, sebaiknya adaptive heartbeat diperkecil, misalnya 5–30 detik.

---

## 9. Ada masalah desain yang lebih fundamental

Sekarang arsitekturnya masih mencampur:

```text
sensor state
     ↓
DecisionEngine state
     ↓
hardware state
```

Padahal ketiganya berbeda.

Contohnya:

```text
DecisionEngine = Charging
Hardware       = NOT charging
Kernel status  = Charging / Unknown
```

Kamu sudah mencoba mengatasi ini dengan verification, tetapi verification datang **setelah keputusan**.

Untuk charger daemon yang robust, saya akan pisahkan menjadi:

```text
┌─────────────────────┐
│ Battery Sensors     │
│ capacity/temp/etc   │
└──────────┬──────────┘
           ↓
┌─────────────────────┐
│ Policy Engine       │
│ What SHOULD happen? │
└──────────┬──────────┘
           ↓
┌─────────────────────┐
│ Hardware Controller │
│ Apply command       │
└──────────┬──────────┘
           ↓
┌─────────────────────┐
│ Verification        │
│ What ACTUALLY happened?│
└──────────┬──────────┘
           ↓
       Fault/Retry
```

Dengan demikian `ChargeState` sebaiknya menggambarkan **policy state**, sedangkan actual hardware state disimpan terpisah.

---


