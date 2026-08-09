use std::{fs, path::Path};

use crate::{
    battery::nodes::{CHARGING_NODES, SUSPEND_NODES},
    error::ChargerError,
};

/// Primary charging control node.
///
/// This device exposes charging control directly under
/// `/sys/class/power_supply/battery/charging_enabled`.
const BATTERY_CHARGING_NODE: &str = "/sys/class/power_supply/battery/charging_enabled";

/// Primary battery input suspend node.
///
/// `1` = suspend battery input
/// `0` = allow battery input
const BATTERY_INPUT_SUSPEND_NODE: &str = "/sys/class/power_supply/battery/input_suspend";

/// Write a value to a sysfs node.
///
/// This function deliberately does not perform an existence check first.
///
/// Sysfs nodes can disappear/reappear with driver state changes, so the
/// write operation itself is the authoritative operation.
pub fn write_sysfs(path: &Path, value: &str) -> Result<(), ChargerError> {
    fs::write(path, value).map_err(|e| ChargerError::SysfsWrite {
        path: path.to_owned(),
        source: e,
    })
}

/// Write to an optional sysfs node.
///
/// Returns:
/// - `Ok(false)` if the node does not exist
/// - `Ok(true)` if the write succeeds
/// - `Err(...)` for a real I/O/permission/write failure
fn write_optional_node(path: &str, value: &str) -> Result<bool, ChargerError> {
    let path_ref = Path::new(path);

    match write_sysfs(path_ref, value) {
        Ok(()) => Ok(true),

        Err(ChargerError::SysfsWrite { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::debug!(path = path, "optional sysfs node not present");

            Ok(false)
        }

        Err(error) => Err(error),
    }
}

/// Read a boolean sysfs node.
///
/// Returns:
/// - `Some(true)` for `"1"`
/// - `Some(false)` for `"0"`
/// - `None` when unavailable or invalid
fn read_bool_node(path: &str) -> Option<bool> {
    match fs::read_to_string(path) {
        Ok(value) => match value.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        },

        Err(_) => None,
    }
}

/// Apply a sequence of sysfs writes.
///
/// The order of nodes is intentional.
///
/// Sysfs does not provide transactions, so callers provide an ordering
/// that minimizes unsafe intermediate states.
///
/// Returns:
/// - `Ok(())` when at least one available node was written successfully
///   and no available node failed.
/// - `NoChargingNodeFound` when none of the requested nodes exists.
/// - `PartialWriteFailure` when one or more nodes succeeded but another
///   available node failed.
fn apply_nodes(nodes: &[(&str, &str)], operation: &str) -> Result<(), ChargerError> {
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
                continue;
            }

            Err(error) => {
                failed += 1;

                tracing::warn!(
                    operation = operation,
                    path = path,
                    value = value,
                    error = %error,
                    "sysfs write failed"
                );
            }
        }
    }

    if succeeded == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }

    if failed > 0 {
        return Err(ChargerError::PartialWriteFailure { succeeded, failed });
    }

    Ok(())
}

/// Enable normal charging.
///
/// Safe ordering:
///
/// 1. Enable charging control.
/// 2. Remove input suspend.
///
/// This prevents a transient state where input is available while
/// charging is still explicitly disabled.
fn enable_charging_nodes() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_CHARGING_NODE, "1"),
        (BATTERY_INPUT_SUSPEND_NODE, "0"),
    ];

    apply_nodes(&nodes, "charging_on")
}

/// Disable normal charging.
///
/// Safe ordering:
///
/// 1. Suspend battery input.
/// 2. Disable charging.
///
/// Suspending input first minimizes the chance of a transient charging
/// state while the charging control is being disabled.
fn disable_charging_nodes() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_INPUT_SUSPEND_NODE, "1"),
        (BATTERY_CHARGING_NODE, "0"),
    ];

    apply_nodes(&nodes, "charging_off")
}

/// Enable or disable normal charging.
///
/// This function intentionally does not perform a read-before-write.
///
/// The monitor already performs hardware reconciliation and avoids calling
/// this function when the requested state is already known to be correct.
pub fn set_charging(enable: bool) -> Result<(), ChargerError> {
    if enable {
        enable_charging_nodes()
    } else {
        disable_charging_nodes()
    }
}

/// Enter bypass mode.
///
/// This device does not expose a separate bypass hardware node.
///
/// Therefore bypass is represented using the same hardware state as
/// charging-disabled:
///
///     input_suspend    = 1
///     charging_enabled = 0
///
/// The monitor distinguishes logical BYPASS from normal charging-disabled
/// using its own `OperatingMode`.
pub fn enter_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_INPUT_SUSPEND_NODE, "1"),
        (BATTERY_CHARGING_NODE, "0"),
    ];

    apply_nodes(&nodes, "bypass_on")
}

