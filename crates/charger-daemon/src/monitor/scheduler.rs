use super::snapshot::{ChargingState, SensorSnapshot};
use std::collections::VecDeque;
use std::time::Duration;

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(90);
const UNPLUGGED_HEARTBEAT: Duration = Duration::from_secs(600); // 10 minutes
const UNPLUGGED_HEARTBEAT_NO_NETLINK: Duration = Duration::from_secs(30);

const DANGER_TEMP_MARGIN: f32 = 3.0;
const DANGER_CAP_MARGIN: f32 = 2.0;
const EMA_ALPHA: f32 = 0.3;
const PREDICTION_SAFETY_FACTOR: f32 = 0.5;
const TEMP_RATE_DANGER: f32 = 0.15;
const EMA_HISTORY_LEN: usize = 5;

pub struct AdaptiveScheduler {
    pub limit: f32,
    pub resume_limit: f32,
    pub thermal_cutoff: f32,

    history: VecDeque<SensorSnapshot>,
    ema_cap_rate: f32,
    ema_temp_rate: f32,
    last_interval: Duration,
}

impl AdaptiveScheduler {
    pub fn new(limit: u8, resume_limit: u8, thermal_cutoff: i32) -> Self {
        Self {
            limit: limit as f32,
            resume_limit: resume_limit as f32,
            thermal_cutoff: thermal_cutoff as f32 / 10.0,
            history: VecDeque::new(),
            ema_cap_rate: 0.0,
            ema_temp_rate: 0.0,
            last_interval: MIN_INTERVAL,
        }
    }

    pub fn observe(&mut self, s: &SensorSnapshot) {
        if let Some(prev) = self.history.back() {
            if prev.charging_state() != s.charging_state() {
                self.ema_cap_rate = 0.0;
                self.ema_temp_rate = 0.0;
            }

            let dt = (s.ts - prev.ts).as_secs_f32().max(0.5);

            if let (Some(cap), Some(prev_cap)) = (s.capacity_pct, prev.capacity_pct) {
                self.ema_cap_rate = EMA_ALPHA * ((cap as f32 - prev_cap as f32) / dt)
                    + (1.0 - EMA_ALPHA) * self.ema_cap_rate;
            }
            if let (Some(temp), Some(prev_temp)) = (s.temp_dc, prev.temp_dc) {
                self.ema_temp_rate = EMA_ALPHA
                    * ((temp as f32 / 10.0 - prev_temp as f32 / 10.0) / dt)
                    + (1.0 - EMA_ALPHA) * self.ema_temp_rate;
            }
        }
        self.history.push_back(s.clone());
        if self.history.len() > EMA_HISTORY_LEN {
            self.history.pop_front();
        }
    }

    pub fn reset_prediction(&mut self) {
        self.history.clear();
        self.ema_cap_rate = 0.0;
        self.ema_temp_rate = 0.0;
        self.last_interval = MIN_INTERVAL;
    }

    pub fn next_interval(&mut self, s: &SensorSnapshot, netlink_alive: bool) -> Duration {
        if s.online == Some(false) {
            self.last_interval = if netlink_alive {
                UNPLUGGED_HEARTBEAT
            } else {
                UNPLUGGED_HEARTBEAT_NO_NETLINK
            };
            return self.last_interval;
        }

        let (Some(cap), Some(temp)) = (s.capacity_pct, s.temp_dc) else {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        };

        let cap = cap as f32;
        let temp = temp as f32 / 10.0;
        let cstate = s.charging_state();

        let dist_to_limit = (self.limit - cap).max(0.0);
        let dist_to_thermal = (self.thermal_cutoff - temp).max(0.0);

        let danger_high = dist_to_limit < DANGER_CAP_MARGIN
            || dist_to_thermal < DANGER_TEMP_MARGIN
            || self.ema_temp_rate > TEMP_RATE_DANGER;

        if danger_high {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        if cstate == ChargingState::Unknown {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        if cstate != ChargingState::Charging && cap <= self.resume_limit {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        let dist_to_resume = (cap - self.resume_limit).max(0.0);
        let danger_low = cstate != ChargingState::Charging && dist_to_resume < DANGER_CAP_MARGIN;

        if danger_low {
            self.last_interval = MIN_INTERVAL;
            return self.last_interval;
        }

        let predicted = if cstate == ChargingState::Charging && self.ema_cap_rate > 0.01 {
            Duration::from_secs_f32(
                (dist_to_limit / self.ema_cap_rate * PREDICTION_SAFETY_FACTOR).max(0.0),
            )
        } else if cstate != ChargingState::Charging && self.ema_cap_rate < -0.01 {
            Duration::from_secs_f32(
                (dist_to_resume / (-self.ema_cap_rate) * PREDICTION_SAFETY_FACTOR).max(0.0),
            )
        } else {
            MAX_INTERVAL
        };
        let target = predicted.clamp(MIN_INTERVAL, MAX_INTERVAL);

        self.last_interval = if target < self.last_interval {
            target
        } else {
            self.last_interval
                .mul_f32(1.5)
                .min(MAX_INTERVAL)
                .min(target.max(self.last_interval))
        };
        self.last_interval
    }
}
