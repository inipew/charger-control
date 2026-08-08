Kalau dibandingkan dengan versi sebelumnya, **arah arsitekturnya sekarang jauh lebih bagus** karena sudah dipisah menjadi:

`Snapshot → DecisionEngine → HardwareController → Scheduler → NetlinkMonitor → Orchestrator`

Tetapi **versi yang kamu paste ini masih punya beberapa bug penting**. Bahkan ada beberapa yang cukup fatal sehingga saya **belum menyarankan rewrite final berdasarkan kode ini**.

### Yang sudah benar

1. **`force_apply` sudah pindah ke `HardwareController`**

   * Ini lebih tepat daripada sebelumnya.
   * `target = Unmanaged` + `force_apply = true` membuat siklus pertama tetap melakukan sinkronisasi hardware.

2. **Generation pada verification**

   ```rust
   generation: u64
   ```

   Ini solusi yang bagus untuk stale verification. Ketika:

   ```rust
   hardware.invalidate_verification();
   ```

   generation berubah sehingga verification lama tidak berlaku.

3. **Verification state dipisahkan dari policy state**

   ```text
   ChargePolicyState
   HardwareTarget
   SyncState
   ```

   Ini jauh lebih bersih daripada versi monolitik sebelumnya.

4. **Netlink reconnect sudah punya backoff**
   Konsep:

   ```text
   1s → 2s → 4s → ... → 60s
   ```

   sudah benar.

---

# Tetapi ada bug yang harus diperbaiki

## 1. `try_reconnect()` masih placeholder

Ini yang paling jelas:

```rust
pub fn try_reconnect(&mut self, _now: Instant) -> bool {
    /* ... creates socket ... */
    true
}
```

Ini belum implementasi.

Lebih parah lagi, kalau dipanggil:

```rust
if netlink.should_reconnect(now) {
    netlink.try_reconnect(now);
}
```

ia selalu mengembalikan `true`, tetapi socket tetap `None`.

Jadi **Netlink sebenarnya belum berfungsi** pada kode yang kamu paste.

---

# 2. `NetlinkMonitor::should_reconnect()` salah secara semantik

Sekarang:

```rust
pub fn should_reconnect(&self, now: Instant) -> bool {
    self.socket.is_none() && self.reconnect_at.map_or(true, |t| now >= t)
}
```

Kalau socket `None` dan `reconnect_at == None`, hasilnya:

```text
true
```

Artinya langsung mencoba reconnect terus.

Seharusnya state awal memang harus menjadwalkan reconnect secara eksplisit, misalnya:

```rust
reconnect_at: Some(Instant::now() + INITIAL_BACKOFF)
```

atau `should_reconnect()` hanya true kalau memang ada jadwal:

```rust
self.socket.is_none()
    && self.reconnect_at.is_some_and(|t| now >= t)
```

---

# 3. Backoff tidak di-reset setelah reconnect berhasil

Di versi sebelumnya kamu sudah punya:

```rust
netlink_reconnect_backoff = INITIAL_BACKOFF;
```

setelah berhasil.

Di versi baru:

```rust
pub fn try_reconnect(&mut self, _now: Instant) -> bool {
    ...
}
```

belum terlihat mekanisme reset.

Harus ada:

```text
reconnect gagal:
1 → 2 → 4 → 8 → ...

reconnect berhasil:
kembali 1 detik
```

Kalau tidak, setelah beberapa kegagalan lalu berhasil, kegagalan berikutnya bisa langsung mulai dari backoff besar.

---

# 4. BUG penting: `handle_events()` tidak membaca Netlink

Sekarang:

```rust
pub fn handle_events(&mut self, now: Instant) -> bool {
    self.debounce_target = Some(now + Duration::from_millis(250));
    false
}
```

Ini hanya memasang debounce.

Tidak ada:

```text
recv()
↓
parse uevent
↓
cek SUBSYSTEM=power_supply
↓
cek ACTION=change
```

Akibatnya **setiap POLLIN Netlink dianggap event baterai**, termasuk event yang mungkin tidak berkaitan dengan power supply.

