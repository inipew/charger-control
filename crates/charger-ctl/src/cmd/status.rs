use charger_core::battery::{health, reader};
use charger_core::error::ChargerError;

use crate::display;

pub fn run(json: bool) -> Result<(), ChargerError> {
    if let Some(json_str) = try_get_ipc_status_json() {
        if json {
            println!("{json_str}");
            return Ok(());
        }

        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&json_str) {
            display::title("Daemon & Battery Status");
            display::key_val(
                "Daemon Mode",
                data["mode"].as_str().unwrap_or("Unknown").to_string(),
            );
            if let Some(fsm) = data["fsm_state"].as_str() {
                display::key_val("FSM State", fsm);
            }
            if let Some(target) = data["target_decision"].as_str() {
                display::key_val("Target Action", target);
            }
            if let Some(cur_reg) = data["current_regulation"].as_str() {
                display::key_val("Current Regulation", cur_reg);
            }
            if let Some(hw) = data["hardware_state"].as_str() {
                display::key_val("Hardware State", hw);
            }
            if let Some(conv) = data["convergence_state"].as_str() {
                display::key_val("Convergence", conv);
            }
            display::key_val(
                "Daemon CPU",
                format!("{:.2}%", data["cpu_percent"].as_f64().unwrap_or(0.0)),
            );
            display::key_val(
                "Daemon Memory",
                format!("{:.2} MB", data["memory_rss_mb"].as_f64().unwrap_or(0.0)),
            );
            display::key_val(
                "Poll Interval",
                format!("{} ms", data["poll_interval_ms"].as_u64().unwrap_or(0)),
            );

            let level = data["battery_level_percent"]
                .as_u64()
                .map(|v| v as u8)
                .or_else(|| reader::read_capacity().ok());
            if let Some(l) = level {
                if let Ok(raw_soc) = reader::read_capacity_raw() {
                    display::key_val("Level", format!("{l}% ({raw_soc:.2}%)"));
                } else {
                    display::key_val("Level", format!("{l}%"));
                }
            }

            display::key_val(
                "Power State",
                data["power_state"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_string(),
            );

            if let Ok(metrics) = reader::get_battery_metrics() {
                let sign_str = if metrics.is_charging_flow { "+" } else { "-" };
                let state_str = if metrics.is_charging_flow {
                    "Charging"
                } else {
                    "Discharging"
                };
                display::key_val(
                    "Battery Current",
                    format!("{}{:.1} mA ({})", sign_str, metrics.current_ma, state_str),
                );
                display::key_val("Battery Power", format!("{:.2} W", metrics.wattage_w));
            }

            let temp = data["battery_temperature_c"]
                .as_f64()
                .map(|v| v as f32)
                .or_else(|| reader::read_temperature_c().ok());
            if let Some(t) = temp {
                display::key_val("Temperature", format!("{t:.1} °C"));
            }

            display::key_val(
                "Charge Limit",
                format!("{}%", data["charge_limit"].as_u64().unwrap_or(0)),
            );
            display::key_val(
                "Resume Limit",
                format!("{}%", data["resume_limit"].as_u64().unwrap_or(0)),
            );

            let max_curr = data["max_charge_current_ma"].as_u64().unwrap_or(0);
            if max_curr > 0 {
                display::key_val("Max Charge Current", format!("{max_curr} mA"));
            } else {
                display::key_val("Max Charge Current", "Unconstrained (Full Speed)");
            }

            if let Some(fc_status) = data["fast_charge_status"].as_str() {
                display::key_val("Fast Charge Bypass", fc_status);
            } else if let Some(fc) = data["fast_charge"].as_bool() {
                let max_soc = data["fast_charge_max_soc"].as_u64().unwrap_or(90);
                display::key_val(
                    "Fast Charge Bypass",
                    if fc {
                        format!("Enabled (Max: {max_soc}%)")
                    } else {
                        "Disabled".to_string()
                    },
                );
            }

            display::key_val(
                "Thermal Throttling",
                if data["thermal_throttling_enabled"].as_bool().unwrap_or(true) {
                    "ON (Stepped)".to_string()
                } else {
                    "OFF".to_string()
                },
            );

            display::key_val(
                "Thermal Cutoff",
                if data["thermal_cutoff"].as_bool().unwrap_or(false) {
                    "ON".to_string()
                } else {
                    "OFF".to_string()
                },
            );
            display::key_val(
                "Max Temp",
                format!("{:.1} °C", data["max_temp_c"].as_f64().unwrap_or(0.0)),
            );

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

            return Ok(());
        }
    }

    if json {
        let input_current = reader::read_input_current_ua().ok();
        let voltage_uv = reader::read_voltage_uv().ok();
        let metrics = reader::get_battery_metrics().ok();

        let status_data = serde_json::json!({
            "battery_level_percent": reader::read_capacity().ok(),
            "battery_level_fractional": reader::read_capacity_raw().ok(),
            "power_state": reader::get_power_state().ok().map(|v| format!("{v:?}")),
            "battery_current_ma": metrics.map(|m| m.current_ma),
            "battery_signed_current_ma": metrics.map(|m| m.signed_ma),
            "is_charging_flow": metrics.map(|m| m.is_charging_flow),
            "input_current_ma": input_current.map(|v| v as f32 / 1000.0),
            "voltage_mv": voltage_uv.map(|v| v / 1000),
            "wattage_w": metrics.map(|m| m.wattage_w),
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

    display::title("Battery Status (Fallback)");

    let raw_soc = reader::read_capacity_raw().ok();
    let level = reader::read_capacity().unwrap_or(0);
    if let Some(raw) = raw_soc {
        display::key_val("Level", format!("{level}% ({raw:.2}%)"));
    } else {
        display::key_val("Level", format!("{level}%"));
    }

    if let Ok(power_state) = reader::get_power_state() {
        display::key_val("Power State", format!("{:?}", power_state));
    }

    if let Ok(metrics) = reader::get_battery_metrics() {
        let sign_str = if metrics.is_charging_flow { "+" } else { "-" };
        let state_str = if metrics.is_charging_flow {
            "Charging"
        } else {
            "Discharging"
        };
        display::key_val(
            "Battery Current",
            format!("{}{:.1} mA ({})", sign_str, metrics.current_ma, state_str),
        );
        display::key_val("Battery Power", format!("{:.2} W", metrics.wattage_w));
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
