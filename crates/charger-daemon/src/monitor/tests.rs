#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use charger_core::config::schema::Config;

    use crate::monitor::{
        decision::{ChargingDecision, DesiredHardwareState, WaitReason},
        hardware::{HardwareFault, HardwareStatus, HardwareTrack},
        intent::OperatingIntent,
        policy::{evaluate_policy, PolicyBlock, PolicyResult, PolicyRuntime, CHARGE_LIMIT_SUSPEND_DELAY},
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
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // t=0: SOC 100% → mulai grace timer
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
        let _p0 = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);

        // t=5m: grace period selesai → ChargeLimit blocked
        let after_grace = now + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: after_grace,
            }),
            after_grace,
        );
        let policy1 = evaluate_policy(&observed, &config, &_p0, &mut runtime, after_grace);
        assert!(policy1.is_blocked_by(PolicyBlock::ChargeLimit));

        // SOC 99% masih di atas resume (98%) → tetap blocked (hysteresis)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 99.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: after_grace,
            }),
            after_grace,
        );

        let policy2 = evaluate_policy(&observed, &config, &policy1, &mut runtime, after_grace);
        assert!(policy2.is_blocked_by(PolicyBlock::ChargeLimit));

        // SOC 97% di bawah resume (98%) → unblocked
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 97.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: after_grace,
            }),
            after_grace,
        );

        let policy3 = evaluate_policy(&observed, &config, &policy2, &mut runtime, after_grace);
        assert!(!policy3.is_blocked_by(PolicyBlock::ChargeLimit));
    }

    #[test]
    fn test_thermal_emergency_latching() {
        let mut now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.max_temp_dc = 420;

        let mut runtime = PolicyRuntime::default();

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

        let policy1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);
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

        let policy2 = evaluate_policy(&observed, &config, &policy1, &mut runtime, now);
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

        let policy3 = evaluate_policy(&observed, &config, &policy2, &mut runtime, now);
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
        assert!(hw_track.last_verified_obs.verified_at.is_none());
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
            track.last_verified_obs.mode,
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled
        );
        assert!(track.last_verified_obs.verified_at.is_some());
        assert!(!track.verification_needed);

        track.reset_on_disconnect();

        assert_eq!(
            track.last_verified_obs.mode,
            charger_core::battery::control::ActualHardwareMode::Unknown
        );
        assert!(track.last_verified_obs.verified_at.is_none());
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

    #[test]
    fn test_permission_denied_emergency_bypass_still_deferred() {
        // Confirming that PermissionDenied (FaultRetryPolicy::Never) returns Deferred even if bypass_retry_delay is true
        let mut track = HardwareTrack::new();
        track.mark_fault(HardwareFault::PermissionDenied, Instant::now());

        let opts = crate::monitor::hardware::ReconcileOptions {
            bypass_retry_delay: true,
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

    #[test]
    fn test_invariant_l_sensor_failure_uses_stale_policy() {
        // Invariant L: Sensor read failure does not invalidate the old sample immediately.
        // It relies on the stale threshold.
        let mut observed = ObservedState::new();
        let now = Instant::now();
        
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 35.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        
        // Mark that a subsequent read failed
        let failure_time = now + Duration::from_secs(10);
        observed.mark_sample_failed(failure_time + Duration::from_secs(2));
        
        // Policy should STILL consider it fresh because 10s < 60s
        assert!(observed.has_fresh_sample(failure_time));
        
        // But after 60s from the original sample time, it becomes stale
        let stale_time = now + crate::monitor::reality::SAMPLE_STALE_THRESHOLD;
        assert!(!observed.has_fresh_sample(stale_time));
    }

    #[test]
    fn test_charge_limit_grace_period_blocks_after_delay() {
        // Grace Period: SOC >= limit selama 5 menit → baru Block
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 100;
        let mut runtime = PolicyRuntime::default();

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

        // t=0: SOC >= limit, tapi grace period belum habis → belum block
        let p1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(runtime.charge_limit_grace_started_at.is_some());

        // t=2m: masih dalam grace → belum block
        let t2 = now + Duration::from_secs(120);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t2,
            }),
            t2,
        );
        let p2 = evaluate_policy(&observed, &config, &p1, &mut runtime, t2);
        assert!(!p2.is_blocked_by(PolicyBlock::ChargeLimit));

        // t=5m: grace period selesai → BLOCK!
        let t5 = now + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5,
            }),
            t5,
        );
        let p3 = evaluate_policy(&observed, &config, &p2, &mut runtime, t5);
        assert!(p3.is_blocked_by(PolicyBlock::ChargeLimit));
    }

    #[test]
    fn test_charge_limit_grace_timer_resets_on_soc_drop() {
        // Timer reset jika SOC turun di bawah limit
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 100;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // t=0: SOC 100% → timer mulai
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
        let p1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(runtime.charge_limit_grace_started_at.is_some());

        // t=3m: SOC turun ke 99% (< limit) → timer RESET
        let t3 = now + Duration::from_secs(180);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 99.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t3,
            }),
            t3,
        );
        let p2 = evaluate_policy(&observed, &config, &p1, &mut runtime, t3);
        assert!(!p2.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(runtime.charge_limit_grace_started_at.is_none()); // Timer reset!

        // t=3m: SOC kembali 100% → timer mulai ULANG dari sekarang
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t3,
            }),
            t3,
        );
        let p3 = evaluate_policy(&observed, &config, &p2, &mut runtime, t3);
        assert!(!p3.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_grace_started_at, Some(t3)); // Timer baru!

        // t=5m dari timer awal (t=0+5m) → seharusnya BELUM block karena timer reset di t=3m
        let t5_from_start = now + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5_from_start,
            }),
            t5_from_start,
        );
        let p4 = evaluate_policy(&observed, &config, &p3, &mut runtime, t5_from_start);
        assert!(!p4.is_blocked_by(PolicyBlock::ChargeLimit)); // Belum 5 menit dari t3!

        // t=8m (t3 + 5m) → SEKARANG baru block
        let t8 = t3 + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 100.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t8,
            }),
            t8,
        );
        let p5 = evaluate_policy(&observed, &config, &p4, &mut runtime, t8);
        assert!(p5.is_blocked_by(PolicyBlock::ChargeLimit));
    }

    #[test]
    fn test_charge_limit_grace_timer_resets_on_sensor_stale() {
        // Timer reset saat sensor stale
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 100;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // t=0: SOC 100% → timer mulai
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
        let p1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);
        assert!(runtime.charge_limit_grace_started_at.is_some());

        // t=61s: sample sekarang stale → timer HARUS reset
        let t_stale = now + crate::monitor::reality::SAMPLE_STALE_THRESHOLD;
        let p2 = evaluate_policy(&observed, &config, &p1, &mut runtime, t_stale);
        assert!(p2.is_blocked_by(PolicyBlock::SensorStale));
        assert!(runtime.charge_limit_grace_started_at.is_none()); // Timer reset!
    }

    #[test]
    fn test_charge_limit_grace_lifecycle() {
        let mut now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 80;
        config.resume_limit = 78;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // 1. t=0: SOC 80% (reach limit)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample { capacity: 80.0, temperature_c: 30.0, power_state: charger_core::battery::reader::PowerState::Connected, timestamp: now }),
            now,
        );
        let mut p = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_grace_started_at, Some(now));

        // 2. t=299s: SOC 80% (almost 5 mins) -> still allowed
        now += Duration::from_secs(299);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample { capacity: 80.0, temperature_c: 30.0, power_state: charger_core::battery::reader::PowerState::Connected, timestamp: now }),
            now,
        );
        p = evaluate_policy(&observed, &config, &p, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));

        // 3. t=300s: SOC 80% (exact 5 mins) -> BLOCKED
        now += Duration::from_secs(1);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample { capacity: 80.0, temperature_c: 30.0, power_state: charger_core::battery::reader::PowerState::Connected, timestamp: now }),
            now,
        );
        p = evaluate_policy(&observed, &config, &p, &mut runtime, now);
        assert!(p.is_blocked_by(PolicyBlock::ChargeLimit));

        // 4. t=310s: SOC drops to 77% (below resume limit) -> UNBLOCKED and reset
        now += Duration::from_secs(10);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample { capacity: 77.0, temperature_c: 30.0, power_state: charger_core::battery::reader::PowerState::Connected, timestamp: now }),
            now,
        );
        p = evaluate_policy(&observed, &config, &p, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(runtime.charge_limit_grace_started_at.is_none());

        // 5. t=320s: SOC goes back to 80% -> timer starts AGAIN
        now += Duration::from_secs(10);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample { capacity: 80.0, temperature_c: 30.0, power_state: charger_core::battery::reader::PowerState::Connected, timestamp: now }),
            now,
        );
        let start_time_2 = now;
        p = evaluate_policy(&observed, &config, &p, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_grace_started_at, Some(start_time_2));

        // 6. t=330s: Disconnect happens! -> everything clears
        now += Duration::from_secs(10);
        observed.connection = ConnectionState::Disconnected;
        p = evaluate_policy(&observed, &config, &p, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(runtime.charge_limit_grace_started_at.is_none());

        // 7. t=340s: Reconnect, SOC 80% -> timer starts fresh
        now += Duration::from_secs(10);
        observed.connection = ConnectionState::Attached;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample { capacity: 80.0, temperature_c: 30.0, power_state: charger_core::battery::reader::PowerState::Connected, timestamp: now }),
            now,
        );
        p = evaluate_policy(&observed, &config, &p, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_grace_started_at, Some(now));
    }

    #[test]
    fn test_charge_limit_grace_resets_when_config_changes() {
        // Invariant: mengubah charge_limit config harus mereset grace timer.
        // Tanpa reset, SOC yang baru saja mencapai limit baru bisa langsung
        // diblokir menggunakan timer dari sesi config lama.
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 80;
        config.resume_limit = 78;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 80.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );

        // t=0: SOC 80% → timer mulai
        let p1 = evaluate_policy(&observed, &config, &PolicyResult::clear(), &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_grace_started_at, Some(now));

        // Simulasi ConfigReload: charge_limit naik ke 90
        // Runtime di-reset oleh ConfigReload handler (seperti di events.rs)
        runtime.clear();

        // t=301s: SOC kini 90% (sesuai limit baru), tapi timer baru saja di-reset
        let changed = now + Duration::from_secs(301);
        let mut new_config = config.clone();
        new_config.charge_limit = 90;
        new_config.resume_limit = 88;

        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 90.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: changed,
            }),
            changed,
        );

        let p2 = evaluate_policy(&observed, &new_config, &p1, &mut runtime, changed);

        // Harus BELUM blocked — timer baru saja dimulai dari `changed`
        assert!(!p2.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_grace_started_at, Some(changed));
    }
}
