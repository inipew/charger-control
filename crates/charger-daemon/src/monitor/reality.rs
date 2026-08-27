use std::time::{Duration, Instant};

use charger_core::battery::reader::{self, PowerState};

pub const ATTACH_SETTLE_WINDOW: Duration = Duration::from_secs(5);
pub const SAMPLE_STALE_THRESHOLD: Duration = Duration::from_secs(60);

/// Snapshot pengamatan baterai pada satu titik waktu.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub capacity: f32,
    pub temperature_c: f32,
    #[allow(dead_code)]
    pub power_state: PowerState,
    pub timestamp: Instant,
}

impl Sample {
    pub fn read(
        power_state: PowerState,
        now: Instant,
    ) -> Result<Self, charger_core::error::ChargerError> {
        let capacity = reader::read_capacity_raw()?;
        let temperature_c = reader::read_temperature_c()?;

        Ok(Self {
            capacity,
            temperature_c,
            power_state,
            timestamp: now,
        })
    }

    pub fn is_stale(&self, now: Instant) -> bool {
        now.duration_since(self.timestamp) >= SAMPLE_STALE_THRESHOLD
    }
}

/// Status koneksi steker/pengisi daya.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Attaching { since: Instant },
    Attached,
}

impl ConnectionState {
    #[allow(dead_code)]
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::Disconnected)
    }

    #[allow(dead_code)]
    pub fn is_stable(&self) -> bool {
        matches!(self, Self::Attached)
    }

    #[allow(dead_code)]
    pub fn is_operational(&self) -> bool {
        matches!(self, Self::Attached)
    }

    pub fn is_connected(&self) -> bool {
        !matches!(self, Self::Disconnected)
    }

    pub fn update(&mut self, power_state: PowerState, now: Instant) {
        let is_plugged = power_state.is_plugged_in();

        match (*self, is_plugged, power_state) {
            (_, false, PowerState::Unknown) => {
                // Do not transition to Disconnected on Unknown read failure
            }
            (Self::Disconnected, true, _) => {
                *self = Self::Attaching { since: now };
            }
            (Self::Attaching { since }, true, _) => {
                if now.duration_since(since) >= ATTACH_SETTLE_WINDOW {
                    *self = Self::Attached;
                }
            }
            (Self::Attached, true, _) => {}
            (_, false, _) => {
                *self = Self::Disconnected;
            }
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if let Self::Attaching { since } = *self {
            if now.duration_since(since) >= ATTACH_SETTLE_WINDOW {
                *self = Self::Attached;
            }
        }
    }

    pub fn next_transition(&self) -> Option<Instant> {
        match self {
            Self::Attaching { since } => Some(*since + ATTACH_SETTLE_WINDOW),
            _ => None,
        }
    }
}

/// Status kesehatan pembacaan sensor telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorHealth {
    pub consecutive_failures: u32,
    pub last_failure_at: Option<Instant>,
    pub last_success_at: Option<Instant>,
}

impl SensorHealth {
    pub const fn new() -> Self {
        Self {
            consecutive_failures: 0,
            last_failure_at: None,
            last_success_at: None,
        }
    }

    pub fn mark_success(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        self.last_success_at = Some(now);
    }

    pub fn mark_failure(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_failure_at = Some(now);
    }
}

impl Default for SensorHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Fakta murni dari sensor (Power State & Battery Snapshot).
///
/// Modul ini TIDAK menyimpan boolean policy atau status hardware yang diharapkan.
#[derive(Debug, Clone)]
pub struct ObservedState {
    pub connection: ConnectionState,
    pub power_state: PowerState,
    pub sample: Option<Sample>,
    pub timestamp: Instant,
    pub sample_retry_at: Option<Instant>,
    pub sensor_health: SensorHealth,
}

impl ObservedState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            power_state: PowerState::Unknown,
            sample: None,
            timestamp: Instant::now(),
            sample_retry_at: None,
            sensor_health: SensorHealth::new(),
        }
    }

    pub fn update_connection(&mut self, power_state: PowerState, now: Instant) {
        self.power_state = power_state;
        self.connection.update(power_state, now);
        self.connection.tick(now);
    }

    pub fn update(&mut self, power_state: PowerState, sample: Option<Sample>, now: Instant) {
        self.power_state = power_state;
        if let Some(s) = sample {
            self.sample = Some(s);
            self.sample_retry_at = None;
            self.sensor_health.mark_success(now);
        }
        self.timestamp = now;
    }

    pub fn mark_sample_failed(&mut self, retry_at: Instant, now: Instant) {
        self.sample_retry_at = Some(retry_at);
        self.sensor_health.mark_failure(now);
    }

    pub fn clear_sample(&mut self) {
        self.sample = None;
        self.sample_retry_at = None;
    }

    pub fn next_sensor_retry(&self) -> Option<Instant> {
        self.sample_retry_at
    }

    #[allow(dead_code)]
    pub fn has_fresh_sample(&self, now: Instant) -> bool {
        match self.sample {
            Some(s) => !s.is_stale(now),
            None => false,
        }
    }

    /// Memeriksa apakah data sensor aman dan valid untuk evaluasi kebijakan keselamatan.
    ///
    /// Jika pembacaan sensor gagal berulang (consecutive_failures >= 2), data sensor lama
    /// tidak boleh dipercaya demi keselamatan fisik/termal baterai.
    pub fn is_sensor_safe(&self, now: Instant) -> bool {
        if self.sensor_health.consecutive_failures >= 2 {
            return false;
        }
        match self.sample {
            Some(s) => !s.is_stale(now),
            None => false,
        }
    }

    pub fn sample_stale_deadline(&self) -> Option<Instant> {
        self.sample.map(|s| s.timestamp + SAMPLE_STALE_THRESHOLD)
    }
}