Versi sebelumnya justru punya:

```rust
contains_subslice(buf_slice, b"SUBSYSTEM=power_supply")
contains_subslice(buf_slice, b"ACTION=change")
```

Itu jangan hilang.

---

# 5. `handle_events()` selalu mengembalikan `false`

```rust
-> bool
```

tetapi:

```rust
false
```

selamanya.

Kalau return value memang dimaksudkan sebagai:

```text
ada battery event atau tidak
```

maka harus mengembalikan `true` hanya jika benar-benar menemukan event:

```text
SUBSYSTEM=power_supply
ACTION=change
```

Kalau tidak butuh return value, lebih baik:

```rust
pub fn handle_events(&mut self, now: Instant)
```

---

# 6. BUG besar pada `AdaptiveScheduler::eta_to()`

Ini sangat penting.

Kamu membuat:

```rust
let cap_eta = self.eta_to(
    s.capacity_pct.map(|c| c as f32),
    self.limit,
    self.cap_rate_ema,
    SAFETY_FACTOR
);
```

Tetapi:

```rust
fn eta_to(
    &self,
    current: Option<f32>,
    threshold: f32,
    rate: Option<f32>,
    safety: f32
) -> Option<Duration> {
    let (current, rate) = (current?, rate?);
    if rate <= 0.01 { return None; }

    Some(Duration::from_secs_f32(
        ((threshold - current).max(0.0) / rate * safety).max(0.0)
    ))
}
```

Ini hanya bekerja untuk **rate positif**.

Padahal:

### Saat charging

```text
capacity:
70 → 71 → 72

rate = +0.5 %/s
```

benar.

### Saat tidak charging

Misalnya:

```text
70 → 69 → 68

rate = -0.5 %/s
```

langsung:

```rust
rate <= 0.01
```

→ `None`.

Jadi scheduler **tidak bisa memprediksi kapan mencapai `resume_limit` saat baterai sedang turun**.

Ini berbeda dengan versi awalmu yang memang membedakan:

```text
charging → menuju limit
not charging → menuju resume
```

Ini harus dikembalikan.

---

# 7. Bahkan perhitungan temperature ETA juga belum benar

Kamu menggunakan:

```rust
self.eta_to(
    s.temp_dc.map(|t| t as f32),
    self.thermal_cutoff * 10.0,
    self.temp_rate_ema,
    THERMAL_SAFETY_FACTOR
)
```

Temperatur hanya perlu diprediksi kalau:

```text
temp_rate > 0
```

Jadi harus ada arah prediksi:

```text
temperature naik → menuju thermal cutoff
temperature turun → tidak perlu menghitung cutoff
```

Sekarang `eta_to()` tidak memahami arah target.

---

# 8. `sync_config()` punya bug resume limit

Di scheduler:

```rust
let new_resume = cfg.resume_limit as f32;
```

Tetapi di `DecisionEngine`, kamu menggunakan fallback:

```rust
let resume =
    if cfg.resume_limit > 0 && cfg.resume_limit < limit {
        cfg.resume_limit
    } else {
        limit.saturating_sub(2)
    };
```

Jadi bisa terjadi:

```text
Config:
charge_limit = 80
resume_limit = 0
```

DecisionEngine:

```text
resume = 78
```

Scheduler:

```text
resume_limit = 0
```

**DecisionEngine dan Scheduler memiliki model state berbeda.**

Ini bug desain.

Scheduler harus menerima **effective resume limit**, bukan raw config.

---

# 9. `Disabled` dan `Offline` → `Unmanaged` perlu diperjelas

Sekarang:

```rust
ChargePolicyState::Disabled
| ChargePolicyState::Offline
    => HardwareTarget::Unmanaged
```

dan:

```rust
HardwareTarget::Unmanaged => control::set_charging(true)
```

Artinya:

### Daemon disabled

```text
Disabled
 ↓
Unmanaged
 ↓
set_charging(true)
```

### Charger offline

```text
Offline
 ↓
Unmanaged
 ↓
set_charging(true)
```

Ini mungkin memang desain yang kamu inginkan: **daemon melepas kontrol dan mengembalikan charging normal**.

