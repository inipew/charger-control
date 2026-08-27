use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    battery::nodes::{
        CHARGING_NODES, FAST_CHARGE_CURRENT_NODES, SUSPEND_NODES, THERMAL_INPUT_CURRENT_NODES,
    },
    error::ChargerError,
};

/// Actual physical charging state.
///
/// This represents what can be inferred from the hardware control nodes.
/// It is deliberately separate from the monitor's logical operating mode.
///
/// For this platform:
///
/// - `ChargingEnabled` = `charging_enabled=1` + `input_suspend=0`
/// - `ChargingDisabled` = `charging_enabled=0` + `input_suspend=1`
/// - `Bypass` = not physically distinguishable from `ChargingDisabled`
/// - `Inconsistent` = both nodes are readable but contain an unexpected combination
/// - `Unknown` = one or both nodes cannot be read
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualHardwareMode {
    Unknown,
    ChargingEnabled,
    ChargingDisabled,
    Bypass,
    Inconsistent,
}

impl ActualHardwareMode {
    pub fn is_known(self) -> bool {
        !matches!(self, Self::Unknown | Self::Inconsistent)
    }

    pub fn is_charging_enabled(self) -> bool {
        matches!(self, Self::ChargingEnabled)
    }

    pub fn is_charging_disabled(self) -> bool {
        matches!(self, Self::ChargingDisabled)
    }
}

/// Write one sysfs value.
///
/// The write operation itself is authoritative. We intentionally do not
/// perform an existence check before writing because sysfs nodes can appear
/// or disappear dynamically.
///
/// Verification, when required, is performed separately through
/// `get_actual_charging_state()`.
pub fn write_sysfs(path: &Path, value: &str) -> Result<(), ChargerError> {
    fs::write(path, value).map_err(|source| ChargerError::SysfsWrite {
        path: path.to_owned(),
        source,
    })
}

/// Write one optional sysfs node.
///
/// `Ok(true)`  = node exists and write succeeded.
/// `Ok(false)` = node does not exist.
/// `Err(_)`    = node exists but the write failed.
///
/// ENOENT is treated as "node unavailable" here because callers may have
/// multiple candidate nodes in their node list.
///
/// Whether an unavailable node is acceptable is decided by `apply_nodes()`.
fn write_optional_node(path: &str, value: &str) -> Result<bool, ChargerError> {
    match write_sysfs(Path::new(path), value) {
        Ok(()) => Ok(true),

        Err(ChargerError::SysfsWrite { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::debug!(path, "sysfs node is not present");
            Ok(false)
        }

        Err(error) => Err(error),
    }
}

/// Apply an ordered collection of sysfs writes.
///
/// Sysfs does not provide transactions, so multi-node operations are never
/// atomic.
///
/// Important semantics:
///
/// - every supplied node is attempted;
/// - successful writes are counted;
/// - missing nodes are counted separately;
/// - hard write errors are counted separately;
/// - `Ok(())` is returned only when ALL supplied nodes were successfully
///   written;
/// - any missing or failed node makes the operation fail;
/// - partial writes are reported explicitly.
///
/// The monitor must perform a read-back through
/// `get_actual_charging_state()` after an operation if it needs to know the
/// resulting physical state.
fn apply_nodes(nodes: &[(&str, &str)], operation: &'static str) -> Result<(), ChargerError> {
    let mut succeeded = 0usize;
    let mut missing = 0usize;
    let mut failed = 0usize;

    for &(path, value) in nodes {
        match write_optional_node(path, value) {
            Ok(true) => {
                succeeded += 1;

                tracing::debug!(operation, path, value, "sysfs write succeeded");
            }

            Ok(false) => {
                missing += 1;

                tracing::warn!(operation, path, "required sysfs node is unavailable");
            }

            Err(error) => {
                failed += 1;

                tracing::warn!(
                    operation,
                    path,
                    value,
                    error = %error,
                    "sysfs write failed"
                );
            }
        }
    }

    /*
     * Nothing was writable.
     */
    if succeeded == 0 {
        if failed == 0 && missing > 0 {
            return Err(ChargerError::NoChargingNodeFound);
        }

        return Err(ChargerError::PartialWriteFailure {
            succeeded,
            failed: failed + missing,
        });
    }

    /*
     * At least one node was written, but the complete operation was not.
     *
     * Missing nodes are deliberately included in the failure count because
     * the caller requested a multi-node hardware state.
     */
    if missing > 0 || failed > 0 {
        return Err(ChargerError::PartialWriteFailure {
            succeeded,
            failed: failed + missing,
        });
    }

    /*
     * Every requested node was successfully written.
     *
     * This still does NOT prove the hardware reached the requested state.
     * The monitor owns read-back verification.
     */
    Ok(())
}

/// Return the primary charging control node.
///
/// The current platform exposes exactly one authoritative node. Keeping this
/// helper based on `CHARGING_NODES` avoids duplicating the path in control.rs.
static CHARGING_NODE_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static SUSPEND_NODE_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);

