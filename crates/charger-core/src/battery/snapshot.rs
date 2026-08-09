use crate::battery::reader::BatteryStatus;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct SensorSnapshot {
    pub capacity_pct: Option<u8>,
    pub temp_dc: Option<i32>,
    /// Arus baterai aktual (mA). Sign bersifat vendor-specific — jangan dibalik.
    /// Pada sebagian device: negatif = charging, positif = discharging.
    pub battery_current_ma: Option<i32>,
    /// Arus dari sumber daya eksternal (mA). None = sensor tidak terbaca (bukan Offline).
    pub input_current_ma: Option<i32>,
    pub status: Option<BatteryStatus>,
    // `online` dihapus — presence sekarang diturunkan oleh PresenceTracker dari input_current_ma
    pub ts: Instant,
}