Tetapi nama `Unmanaged` agak menipu karena sebenarnya dia **melakukan command enable**.

Saya lebih suka:

```rust
HardwareTarget::RestoreCharging
```

daripada:

```rust
HardwareTarget::Unmanaged
```

karena semantiknya lebih jelas.

---

# 10. `HardwareController::apply_target()` melakukan verification untuk `Unmanaged`

Sekarang:

```rust
HardwareTarget::Unmanaged => control::set_charging(true).is_ok()
```

lalu:

```rust
self.sync = SyncState::Pending;
```

dan:

```rust
verification: Some(...)
```

Tetapi `verify()`:

```rust
HardwareTarget::Unmanaged => true,
```

Jadi sebenarnya **tidak ada gunanya melakukan verification untuk Unmanaged**.

Lebih baik:

```text
RestoreCharging
    ↓
set_charging(true)
    ↓
Synced
```

tanpa verification.

---

# 11. BUG pada retry count

Kamu punya:

```rust
const MAX_VERIFICATION_RETRIES: u8 = 3;
```

dan:

```rust
self.verification_failures =
    self.verification_failures.saturating_add(1);

if self.verification_failures > MAX_VERIFICATION_RETRIES {
```

Dengan `3`, failure terjadi:

```text
failure #1
failure #2
failure #3
failure #4 → Failed
```

Jadi sebenarnya **4 kegagalan**, bukan 3.

Kalau maksudnya maksimal 3:

```rust
if self.verification_failures >= MAX_VERIFICATION_RETRIES
```

---

# 12. State transition belum benar-benar di-log

Versi lama punya:

```rust
State transition: {:?} -> {:?}
```

Sekarang:

```rust
if decision.target != old_target {
    hardware.invalidate_verification();
    hardware.force_apply = true;
}
```

Tetapi tidak ada observability seperti:

```text
Charging → LimitReached
NormalCharging → ChargeLimitReached
```

Untuk daemon yang harus reliable, logging transition sangat berguna.

---

# 13. Config initial scheduler hardcoded

Ini:

```rust
let mut scheduler = AdaptiveScheduler::new(80, 78, 400);
```

adalah bug yang cukup jelas.

Harusnya:

```rust
let cfg = config.read()...
```

lalu:

```rust
let effective_resume = ...
let mut scheduler = AdaptiveScheduler::new(
    cfg.charge_limit,
    effective_resume,
    cfg.max_temp_dc,
);
```

Kalau config user:

```text
limit = 85
max_temp = 450
```

scheduler awal masih:

```text
80%
400 dC
```

Siklus pertama bisa memakai parameter salah.

---

# 14. Initial Netlink juga belum dibuat

Versi lama langsung:

```rust
create_netlink_socket()
```

saat startup.

Versi baru:

```rust
let mut netlink = NetlinkMonitor::new();
```

tetapi `new()`:

```rust
socket: None,
reconnect_at: None,
```

dan tidak ada initial connection.

Memang `should_reconnect()` sekarang akan mengembalikan true karena:

```rust
map_or(true, ...)
```

tetapi itu bukan desain yang bagus.

Lebih baik:

```text
new()
 ↓
try initial connect
 ↓
success → socket Some
failure → reconnect_at = now + 1s
```

---

# 15. `poll()` error handling terlalu kasar

Sekarang:

```rust
if ret <= 0 {
    should_evaluate = true;
    break;
}
```

Padahal:

```text
ret == 0 → timeout
ret < 0  → error
```

Dan `EINTR` harus diperlakukan berbeda.

Versi lama sudah lebih benar:

```rust
if ret < 0 {
    if err.kind() == Interrupted {
        continue;
    }
    ...
} else if ret == 0 {
    ...
}
```

Saya sarankan **pertahankan pola itu**.

---

# 16. Poll timeout cast bisa bermasalah

```rust
remaining.as_millis() as i32
```

Secara praktik interval kamu kecil, jadi tidak masalah sekarang. Tetapi lebih bersih:

