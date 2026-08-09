use crate::error::ChargerError;
use std::path::Path;

use crate::hardware::io::HardwareIo;

/// Write a value to a sysfs node with proper error context.
pub fn write_sysfs(path: &Path, value: &str, io: &dyn HardwareIo) -> Result<(), ChargerError> {
    io.write(path, value)
}

/// Result of attempting to write all charging-control nodes.
///
/// `attempted` = number of existing nodes we tried to write.
/// `succeeded` = number of successful writes.
/// `failed` = number of failed writes.
///
/// Important:
/// - `succeeded == attempted` => all writes succeeded.
/// - `succeeded > 0 && failed > 0` => partial failure.
/// - `succeeded == 0 && attempted > 0` => all writes failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargingWriteResult {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl ChargingWriteResult {
    #[inline]
    pub fn all_succeeded(&self) -> bool {
        self.attempted > 0 && self.failed == 0
    }

    #[inline]
    pub fn partial_failure(&self) -> bool {
        self.succeeded > 0 && self.failed > 0
    }

    #[inline]
    pub fn all_failed(&self) -> bool {
        self.attempted > 0 && self.succeeded == 0
    }
}

/// Enable or disable charging across all known nodes.
///
/// Semantics:
///
/// - No node exists:
///     Err(NoChargingNodeFound)
///
/// - At least one node exists and all writes succeed:
///     Ok(all_succeeded)
///
/// - At least one write succeeds but another fails:
///     Ok(partial_failure)
///
/// - Nodes exist but every write fails:
///     Err(last_write_error)
///
/// This is important because a partial write means the hardware may be in
/// a mixed state and should not be treated as a fully successful operation.
pub fn set_charging(enable: bool, profile: &crate::hardware::profile::HardwareProfile, io: &dyn HardwareIo) -> Result<ChargingWriteResult, ChargerError> {
    let charge_val = if enable { "1" } else { "0" };
    let suspend_val = if enable { "0" } else { "1" };

    let mut result = ChargingWriteResult {
        attempted: 0,
        succeeded: 0,
        failed: 0,
    };

    let mut last_error: Option<ChargerError> = None;

    // charging_enabled-style nodes.
    for node in profile.charging_nodes {
        let path = Path::new(node);


        if !io.exists(path) {
            continue;
        }

        result.attempted += 1;

        match write_sysfs(path, charge_val, io) {
            Ok(()) => {
                result.succeeded += 1;
                tracing::debug!(
                    "Charging node write succeeded: {} = {}",
                    path.display(),
                    charge_val
                );
            }
            Err(e) => {
                result.failed += 1;

                tracing::warn!(
                    "Charging node write failed: {} = {}: {}",
                    path.display(),
                    charge_val,
                    e
                );

                last_error = Some(e);
            }
        }
    }

    // input_suspend-style nodes.
    for node in profile.suspend_nodes {
        let path = Path::new(node);

        if !io.exists(path) {
            continue;
        }

        result.attempted += 1;

        match write_sysfs(path, suspend_val, io) {
            Ok(()) => {
                result.succeeded += 1;
                tracing::debug!(
                    "Suspend node write succeeded: {} = {}",
                    path.display(),
                    suspend_val
                );
            }
            Err(e) => {
                result.failed += 1;

                tracing::warn!(
                    "Suspend node write failed: {} = {}: {}",
                    path.display(),
                    suspend_val,
                    e
                );

                last_error = Some(e);
            }
        }
    }

    // No usable charging-control node exists.
    if result.attempted == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }

    // Every existing node failed.
    //
    // Return the actual sysfs error instead of fabricating a generic error.
    if result.all_failed() {
        if let Some(error) = last_error {
            return Err(error);
        }

        // Defensive fallback. This should theoretically be unreachable because
        // all_failed() implies at least one attempted write and therefore at
        // least one error should have been recorded.
        return Err(ChargerError::SysfsWrite {
            path: Path::new("charging_nodes").to_path_buf(),
            source: std::io::Error::other(
                "All charging node writes failed",
            ),
        });
    }

    // Either:
    //   1. all writes succeeded, or
    //   2. some succeeded and some failed.
    //
    // The caller MUST inspect `failed` / `partial_failure()`.
    if result.partial_failure() {
        tracing::warn!(
            "Charging control partially applied: {}/{} writes succeeded, {} failed",
            result.succeeded,
            result.attempted,
            result.failed
        );
    } else {
        tracing::info!(
            "Charging control applied successfully: {}/{} writes succeeded",
            result.succeeded,
            result.attempted
        );
    }

    Ok(result)
}

