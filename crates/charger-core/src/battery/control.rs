use std::{
    fs,
    path::{Path},
};

use crate::{
    battery::nodes::*,
    error::ChargerError,
};

const BATTERY_CHARGING_NODE: &str =
    "/sys/class/power_supply/battery/charging_enabled";

const BATTERY_INPUT_SUSPEND_NODE: &str =
    "/sys/class/power_supply/battery/input_suspend";

/// Write a value to a sysfs node.
///
/// This function deliberately does not perform existence checks first.
/// Sysfs nodes can disappear/reappear with driver state changes, so the
/// write itself is the authoritative operation.
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
/// Returns:
/// - Ok(false) if the node does not exist
/// - Ok(true) if the write succeeds
/// - Err(...) for a real I/O/permission/write failure
fn write_optional_node(
    path: &str,
    value: &str,
) -> Result<bool, ChargerError> {
    let path_ref = Path::new(path);

    match write_sysfs(path_ref, value) {
        Ok(()) => Ok(true),

        Err(ChargerError::SysfsWrite { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::debug!(
                path = path,
                "optional sysfs node not present"
            );

            Ok(false)
        }

        Err(error) => Err(error),
    }
}

/// Read a boolean sysfs node.
///
/// Returns:
/// - Some(true) for "1"
/// - Some(false) for "0"
/// - None when unavailable or invalid
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
/// We do not try to make sysfs transactional because sysfs itself does not
/// provide transactions. Instead, callers provide a safe ordering so that
/// partial failure leaves the hardware in the least dangerous state.
///
/// Returns:
/// - Ok(()) when at least one available node was written successfully and
///   no available node failed.
/// - NoChargingNodeFound when none of the requested nodes exists.
/// - PartialWriteFailure when one or more nodes succeeded but another
///   available node failed.
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
                // Optional node simply does not exist.
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
        return Err(ChargerError::PartialWriteFailure {
            succeeded,
            failed,
        });
    }

    Ok(())
}

/// Apply charging ON.
///
/// Safe ordering:
///
/// 1. Enable charging control.
/// 2. Remove input suspend.
///
/// This avoids briefly exposing a state where input is enabled while the
/// charging control is still explicitly disabled.
fn enable_charging_nodes() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_CHARGING_NODE, "1"),
        (MAIN_CHARGING_NODE, "1"),
        (BATTERY_INPUT_SUSPEND_NODE, "0"),
    ];

    apply_nodes(&nodes, "charging_on")
}

/// Apply charging OFF.
///
/// Safe ordering:
///
/// 1. Suspend battery input.
/// 2. Disable battery charging.
/// 3. Disable main charging control.
///
/// Suspending input first reduces the chance of a transient charging state
/// while the other control nodes are being updated.
fn disable_charging_nodes() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_INPUT_SUSPEND_NODE, "1"),
        (BATTERY_CHARGING_NODE, "0"),
        (MAIN_CHARGING_NODE, "0"),
    ];

    apply_nodes(&nodes, "charging_off")
}

/// Enable or disable normal charging.
///
/// The function intentionally does not perform a read-before-write.
/// The monitor already performs hardware reconciliation and avoids calling
/// this function when the requested state is already known to be correct.
///
/// Keeping this function write-oriented also avoids an extra set of sysfs
/// reads during every policy transition.
pub fn set_charging(
    enable: bool,
) -> Result<(), ChargerError> {
    if enable {
        enable_charging_nodes()
    } else {
        disable_charging_nodes()
    }
}

/// Enter bypass mode.
///
/// Bypass is represented by:
///
///     input_suspend    = 1
///     charging_enabled = 0
///     main/charging    = 0   (when available)
///
/// The suspend operation is deliberately performed first.
pub fn enter_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_INPUT_SUSPEND_NODE, "1"),
        (BATTERY_CHARGING_NODE, "0"),
        (MAIN_CHARGING_NODE, "0"),
    ];

    apply_nodes(&nodes, "bypass_on")
}