```rust
let timeout_ms = remaining
    .as_millis()
    .min(i32::MAX as u128) as i32;
```

---

# 17. Scheduler sekarang bisa terlalu cepat

Fallback:

```rust
MIN_INTERVAL + ...
```

dan kemudian:

```rust
self.last_interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
```

Tetapi tidak ada lagi logika adaptive hysteresis seperti versi sebelumnya:

```text
interval naik perlahan
interval turun cepat
```

Akibatnya scheduler bisa lebih mudah oscillate:

```text
90s
→ 20s
→ 90s
→ 30s
→ ...
```

Tidak fatal, tapi kurang optimal.

---

# Kesimpulan

**Struktur baru: 8.5/10.**

Secara arsitektur saya **lebih menyukai versi ini daripada kode monolitik sebelumnya**.

Tetapi secara implementasi saat ini saya belum akan menyebutnya final karena ada **4 masalah prioritas tinggi**:

### 🔴 Harus diperbaiki

1. `NetlinkMonitor::try_reconnect()` masih placeholder.
2. `handle_events()` belum benar-benar membaca/memfilter uevent.
3. `AdaptiveScheduler::eta_to()` salah untuk discharge/negative rate.
4. `scheduler` menggunakan `resume_limit` yang berbeda dengan `DecisionEngine`.

### 🟠 Sangat disarankan diperbaiki

5. Initial Netlink connection.
6. Reset backoff setelah reconnect.
7. `MAX_VERIFICATION_RETRIES` off-by-one.
8. `Unmanaged` sebaiknya `RestoreCharging`.
9. Jangan verification `Unmanaged`.
10. Scheduler jangan hardcode `80, 78, 400`.
11. Poll `EINTR` handling.
12. Tambahkan logging state transition.

**Jadi jangan rewrite lagi seluruh file.** Struktur ini sudah layak dipertahankan; yang perlu dilakukan sekarang adalah **fix per modul**, terutama `hardware.rs`, `netlink.rs`, dan `scheduler.rs`.

Kalau ini memang hasil pemisahan dari file `monitor` sebelumnya, **yang paling saya butuhkan sekarang adalah isi asli `netlink.rs`, `hardware.rs`, dan `scheduler.rs`**—bukan `mod.rs` lagi. Dengan tiga file itu saya bisa cek apakah bug di atas memang hanya akibat kode yang kamu paste disingkat (`/* ... creates socket ... */`) atau benar-benar ada di implementasi project kamu.







Oke, saya susun semua fix jadi satu paket, per bagian. Saya taruh helper function `effective_resume_limit`/`effective_thermal_resume` dulu karena dipakai bareng oleh `DecisionEngine` dan `Scheduler` — itu yang nutup gap #8.

## 0. Helper bersama (single source of truth)

```rust
/// Effective resume threshold — dipakai bareng oleh DecisionEngine dan
/// AdaptiveScheduler biar dua-duanya nggak nyimpang. Idealnya ini method
/// di `Config` sendiri; sementara taruh di sini dulu.
fn effective_resume_limit(cfg: &Config) -> u8 {
    if cfg.resume_limit > 0 && cfg.resume_limit < cfg.charge_limit {
        cfg.resume_limit
    } else {
        cfg.charge_limit.saturating_sub(2)
    }
}

fn effective_thermal_resume(cfg: &Config) -> i32 {
    let safe_hysteresis = cfg
        .thermal_resume_hysteresis_dc
        .clamp(1, cfg.max_temp_dc.saturating_sub(1).max(1));
    cfg.max_temp_dc.saturating_sub(safe_hysteresis)
}
```

## 1. `DecisionEngine::evaluate` — pakai helper, bukan hitung ulang

```rust
let thermal_max = cfg.max_temp_dc;
let thermal_resume = effective_thermal_resume(cfg);

let is_thermal = cfg.thermal_cutoff && if self.policy == ChargePolicyState::ThermalCutoff {
    temp > thermal_resume
} else {
    temp >= thermal_max
};

let limit = cfg.charge_limit;
let resume = effective_resume_limit(cfg);

let is_limit = if self.policy == ChargePolicyState::LimitReached {
    cap > resume
} else {
    cap >= limit
};
```
(bagian setelahnya — priority routing — nggak berubah)