/// Activate bypass mode (disconnect input power from battery).
pub fn enter_bypass_mode(io: &dyn HardwareIo) -> Result<ChargingWriteResult, ChargerError> {
    let nodes = [
        ("/sys/class/power_supply/battery/input_suspend", "1"),
        ("/sys/class/power_supply/battery/charging_enabled", "0"),
        ("/sys/class/power_supply/main/charging_enabled", "0"),
    ];
    let mut result = ChargingWriteResult {
        attempted: 0,
        succeeded: 0,
        failed: 0,
    };
    let mut last_error: Option<ChargerError> = None;

    for (path, val) in &nodes {
        let p = Path::new(path);
        if io.exists(p) {
            result.attempted += 1;
            if let Err(e) = write_sysfs(p, val, io) {
                result.failed += 1;
                last_error = Some(e);
            } else {
                result.succeeded += 1;
            }
        }
    }

    if result.attempted == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }
    
    if result.all_failed() {
        if let Some(error) = last_error {
            return Err(error);
        }
        return Err(ChargerError::SysfsWrite {
            path: Path::new("bypass_nodes").to_path_buf(),
            source: std::io::Error::other(
                "All bypass node writes failed",
            ),
        });
    }

    Ok(result)
}

/// Restore normal charging from bypass mode.
pub fn exit_bypass_mode(io: &dyn HardwareIo) -> Result<ChargingWriteResult, ChargerError> {
    let nodes = [
        ("/sys/class/power_supply/battery/input_suspend", "0"),
        ("/sys/class/power_supply/battery/charging_enabled", "1"),
        ("/sys/class/power_supply/main/charging_enabled", "1"),
    ];
    let mut result = ChargingWriteResult {
        attempted: 0,
        succeeded: 0,
        failed: 0,
    };
    let mut last_error: Option<ChargerError> = None;

    for (path, val) in &nodes {
        let p = Path::new(path);
        if io.exists(p) {
            result.attempted += 1;
            if let Err(e) = write_sysfs(p, val, io) {
                result.failed += 1;
                last_error = Some(e);
            } else {
                result.succeeded += 1;
            }
        }
    }

    if result.attempted == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }
    
    if result.all_failed() {
        if let Some(error) = last_error {
            return Err(error);
        }
        return Err(ChargerError::SysfsWrite {
            path: Path::new("bypass_nodes").to_path_buf(),
            source: std::io::Error::other(
                "All bypass node writes failed",
            ),
        });
    }

    Ok(result)
}