/// Exit bypass mode.
///
/// Restore order:
///
///     charging_enabled = 1
///     main/charging    = 1   (when available)
///     input_suspend    = 0
///
/// Input suspend is removed last so that charging controls are already
/// enabled before power input is allowed again.
pub fn exit_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        (BATTERY_CHARGING_NODE, "1"),
        (MAIN_CHARGING_NODE, "1"),
        (BATTERY_INPUT_SUSPEND_NODE, "0"),
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
/// Important:
/// - battery/charging_enabled + input_suspend are the primary pair.
/// - main/charging_enabled is optional.
/// - absence of the optional main node must NOT turn an otherwise valid
///   battery state into Unknown.
///
/// Classification:
///
/// ChargingEnabled:
///     charging_enabled = 1
///     input_suspend    = 0
///     optional main node, if present, must be 1
///
/// ChargingDisabled:
///     charging_enabled = 0
///     input_suspend    = 1
///
/// Bypass:
///     charging_enabled = 0
///     input_suspend    = 1
///     main/charging_enabled = 0
///
/// Inconsistent:
///     readable nodes disagree.
///
/// Unknown:
///     no useful charging-control information can be read.
pub fn get_actual_charging_state() -> ActualHardwareMode {
    let battery_charging =
        read_bool_node(BATTERY_CHARGING_NODE);

    let battery_suspend =
        read_bool_node(BATTERY_INPUT_SUSPEND_NODE);

    let main_charging =
        read_bool_node(MAIN_CHARGING_NODE);

    // ---------------------------------------------------------
    // Nothing readable.
    // ---------------------------------------------------------

    if battery_charging.is_none()
        && battery_suspend.is_none()
        && main_charging.is_none()
    {
        return ActualHardwareMode::Unknown;
    }

    // ---------------------------------------------------------
    // BYPASS / CHARGING DISABLED
    // ---------------------------------------------------------

    let battery_off =
        battery_charging == Some(false);

    let suspended =
        battery_suspend == Some(true);

    if battery_off && suspended {
        // A readable main node with value 0 distinguishes
        // the dedicated bypass state.
        if main_charging == Some(false) {
            return ActualHardwareMode::Bypass;
        }

        // If main node does not exist, this is ordinary charging
        // disabled on a device without a distinct bypass control.
        if main_charging.is_none()
            || main_charging == Some(true)
        {
            return ActualHardwareMode::ChargingDisabled;
        }

        return ActualHardwareMode::Inconsistent;
    }

    // ---------------------------------------------------------
    // CHARGING ENABLED
    // ---------------------------------------------------------

    if battery_charging == Some(true)
        && battery_suspend == Some(false)
    {
        // If main charging exists, it must agree.
        match main_charging {
            Some(true) => {
                return ActualHardwareMode::ChargingEnabled;
            }

            None => {
                return ActualHardwareMode::ChargingEnabled;
            }

            Some(false) => {
                return ActualHardwareMode::Inconsistent;
            }
        }
    }

    // ---------------------------------------------------------
    // PARTIAL / CONFLICTING STATE
    // ---------------------------------------------------------

    ActualHardwareMode::Inconsistent
}

/// Returns true when the device exposes a distinct main charging node.
///
/// This function only checks whether the node exists. It does not claim
/// that bypass is currently active.
pub fn has_distinct_bypass_node() -> bool {
    Path::new(MAIN_CHARGING_NODE).exists()
}

/// Grant write permission to all known charging nodes.
///
/// This should normally be called once during daemon initialization,
/// not from the monitor loop.
///
/// Changing sysfs permissions is intentionally kept outside normal
/// charging transitions to avoid unnecessary filesystem operations.
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

        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,

            Err(error)
                if error.kind()
                    == std::io::ErrorKind::NotFound =>
            {
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

        let mut permissions =
            metadata.permissions();

        // Keep the existing daemon behaviour.
        //
        // NOTE:
        // If the daemon is intended for production/root environments,
        // a udev/init/device-specific permission setup is preferable to
        // chmod 0644 from the daemon itself.
        permissions.set_mode(0o644);

        if let Err(error) =
            fs::set_permissions(path, permissions)
        {
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