## 2. `AdaptiveScheduler` — arah naik/turun + init dari config asli

```rust
const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600);
const UNPLUGGED_HEARTBEAT_NO_NETLINK: Duration = Duration::from_secs(30);

const HISTORY_LEN: usize = 6;
const EMA_ALPHA: f32 = 0.35;
const SAFETY_FACTOR: f32 = 0.25;
const THERMAL_SAFETY_FACTOR: f32 = 0.15;
const RATE_EPSILON: f32 = 0.02; // di bawah ini dianggap flat/noise, bukan gerak beneran

pub struct AdaptiveScheduler {
    pub limit: f32,
    pub resume_limit: f32,   // effective resume, bukan raw cfg.resume_limit
    pub thermal_cutoff: f32,
    pub thermal_resume: f32, // baru — dulu nggak ada, makanya arah turun nggak kehitung
    history: VecDeque<SensorSnapshot>,
    pub last_interval: Duration,
    cap_rate_ema: Option<f32>,
    temp_rate_ema: Option<f32>,
}

impl AdaptiveScheduler {
    /// Diinisialisasi langsung dari config asli — bukan literal hardcode
    /// (80, 78, 400) yang kepisah dari cfg beneran.
    pub fn new(cfg: &Config) -> Self {
        let mut s = Self {
            limit: 0.0, resume_limit: 0.0, thermal_cutoff: 0.0, thermal_resume: 0.0,
            history: VecDeque::new(), last_interval: MIN_INTERVAL,
            cap_rate_ema: None, temp_rate_ema: None,
        };
        s.sync_config(cfg);
        s
    }

    /// Panggil tiap loop (murah — cuma reset prediksi kalau threshold beneran berubah).
    pub fn sync_config(&mut self, cfg: &Config) {
        let new_limit = cfg.charge_limit as f32;
        let new_resume = effective_resume_limit(cfg) as f32;
        let new_thermal_max = cfg.max_temp_dc as f32 / 10.0;
        let new_thermal_resume = effective_thermal_resume(cfg) as f32 / 10.0;

        let changed = (self.limit - new_limit).abs() > f32::EPSILON
            || (self.resume_limit - new_resume).abs() > f32::EPSILON
            || (self.thermal_cutoff - new_thermal_max).abs() > f32::EPSILON
            || (self.thermal_resume - new_thermal_resume).abs() > f32::EPSILON;

        if changed {
            self.limit = new_limit;
            self.resume_limit = new_resume;
            self.thermal_cutoff = new_thermal_max;
            self.thermal_resume = new_thermal_resume;
            self.reset_prediction();
        }
    }

    pub fn observe(&mut self, s: &SensorSnapshot) {
        if let Some(prev) = self.history.back() {
            let dt = s.ts.saturating_duration_since(prev.ts).as_secs_f32();
            if dt >= 0.5 {
                if let (Some(cap), Some(pcap)) = (s.capacity_pct, prev.capacity_pct) {
                    self.cap_rate_ema = Some(ema(self.cap_rate_ema, (cap as f32 - pcap as f32) / dt));
                }
                if let (Some(temp), Some(ptemp)) = (s.temp_dc, prev.temp_dc) {
                    self.temp_rate_ema = Some(ema(self.temp_rate_ema, (temp as f32 - ptemp as f32) / dt));
                }
            }
        }
        self.history.push_back(s.clone());
        while self.history.len() > HISTORY_LEN { self.history.pop_front(); }
    }

    pub fn reset_prediction(&mut self) {
        self.last_interval = MIN_INTERVAL;
        self.cap_rate_ema = None;
        self.temp_rate_ema = None;
        self.history.clear();
    }

    pub fn next_interval(&mut self, s: &SensorSnapshot, netlink_alive: bool) -> Duration {
        if s.online == Some(false) {
            self.last_interval = if netlink_alive { UNPLUGGED_HEARTBEAT } else { UNPLUGGED_HEARTBEAT_NO_NETLINK };
            return self.last_interval;
        }

        let cap = s.capacity_pct.map(|c| c as f32);
        let temp = s.temp_dc.map(|t| t as f32);

        // Naik → menuju limit (lagi charging). Turun → menuju resume (lagi drain).
        let cap_eta = self
            .eta_to_rising(cap, self.limit, self.cap_rate_ema, SAFETY_FACTOR)
            .or_else(|| self.eta_to_falling(cap, self.resume_limit, self.cap_rate_ema, SAFETY_FACTOR));

        // Naik → menuju thermal cutoff. Turun → menuju thermal resume.
        let temp_eta = self
            .eta_to_rising(temp, self.thermal_cutoff * 10.0, self.temp_rate_ema, THERMAL_SAFETY_FACTOR)
            .or_else(|| self.eta_to_falling(temp, self.thermal_resume * 10.0, self.temp_rate_ema, THERMAL_SAFETY_FACTOR));

        let interval = match (cap_eta, temp_eta) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => self.fallback_interval(s),
        };

        self.last_interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
        self.last_interval
    }

    fn eta_to_rising(&self, current: Option<f32>, threshold: f32, rate: Option<f32>, safety: f32) -> Option<Duration> {
        let (current, rate) = (current?, rate?);
        if rate <= RATE_EPSILON { return None; }
        let distance = (threshold - current).max(0.0);
        Some(Duration::from_secs_f32((distance / rate * safety).max(0.0)))
    }

    fn eta_to_falling(&self, current: Option<f32>, threshold: f32, rate: Option<f32>, safety: f32) -> Option<Duration> {
        let (current, rate) = (current?, rate?);
        if rate >= -RATE_EPSILON { return None; }
        let distance = (current - threshold).max(0.0);
        Some(Duration::from_secs_f32((distance / -rate * safety).max(0.0)))
    }

    /// Jaring pengaman kalau kedua arah belum ada rate terpercaya (baru
    /// start, atau lagi flat). Sekarang pertimbangin jarak ke DUA batas
    /// (limit di atas, resume di bawah) — bukan cuma limit kayak sebelumnya.
    fn fallback_interval(&self, s: &SensorSnapshot) -> Duration {
        let cap_frac = s.capacity_pct.map(|c| {
            let c = c as f32;
            let closest = (self.limit - c).max(0.0).min((c - self.resume_limit).max(0.0));
            (closest / self.limit.max(1.0)).clamp(0.0, 1.0)
        });
        let temp_frac = s.temp_dc.map(|t| {
            let t = t as f32;
            let max_dc = self.thermal_cutoff * 10.0;
            let resume_dc = self.thermal_resume * 10.0;
            let closest = (max_dc - t).max(0.0).min((t - resume_dc).max(0.0));
            (closest / max_dc.max(1.0)).clamp(0.0, 1.0)
        });

        let frac = match (cap_frac, temp_frac) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => 1.0,
        };

        MIN_INTERVAL + Duration::from_secs_f32((MAX_INTERVAL - MIN_INTERVAL).as_secs_f32() * frac)
    }
}

fn ema(prev: Option<f32>, sample: f32) -> f32 {
    match prev { Some(p) => EMA_ALPHA * sample + (1.0 - EMA_ALPHA) * p, None => sample }
}
```

