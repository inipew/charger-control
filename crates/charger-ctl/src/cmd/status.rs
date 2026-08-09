use charger_core::battery::{health, reader};
use charger_core::error::ChargerError;

use crate::display;

pub fn run() -> Result<(), ChargerError> {
    display::title("Battery Status");

    let level = reader::read_capacity().unwrap_or(0);

    display::key_val(
        "Level",
        format!("{}%", level),
    );

    if let Ok(power_state) = reader::get_power_state() {
        display::key_val(
            "Power State",
            format!("{:?}", power_state),
        );
    }

    if let Ok(current_ua) = reader::read_battery_current_ua() {
        display::key_val(
            "Battery Current",
            format!("{:.1} mA", current_ua as f32 / 1000.0),
        );
    }

    let input_current = reader::read_input_current_ua().ok();

    if let Some(current_ua) = input_current {
        display::key_val(
            "Input Current",
            format!("{:.1} mA", current_ua as f32 / 1000.0),
        );
    }

    let voltage_uv = reader::read_voltage_uv().ok();

    if let Some(voltage) = voltage_uv {
        display::key_val(
            "Voltage",
            format!("{} mV", voltage / 1000),
        );
    }

    if let (Some(current_ua), Some(voltage_uv)) =
        (input_current, voltage_uv)
    {
        let watts = reader::calc_wattage_w(
            voltage_uv,
            current_ua as f32 / 1000.0,
        );

        display::key_val(
            "Wattage",
            format!("{:.2} W", watts.abs()),
        );
    }

    if let Ok(temp) = reader::read_temperature_dc() {
        display::key_val(
            "Temperature",
            format!("{:.1} °C", temp as f32 / 10.0),
        );
    }

    if let Ok(health_status) = health::read_health() {
        display::key_val(
            "Health",
            health_status,
        );
    }

    if let Ok(technology) = reader::read_technology() {
        display::key_val(
            "Technology",
            technology,
        );
    }

    if let Ok(capacity) = reader::read_charge_full_design() {
        display::key_val(
            "Design Capacity",
            format!("{} mAh", capacity),
        );
    }

    if let Ok(cycles) = reader::read_cycle_count() {
        display::key_val(
            "Cycle Count",
            cycles,
        );
    }

    Ok(())
}