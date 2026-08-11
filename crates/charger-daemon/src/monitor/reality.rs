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
}

impl ObservedState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            power_state: PowerState::Unknown,
            sample: None,
            timestamp: Instant::now(),
            sample_retry_at: None,
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
        }
        self.timestamp = now;
    }

    pub fn mark_sample_failed(&mut self, retry_at: Instant) {
        self.sample_retry_at = Some(retry_at);
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

    pub fn sample_stale_deadline(&self) -> Option<Instant> {
        self.sample.map(|s| s.timestamp + SAMPLE_STALE_THRESHOLD)
    }
}