## 3. `NetlinkMonitor` — filter uevent beneran + backoff reset

```rust
impl NetlinkMonitor {
    pub fn new() -> Self { Self { socket: None, reconnect_at: None, backoff: Duration::from_secs(1), debounce_target: None } }
    pub fn is_connected(&self) -> bool { self.socket.is_some() }
    pub fn as_raw_fd(&self) -> Option<i32> { self.socket.as_ref().map(|s| s.as_raw_fd()) }
    pub fn disconnect(&mut self) { self.socket = None; }

    pub fn schedule_reconnect(&mut self, now: Instant) {
        self.reconnect_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
    }

    pub fn should_reconnect(&self, now: Instant) -> bool {
        self.socket.is_none() && self.reconnect_at.map_or(true, |t| now >= t)
    }

    /// NB: create_kobject_uevent_socket() itu placeholder nama — ganti
    /// dengan implementasi socket-mu yang asli. Yang penting: backoff
    /// di-reset ke 1s begitu berhasil, dan schedule_reconnect() dipanggil
    /// dari SINI kalau gagal (bukan cuma dari POLLERR/POLLHUP).
    pub fn try_reconnect(&mut self, now: Instant) -> bool {
        match create_kobject_uevent_socket() {
            Ok(fd) => {
                self.socket = Some(fd);
                self.reconnect_at = None;
                self.backoff = Duration::from_secs(1); // reset backoff
                true
            }
            Err(_) => {
                self.schedule_reconnect(now);
                false
            }
        }
    }

    /// Sekarang beneran baca & filter payload uevent, dan drain semua
    /// pesan yang lagi antre (non-blocking loop) — bukan cuma anggap
    /// tiap POLLIN itu event baterai.
    pub fn handle_events(&mut self, now: Instant) -> bool {
        let Some(fd) = self.as_raw_fd() else { return false };
        let mut buf = [0u8; 4096];
        let mut relevant = false;

        loop {
            let n = unsafe {
                libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT)
            };
            if n <= 0 { break; } // EAGAIN atau habis — selesai drain
            if is_power_supply_change(&buf[..n as usize]) {
                relevant = true;
            }
        }

        if relevant {
            self.debounce_target = Some(now + Duration::from_millis(250));
        }
        relevant
    }

    pub fn debounce_due(&mut self, now: Instant) -> bool {
        if let Some(target) = self.debounce_target {
            if now >= target { self.debounce_target = None; return true; }
        }
        false
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.debounce_target.or(self.reconnect_at)
    }
}

fn is_power_supply_change(buf: &[u8]) -> bool {
    contains_subslice(buf, b"SUBSYSTEM=power_supply") && contains_subslice(buf, b"ACTION=change")
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
```

