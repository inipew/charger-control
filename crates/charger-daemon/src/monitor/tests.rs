#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use charger_core::config::schema::Config;

    use crate::monitor::{
        decision::{ChargingDecision, DesiredHardwareState, WaitReason},
        hardware::{HardwareFault, HardwareStatus, HardwareTrack},
        intent::OperatingIntent,
        policy::{evaluate_policy, PolicyBlock, PolicyResult},
        reality::{ConnectionState, ObservedState, Sample},
        scheduler::Urgency,
    };

    #[test]
    fn test_soc_limit_hysteresis() {
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 100;
        config.resume_limit = 98;

        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        observed.connection = ConnectionState::Attached;

        let policy1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), now);
        assert!(policy1.is_blocked_by(PolicyBlock::ChargeLimit));

        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 99.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );

        let policy2 = evaluate_policy(&observed, &config, &policy1, now);
        assert!(policy2.is_blocked_by(PolicyBlock::ChargeLimit));

        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 97.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );

        let policy3 = evaluate_policy(&observed, &config, &policy2, now);
        assert!(!policy3.is_blocked_by(PolicyBlock::ChargeLimit));
    }

    #[test]
    fn test_thermal_emergency_latching() {
        let mut now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.max_temp_dc = 420;

        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 46.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        observed.connection = ConnectionState::Attached;

        let policy1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), now);
        assert!(policy1.is_blocked_by(PolicyBlock::ThermalEmergency));

        now += Duration::from_secs(10);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 40.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );

        let policy2 = evaluate_policy(&observed, &config, &policy1, now);
        assert!(policy2.is_blocked_by(PolicyBlock::ThermalEmergency));

        now += Duration::from_secs(10);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 37.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );

        let policy3 = evaluate_policy(&observed, &config, &policy2, now);
        assert!(!policy3.is_blocked_by(PolicyBlock::ThermalEmergency));
    }

    // --- INVARIANT TESTS ---

    #[test]
    fn test_invariant_a_decision_not_affected_by_hardware_fault() {
        // Invariant A: Decision tidak dipengaruhi hardware fault
        // Intent = Normal, Policy = Allow, Hardware = Fault => Decision = Allow
        let mut hw_track = HardwareTrack::new();
        let now = Instant::now();
        hw_track.mark_fault(HardwareFault::WriteFailed, now);

        let mut observed = ObservedState::new();
        observed.connection = ConnectionState::Attached;
        let policy = PolicyResult::clear();

        let decision =
            ChargingDecision::resolve(&observed, &OperatingIntent::normal(), &policy, now);

        assert_eq!(decision, ChargingDecision::Allow);
        assert_eq!(
            decision.to_desired_hardware(),
            DesiredHardwareState::ChargingEnabled
        );
    }

    #[test]
    fn test_invariant_b_fault_does_not_mutate_incorrectly() {
        // Invariant B: Fault tidak menyebabkan mutation ke state yang salah (Deferred)
        let mut hw_track = HardwareTrack::new();
        let now = Instant::now();
        hw_track.mark_fault(HardwareFault::WriteFailed, now);

        let early_now = now + Duration::from_secs(1);
        let opts = crate::monitor::hardware::ReconcileOptions {
            bypass_retry_delay: true,
            force_verification: true,
        };

        let res = crate::monitor::hardware::reconcile(
            DesiredHardwareState::ChargingDisabled,
            &mut hw_track,
            false,
            opts,
            early_now,
        );

        assert!(!matches!(
            res,
            crate::monitor::hardware::ReconcileResult::Deferred
        ));
    }

    #[test]
    fn test_invariant_h_force_verify_not_force_retry() {
        // Invariant H: force_verification = true TIDAK SAMA DENGAN force_retry.
        // Jika status Fault dan retry belum due, harus tetap Deferred meskipun force_verification = true.
        let mut hw_track = HardwareTrack::new();
        let now = Instant::now();
        hw_track.mark_fault(HardwareFault::WriteFailed, now); // Retry dalam 5 detik

        let early_now = now + Duration::from_secs(1); // Belum 5 detik

        let opts = crate::monitor::hardware::ReconcileOptions {
            bypass_retry_delay: false, // Tidak boleh mem-bypass delay
            force_verification: true,  // Tapi dipaksa verifikasi
        };

        let res = crate::monitor::hardware::reconcile(
            DesiredHardwareState::ChargingEnabled,
            &mut hw_track,
            false,
            opts,
            early_now,
        );

        // Harus ditunda karena bypass_retry_delay = false
        assert!(matches!(
            res,
            crate::monitor::hardware::ReconcileResult::Deferred
        ));
    }

    // TODO: test_invariant_c_retry_retains_target
    // Membutuhkan abstraksi actuator/mock sysfs agar tidak bergantung pada environment OS asli.

    #[test]
    fn test_invariant_d_safety_urgency_always_wins() {
        // Invariant D: Safety selalu menang
        // Decision urgency = Safety, Retry pending = true => scheduler urgency = Safety
        let mut hw_track = HardwareTrack::new();
        let now = Instant::now();
        hw_track.mark_fault(HardwareFault::WriteFailed, now); // Next deadline is some

        let decision_urgency = Urgency::Safety;
        let retry_urgency = Urgency::Recovery;

        assert_eq!(decision_urgency.max(retry_urgency), Urgency::Safety);
    }

    // TODO: test_invariant_e_idempotency_even_on_emergency
    // Membutuhkan abstraksi actuator/mock sysfs untuk menghitung jumlah (0) write call ke sysfs saat kondisi stabil.

    #[test]
    fn test_invariant_f_unknown_verification_is_not_fresh() {
        // Invariant F: Unknown verification tidak dianggap fresh
        // verified_at = None => verification_needed = true
        let hw_track = HardwareTrack::new();
        assert!(hw_track.current_obs.verified_at.is_none());
        assert!(hw_track.verification_needed);
    }

    #[test]
    fn test_invariant_g_disconnected_nochange() {
        // Invariant G: Disconnected => Decision = NoChange => no charging mutation
        let observed = ObservedState::new();
        let now = Instant::now();
        let policy = PolicyResult::clear();

        let decision =
            ChargingDecision::resolve(&observed, &OperatingIntent::normal(), &policy, now);
        assert_eq!(
            decision,
            ChargingDecision::Wait {
                reason: WaitReason::Disconnected
            }
        );
        assert_eq!(
            decision.to_desired_hardware(),
            DesiredHardwareState::NoChange
        );

        // Disconnected with sleep forever
        // Wait, sleep forever is tested in can_sleep_forever() which is in mod.rs, not pub.
    }

    #[test]
    fn test_invariant_i_bypass_fallback_to_disabled() {
        // Invariant I: Jika has_distinct_bypass = false, maka Desired::Bypass -> ChargingDisabled
        let expected = DesiredHardwareState::Bypass.hardware_mode(false);
        assert_eq!(
            expected,
            charger_core::battery::control::ActualHardwareMode::ChargingDisabled
        );
    }

    #[test]
    fn test_invariant_j_disconnect_clears_observation() {
        // Invariant J: Disconnect mereset current_obs dan meminta verifikasi
        let mut track = HardwareTrack::new();
        track.update_observation(
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled,
            Instant::now(),
        );

        assert_eq!(
            track.current_obs.mode,
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled
        );
        assert!(track.current_obs.verified_at.is_some());
        assert!(!track.verification_needed);

        track.reset_on_disconnect();

        assert_eq!(
            track.current_obs.mode,
            charger_core::battery::control::ActualHardwareMode::Unknown
        );
        assert!(track.current_obs.verified_at.is_none());
        assert!(track.verification_needed);
    }

    #[test]
    fn test_invariant_k_permission_denied_never_retry_preserves_deferral() {
        // Invariant K: Fault dengan retry Never akan selalu Deferred kecuali ada bypass_retry_delay
        let mut track = HardwareTrack::new();
        track.mark_fault(HardwareFault::PermissionDenied, Instant::now());

        let opts = crate::monitor::hardware::ReconcileOptions {
            bypass_retry_delay: false,
            force_verification: true,
        };

        let res = crate::monitor::hardware::reconcile(
            DesiredHardwareState::ChargingEnabled,
            &mut track,
            true,
            opts,
            Instant::now() + Duration::from_secs(10),
        );

        assert!(matches!(
            res,
            crate::monitor::hardware::ReconcileResult::Deferred
        ));
    }
}
