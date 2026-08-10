use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    battery::nodes::{CHARGING_NODES, SUSPEND_NODES},
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

fn charging_node() -> Option<&'static str> {
    detect_node_cached("charging_control", CHARGING_NODES, &CHARGING_NODE_IDX)
}

/// Return the primary input-suspend control node.
fn suspend_node() -> Option<&'static str> {
    detect_node_cached("input_suspend", SUSPEND_NODES, &SUSPEND_NODE_IDX)
}

/// Invalidate the cached node indices for both charging and suspend nodes.
///
/// Call this when a write operation returns `NoChargingNodeFound` so that
/// the next evaluation re-discovers which nodes are physically available.
pub fn reset_node_caches() {
    CHARGING_NODE_IDX.store(usize::MAX, Ordering::Relaxed);
    SUSPEND_NODE_IDX.store(usize::MAX, Ordering::Relaxed);
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

    let nodes = CHARGING_NODES.iter().chain(SUSPEND_NODES.iter()).copied();

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