## 4. `HardwareController::apply_target` — `Unmanaged` skip verifikasi

```rust
pub fn apply_target(&mut self, target: HardwareTarget) {
    self.target = target;

    if target == HardwareTarget::Unmanaged {
        // verify() buat Unmanaged selalu true, jadi nggak perlu nunggu
        // siklus verifikasi 500ms — langsung anggap synced.
        let success = control::set_charging(true).is_ok();
        self.verification = None;
        self.verification_failures = 0;
        self.generation += 1;
        if success {
            self.force_apply = false;
            self.sync = SyncState::Synced;
        } else {
            self.force_apply = true;
            self.sync = SyncState::Failed;
        }
        return;
    }

    let success = match target {
        HardwareTarget::ChargingEnabled => control::set_charging(true).is_ok(),
        HardwareTarget::ChargingDisabled => control::set_charging(false).is_ok(),
        HardwareTarget::Unmanaged => unreachable!(),
    };

    if success {
        self.force_apply = false;
        self.sync = SyncState::Pending;
        self.verification_failures = 0;
        self.generation += 1;
        self.verification = Some(Verification {
            generation: self.generation,
            target,
            deadline: Instant::now() + VERIFY_DELAYS[0],
        });
    } else {
        self.invalidate_verification();
        self.force_apply = true;
        self.sync = SyncState::Failed;
    }
}
```
*(Catatan: di jalur `Unmanaged` sengaja nggak pakai `invalidate_verification()` — soalnya itu nge-set `sync = Unknown`, yang bakal nimpa `Synced` yang baru kita set. Ini persis bug yang sama seperti fix pertama kita, jadi saya hindari manggilnya di sini.)*

## 5. Main loop — init scheduler, sync_config, EINTR, logging

