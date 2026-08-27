#[cfg(test)]
#[allow(clippy::field_reassign_with_default, clippy::module_inception)]
mod tests {
    use std::time::{Duration, Instant};

    use charger_core::config::schema::Config;

    use crate::monitor::{
        classify_uevent,
        decision::{
            resolve_current_regulation, BlockCause, ChargingDecision, CurrentRegulation,
            DesiredHardwareState, WaitReason,
        },
        events::UeventKind,
        hardware::{HardwareFault, HardwareTrack},
        intent::OperatingIntent,
        policy::{
            evaluate_policy, evaluate_thermal_stepping, ChargeLimitState, PolicyBlock,
            PolicyResult, PolicyRuntime, ThermalStep, CHARGE_LIMIT_SUSPEND_DELAY,
        },
        reality::{ConnectionState, ObservedState, Sample},
        scheduler::{AdaptiveScheduler, Urgency},
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
        let _p0 = evaluate_policy(&observed, &config, &mut runtime, now);

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
        let policy1 = evaluate_policy(&observed, &config, &mut runtime, after_grace);
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

        let policy2 = evaluate_policy(&observed, &config, &mut runtime, after_grace);
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

        let policy3 = evaluate_policy(&observed, &config, &mut runtime, after_grace);
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

        let policy1 = evaluate_policy(&observed, &config, &mut runtime, now);
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

        let policy2 = evaluate_policy(&observed, &config, &mut runtime, now);
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

        let policy3 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!policy3.is_blocked_by(PolicyBlock::ThermalEmergency));
    }

    // --- INVARIANT TESTS ---

    #[test]
    fn test_invariant_a_decision_not_affected_by_hardware_fault() {
        // Invariant A: Decision tidak dipengaruhi hardware fault
        // Intent = Normal, Policy = Allow, Hardware = Fault => Decision = Allow
        let mut hw_track = HardwareTrack::new();
        let now = Instant::now();
        hw_track.charger.mark_fault(HardwareFault::WriteFailed, now);

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
        hw_track.charger.mark_fault(HardwareFault::WriteFailed, now);

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
        hw_track.charger.mark_fault(HardwareFault::WriteFailed, now); // Retry dalam 5 detik

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
        hw_track.charger.mark_fault(HardwareFault::WriteFailed, now); // Next deadline is some

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
        assert!(hw_track.charger.observation.verified_at.is_none());
        assert!(hw_track.charger.verification_needed);
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
        let now = Instant::now();
        track.charger.update_observation(
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled,
            now,
        );

        assert_eq!(
            track.charger.observation.mode,
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled
        );
        assert!(track.charger.observation.verified_at.is_some());
        assert!(!track.charger.verification_needed);

        track.reset_on_disconnect();

        assert_eq!(
            track.charger.observation.mode,
            charger_core::battery::control::ActualHardwareMode::Unknown
        );
        assert!(track.charger.observation.verified_at.is_none());
        assert!(track.charger.verification_needed);
    }

    #[test]
    fn test_invariant_k_permission_denied_never_retry_preserves_deferral() {
        // Invariant K: Fault dengan retry Never akan selalu Deferred kecuali ada bypass_retry_delay
        let mut track = HardwareTrack::new();
        track
            .charger
            .mark_fault(HardwareFault::PermissionDenied, Instant::now());

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
        track
            .charger
            .mark_fault(HardwareFault::PermissionDenied, Instant::now());

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
        let p1 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(matches!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { .. }
        ));

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
        let p2 = evaluate_policy(&observed, &config, &mut runtime, t2);
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
        let p3 = evaluate_policy(&observed, &config, &mut runtime, t5);
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
        let p1 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(matches!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { .. }
        ));

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
        let p2 = evaluate_policy(&observed, &config, &mut runtime, t3);
        assert!(!p2.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal); // Timer reset!

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
        let p3 = evaluate_policy(&observed, &config, &mut runtime, t3);
        assert!(!p3.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { started_at: t3 }
        ); // Timer baru!

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
        let p4 = evaluate_policy(&observed, &config, &mut runtime, t5_from_start);
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
        let p5 = evaluate_policy(&observed, &config, &mut runtime, t8);
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
        let _p1 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(matches!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { .. }
        ));

        // t=61s: sample sekarang stale → timer HARUS reset
        let t_stale = now + crate::monitor::reality::SAMPLE_STALE_THRESHOLD;
        let p2 = evaluate_policy(&observed, &config, &mut runtime, t_stale);
        assert!(p2.is_blocked_by(PolicyBlock::SensorStale));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal); // Timer reset!
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
            Some(Sample {
                capacity: 80.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        let mut p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { started_at: now }
        );

        // 2. t=299s: SOC 80% (almost 5 mins) -> still allowed
        now += Duration::from_secs(299);
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
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));

        // 3. t=300s: SOC 80% (exact 5 mins) -> BLOCKED
        now += Duration::from_secs(1);
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
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(p.is_blocked_by(PolicyBlock::ChargeLimit));

        // 4. t=310s: SOC drops to 77% (below resume limit) -> UNBLOCKED and reset
        now += Duration::from_secs(10);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 77.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal);

        // 5. t=320s: SOC goes back to 80% -> timer starts AGAIN
        now += Duration::from_secs(10);
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
        let start_time_2 = now;
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace {
                started_at: start_time_2
            }
        );

        // 6. t=330s: Disconnect happens! -> everything clears
        now += Duration::from_secs(10);
        observed.connection = ConnectionState::Disconnected;
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal);

        // 7. t=340s: Reconnect, SOC 80% -> timer starts fresh
        now += Duration::from_secs(10);
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
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { started_at: now }
        );
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
        let p1 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { started_at: now }
        );

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

        let p2 = evaluate_policy(&observed, &new_config, &mut runtime, changed);

        // Harus BELUM blocked — timer baru saja dimulai dari `changed`
        assert!(!p2.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace {
                started_at: changed
            }
        );
    }

    #[test]
    fn test_charge_limit_resume_boundary() {
        // Verifikasi boundary tepat resume_limit:
        //   SOC > resume_limit  → tetap Suspended (Block)
        //   SOC == resume_limit → Suspended → Normal (Allow) ← inklusif
        //   SOC < resume_limit  → Suspended → Normal (Allow)
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 80;
        config.resume_limit = 78;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // Bawa ke Suspended: SOC 80% selama 5 menit
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
        let p0 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p0.is_blocked_by(PolicyBlock::ChargeLimit));

        let t5 = now + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 80.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5,
            }),
            t5,
        );
        let p_suspended = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(p_suspended.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // SOC 78.1 (> resume_limit 78) → tetap Block
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 78.1,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5,
            }),
            t5,
        );
        let p_above = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            p_above.is_blocked_by(PolicyBlock::ChargeLimit),
            "78.1 > resume 78 harus tetap Block"
        );
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // SOC 78.0 (== resume_limit) → Allow (boundary inklusif: <= resume_limit)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 78.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5,
            }),
            t5,
        );
        let p_at = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            !p_at.is_blocked_by(PolicyBlock::ChargeLimit),
            "78.0 == resume 78 harus Allow"
        );
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal);

        // SOC 77.9 (< resume_limit) → Allow juga (state sudah Normal)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 77.9,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5,
            }),
            t5,
        );
        let p_below = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            !p_below.is_blocked_by(PolicyBlock::ChargeLimit),
            "77.9 < resume 78 harus Allow"
        );
    }

    #[test]
    fn test_charge_limit_no_rapid_toggle() {
        // Setelah SOC resume dan naik lagi ke limit, harus masuk Grace (bukan langsung Suspended).
        // Mencegah pola agresif: 78→ON, 79→ON, 80→OFF, 79→ON, 80→OFF berulang tanpa grace.
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 80;
        config.resume_limit = 78;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // 1. Bawa ke Suspended
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
        let _p0 = evaluate_policy(&observed, &config, &mut runtime, now);

        let t5 = now + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 80.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t5,
            }),
            t5,
        );
        let p_suspended = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(p_suspended.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // 2. SOC turun ke 78 → resume (Normal)
        let t_resume = t5 + Duration::from_secs(60);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 78.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t_resume,
            }),
            t_resume,
        );
        let p_normal = evaluate_policy(&observed, &config, &mut runtime, t_resume);
        assert!(!p_normal.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal);

        // 3. SOC naik lagi ke 80 → harus masuk Grace, BUKAN langsung Suspended
        let t_back = t_resume + Duration::from_secs(5);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 80.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t_back,
            }),
            t_back,
        );
        let p_grace = evaluate_policy(&observed, &config, &mut runtime, t_back);
        assert!(
            !p_grace.is_blocked_by(PolicyBlock::ChargeLimit),
            "Baru naik ke limit lagi harus dalam Grace, belum Block"
        );
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { started_at: t_back }
        );

        // 4. Setelah 5 menit baru Suspended kembali
        let t_next_suspend = t_back + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 80.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t_next_suspend,
            }),
            t_next_suspend,
        );
        let p_next_suspend = evaluate_policy(&observed, &config, &mut runtime, t_next_suspend);
        assert!(p_next_suspend.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);
    }

    #[test]
    fn test_charge_limit_deadline_only_during_grace() {
        // charge_limit_deadline() harus:
        //   Some saat Grace (untuk scheduler wake-up)
        //   None saat Normal (tidak ada timer aktif)
        //   None saat Suspended (grace timer sudah tidak aktif)
        let now = Instant::now();
        let mut runtime = PolicyRuntime::default();

        // Normal → None
        assert!(
            runtime.charge_limit_deadline().is_none(),
            "Normal: deadline harus None"
        );

        // Grace → Some(started_at + 5min)
        runtime.charge_limit_state = ChargeLimitState::Grace { started_at: now };
        let deadline = runtime.charge_limit_deadline();
        assert_eq!(
            deadline,
            Some(now + CHARGE_LIMIT_SUSPEND_DELAY),
            "Grace: deadline harus Some(t + 5min)"
        );

        // Suspended → None (grace timer tidak lagi aktif)
        runtime.charge_limit_state = ChargeLimitState::Suspended;
        assert!(
            runtime.charge_limit_deadline().is_none(),
            "Suspended: deadline harus None"
        );
    }

    #[test]
    fn test_charge_limit_regression_full_descent() {
        // Regression test: memastikan SOC 100→99→98→97→96→95 berperilaku benar.
        // Bug sebelumnya: 100 → Block, lalu 99 langsung → Allow.
        // Sekarang harus: tetap Block sampai SOC <= resume_limit (95).
        //
        // charge_limit = 100, resume_limit = 95
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 100;
        config.resume_limit = 95;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        let make_sample = |soc: f32, t: std::time::Instant| Sample {
            capacity: soc,
            temperature_c: 30.0,
            power_state: charger_core::battery::reader::PowerState::Connected,
            timestamp: t,
        };

        // t=0: SOC 100 → Grace dimulai
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(100.0, now)),
            now,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert!(matches!(
            runtime.charge_limit_state,
            ChargeLimitState::Grace { .. }
        ));

        // t=5m: Grace habis → Suspended
        let t5 = now + CHARGE_LIMIT_SUSPEND_DELAY;
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(100.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 100 setelah 5m harus Block"
        );
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // SOC 99 → harus tetap Block (bukan langsung Allow!)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(99.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 99 > resume 95 harus tetap Block"
        );
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // SOC 98 → tetap Block
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(98.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 98 > resume 95 harus tetap Block"
        );

        // SOC 97 → tetap Block
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(97.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 97 > resume 95 harus tetap Block"
        );

        // SOC 96 → tetap Block
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(96.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 96 > resume 95 harus tetap Block"
        );

        // SOC 95 → Allow (boundary inklusif: 95 <= 95)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(95.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            !p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 95 == resume 95 harus Allow"
        );
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal);

        // SOC 94 → Allow
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(make_sample(94.0, t5)),
            t5,
        );
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(
            !p.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 94 < resume 95 harus Allow"
        );
    }

    #[test]
    fn test_charge_limit_suspended_survives_detach_reattach() {
        // State Suspended harus tetap ada setelah charger dicabut dan dipasang kembali
        // (simulate glitch koneksi / bounce), bukan langsung di-reset ke Normal.
        //
        // Ini memastikan fix P0-1 bekerja: evaluate_policy saat Disconnected
        // TIDAK menghapus policy_runtime, sehingga saat re-attach dengan SOC 99%
        // charging tetap diblokir.
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 100;
        config.resume_limit = 95;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;

        // 1. Bawa ke state Suspended
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
        let _p = evaluate_policy(&observed, &config, &mut runtime, now);

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
        let p = evaluate_policy(&observed, &config, &mut runtime, t5);
        assert!(p.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // 2. Simulasi detach penuh di production:
        //    Connection menjadi Disconnected, sample dibersihkan, dan evaluate_policy dijalankan.
        observed.connection = ConnectionState::Disconnected;
        observed.clear_sample();
        let t_detach = t5 + Duration::from_secs(1);
        let p_disconnected = evaluate_policy(&observed, &config, &mut runtime, t_detach);

        // Bitmask policy result harus clear saat disconnected
        assert_eq!(
            p_disconnected,
            PolicyResult::clear(),
            "Saat disconnected, policy result harus clear"
        );
        // TETAPI policy_runtime (Suspended) HARUS tetap ada!
        assert_eq!(
            runtime.charge_limit_state,
            ChargeLimitState::Suspended,
            "State Suspended harus tetap bertahan saat disconnected"
        );

        // 3. Setelah re-attach (5 detik kemudian), evaluate_policy dipanggil lagi
        observed.connection = ConnectionState::Attached;
        let t_reattach = t_detach + Duration::from_secs(5);
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 99.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: t_reattach,
            }),
            t_reattach,
        );
        let p_after = evaluate_policy(&observed, &config, &mut runtime, t_reattach);

        // SOC 99 > resume 95 → harus tetap Block karena runtime masih Suspended
        assert!(
            p_after.is_blocked_by(PolicyBlock::ChargeLimit),
            "SOC 99 setelah re-attach harus tetap Block — runtime Suspended harus survive detach"
        );
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);
    }

    #[test]
    fn test_classify_uevent_all_variants() {
        // Test parsing & classification untuk berbagai payload uevent kernel

        // Type-C via subsystem
        let uevent_typec = b"ACTION=change\0SUBSYSTEM=typec\0DEVPATH=/sys/class/typec/port0\0";
        assert_eq!(classify_uevent(uevent_typec), UeventKind::TypeC);

        // Type-C via devpath
        let uevent_typec_path = b"ACTION=change\0SUBSYSTEM=power_supply\0DEVPATH=/devices/platform/soc/typec/power_supply/usb\0";
        assert_eq!(classify_uevent(uevent_typec_path), UeventKind::TypeC);

        // AC power supply
        let uevent_ac = b"ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_NAME=ac\0";
        assert_eq!(classify_uevent(uevent_ac), UeventKind::Ac);

        // USB power supply
        let uevent_usb = b"ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_NAME=usb\0";
        assert_eq!(classify_uevent(uevent_usb), UeventKind::Usb);

        // Charger alias for USB
        let uevent_charger = b"ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_NAME=charger\0";
        assert_eq!(classify_uevent(uevent_charger), UeventKind::Usb);

        // Battery power supply
        let uevent_battery = b"ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_NAME=battery\0";
        assert_eq!(classify_uevent(uevent_battery), UeventKind::Battery);

        // BMS power supply
        let uevent_bms = b"ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_NAME=bms\0";
        assert_eq!(classify_uevent(uevent_bms), UeventKind::Bms);

        // Vendor BMS via devpath
        let uevent_bms_devpath = b"ACTION=change\0DEVPATH=/devices/platform/soc/bms\0";
        assert_eq!(classify_uevent(uevent_bms_devpath), UeventKind::Bms);

        // Unrelated uevent
        let uevent_other =
            b"ACTION=change\0SUBSYSTEM=input\0DEVPATH=/devices/virtual/input/input0\0";
        assert_eq!(classify_uevent(uevent_other), UeventKind::Other);
    }

    #[test]
    fn test_attaching_settling_window_transition() {
        let now = Instant::now();
        let mut conn = ConnectionState::Disconnected;

        // Disconnected -> Plugged in -> Attaching
        conn.update(charger_core::battery::reader::PowerState::Connected, now);
        assert!(matches!(conn, ConnectionState::Attaching { .. }));
        assert!(conn.is_connected());

        // Decision during Attaching should be Wait(AttachingSettleWindow)
        let observed = ObservedState {
            connection: conn,
            power_state: charger_core::battery::reader::PowerState::Connected,
            sample: None,
            timestamp: now,
            sample_retry_at: None,
        };
        let intent = OperatingIntent::normal();
        let policy_res = PolicyResult::clear();
        let dec = ChargingDecision::resolve(&observed, &intent, &policy_res, now);
        assert_eq!(
            dec,
            ChargingDecision::Wait {
                reason: WaitReason::AttachingSettleWindow
            }
        );

        // After 4s (< 5s settle window) -> still Attaching
        let t4 = now + Duration::from_secs(4);
        conn.tick(t4);
        assert!(matches!(conn, ConnectionState::Attaching { .. }));

        // After 5s (settle window elapsed) -> Attached
        let t5 = now + Duration::from_secs(5);
        conn.tick(t5);
        assert_eq!(conn, ConnectionState::Attached);
    }

    #[test]
    fn test_stepped_thermal_regulation_curve() {
        let mut now = Instant::now();
        let mut runtime = PolicyRuntime::default();
        let mut config = Config::default();
        config.thermal_throttling_enabled = true;

        // 1. T = 35.0°C (< 38.0°C) -> Normal
        let step0 = evaluate_thermal_stepping(350, &config, &mut runtime, now);
        assert_eq!(step0, ThermalStep::Normal);
        assert_eq!(step0.target_ua(), None);

        // 2. T = 39.0°C (>= 38.0°C) -> Step 1 (2500 mA)
        now += Duration::from_secs(2);
        let step1 = evaluate_thermal_stepping(390, &config, &mut runtime, now);
        assert_eq!(step1, ThermalStep::Step1);
        assert_eq!(step1.target_ua(), Some(2_500_000));

        // 3. T = 41.5°C (>= 41.0°C) -> Step 2 (1500 mA)
        now += Duration::from_secs(2);
        let step2 = evaluate_thermal_stepping(415, &config, &mut runtime, now);
        assert_eq!(step2, ThermalStep::Step2);
        assert_eq!(step2.target_ua(), Some(1_500_000));

        // 4. T = 43.5°C (>= 43.0°C) -> Step 3 (800 mA)
        now += Duration::from_secs(2);
        let step3 = evaluate_thermal_stepping(435, &config, &mut runtime, now);
        assert_eq!(step3, ThermalStep::Step3);
        assert_eq!(step3.target_ua(), Some(800_000));
    }

    #[test]
    fn test_thermal_step_hold_hysteresis() {
        let mut now = Instant::now();
        let mut runtime = PolicyRuntime::default();
        let config = Config::default();

        // 1. Naik ke Step 2 (41.5°C) -> langsung berubah
        let step = evaluate_thermal_stepping(415, &config, &mut runtime, now);
        assert_eq!(step, ThermalStep::Step2);

        // 2. 3 detik kemudian, suhu turun ke 39.0°C (harusnya Step 1),
        // tapi karena hold window (10s) belum lewat, step tetap di Step 2!
        now += Duration::from_secs(3);
        let step_hold = evaluate_thermal_stepping(390, &config, &mut runtime, now);
        assert_eq!(step_hold, ThermalStep::Step2);

        // 3. 11 detik kemudian, hold window kadaluwarsa -> step turun ke Step 1
        now += Duration::from_secs(8);
        let step_down = evaluate_thermal_stepping(390, &config, &mut runtime, now);
        assert_eq!(step_down, ThermalStep::Step1);
    }

    #[test]
    fn test_current_arbitration_hierarchy() {
        let mut config = Config::default();
        config.max_charge_current_ma = 2000; // User limit 2000 mA
        config.thermal_throttling_enabled = true;
        let mut runtime = PolicyRuntime::default();

        // Case A: Normal Charging + User Limit (2000 mA)
        let dec_allow = ChargingDecision::Allow;
        let reg_a = resolve_current_regulation(&config, &runtime, &dec_allow);
        assert_eq!(
            reg_a,
            CurrentRegulation::ConfigLimit {
                target_ua: 2_000_000
            }
        );

        // Case B: Stepped Thermal Throttle Step 3 (800 mA) < User Limit (2000 mA) -> Thermal wins!
        runtime.thermal_step = ThermalStep::Step3;
        let reg_b = resolve_current_regulation(&config, &runtime, &dec_allow);
        assert_eq!(
            reg_b,
            CurrentRegulation::ThermalThrottle {
                step: 3,
                target_ua: 800_000
            }
        );

        // Case C: User Limit (500 mA) < Thermal Step 1 (2500 mA) -> User Limit wins (more strict)!
        config.max_charge_current_ma = 500;
        runtime.thermal_step = ThermalStep::Step1;
        let reg_c = resolve_current_regulation(&config, &runtime, &dec_allow);
        assert_eq!(
            reg_c,
            CurrentRegulation::ThermalThrottle {
                step: 1,
                target_ua: 500_000
            }
        );

        // Case D: Decision is Block/Wait -> Disabled (0 mA)
        let dec_block = ChargingDecision::Block {
            cause: BlockCause::ThermalEmergency,
        };
        let reg_d = resolve_current_regulation(&config, &runtime, &dec_block);
        assert_eq!(reg_d, CurrentRegulation::Disabled);
    }

    #[test]
    fn test_grace_period_current_cap() {
        let mut config = Config::default();
        config.max_charge_current_ma = 2500; // User set 2.5A
        config.thermal_throttling_enabled = true;
        let mut runtime = PolicyRuntime::default();
        let now = Instant::now();

        // 1. When in Grace period, current MUST be capped to 1000 mA (top-off saturation protection)
        runtime.charge_limit_state = ChargeLimitState::Grace { started_at: now };
        let reg = resolve_current_regulation(&config, &runtime, &ChargingDecision::Allow);
        assert_eq!(
            reg,
            CurrentRegulation::GraceCap {
                target_ua: 1_000_000
            }
        );

        // 2. Collision Test: Grace (1000 mA) + Thermal Step 1 (2500 mA) -> Grace Cap (1000 mA) MUST WIN!
        runtime.thermal_step = ThermalStep::Step1; // 2500 mA
        let reg_step1 = resolve_current_regulation(&config, &runtime, &ChargingDecision::Allow);
        assert_eq!(
            reg_step1,
            CurrentRegulation::GraceCap {
                target_ua: 1_000_000
            },
            "Grace Cap 1000 mA must take precedence over Thermal Step 1 (2500 mA)"
        );

        // 3. Collision Test: Grace (1000 mA) + Thermal Step 3 (800 mA) -> Thermal Step 3 MUST WIN (strictest limit)!
        runtime.thermal_step = ThermalStep::Step3; // 800 mA
        let reg_step3 = resolve_current_regulation(&config, &runtime, &ChargingDecision::Allow);
        assert_eq!(
            reg_step3,
            CurrentRegulation::ThermalThrottle {
                step: 3,
                target_ua: 800_000
            },
            "Thermal Step 3 (800 mA) must take precedence over Grace Cap (1000 mA)"
        );

        // 4. If user set even lower (e.g. 500 mA), user lower limit wins!
        runtime.thermal_step = ThermalStep::Normal;
        config.max_charge_current_ma = 500;
        let reg_low = resolve_current_regulation(&config, &runtime, &ChargingDecision::Allow);
        assert_eq!(reg_low, CurrentRegulation::GraceCap { target_ua: 500_000 });
    }

    #[test]
    fn test_unconstrained_charging_flow() {
        let config = Config::default(); // max_charge_current_ma = 0
        let runtime = PolicyRuntime::default(); // Normal state, Normal thermal
        let reg = resolve_current_regulation(&config, &runtime, &ChargingDecision::Allow);
        assert_eq!(reg, CurrentRegulation::Unconstrained);
        assert_eq!(reg.target_ua(), None);
    }

    #[test]
    fn test_fractional_soc_resume_precision() {
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.charge_limit = 80;
        config.resume_limit = 78;
        let mut runtime = PolicyRuntime::default();

        observed.connection = ConnectionState::Attached;
        runtime.charge_limit_state = ChargeLimitState::Suspended;

        // 1. SOC = 78.05% (still > 78.0%) -> remains Suspended (Block)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 78.05,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        let p1 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(p1.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Suspended);

        // 2. SOC = 78.00% (<= 78.0%) -> transitions to Normal (Allow!)
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 78.00,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        let p2 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p2.is_blocked_by(PolicyBlock::ChargeLimit));
        assert_eq!(runtime.charge_limit_state, ChargeLimitState::Normal);
    }

    #[test]
    fn test_thermal_emergency_full_recovery_cycle() {
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.max_temp_dc = 420; // 42.0 C
        config.thermal_cutoff = true;
        // Emergency offset is +3.0 C = 45.0 C (450 dc)
        // Recovery offset is -4.0 C = 38.0 C (380 dc)

        observed.connection = ConnectionState::Attached;
        let mut runtime = PolicyRuntime::default();

        // 1. T = 44.5 C (< 45.0 C emergency) -> Standard thermal block, but NOT emergency latch
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 44.5,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        let mut p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(p.is_blocked_by(PolicyBlock::Thermal));
        assert!(!p.is_blocked_by(PolicyBlock::ThermalEmergency));

        // 2. T = 45.5 C (>= 45.0 C emergency) -> Emergency latch ACTIVATED!
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 45.5,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(p.is_blocked_by(PolicyBlock::ThermalEmergency));

        // 3. T drops to 40.0 C (> 38.0 C release threshold) -> MUST REMAIN LATCHED
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
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(
            p.is_blocked_by(PolicyBlock::ThermalEmergency),
            "Latch must hold at 40C"
        );

        // 4. T drops to 37.5 C (<= 38.0 C release threshold) -> Latch UNLATCHED!
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 37.5,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        p = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(
            !p.is_blocked_by(PolicyBlock::ThermalEmergency),
            "Latch must release at 37.5C"
        );
        assert!(!p.is_blocked_by(PolicyBlock::Thermal));
    }

    #[test]
    fn test_thermal_cutoff_toggle_behavior() {
        let now = Instant::now();
        let mut observed = ObservedState::new();
        let mut config = Config::default();
        config.max_temp_dc = 400; // 40.0 C
        config.thermal_cutoff = false; // Disabled!

        observed.connection = ConnectionState::Attached;
        let mut runtime = PolicyRuntime::default();

        // 1. T = 41.0 C (> 40.0 C max_temp_dc, but < 43.0 C emergency):
        // Since thermal_cutoff is false, it should NOT block!
        observed.update(
            charger_core::battery::reader::PowerState::Connected,
            Some(Sample {
                capacity: 50.0,
                temperature_c: 41.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            now,
        );
        let p1 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(!p1.is_blocked_by(PolicyBlock::Thermal));
        assert!(!p1.is_blocked_by(PolicyBlock::ThermalEmergency));

        // 2. Enable thermal_cutoff -> now 41.0 C MUST block with PolicyBlock::Thermal!
        config.thermal_cutoff = true;
        let p2 = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(p2.is_blocked_by(PolicyBlock::Thermal));
        assert!(!p2.is_blocked_by(PolicyBlock::ThermalEmergency));
    }

    #[test]
    fn test_intent_disabled_and_bypass_decisions() {
        let now = Instant::now();
        let observed = ObservedState {
            connection: ConnectionState::Attached,
            power_state: charger_core::battery::reader::PowerState::Connected,
            sample: Some(Sample {
                capacity: 50.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Connected,
                timestamp: now,
            }),
            timestamp: now,
            sample_retry_at: None,
        };
        let policy_res = PolicyResult::clear();

        // 1. Intent Disabled -> Block(UserDisabled)
        let intent_disabled = OperatingIntent {
            mode: crate::monitor::intent::IntentMode::Disabled,
            expires_at: None,
        };
        let dec_disabled = ChargingDecision::resolve(&observed, &intent_disabled, &policy_res, now);
        assert_eq!(
            dec_disabled,
            ChargingDecision::Block {
                cause: BlockCause::UserDisabled
            }
        );

        // 2. Intent Bypass -> Bypass
        let intent_bypass = OperatingIntent {
            mode: crate::monitor::intent::IntentMode::Bypass,
            expires_at: None,
        };
        let dec_bypass = ChargingDecision::resolve(&observed, &intent_bypass, &policy_res, now);
        assert_eq!(dec_bypass, ChargingDecision::Bypass);

        // 3. Intent Normal -> Allow
        let intent_normal = OperatingIntent::normal();
        let dec_normal = ChargingDecision::resolve(&observed, &intent_normal, &policy_res, now);
        assert_eq!(dec_normal, ChargingDecision::Allow);
    }

    #[test]
    fn test_sensor_stale_defensive_safety_block() {
        let now = Instant::now();
        let observed = ObservedState {
            connection: ConnectionState::Attached,
            power_state: charger_core::battery::reader::PowerState::Connected,
            sample: None, // No sensor data available
            timestamp: now,
            sample_retry_at: None,
        };
        let config = Config::default();
        let mut runtime = PolicyRuntime::default();

        let policy_res = evaluate_policy(&observed, &config, &mut runtime, now);
        assert!(policy_res.is_blocked_by(PolicyBlock::SensorStale));

        let intent = OperatingIntent::normal();
        let dec = ChargingDecision::resolve(&observed, &intent, &policy_res, now);

        // Sensor stale MUST resolve to Safety Block (SensorStale) to physically disable charging!
        assert_eq!(
            dec,
            ChargingDecision::Block {
                cause: BlockCause::SensorStale
            }
        );
        assert_eq!(
            dec.to_desired_hardware(),
            DesiredHardwareState::ChargingDisabled
        );
        assert_eq!(dec.to_urgency(), Urgency::Recovery);
    }

    #[test]
    fn test_thermal_step_deadline_scheduler_integration() {
        let now = Instant::now();
        let mut runtime = PolicyRuntime::default();

        // 1. Normal -> no deadline
        assert_eq!(runtime.thermal_step_deadline(), None);

        // 2. When throttled to Step 2 -> hold window deadline active
        runtime.thermal_step = ThermalStep::Step2;
        runtime.thermal_step_updated_at = Some(now);
        assert_eq!(
            runtime.thermal_step_deadline(),
            Some(now + Duration::from_secs(10))
        );
    }

    #[test]
    fn test_disconnect_cleans_applied_current_limit() {
        let mut track = HardwareTrack::new();
        track.current_limit.applied_limit_ua = Some(2_000_000);
        track.current_limit.reconcile_needed = false;

        track.reset_on_disconnect();

        // Disconnect cleans in-memory applied limit without physical sysfs write.
        // It resets applied_limit_ua to None and reconcile_needed to true.
        assert_eq!(track.current_limit.applied_limit_ua, None);
        assert!(track.current_limit.reconcile_needed);
        assert_eq!(
            track.current_limit.status,
            crate::monitor::hardware::CurrentLimitStatus::Unknown
        );
    }

    #[test]
    fn test_adaptive_scheduler_suspended_proximity_fix() {
        let now = Instant::now();
        // charge_limit = 80%, resume_limit = 75%, base_interval = 5s
        let mut sched = AdaptiveScheduler::new(Duration::from_secs(5), 80.0, 75.0, 42.0);

        // Case 1: SOC 79% (suspended mode). In old buggy logic, 80 - 79 = 1 <= 5, which caused
        // aggressive 2.5s polling. With fix, it checks 79 - 75 = 4 > 1, so interval is relaxed to 2.0x base (10.0s).
        let sample_far = Sample {
            capacity: 79.0,
            temperature_c: 30.0,
            power_state: charger_core::battery::reader::PowerState::Connected,
            timestamp: now,
        };
        sched.update_sample(&sample_far);
        let interval_far = sched.calculate_next_interval(Urgency::Monitoring, None, now);
        assert_eq!(interval_far, Duration::from_secs(10));

        // Case 2: SOC 75.8% (suspended mode, close to resume_limit 75.0%).
        // 75.8 - 75.0 = 0.8 <= 1.0 -> Accelerate to max(2.0s, 5.0s * 0.5) = 2.5s.
        let sample_close = Sample {
            capacity: 75.8,
            temperature_c: 30.0,
            power_state: charger_core::battery::reader::PowerState::Connected,
            timestamp: now,
        };
        sched.update_sample(&sample_close);
        let interval_close = sched.calculate_next_interval(Urgency::Monitoring, None, now);
        assert_eq!(interval_close, Duration::from_millis(2500));
    }

    #[test]
    fn test_adaptive_scheduler_normal_proximity() {
        let now = Instant::now();
        let mut sched = AdaptiveScheduler::new(Duration::from_secs(5), 80.0, 75.0, 42.0);

        // Case 1: SOC 70% (far from charge_limit 80%).
        // 80 - 70 = 10 > 3.0 -> Standard normal interval (1.0x base = 5.0s).
        let sample_far = Sample {
            capacity: 70.0,
            temperature_c: 30.0,
            power_state: charger_core::battery::reader::PowerState::Connected,
            timestamp: now,
        };
        sched.update_sample(&sample_far);
        let interval_far = sched.calculate_next_interval(Urgency::Normal, None, now);
        assert_eq!(interval_far, Duration::from_secs(5));

        // Case 2: SOC 78% (close to charge_limit 80%).
        // 80 - 78 = 2 <= 3.0 -> Accelerate to max(2.0s, 5.0s * 0.5) = 2.5s.
        let sample_close = Sample {
            capacity: 78.0,
            temperature_c: 30.0,
            power_state: charger_core::battery::reader::PowerState::Connected,
            timestamp: now,
        };
        sched.update_sample(&sample_close);
        let interval_close = sched.calculate_next_interval(Urgency::Normal, None, now);
        assert_eq!(interval_close, Duration::from_millis(2500));
    }

    #[test]
    fn test_adaptive_scheduler_idle_and_safety() {
        let now = Instant::now();
        let sched = AdaptiveScheduler::new(Duration::from_secs(5), 80.0, 75.0, 42.0);

        // Idle: 6.0x base = 30.0s
        let interval_idle = sched.calculate_next_interval(Urgency::Idle, None, now);
        assert_eq!(interval_idle, Duration::from_secs(30));

        // Safety: 2.0s fixed
        let interval_safety = sched.calculate_next_interval(Urgency::Safety, None, now);
        assert_eq!(interval_safety, Duration::from_secs(2));
    }

    #[test]
    fn test_actuator_fault_isolation() {
        let now = Instant::now();
        let mut track = HardwareTrack::new();

        // 1. Charger binary is verified and stable
        track.charger.update_observation(
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled,
            now,
        );
        assert_eq!(
            track.charger.observation.mode,
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled
        );
        assert_eq!(
            track.charger.status,
            crate::monitor::hardware::HardwareStatus::Stable {
                mode: charger_core::battery::control::ActualHardwareMode::ChargingEnabled
            }
        );

        // 2. Current limit fails with CurrentLimitWriteFailed
        track
            .current_limit
            .mark_fault(HardwareFault::CurrentLimitWriteFailed, now);

        // 3. Charger binary observation and status MUST NOT be polluted!
        assert_eq!(
            track.charger.observation.mode,
            charger_core::battery::control::ActualHardwareMode::ChargingEnabled
        );
        assert_eq!(
            track.charger.status,
            crate::monitor::hardware::HardwareStatus::Stable {
                mode: charger_core::battery::control::ActualHardwareMode::ChargingEnabled
            }
        );

        // 4. Current limit is in Fault, charger is Converged -> overall convergence is Fault
        assert_eq!(
            track.charger.convergence(),
            crate::monitor::hardware::ConvergenceState::Converged
        );
        assert_eq!(
            track.current_limit.convergence(),
            crate::monitor::hardware::ConvergenceState::Fault
        );
        assert_eq!(
            track.overall_convergence(),
            crate::monitor::hardware::ConvergenceState::Fault
        );

        // 5. Binary reconciliation MUST NOT be deferred by current limit's fault!
        let opts = crate::monitor::hardware::ReconcileOptions {
            bypass_retry_delay: false,
            force_verification: false,
        };
        let res = crate::monitor::hardware::reconcile(
            DesiredHardwareState::ChargingEnabled,
            &mut track,
            false,
            opts,
            now,
        );
        assert!(matches!(
            res,
            crate::monitor::hardware::ReconcileResult::Stable(_)
        ));
    }

    #[test]
    fn test_actuator_combined_convergence() {
        use crate::monitor::hardware::ConvergenceState;

        assert_eq!(
            ConvergenceState::Converged.combine(ConvergenceState::Converged),
            ConvergenceState::Converged
        );
        assert_eq!(
            ConvergenceState::Converged.combine(ConvergenceState::Deferred),
            ConvergenceState::Deferred
        );
        assert_eq!(
            ConvergenceState::Deferred.combine(ConvergenceState::Reconciling),
            ConvergenceState::Reconciling
        );
        assert_eq!(
            ConvergenceState::Reconciling.combine(ConvergenceState::Fault),
            ConvergenceState::Fault
        );
        assert_eq!(
            ConvergenceState::Fault.combine(ConvergenceState::Converged),
            ConvergenceState::Fault
        );
    }

    #[test]
    fn test_current_limit_fault_respects_retry_deadline() {
        use crate::monitor::decision::{CurrentRegulation, DesiredHardwareState};
        use crate::monitor::hardware::{CurrentReconcileOptions, CurrentReconcileResult};

        let now = Instant::now();
        let mut track = HardwareTrack::new();

        // 1. Current limit write fails at t0 with WriteFailed (retry delay = 5s)
        track
            .current_limit
            .mark_fault(HardwareFault::CurrentLimitWriteFailed, now);

        let target = CurrentRegulation::ConfigLimit {
            target_ua: 1_500_000,
        };
        let default_opts = CurrentReconcileOptions {
            bypass_retry_delay: false,
        };
        let bypass_opts = CurrentReconcileOptions {
            bypass_retry_delay: true,
        };

        // 2. t = now + 1s (before 5s deadline, bypass = false): reconcile_current must return Deferred and not retry sysfs write
        let t1 = now + Duration::from_secs(1);
        let res1 = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            default_opts,
            t1,
        );
        assert!(matches!(res1, CurrentReconcileResult::Deferred));
        assert_eq!(
            track.current_limit.convergence(),
            crate::monitor::hardware::ConvergenceState::Fault
        );

        // 3. t = now + 4.9s (still before deadline): must still return Deferred
        let t4_9 = now + Duration::from_millis(4900);
        let res2 = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            default_opts,
            t4_9,
        );
        assert!(matches!(res2, CurrentReconcileResult::Deferred));

        // 4. t = now + 5.1s (deadline reached): retry is due and write is attempted (fails with NoChargingNodeFound on host)
        let t5_1 = now + Duration::from_millis(5100);
        let res_due = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            default_opts,
            t5_1,
        );
        assert!(!matches!(res_due, CurrentReconcileResult::Deferred));

        // 5. With new fault at t5_1 (NodeMissing with 30s delay), verify bypass_retry_delay = true bypasses delay
        let t6 = t5_1 + Duration::from_secs(1);
        let res_deferred = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            default_opts,
            t6,
        );
        assert!(matches!(res_deferred, CurrentReconcileResult::Deferred));

        let res_force = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            bypass_opts,
            t6,
        );
        assert!(!matches!(res_force, CurrentReconcileResult::Deferred));
    }

    #[test]
    fn test_current_limit_never_retry_always_deferred() {
        use crate::monitor::decision::{CurrentRegulation, DesiredHardwareState};
        use crate::monitor::hardware::{CurrentReconcileOptions, CurrentReconcileResult};

        let now = Instant::now();
        let mut track = HardwareTrack::new();
        // PermissionDenied has FaultRetryPolicy::Never
        track
            .current_limit
            .mark_fault(HardwareFault::PermissionDenied, now);

        let target = CurrentRegulation::ConfigLimit {
            target_ua: 1_500_000,
        };
        let bypass_opts = CurrentReconcileOptions {
            bypass_retry_delay: true,
        };

        // Even with bypass_retry_delay = true, FaultRetryPolicy::Never remains Deferred
        let res = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            bypass_opts,
            now + Duration::from_secs(10),
        );
        assert!(matches!(res, CurrentReconcileResult::Deferred));
    }

    #[test]
    fn test_current_limit_idempotency_cleans_reconcile_needed() {
        use crate::monitor::decision::{CurrentRegulation, DesiredHardwareState};
        use crate::monitor::hardware::{CurrentReconcileOptions, CurrentReconcileResult};

        let now = Instant::now();
        let mut track = HardwareTrack::new();

        // 1. Initially mark_applied as 1.5A
        track.current_limit.mark_applied(Some(1_500_000), true);
        assert_eq!(track.current_limit.applied_limit_ua, Some(1_500_000));

        // 2. An event turns on reconcile_needed = true
        track.current_limit.reconcile_needed = true;

        let target = CurrentRegulation::ConfigLimit {
            target_ua: 1_500_000,
        };
        let opts = CurrentReconcileOptions {
            bypass_retry_delay: false,
        };

        // 3. reconcile_current should immediately return Stable without writing to sysfs, and clean reconcile_needed
        let res = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target,
            &mut track,
            opts,
            now,
        );
        assert!(matches!(
            res,
            CurrentReconcileResult::Stable(Some(1_500_000))
        ));
        assert!(!track.current_limit.reconcile_needed);

        // 4. Test Unconstrained (desired = None)
        track.current_limit.mark_applied(None, true);
        track.current_limit.reconcile_needed = true;

        let target_unconstrained = CurrentRegulation::Unconstrained;
        let res_unconstrained = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::ChargingEnabled,
            target_unconstrained,
            &mut track,
            opts,
            now,
        );
        assert!(matches!(
            res_unconstrained,
            CurrentReconcileResult::Stable(None)
        ));
        assert!(!track.current_limit.reconcile_needed);
    }

    #[test]
    fn test_current_limit_skipped_on_no_change() {
        use crate::monitor::decision::{CurrentRegulation, DesiredHardwareState};
        use crate::monitor::hardware::{CurrentReconcileOptions, CurrentReconcileResult};

        let now = Instant::now();
        let mut track = HardwareTrack::new();
        track.current_limit.reconcile_needed = true;

        let target = CurrentRegulation::Disabled;
        let opts = CurrentReconcileOptions::default();

        // When desired_hw is NoChange (e.g. Disconnected), reconcile_current must return Skipped without touching sysfs
        let res = crate::monitor::hardware::reconcile_current(
            DesiredHardwareState::NoChange,
            target,
            &mut track,
            opts,
            now,
        );
        assert!(matches!(res, CurrentReconcileResult::Skipped));
        assert!(track.current_limit.reconcile_needed);
    }

    #[test]
    fn test_fast_charge_policy_under_90_percent_active() {
        use crate::monitor::policy::{evaluate_fast_charge_policy, FastChargePolicy};

        let now = Instant::now();
        let mut observed = ObservedState::new();
        observed.connection = ConnectionState::Attached;
        observed.update(
            charger_core::battery::reader::PowerState::Charging,
            Some(Sample {
                capacity: 85.0,
                temperature_c: 32.0,
                power_state: charger_core::battery::reader::PowerState::Charging,
                timestamp: now,
            }),
            now,
        );

        let mut config = Config::default();
        config.fast_charge = true;
        config.fast_charge_max_soc = 90;

        let runtime = PolicyRuntime::default();
        let policy_res = PolicyResult::clear();

        let fc_pol = evaluate_fast_charge_policy(&observed, &config, &runtime, &policy_res, now);
        assert_eq!(
            fc_pol,
            FastChargePolicy::Active {
                target_ua: 5_850_000
            }
        );
        assert!(fc_pol.is_active());
    }

    #[test]
    fn test_fast_charge_policy_at_or_above_90_percent_suppressed() {
        use crate::monitor::policy::{evaluate_fast_charge_policy, FastChargePolicy};

        let now = Instant::now();
        let mut observed = ObservedState::new();
        observed.connection = ConnectionState::Attached;
        observed.update(
            charger_core::battery::reader::PowerState::Charging,
            Some(Sample {
                capacity: 90.5,
                temperature_c: 32.0,
                power_state: charger_core::battery::reader::PowerState::Charging,
                timestamp: now,
            }),
            now,
        );

        let mut config = Config::default();
        config.fast_charge = true;
        config.fast_charge_max_soc = 90;

        let runtime = PolicyRuntime::default();
        let policy_res = PolicyResult::clear();

        let fc_pol = evaluate_fast_charge_policy(&observed, &config, &runtime, &policy_res, now);
        assert_eq!(
            fc_pol,
            FastChargePolicy::SuppressedSocLimit {
                current_soc: 90.5,
                max_soc: 90
            }
        );
        assert!(!fc_pol.is_active());
    }

    #[test]
    fn test_fast_charge_policy_suppressed_on_thermal_or_charge_limit() {
        use crate::monitor::policy::{evaluate_fast_charge_policy, FastChargePolicy};

        let now = Instant::now();
        let mut observed = ObservedState::new();
        observed.connection = ConnectionState::Attached;
        observed.update(
            charger_core::battery::reader::PowerState::Charging,
            Some(Sample {
                capacity: 75.0,
                temperature_c: 42.0,
                power_state: charger_core::battery::reader::PowerState::Charging,
                timestamp: now,
            }),
            now,
        );

        let mut config = Config::default();
        config.fast_charge = true;
        config.fast_charge_max_soc = 90;

        let mut runtime = PolicyRuntime::default();
        runtime.thermal_step = ThermalStep::Step2; // Throttling active
        let policy_res = PolicyResult::clear();

        let fc_pol = evaluate_fast_charge_policy(&observed, &config, &runtime, &policy_res, now);
        assert_eq!(fc_pol, FastChargePolicy::SuppressedThermal);
    }

    #[test]
    fn test_fast_charge_policy_respects_user_max_current() {
        use crate::monitor::policy::{evaluate_fast_charge_policy, FastChargePolicy};

        let now = Instant::now();
        let mut observed = ObservedState::new();
        observed.connection = ConnectionState::Attached;
        observed.update(
            charger_core::battery::reader::PowerState::Charging,
            Some(Sample {
                capacity: 60.0,
                temperature_c: 30.0,
                power_state: charger_core::battery::reader::PowerState::Charging,
                timestamp: now,
            }),
            now,
        );

        let mut config = Config::default();
        config.fast_charge = true;
        config.fast_charge_max_soc = 90;
        config.max_charge_current_ma = 2000; // User sets limit to 2000 mA

        let runtime = PolicyRuntime::default();
        let policy_res = PolicyResult::clear();

        let fc_pol = evaluate_fast_charge_policy(&observed, &config, &runtime, &policy_res, now);
        assert_eq!(
            fc_pol,
            FastChargePolicy::Active {
                target_ua: 2_000_000
            }
        );
    }

    #[test]
    fn test_safe_hardware_state_invariants() {
        use crate::monitor::hardware::SafeHardwareState;

        let safe = SafeHardwareState::new();
        assert_eq!(
            safe.charge_path,
            charger_core::battery::control::ActualHardwareMode::ChargingDisabled
        );
        assert_eq!(safe.current_limit_ua, Some(500_000));
        assert!(!safe.fast_charge);
    }

    #[test]
    fn test_connection_state_predicates() {
        let now = Instant::now();

        let disconnected = ConnectionState::Disconnected;
        assert!(!disconnected.is_present());
        assert!(!disconnected.is_stable());
        assert!(!disconnected.is_operational());

        let attaching = ConnectionState::Attaching { since: now };
        assert!(attaching.is_present());
        assert!(!attaching.is_stable());
        assert!(!attaching.is_operational());

        let attached = ConnectionState::Attached;
        assert!(attached.is_present());
        assert!(attached.is_stable());
        assert!(attached.is_operational());
    }

    #[test]
    fn test_current_limit_verified_vs_commanded() {
        use crate::monitor::hardware::{ConvergenceState, CurrentLimitStatus};

        let mut track = HardwareTrack::new();
        assert_eq!(track.current_limit.status, CurrentLimitStatus::Unknown);

        // Mark Commanded
        track.current_limit.mark_applied(Some(1_500_000), false);
        assert!(matches!(
            track.current_limit.status,
            CurrentLimitStatus::Commanded {
                applied_ua: Some(1_500_000)
            }
        ));
        assert_eq!(track.current_limit.convergence(), ConvergenceState::Converged);

        // Mark Verified
        track.current_limit.mark_applied(Some(2_000_000), true);
        assert!(matches!(
            track.current_limit.status,
            CurrentLimitStatus::Verified {
                applied_ua: Some(2_000_000)
            }
        ));
        assert_eq!(track.current_limit.convergence(), ConvergenceState::Converged);
    }

    #[test]
    fn test_actual_hardware_mode_truth_variants() {
        use charger_core::battery::control::ActualHardwareMode;

        let enabled = ActualHardwareMode::ChargingEnabled;
        assert!(enabled.is_known());
        assert!(enabled.is_charging_enabled());
        assert!(!enabled.is_charging_disabled());

        let disabled = ActualHardwareMode::ChargingDisabled;
        assert!(disabled.is_known());
        assert!(!disabled.is_charging_enabled());
        assert!(disabled.is_charging_disabled());

        let inconsistent = ActualHardwareMode::Inconsistent;
        assert!(!inconsistent.is_known());

        let unknown = ActualHardwareMode::Unknown;
        assert!(!unknown.is_known());
    }
}

