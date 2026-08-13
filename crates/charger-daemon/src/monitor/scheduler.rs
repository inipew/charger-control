use std::time::{Duration, Instant};

use super::reality::Sample;

pub const ERROR_BACKOFF_INITIAL: Duration = Duration::from_secs(2);
pub const ERROR_BACKOFF_MAX: Duration = Duration::from_secs(60);

// FailureKind removed as hardware errors are handled entirely by HardwareTrack.

/// Klasifikasi tingkat urgensi polling scheduler daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Idle,
    Normal,
    Monitoring,
    Recovery,
    Safety,
}

impl Urgency {
    pub const fn priority(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Normal => 1,
            Self::Monitoring => 2,
            Self::Recovery => 3,
            Self::Safety => 4,
        }
    }

    pub const fn max(self, other: Self) -> Self {
        if self.priority() >= other.priority() {
            self
        } else {
            other
        }
    }
}

/// Penjejak status penjadwalan.
#[derive(Debug)]
pub struct SchedulingState {
    pub evaluation_requested: bool,
    pub force_hardware_verification: bool,
    pub snapshot_backoff: Duration,
}

impl SchedulingState {
    pub fn new() -> Self {
        Self {
            evaluation_requested: true,
            force_hardware_verification: true,
            snapshot_backoff: ERROR_BACKOFF_INITIAL,
        }
    }

    pub fn mark_evaluation_requested(&mut self) {
        self.evaluation_requested = true;
    }

    pub fn mark_force_hardware_verification(&mut self) {
        self.force_hardware_verification = true;
    }

    pub fn clear_evaluation_request(&mut self) {
        self.evaluation_requested = false;
    }

    pub fn clear_hardware_verification_request(&mut self) {
        self.force_hardware_verification = false;
    }

    pub fn mark_snapshot_success(&mut self) {
        self.snapshot_backoff = ERROR_BACKOFF_INITIAL;
    }

    pub fn mark_snapshot_failure(&mut self) -> Duration {
        let current = self.snapshot_backoff;
        self.snapshot_backoff = (self.snapshot_backoff * 2).min(ERROR_BACKOFF_MAX);
        current
    }
}

/// Adaptive Scheduler yang menyesuaikan selang polling berbasis urgensi dan earliest deadlines.
#[derive(Debug)]
pub struct AdaptiveScheduler {
    pub configured_interval: Duration,
    pub charge_limit: f32,
    pub max_temp_c: f32,
    pub last_capacity: Option<f32>,
    pub last_temp_c: Option<f32>,
}

impl AdaptiveScheduler {
    pub fn new(configured_interval: Duration, charge_limit: f32, max_temp_c: f32) -> Self {
        Self {
            configured_interval,
            charge_limit,
            max_temp_c,
            last_capacity: None,
            last_temp_c: None,
        }
    }

    pub fn update_config(&mut self, interval: Duration, charge_limit: f32, max_temp_c: f32) {
        self.configured_interval = interval;
        self.charge_limit = charge_limit;
        self.max_temp_c = max_temp_c;
    }

    pub fn reset_history(&mut self) {
        self.last_capacity = None;
        self.last_temp_c = None;
    }

    pub fn update_sample(&mut self, sample: &Sample) {
        self.last_capacity = Some(sample.capacity);
        self.last_temp_c = Some(sample.temperature_c);
    }

    pub fn calculate_next_interval(
        &self,
        urgency: Urgency,
        earliest_deadline: Option<Instant>,
        now: Instant,
    ) -> Duration {
        if urgency == Urgency::Recovery {
            if let Some(target) = earliest_deadline {
                if target > now {
                    return (target - now).clamp(Duration::from_secs(1), ERROR_BACKOFF_MAX);
                }
            }
            return ERROR_BACKOFF_INITIAL;
        }

        let base_secs = self.configured_interval.as_secs_f32();

        let mut secs = match urgency {
            Urgency::Idle => base_secs * 6.0,
            Urgency::Normal => base_secs,
            Urgency::Monitoring => base_secs * 0.5,
            Urgency::Recovery => ERROR_BACKOFF_INITIAL.as_secs_f32(),
            Urgency::Safety => 2.0,
        };

        // Adaptasi Dinamis: Jika mendekati batas charge limit (< 5%) atau batas suhu (< 2.0°C), percepat interval
        if urgency == Urgency::Normal || urgency == Urgency::Monitoring {
            let close_to_limit = self
                .last_capacity
                .is_some_and(|cap| cap < self.charge_limit && (self.charge_limit - cap) <= 5.0);
            let close_to_temp = self
                .last_temp_c
                .is_some_and(|temp| temp < self.max_temp_c && (self.max_temp_c - temp) <= 2.0);

            if close_to_limit || close_to_temp {
                secs = (secs * 0.5).max(2.0);
            }
        }

        let calculated = Duration::from_secs_f32(secs.max(1.0));

        if let Some(target) = earliest_deadline {
            if target > now {
                let remaining = target - now;
                return calculated.min(remaining);
            }
        }

        calculated
    }
}
