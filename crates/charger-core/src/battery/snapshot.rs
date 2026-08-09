use crate::battery::reader::BatteryStatus;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temp_dc: Option<i32>,
    pub current_ma: Option<i32>,
    pub status: Option<BatteryStatus>,
    pub online: Option<bool>,
    pub ts: Instant,
}
