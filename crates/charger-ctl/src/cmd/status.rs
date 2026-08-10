use charger_core::battery::{health, reader};
use charger_core::error::ChargerError;

use crate::display;

pub fn run(json: bool) -> Result<(), ChargerError> {
    if json {
        if let Some(json_str) = try_get_ipc_status_json() {
            println!("{json_str}");
            return Ok(());
        }

        let input_current = reader::read_input_current_ua().ok();
        let voltage_uv = reader::read_voltage_uv().ok();
        let wattage = match (input_current, voltage_uv) {
            (Some(c), Some(v)) => Some(reader::calc_wattage_from_ua_w(v, c).abs()),
            _ => None,
        };

        let status_data = serde_json::json!({
            "battery_level_percent": reader::read_capacity().ok(),
            "power_state": reader::get_power_state().ok().map(|v| format!("{v:?}")),
            "battery_current_ma": reader::read_battery_current_ua().ok().map(|v| v as f32 / 1000.0),
            "input_current_ma": input_current.map(|v| v as f32 / 1000.0),
            "voltage_mv": voltage_uv.map(|v| v / 1000),
            "wattage_w": wattage,
            "temperature_c": reader::read_temperature_dc().ok().map(|v| v as f32 / 10.0),
            "health": health::read_health().ok().map(|v| v.to_string()),
            "technology": reader::read_technology().ok(),
            "design_capacity_mah": reader::read_charge_full_design().ok(),
            "cycle_count": reader::read_cycle_count().ok(),
        });

        if let Ok(json_output) = serde_json::to_string_pretty(&status_data) {
            println!("{json_output}");
        }
        return Ok(());
    }

    display::title("Battery Status");

    let level = reader::read_capacity().unwrap_or(0);

    display::key_val("Level", format!("{}%", level));

    if let Ok(power_state) = reader::get_power_state() {
        display::key_val("Power State", format!("{:?}", power_state));
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
        display::key_val("Voltage", format!("{} mV", voltage / 1000));
    }

    if let (Some(current_ua), Some(voltage_uv)) = (input_current, voltage_uv) {
        let watts = reader::calc_wattage_from_ua_w(voltage_uv, current_ua);

        display::key_val("Wattage", format!("{:.2} W", watts.abs()));
    }

    if let Ok(temp) = reader::read_temperature_dc() {
        display::key_val("Temperature", format!("{:.1} °C", temp as f32 / 10.0));
    }

    if let Ok(health_status) = health::read_health() {
        display::key_val("Health", health_status);
    }

    if let Ok(technology) = reader::read_technology() {
        display::key_val("Technology", technology);
    }

    if let Ok(capacity) = reader::read_charge_full_design() {
        display::key_val("Design Capacity", format!("{} mAh", capacity));
    }

    if let Ok(cycles) = reader::read_cycle_count() {
        display::key_val("Cycle Count", format!("{}", cycles));
    }

    Ok(())
}

fn try_get_ipc_status_json() -> Option<String> {
    match crate::client::IpcClient::send_command(b"status json", std::time::Duration::from_secs(2))
    {
        Ok(response) if response.starts_with("OK:") => Some(
            response
                .strip_prefix("OK:")
                .unwrap_or(&response)
                .trim()
                .to_string(),
        ),
        _ => None,
    }
}