// ============================================================================
// Charging state
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingNodeState {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingState {
    Enabled,
    Disabled,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    ChargingEnabled,
    InputSuspend,
}

#[derive(Debug, Clone, Copy)]
struct ChargingNode {
    path: &'static str,
    kind: NodeKind,

    /// Higher priority wins when there is disagreement.
    priority: u8,
}

impl ChargingNode {
    const fn charging_enabled(path: &'static str, priority: u8) -> Self {
        Self {
            path,
            kind: NodeKind::ChargingEnabled,
            priority,
        }
    }

    const fn input_suspend(path: &'static str, priority: u8) -> Self {
        Self {
            path,
            kind: NodeKind::InputSuspend,
            priority,
        }
    }

    fn read_state(&self, io: &dyn HardwareIo) -> Result<ChargingNodeState, std::io::Error> {
        let content = io.read(Path::new(self.path)).map_err(|e| std::io::Error::other(e.to_string()))?;
        let value = content.trim();

        match self.kind {
            NodeKind::ChargingEnabled => match value {
                "1" => Ok(ChargingNodeState::Enabled),
                "0" => Ok(ChargingNodeState::Disabled),
                _ => Ok(ChargingNodeState::Unknown),
            },

            NodeKind::InputSuspend => match value {
                "0" => Ok(ChargingNodeState::Enabled),
                "1" => Ok(ChargingNodeState::Disabled),
                _ => Ok(ChargingNodeState::Unknown),
            },
        }
    }
}

/// Build the node table.
///
/// Priority rationale:
///
/// 100 = battery charging control
///  90 = main charging control
///  80 = input suspend
///
/// The exact vendor hierarchy can later be adjusted in one place.
fn charging_nodes(profile: &crate::hardware::profile::HardwareProfile) -> impl Iterator<Item = ChargingNode> {
    profile.charging_nodes
        .iter()
        .copied()
        .map(|path| {
            let priority = if path.contains("/battery/") {
                100
            } else if path.contains("/main/") {
                90
            } else {
                80
            };

            ChargingNode::charging_enabled(path, priority)
        })
        .chain(profile.suspend_nodes.iter().copied().map(|path| {
            ChargingNode::input_suspend(path, 80)
        }))
}

#[derive(Debug, Clone, Copy)]
struct NodeObservation {
    state: ChargingNodeState,
    priority: u8,
}

/// Read charging state using:
///
/// 1. Consensus if all readable nodes agree.
/// 2. Highest priority if nodes disagree.
/// 3. Mixed if multiple nodes with the same highest
///    priority disagree.
/// 4. Unknown if nothing can be read.
///
/// This prevents a stale low-priority vendor node from
/// overriding the actual primary charging controller.
pub fn read_charging_state(profile: &crate::hardware::profile::HardwareProfile, io: &dyn HardwareIo) -> Result<ChargingState, ChargerError> {
    let mut observations: Vec<NodeObservation> =
        Vec::with_capacity(profile.charging_nodes.len() + profile.suspend_nodes.len());

    for node in charging_nodes(profile) {
        let path = Path::new(node.path);

        if !io.exists(path) {
            continue;
        }

        let state = match node.read_state(io) {
            Ok(state) => state,

            Err(e) => {
                tracing::debug!(
                    "Unable to read charging node {}: {}",
                    node.path,
                    e
                );

                continue;
            }
        };

        if state == ChargingNodeState::Unknown {
            continue;
        }

        observations.push(NodeObservation {
            state,
            priority: node.priority,
        });
    }

    if observations.is_empty() {
        return Err(ChargerError::NoChargingNodeFound);
    }

    // ========================================================================
    // Step 1: Consensus
    // ========================================================================

    let all_enabled = observations
        .iter()
        .all(|n| n.state == ChargingNodeState::Enabled);

    if all_enabled {
        return Ok(ChargingState::Enabled);
    }

    let all_disabled = observations
        .iter()
        .all(|n| n.state == ChargingNodeState::Disabled);

    if all_disabled {
        return Ok(ChargingState::Disabled);
    }

    // ========================================================================
    // Step 2: Priority
    // ========================================================================

    let highest_priority = observations
        .iter()
        .map(|n| n.priority)
        .max()
        .unwrap_or(0);

    let highest: Vec<_> = observations
        .iter()
        .filter(|n| n.priority == highest_priority)
        .collect();

    let primary_enabled = highest
        .iter()
        .all(|n| n.state == ChargingNodeState::Enabled);

    let primary_disabled = highest
        .iter()
        .all(|n| n.state == ChargingNodeState::Disabled);

    /*
     * Multiple nodes at the same highest priority
     * must agree.
     *
     * Example:
     *
     * battery/charging_enabled = 1  priority 100
     * battery/another_control  = 0  priority 100
     *
     * => Mixed
     */
    if primary_enabled {
        tracing::debug!(
            "Charging state resolved by priority: \
             ENABLED (priority={})",
            highest_priority
        );

        return Ok(ChargingState::Enabled);
    }

    if primary_disabled {
        tracing::debug!(
            "Charging state resolved by priority: \
             DISABLED (priority={})",
            highest_priority
        );

        return Ok(ChargingState::Disabled);
    }

    tracing::warn!(
        "Charging nodes disagree at highest priority {}",
        highest_priority
    );

    Ok(ChargingState::Mixed)
}

/// Compatibility helper.
pub fn is_charging_enabled(profile: &crate::hardware::profile::HardwareProfile, io: &dyn HardwareIo) -> Result<bool, ChargerError> {
    match read_charging_state(profile, io)? {
        ChargingState::Enabled => Ok(true),
        ChargingState::Disabled => Ok(false),
        ChargingState::Mixed | ChargingState::Unknown => {
            Err(ChargerError::NoChargingNodeFound) // Or create a specific ChargingStateUnknown error
        }
    }
}