fn detect_node_cached(
    category: &'static str,
    candidates: &[&'static str],
    cached_idx: &AtomicUsize,
) -> Option<&'static str> {
    let idx = cached_idx.load(Ordering::Relaxed);
    if idx < candidates.len() {
        /*
         * Return the cached path unconditionally — no exists() probe.
         *
         * write_optional_node() handles ENOENT gracefully (returns Ok(false)).
         * If the node has disappeared since it was cached, apply_nodes() will
         * report NoChargingNodeFound and the monitor will call
         * reset_node_caches(), which sets both indices back to usize::MAX so
         * the next call re-discovers the active node.
         */
        return Some(candidates[idx]);
    }

    /*
     * No valid cache yet. Scan candidates with a lightweight metadata probe.
     * This path is only taken once per boot (or after cache invalidation).
     */
    for (i, &path) in candidates.iter().enumerate() {
        if Path::new(path).exists() {
            let prev = cached_idx.swap(i, Ordering::Relaxed);
            if prev != i {
                tracing::info!(category, path, "active control sysfs node resolved");
            }
            return Some(path);
        }
    }

    None
}

static FAST_CHARGE_NODE_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);
static THERMAL_INPUT_NODE_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);

fn charging_node() -> Option<&'static str> {
    detect_node_cached("charging_control", CHARGING_NODES, &CHARGING_NODE_IDX)
}

/// Return the primary input-suspend control node.
fn suspend_node() -> Option<&'static str> {
    detect_node_cached("input_suspend", SUSPEND_NODES, &SUSPEND_NODE_IDX)
}

/// Return the primary fast-charge current control node.
fn fast_charge_node() -> Option<&'static str> {
    detect_node_cached(
        "fast_charge_current",
        FAST_CHARGE_CURRENT_NODES,
        &FAST_CHARGE_NODE_IDX,
    )
}

/// Return the primary thermal input current control node.
fn thermal_input_node() -> Option<&'static str> {
    detect_node_cached(
        "thermal_input_current",
        THERMAL_INPUT_CURRENT_NODES,
        &THERMAL_INPUT_NODE_IDX,
    )
}

/// Invalidate the cached node indices for both charging and suspend nodes.
///
/// Call this when a write operation returns `NoChargingNodeFound` so that
/// the next evaluation re-discovers which nodes are physically available.
pub fn reset_node_caches() {
    CHARGING_NODE_IDX.store(usize::MAX, Ordering::Relaxed);
    SUSPEND_NODE_IDX.store(usize::MAX, Ordering::Relaxed);
    FAST_CHARGE_NODE_IDX.store(usize::MAX, Ordering::Relaxed);
    THERMAL_INPUT_NODE_IDX.store(usize::MAX, Ordering::Relaxed);
}

/// Set fast charge current limit in microamperes (µA).
///
/// Ensures a hardware safety floor of at least 500,000 µA (500 mA).
pub fn set_fast_charge_current(current_ua: u32) -> Result<(), ChargerError> {
    let safe_ua = current_ua.max(500_000);
    let val_str = safe_ua.to_string();
    let mut any_succeeded = false;

    if let Some(node) = fast_charge_node() {
        if write_optional_node(node, &val_str).unwrap_or(false) {
            tracing::info!(
                path = node,
                current_ua = safe_ua,
                "fast charge current limit written"
            );
            any_succeeded = true;
        }
    }

    if let Some(node) = thermal_input_node() {
        if write_optional_node(node, &val_str).unwrap_or(false) {
            tracing::info!(
                path = node,
                current_ua = safe_ua,
                "thermal input current limit written"
            );
            any_succeeded = true;
        }
    }

    if any_succeeded {
        Ok(())
    } else {
        Err(ChargerError::NoChargingNodeFound)
    }
}

/// Read the currently configured fast charge current in µA from sysfs.
pub fn read_fast_charge_current() -> Option<u32> {
    if let Some(node) = fast_charge_node() {
        if let Ok(raw) = fs::read_to_string(node) {
            if let Ok(ua) = raw.trim().parse::<u32>() {
                return Some(ua);
            }
        }
    }
    None
}

