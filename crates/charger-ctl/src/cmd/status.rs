use charger_core::error::ChargerError;
use charger_core::battery::{reader, health};
use crate::display;

pub fn run() -> Result<(), ChargerError> {
    display::title("Battery Status");

    let level = reader::read_capacity().unwrap_or(0);
    display::key_val("Level", format!("{}%", level));

    if let Ok(ua) = reader::read_battery_current_ua() {
        display::key_val("Battery Current", format!("{:.1} mA", ua as f32 / 1000.0));
    }

    if let Ok(ua) = reader::read_input_current_ua() {
        display::key_val("Input Current", format!("{:.1} mA", ua as f32 / 1000.0));
    }

    if let Ok(uv) = reader::read_voltage_uv() {
        display::key_val("Voltage", format!("{} mV", uv / 1000));
    }

    if let Ok(temp) = reader::read_temperature_dc() {
        display::key_val("Temperature", format!("{:.1} °C", temp as f32 / 10.0));
    }

    if let Ok(ua) = reader::read_input_current_ua() {
        if let Ok(uv) = reader::read_voltage_uv() {
            let watts = reader::calc_wattage_w(uv, ua as f32 / 1000.0);
            display::key_val("Wattage", format!("{:.2} W", watts.abs()));
        }
    }

    if let Ok(h) = health::read_health() {
        display::key_val("Health", h);
    }

    if let Ok(tech) = reader::read_technology() {
        display::key_val("Technology", tech);
    }

    if let Ok(cap) = reader::read_charge_full_design() {
        display::key_val("Design Capacity", format!("{} mAh", cap));
    }

    if let Ok(cycles) = reader::read_cycle_count() {
        display::key_val("Cycle Count", cycles);
    }

    Ok(())
}
