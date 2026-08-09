use std::{fs, path::Path};
use crate::{battery::nodes::*, error::ChargerError};

/// Write a value to a sysfs node with proper error context.
pub fn write_sysfs(path: &Path, value: &str) -> Result<(), ChargerError> {
    fs::write(path, value)
        .map_err(|e| ChargerError::SysfsWrite { path: path.to_owned(), source: e })
}

/// Enable or disable charging across all known nodes.
/// Mirrors Kotlin `setChargingEnabled()`.
pub fn set_charging(enable: bool) -> Result<(), ChargerError> {
    let charge_val = if enable { "1" } else { "0" };
    let suspend_val = if enable { "0" } else { "1" };

    let mut succeeded = 0;
    let mut failed = 0;

    for node in CHARGING_NODES {
        let path = Path::new(node);
        match write_sysfs(path, charge_val) {
            Ok(_) => succeeded += 1,
            Err(ChargerError::SysfsWrite { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("Failed to write to {}: {}", node, e);
                failed += 1;
            }
        }
    }
    for node in SUSPEND_NODES {
        let path = Path::new(node);
        match write_sysfs(path, suspend_val) {
            Ok(_) => succeeded += 1,
            Err(ChargerError::SysfsWrite { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("Failed to write to {}: {}", node, e);
                failed += 1;
            }
        }
    }

    if failed > 0 && succeeded > 0 {
        tracing::warn!("Charging state partially applied: {} succeeded, {} failed", succeeded, failed);
        Err(ChargerError::PartialWriteFailure { succeeded, failed })
    } else if succeeded == 0 {
        Err(ChargerError::NoChargingNodeFound)
    } else {
        Ok(())
    }
}

/// Activate bypass mode (disconnect input power from battery).
pub fn enter_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        ("/sys/class/power_supply/battery/input_suspend", "1"),
        ("/sys/class/power_supply/battery/charging_enabled", "0"),
        ("/sys/class/power_supply/main/charging_enabled", "0"),
    ];
    let mut succeeded = 0;
    let mut failed = 0;
    
    for (path, val) in &nodes {
        let p = Path::new(path);
        match write_sysfs(p, val) {
            Ok(_) => succeeded += 1,
            Err(ChargerError::SysfsWrite { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("Bypass ON: Failed to write to {}: {}", path, e);
                failed += 1;
            }
        }
    }
    
    if failed > 0 && succeeded > 0 {
        Err(ChargerError::PartialWriteFailure { succeeded, failed })
    } else if succeeded == 0 {
        Err(ChargerError::NoChargingNodeFound)
    } else {
        Ok(())
    }
}

/// Restore normal charging from bypass mode.
pub fn exit_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        ("/sys/class/power_supply/battery/input_suspend", "0"),
        ("/sys/class/power_supply/battery/charging_enabled", "1"),
        ("/sys/class/power_supply/main/charging_enabled", "1"),
    ];
    let mut succeeded = 0;
    let mut failed = 0;
    
    for (path, val) in &nodes {
        let p = Path::new(path);
        match write_sysfs(p, val) {
            Ok(_) => succeeded += 1,
            Err(ChargerError::SysfsWrite { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("Bypass OFF: Failed to write to {}: {}", path, e);
                failed += 1;
            }
        }
    }
    
    if failed > 0 && succeeded > 0 {
        Err(ChargerError::PartialWriteFailure { succeeded, failed })
    } else if succeeded == 0 {
        Err(ChargerError::NoChargingNodeFound)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    use std::os::unix::fs::PermissionsExt;
    
    let mut any_found = false;
    for node in CHARGING_NODES.iter().chain(SUSPEND_NODES.iter()) {
        let path = Path::new(node);
        if path.exists() {
            any_found = true;
            if let Ok(metadata) = fs::metadata(path) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o644);
                let _ = fs::set_permissions(path, perms);
            }
        }
    }

    if !any_found {
        Err(ChargerError::NoChargingNodeFound)
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    Ok(()) // Dummy for windows
}