/// Reset fast charge current to default maximum (5.85 A).
pub fn reset_fast_charge_current() -> Result<(), ChargerError> {
    set_fast_charge_current(5_850_000)
}

/// Apply or release fast-charge & USB-PD bypass.
///
/// When `enable = true`:
/// Injects fast charge flags to known sysfs nodes (fast_charge_current, thermal_input_current,
/// usb/fastcharge_mode, bms/fastcharge_mode, usb/pd_active, usb/pd_type, usb/pd_authentication,
/// bms/mtk_soc_decimal_rate, ln8000 charge pump).
///
/// When `enable = false`:
/// Resets the bypass flags gracefully without interrupting normal charging.
pub fn apply_fast_charge_bypass(enable: bool, current_ua: u32) -> Result<(), ChargerError> {
    let current_str = (current_ua.max(500_000)).to_string();

    if enable {
        let nodes: &[(&str, &str)] = &[
            (
                "/sys/class/power_supply/battery/fast_charge_current",
                &current_str,
            ),
            (
                "/sys/class/power_supply/battery/thermal_input_current",
                &current_str,
            ),
            ("/sys/class/power_supply/usb/fastcharge_mode", "1"),
            ("/sys/class/power_supply/bms/fastcharge_mode", "1"),
            ("/sys/class/power_supply/usb/pd_active", "1"),
            ("/sys/class/power_supply/usb/pd_type", "3"),
            ("/sys/class/power_supply/usb/pd_authentication", "1"),
            ("/sys/class/power_supply/bms/mtk_soc_decimal_rate", "100"),
            ("/sys/class/power_supply/ln8000/charging_enabled", "1"),
            ("/sys/class/power_supply/ln8000/hv_charge_enable", "1"),
            (
                "/sys/class/power_supply/ln8000/input_current_limit",
                &current_str,
            ),
            ("/sys/class/power_supply/ln8000/bq_charge_done", "0"),
            ("/sys/class/power_supply/ln8000/ti_bypass_mode_enable", "0"),
        ];

        for &(path, value) in nodes {
            let p = Path::new(path);
            if p.exists() {
                let _ = write_optional_node(path, value);
            }
        }
    } else {
        let reset_nodes: &[(&str, &str)] = &[
            ("/sys/class/power_supply/usb/fastcharge_mode", "0"),
            ("/sys/class/power_supply/usb/pd_active", "0"),
            ("/sys/class/power_supply/usb/pd_authentication", "0"),
        ];

        for &(path, value) in reset_nodes {
            let p = Path::new(path);
            if p.exists() {
                let _ = write_optional_node(path, value);
            }
        }
    }

    Ok(())
}

/// Enable normal charging.
///
/// Ordering:
///
/// 1. charging_enabled = 1
/// 2. input_suspend    = 0
///
/// If the second write fails, the operation returns
/// `PartialWriteFailure`.
///
/// The resulting hardware state MUST be reconciled by the monitor.
fn enable_charging_nodes() -> Result<(), ChargerError> {
    let charging = charging_node().ok_or(ChargerError::NoChargingNodeFound)?;

    let suspend = suspend_node().ok_or(ChargerError::NoChargingNodeFound)?;

    apply_nodes(&[(charging, "1"), (suspend, "0")], "charging_on")
}

/// Disable normal charging.
///
/// Ordering:
///
/// 1. input_suspend    = 1
/// 2. charging_enabled = 0
///
/// Suspending input first provides the safer intermediate state if the second
/// write cannot be completed.
///
/// If the second write fails, the hardware should already be in the safer
/// input-suspended state, but the monitor still MUST reconcile the state.
fn disable_charging_nodes() -> Result<(), ChargerError> {
    let charging = charging_node().ok_or(ChargerError::NoChargingNodeFound)?;

    let suspend = suspend_node().ok_or(ChargerError::NoChargingNodeFound)?;

    apply_nodes(&[(suspend, "1"), (charging, "0")], "charging_off")
}

/// Set normal charging state.
///
/// This function deliberately does not perform a read-before-write.
///
/// The monitor owns reconciliation and decides whether a hardware write
/// is necessary.
pub fn set_charging(enable: bool) -> Result<(), ChargerError> {
    if enable {
        enable_charging_nodes()
    } else {
        disable_charging_nodes()
    }
}