```rust
pub fn run_monitor_loop(config: Arc<RwLock<Config>>, rx: UnixDatagram) {
    let mut battery_reader = CachedReader::new();
    let mut netlink = NetlinkMonitor::new();
    let mut engine = DecisionEngine::new();
    let mut hardware = HardwareController::new();
    let mut scheduler = AdaptiveScheduler::new(&config.read().unwrap().clone()); // init dari cfg asli

    let mut pfds = [
        libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: -1, events: 0, revents: 0 },
    ];

    loop {
        let now = Instant::now();
        let cfg = config.read().unwrap().clone();
        scheduler.sync_config(&cfg); // dukung reload tanpa nyangkut di threshold lama

        let snapshot = SensorSnapshot {
            capacity_pct: battery_reader.read_capacity().ok(),
            temp_dc: battery_reader.read_temperature_dc().ok(),
            current_ma: battery_reader.read_current_ma().map(|c| c as i32).ok(),
            status: battery_reader.read_status().ok(),
            online: battery_reader.is_plugged_in().ok(),
            ts: Instant::now(),
        };

        if hardware.sync == SyncState::Synced || hardware.sync == SyncState::Unknown {
            scheduler.observe(&snapshot);
        }

        if hardware.verification_due() {
            hardware.verify(&snapshot);
        }

        let old_target = hardware.target;
        let old_policy = engine.policy;
        let decision = engine.evaluate(&snapshot, &cfg);

        if decision.policy != old_policy {
            eprintln!("[charger] {:?} -> {:?} ({})", old_policy, decision.policy, decision.reason);
        }

        if decision.target != old_target {
            hardware.invalidate_verification();
            hardware.force_apply = true;
        }

        if hardware.needs_apply(decision.target) {
            hardware.apply_target(decision.target);
        }

        if netlink.should_reconnect(now) {
            netlink.try_reconnect(now);
        }

        let timeout = scheduler.next_interval(&snapshot, netlink.is_connected());

        let mut should_evaluate = false;
        let mut loop_now = Instant::now();
        let target_wake = loop_now + timeout;

        while loop_now < target_wake {
            let mut next_wake = target_wake;

            if let Some(nd) = netlink.next_deadline() {
                if loop_now >= nd {
                    if netlink.debounce_due(loop_now) || netlink.should_reconnect(loop_now) {
                        should_evaluate = true; break;
                    }
                }
                next_wake = next_wake.min(nd);
            }

            if let Some(vd) = hardware.next_deadline() {
                if loop_now >= vd {
                    should_evaluate = true; break;
                }
                next_wake = next_wake.min(vd);
            }

            let remaining = next_wake.saturating_duration_since(loop_now);
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;

            pfds[0].revents = 0;
            let mut num_fds = 1;
            if let Some(nl_fd) = netlink.as_raw_fd() {
                pfds[1].fd = nl_fd;
                pfds[1].events = libc::POLLIN;
                pfds[1].revents = 0;
                num_fds = 2;
            } else {
                pfds[1].fd = -1;
            }

            let ret = unsafe { libc::poll(pfds.as_mut_ptr(), num_fds, timeout_ms) };
            loop_now = Instant::now();

            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // EINTR — bukan timeout, ulang tanpa evaluate
                }
                eprintln!("[charger] poll() error: {err}");
                should_evaluate = true;
                break;
            }
            if ret == 0 {
                should_evaluate = true;
                break;
            }

            if pfds[0].revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 1];
                if rx.recv(&mut buf).is_ok() {
                    if buf[0] == 2 { return; }
                    if buf[0] == 1 {
                        should_evaluate = true;
                        hardware.invalidate_verification();
                        hardware.force_apply = true;
                        scheduler.reset_prediction();
                        break;
                    }
                }
            }

            if num_fds > 1 {
                if pfds[1].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    netlink.disconnect();
                    netlink.schedule_reconnect(loop_now);
                } else if pfds[1].revents & libc::POLLIN != 0 {
                    netlink.handle_events(loop_now);
                }
            }
        }
        if !should_evaluate { continue; }
    }
}
```

Catatan jujur: `create_kobject_uevent_socket()` di `try_reconnect()` itu nama placeholder — ganti dengan fungsi socket-creation aslimu (yang menurut memory-mu udah pernah jalan & keverifikasi di device nyata). `eprintln!` buat logging juga cuma ilustrasi; ganti sama `log::info!`/`tracing::info!` kalau kamu udah pakai itu di project.