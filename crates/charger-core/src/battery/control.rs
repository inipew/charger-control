use std::{fs, path::Path};

use crate::{
    battery::nodes::*,
    error::ChargerError,
};

const BATTERY_CHARGING_NODE: &str =
    "/sys/class/power_supply/battery/charging_enabled";

const BATTERY_INPUT_SUSPEND_NODE: &str =
    "/sys/class/power_supply/battery/input_suspend";

/// Write a value to a sysfs node.
pub fn write_sysfs(
    path: &Path,
    value: &str,
) -> Result<(), ChargerError> {
    fs::write(path, value).map_err(|e| {
        ChargerError::SysfsWrite {
            path: path.to_owned(),
            source: e,
        }
    })
}

/// Write to an optional sysfs node.
///
/// `Ok(false)` means the node does not exist.
/// `Ok(true)` means the write succeeded.
fn write_optional_node(
    path: &str,
    value: &str,
) -> Result<bool, ChargerError> {
    let path = Path::new(path);

    if !path.exists() {
        return Ok(false);
    }

    write_sysfs(path, value)?;
    Ok(true)
}

/// Apply a set of sysfs writes.
///
/// Returns:
/// - Ok(()) when all available nodes succeeded
/// - PartialWriteFailure when some nodes failed
/// - NoChargingNodeFound when no node exists
fn apply_nodes(
    nodes: &[(&str, &str)],
    operation: &str,
) -> Result<(), ChargerError> {
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for &(path, value) in nodes {
        match write_optional_node(path, value) {
            Ok(true) => {
                succeeded += 1;

                tracing::debug!(
                    operation = operation,
                    path = path,
                    value = value,
                    "sysfs write succeeded"
                );
            }

            Ok(false) => {
                tracing::debug!(
                    operation = operation,
                    path = path,
                    "sysfs node not present"
                );
            }

            Err(e) => {
                failed += 1;

                tracing::warn!(
                    operation = operation,
                    path = path,
                    error = %e,
                    "sysfs write failed"
                );
            }
        }
    }

    if succeeded == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }

    if failed > 0 {
        return Err(ChargerError::PartialWriteFailure {
            succeeded,
            failed,
        });
    }

    Ok(())
}

/// Enable or disable normal charging.
///
/// Enable:
///     charging_enabled = 1
///     input_suspend    = 0
///
/// Disable:
///     charging_enabled = 0
///     input_suspend    = 1
///
/// The function intentionally writes all known nodes instead of stopping
/// after the first successful write.
pub fn set_charging(enable: bool) -> Result<(), ChargerError> {
    let charging_value = if enable { "1" } else { "0" };
    let suspend_value = if enable { "0" } else { "1" };

    let nodes = [
        (BATTERY_CHARGING_NODE, charging_value),
        (BATTERY_INPUT_SUSPEND_NODE, suspend_value),
    ];

    apply_nodes(
        &nodes,
        if enable {
            "charging_on"
        } else {
            "charging_off"
        },
    )
}

/// Enter bypass mode.
///
/// If `main/charging_enabled` exists, it is disabled too.
///
/// This gives devices with a distinct main charging control node a true
/// bypass state while still supporting devices that only expose battery
/// charging nodes.
pub fn enter_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_INPUT_SUSPEND_NODE, "1"),
        (BATTERY_CHARGING_NODE, "0"),
        (MAIN_CHARGING_NODE, "0"),
    ];

    apply_nodes(&nodes, "bypass_on")
}

/// Exit bypass mode and restore normal charging.
pub fn exit_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_INPUT_SUSPEND_NODE, "0"),
        (BATTERY_CHARGING_NODE, "1"),
        (MAIN_CHARGING_NODE, "1"),
    ];

    apply_nodes(&nodes, "bypass_off")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualHardwareMode {
    Unknown,
    ChargingEnabled,
    ChargingDisabled,
    Bypass,
    Inconsistent,
}

/// Read the actual charging state from sysfs.
///
/// The daemon should use this function for reconciliation instead of
/// trusting its own previous desired state.
pub fn get_actual_charging_state() -> ActualHardwareMode {
    use crate::battery::reader::read_sysfs;

    let battery_charging = read_sysfs(
        Path::new(BATTERY_CHARGING_NODE),
    )
    .ok()
    .and_then(|v| match v.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });

    let battery_suspend = read_sysfs(
        Path::new(BATTERY_INPUT_SUSPEND_NODE),
    )
    .ok()
    .and_then(|v| match v.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });

    let main_charging = read_sysfs(
        Path::new(MAIN_CHARGING_NODE),
    )
    .ok()
    .and_then(|v| match v.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });

    // Nothing readable.
    if battery_charging.is_none()
        && battery_suspend.is_none()
        && main_charging.is_none()
    {
        return ActualHardwareMode::Unknown;
    }

    let battery_off = battery_charging == Some(false);
    let suspended = battery_suspend == Some(true);
    let main_off = main_charging == Some(false);

    // Distinct bypass:
    //
    // battery charging = OFF
    // input suspend     = ON
    // main charging     = OFF
    //
    // Only classify as distinct bypass when main node exists/readable.
    if battery_off
        && suspended
        && main_off
    {
        return ActualHardwareMode::Bypass;
    }

    // Normal charging enabled.
    //
    // Battery nodes are the minimum authoritative pair.
    if battery_charging == Some(true)
        && battery_suspend == Some(false)
    {
        // If main node exists, it must also be enabled.
        if let Some(main) = main_charging {
            if main {
                return ActualHardwareMode::ChargingEnabled;
            }

            return ActualHardwareMode::Inconsistent;
        }

        return ActualHardwareMode::ChargingEnabled;
    }

    // Normal charging disabled.
    if battery_off && suspended {
        return ActualHardwareMode::ChargingDisabled;
    }

    ActualHardwareMode::Inconsistent
}

/// Returns true when the device exposes a distinct main charging node.
///
/// This is used only to distinguish BYPASS from normal charging-disabled
/// state. It does not itself prove that bypass is currently active.
pub fn has_distinct_bypass_node() -> bool {
    Path::new(MAIN_CHARGING_NODE).exists()
}

/// Grant write permission to all known charging nodes.
///
/// This is intended for environments where the daemon is responsible for
/// preparing sysfs permissions before using the nodes.
#[cfg(unix)]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    use std::os::unix::fs::PermissionsExt;

    let nodes = CHARGING_NODES
        .iter()
        .chain(SUSPEND_NODES.iter())
        .copied()
        .chain(std::iter::once(MAIN_CHARGING_NODE));

    let mut any_found = false;

    for node in nodes {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        any_found = true;

        match fs::metadata(path) {
            Ok(metadata) => {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o644);

                if let Err(e) = fs::set_permissions(path, permissions) {
                    tracing::warn!(
                        path = node,
                        error = %e,
                        "Failed to set sysfs permissions"
                    );
                }
            }

            Err(e) => {
                tracing::warn!(
                    path = node,
                    error = %e,
                    "Failed to read sysfs metadata"
                );
            }
        }
    }

    if any_found {
        Ok(())
    } else {
        Err(ChargerError::NoChargingNodeFound)
    }
}

#[cfg(not(unix))]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    Ok(())
}