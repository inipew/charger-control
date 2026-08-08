use charger_core::battery::reader::BatteryStatus;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingState {
    Charging,
    NotCharging,
    Full,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temp_dc: Option<i32>,
    #[allow(dead_code)]
    pub current_ma: Option<i32>,
    pub status: Option<BatteryStatus>,
    pub online: Option<bool>,
    pub ts: Instant,
}

impl SensorSnapshot {
    pub fn charging_state(&self) -> ChargingState {
        match self.status {
            Some(BatteryStatus::Charging) => ChargingState::Charging,
            Some(BatteryStatus::NotCharging) => ChargingState::NotCharging,
            Some(BatteryStatus::Full) => ChargingState::Full,
            _ => ChargingState::Unknown,
        }
    }
}