/// Exit bypass mode.
///
/// Restore order:
///
///     charging_enabled = 1
///     input_suspend    = 0
///
/// Charging control is enabled before input suspend is removed so that
/// power input is only allowed after charging has been explicitly enabled.
pub fn exit_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_CHARGING_NODE, "1"),
        (BATTERY_INPUT_SUSPEND_NODE, "0"),
    ];

    apply_nodes(&nodes, "bypass_off")
}

/// Actual charging state as observed from hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualHardwareMode {
    /// No useful charging-control state can be read.
    Unknown,

    /// Charging is explicitly enabled.
    ChargingEnabled,

    /// Charging is explicitly disabled.
    ChargingDisabled,

    /// A distinct bypass state is available and active.
    ///
    /// This device does not expose such a state.
    Bypass,

    /// Readable control nodes disagree or expose an impossible state.
    Inconsistent,
}

/// Read the actual charging state from sysfs.
///
/// This device uses two primary control nodes:
///
///     battery/charging_enabled
///     battery/input_suspend
///
/// Classification:
///
/// ChargingEnabled:
///     charging_enabled = 1
///     input_suspend    = 0
///
/// ChargingDisabled:
///     charging_enabled = 0
///     input_suspend    = 1
///
/// Inconsistent:
///     both nodes are readable but contain a conflicting combination.
///
/// Unknown:
///     the required control information cannot be read.
///
/// Because this device has no separate bypass control, hardware cannot
/// distinguish "bypass" from "charging disabled". Therefore the hardware
/// state for bypass is reported as `ChargingDisabled`.
pub fn get_actual_charging_state() -> ActualHardwareMode {
    let battery_charging = read_bool_node(BATTERY_CHARGING_NODE);

    let battery_suspend = read_bool_node(BATTERY_INPUT_SUSPEND_NODE);

    // Nothing readable.
    if battery_charging.is_none() && battery_suspend.is_none() {
        return ActualHardwareMode::Unknown;
    }

    // Both primary nodes must be readable for a definitive
    // hardware classification.
    let charging = match battery_charging {
        Some(value) => value,
        None => {
            return ActualHardwareMode::Unknown;
        }
    };

    let suspended = match battery_suspend {
        Some(value) => value,
        None => {
            return ActualHardwareMode::Unknown;
        }
    };

    // CHARGING ENABLED
    if charging && !suspended {
        return ActualHardwareMode::ChargingEnabled;
    }

    // CHARGING DISABLED / BYPASS
    if !charging && suspended {
        return ActualHardwareMode::ChargingDisabled;
    }

    // Conflicting / unsafe combinations:
    //
    // charging_enabled = 1
    // input_suspend    = 1
    //
    // OR
    //
    // charging_enabled = 0
    // input_suspend    = 0
    ActualHardwareMode::Inconsistent
}

/// Returns true when the device exposes a distinct bypass node.
///
/// The current device has no separate bypass hardware node.
///
/// Therefore the monitor must expect:
///
///     ActualHardwareMode::ChargingDisabled
///
/// while operating in logical BYPASS mode.
pub fn has_distinct_bypass_node() -> bool {
    false
}

/// Grant write permission to all known charging-control nodes.
///
/// This should normally be called once during daemon initialization.
///
/// The permission operation covers:
///
/// - all entries in `CHARGING_NODES`
/// - all entries in `SUSPEND_NODES`
#[cfg(unix)]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    use std::os::unix::fs::PermissionsExt;

    let nodes = CHARGING_NODES.iter().chain(SUSPEND_NODES.iter()).copied();

    let mut any_found = false;

    for node in nodes {
        let path = Path::new(node);

        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,

            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }

            Err(error) => {
                tracing::warn!(
                    path = node,
                    error = %error,
                    "Failed to read sysfs metadata"
                );

                continue;
            }
        };

        any_found = true;

        let mut permissions = metadata.permissions();

        /*
         * Keep the existing daemon behaviour.
         *
         * For production/root environments, a udev/init/device-specific
         * permission rule is preferable to chmod from the daemon itself.
         */
        permissions.set_mode(0o644);

        if let Err(error) = fs::set_permissions(path, permissions) {
            tracing::warn!(
                path = node,
                error = %error,
                "Failed to set sysfs permissions"
            );
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