/// Emergency disable charging for thermal or safety failsafe.
///
/// Unlike `disable_charging_nodes()`, which requires all candidate control nodes
/// to be successfully updated and verified, `emergency_disable_charging()` attempts
/// to write to input_suspend and charging_enabled independently.
///
/// It returns `Ok(())` if AT LEAST ONE disable operation succeeded, ensuring that
/// a partial write failure does not cause safety failsafe logic to treat an effective
/// hardware suspend as a complete failure.
pub fn emergency_disable_charging() -> Result<(), ChargerError> {
    let mut any_succeeded = false;

    if let Some(suspend) = suspend_node() {
        if write_optional_node(suspend, "1").unwrap_or(false) {
            tracing::info!(path = suspend, "emergency input suspend written");
            any_succeeded = true;
        }
    }

    if let Some(charging) = charging_node() {
        if write_optional_node(charging, "0").unwrap_or(false) {
            tracing::info!(path = charging, "emergency charging disable written");
            any_succeeded = true;
        }
    }

    if any_succeeded {
        Ok(())
    } else {
        Err(ChargerError::NoChargingNodeFound)
    }
}

/// Enter logical bypass.
///
/// This platform does not expose a separate physical bypass node.
///
/// Therefore bypass is physically represented by the same state as
/// charging-disabled.
pub fn enter_bypass_mode() -> Result<(), ChargerError> {
    disable_charging_nodes()
}

/// Exit bypass and restore normal charging.
///
/// Verification is intentionally not performed here. The monitor owns
/// hardware reconciliation.
pub fn exit_bypass_mode() -> Result<(), ChargerError> {
    enable_charging_nodes()
}

/// Read a boolean sysfs node.
///
/// Supported values:
///
/// - "1" => true
/// - "0" => false
///
/// Any other value or read failure is treated as unknown.
fn read_bool_node(path: &str) -> Option<bool> {
    let value = fs::read_to_string(path).ok()?;

    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Read the actual physical charging state.
///
/// Both control nodes must be readable for a definitive classification.
///
/// Mapping:
///
/// | charging_enabled | input_suspend | result             |
/// |------------------|---------------|--------------------|
/// | 1                | 0             | ChargingEnabled    |
/// | 0                | 1             | ChargingDisabled   |
/// | 1                | 1             | Inconsistent       |
/// | 0                | 0             | Inconsistent       |
/// | unreadable       | any           | Unknown            |
/// | any              | unreadable    | Unknown            |
pub fn get_actual_charging_state() -> ActualHardwareMode {
    let Some(charging_node) = charging_node() else {
        return ActualHardwareMode::Unknown;
    };

    let Some(suspend_node) = suspend_node() else {
        return ActualHardwareMode::Unknown;
    };

    let charging = read_bool_node(charging_node);
    let suspended = read_bool_node(suspend_node);

    let (Some(charging), Some(suspended)) = (charging, suspended) else {
        return ActualHardwareMode::Unknown;
    };

    match (charging, suspended) {
        (true, false) => ActualHardwareMode::ChargingEnabled,

        (false, true) => ActualHardwareMode::ChargingDisabled,

        /*
         * Both 1 and 0/0 are not valid normal states for this platform.
         */
        _ => ActualHardwareMode::Inconsistent,
    }
}

/// This platform does not expose a separate physical bypass state.
pub fn has_distinct_bypass_node() -> bool {
    false
}

/// Grant write permission to known charging nodes.
///
/// This is kept for compatibility with the existing public API.
///
/// In production, device-specific udev/init permissions are preferable to
/// modifying sysfs permissions from the daemon itself.
#[cfg(unix)]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    use std::os::unix::fs::PermissionsExt;

    let nodes = CHARGING_NODES
        .iter()
        .chain(SUSPEND_NODES.iter())
        .chain(FAST_CHARGE_CURRENT_NODES.iter())
        .chain(THERMAL_INPUT_CURRENT_NODES.iter())
        .copied();

    let mut found = false;
    let mut permission_failures = 0usize;

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
                    "failed to read sysfs metadata"
                );

                continue;
            }
        };

        found = true;

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o644);

        if let Err(error) = fs::set_permissions(path, permissions) {
            permission_failures += 1;

            tracing::warn!(
                path = node,
                error = %error,
                "failed to set sysfs permissions"
            );
        }
    }

    if !found {
        return Err(ChargerError::NoChargingNodeFound);
    }

    if permission_failures > 0 {
        return Err(ChargerError::PartialWriteFailure {
            succeeded: 0,
            failed: permission_failures,
        });
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    Ok(())
}
