#[cfg(test)]
mod tests {
    use crate::hardware::controller::{HardwareController, HardwareTarget, SyncState};
    use crate::hardware::profile::{HardwareProfile, CurrentNodeConfig, CurrentUnit, OnlineNodeConfig};
    use crate::battery::snapshot::SensorSnapshot;
    use crate::battery::reader::BatteryStatus;
    use crate::persistence::ownership::Ownership;
    use std::time::Instant;

    const MOCK_PROFILE: HardwareProfile = HardwareProfile {
        name: "mock",
        charging_nodes: &[],
        suspend_nodes: &[],
        current_nodes: &[],
        online_nodes: &[],
        capacity_path: "",
        temperature_path: "",
        status_path: "",
    };

    #[test]
    fn test_initial_state() {
        let controller = HardwareController::new(&MOCK_PROFILE);
        assert_eq!(controller.sync, SyncState::Unknown);
        assert_eq!(controller.desired_target, HardwareTarget::Unmanaged);
        assert_eq!(controller.applied_target, HardwareTarget::Unmanaged);
        assert!(controller.force_apply);
        assert_eq!(controller.ownership, Ownership::NotOwned);
    }

    #[test]
    fn test_target_change_forces_apply() {
        let mut controller = HardwareController::new(&MOCK_PROFILE);
        controller.set_desired_target(HardwareTarget::ChargingEnabled);

        assert_eq!(controller.desired_target, HardwareTarget::ChargingEnabled);
        assert!(controller.force_apply);
        assert_eq!(controller.sync, SyncState::Unknown);
    }

    #[test]
    fn test_needs_apply_logic() {
        let mut controller = HardwareController::new(&MOCK_PROFILE);
        let now = Instant::now();

        // Initially needs apply because force_apply is true
        assert!(controller.needs_apply(HardwareTarget::Unmanaged, now));

        controller.force_apply = false;
        // Does not need apply if targets match and not forced
        assert!(!controller.needs_apply(HardwareTarget::Unmanaged, now));

        // Needs apply if desired target changes
        controller.set_desired_target(HardwareTarget::ChargingDisabled);
        assert!(controller.needs_apply(HardwareTarget::ChargingDisabled, now));
    }
}
