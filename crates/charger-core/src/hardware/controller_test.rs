#[cfg(test)]
mod tests {
    use crate::hardware::controller::*;
    use crate::persistence::ownership::Ownership;
    use crate::hardware::io::{testing::MockHardwareIo, HardwareIo};
    use crate::persistence::io::{testing::MockPersistenceIo, PersistenceIo};
    use crate::time::testing::FakeClock;
    use crate::battery::snapshot::SensorSnapshot;
    use std::sync::Arc;
    use std::time::Instant;

    fn create_test_profile() -> Arc<crate::hardware::profile::HardwareProfile> {
        Arc::new(crate::hardware::profile::GENERIC_PROFILE)
    }

    #[test]
    fn ownership_invariant() {
        let hw_io = Arc::new(MockHardwareIo::new());
        let pers_io = Arc::new(MockPersistenceIo::new());
        let clock = Arc::new(FakeClock::new(Instant::now()));

        let mut ctrl = HardwareController::new(create_test_profile(), hw_io.clone(), pers_io.clone(), clock.clone());

        assert_eq!(ctrl.ownership, Ownership::NotOwned);

        // Precondition: hardware says charging is enabled (1)
        hw_io.set_node(std::path::Path::new("/sys/class/power_supply/battery/charging_enabled"), "1");
        
        let events = ctrl.apply_target(HardwareTarget::ChargingDisabled);
        
        // Should acquire ownership
        assert!(matches!(ctrl.ownership, Ownership::Owned { original_charging: true }));
        // State file should be saved as TOML OwnershipRecord
        let state_str = pers_io.read(std::path::Path::new("/data/adb/charger-control/ownership.state")).unwrap();
        let record: crate::persistence::ownership::OwnershipRecord = toml::from_str(&state_str).unwrap();
        assert_eq!(record.phase, crate::persistence::ownership::OwnershipPhase::Owned);
        assert!(record.original_charging);
        // Apply should succeed
        assert!(events.iter().any(|e| matches!(e, ControllerEvent::ApplySuccess(HardwareTarget::ChargingDisabled))));
        assert_eq!(ctrl.sync, SyncState::Pending);

        // Release ownership
        let events = ctrl.apply_target(HardwareTarget::Unmanaged);
        assert_eq!(ctrl.ownership, Ownership::NotOwned);
        // Original charging state restored to 1
        assert_eq!(hw_io.read(std::path::Path::new("/sys/class/power_supply/battery/charging_enabled")).unwrap(), "1");
        // State file should be deleted
        assert!(pers_io.read(std::path::Path::new("/data/adb/charger-control/ownership.state")).is_err());
        assert!(events.iter().any(|e| matches!(e, ControllerEvent::ApplySuccess(HardwareTarget::Unmanaged))));
    }

    #[test]
    fn partial_write_invariant() {
        let hw_io = Arc::new(MockHardwareIo::new());
        let pers_io = Arc::new(MockPersistenceIo::new());
        let clock = Arc::new(FakeClock::new(Instant::now()));

        // Simulate multiple nodes, one fails
        hw_io.inject_write_error(std::path::Path::new("/sys/class/power_supply/battery/charging_enabled"), std::io::ErrorKind::PermissionDenied);

        let mut ctrl = HardwareController::new(create_test_profile(), hw_io.clone(), pers_io.clone(), clock.clone());
        let events = ctrl.apply_target(HardwareTarget::ChargingDisabled);

        // Verify partial write means it's not synced
        assert_eq!(ctrl.sync, SyncState::Failed);
        assert!(events.iter().any(|e| matches!(e, ControllerEvent::ApplyFailed)));
    }

    #[test]
    fn verification_invariant() {
        let hw_io = Arc::new(MockHardwareIo::new());
        let pers_io = Arc::new(MockPersistenceIo::new());
        let clock = Arc::new(FakeClock::new(Instant::now()));

        let mut ctrl = HardwareController::new(create_test_profile(), hw_io.clone(), pers_io.clone(), clock.clone());

        hw_io.set_node(std::path::Path::new("/sys/class/power_supply/battery/charging_enabled"), "1");
        
        let _events = ctrl.apply_target(HardwareTarget::ChargingDisabled);
        assert_eq!(ctrl.sync, SyncState::Pending);

        let snapshot = SensorSnapshot {
            online: Some(true),
            current_ma: Some(10),
            ..SensorSnapshot {
                capacity_pct: None,
                temp_dc: None,
                online: None,
                current_ma: None,
                status: Some(crate::battery::reader::BatteryStatus::Unknown),
                ts: Instant::now(),
            }
        };
        hw_io.set_node(std::path::Path::new("/sys/class/power_supply/battery/charging_enabled"), "0"); // successfully disabled

        let events = ctrl.verify(&snapshot);
        assert!(events.iter().any(|e| matches!(e, ControllerEvent::VerificationSuccess)));
        assert_eq!(ctrl.sync, SyncState::Synced); // remains synced

        // Now simulate external modification (charging enabled externally)
        hw_io.set_node(std::path::Path::new("/sys/class/power_supply/battery/charging_enabled"), "1");
        
        // Next reconciliation detects mismatch
        let events = ctrl.reconcile();
        assert!(events.iter().any(|e| matches!(e, ControllerEvent::ExternalModificationDetected)));
        assert_eq!(ctrl.sync, SyncState::Unknown);
    }
}
