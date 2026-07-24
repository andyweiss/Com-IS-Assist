use com_is_assist_core::automix::{AutomixEngine, AutomixEngineConfig, GainComputerConfig};
use com_is_assist_core::ratio::RatioResult;

const TICK_SECONDS: f64 = 0.1;

fn fast_engine() -> AutomixEngine {
    // Fast attack/release so tests converge in a small, bounded number of ticks.
    AutomixEngine::new(
        AutomixEngineConfig::default(),
        GainComputerConfig {
            attack_seconds: 0.05,
            hold_seconds: 0.0,
            release_seconds: 0.2,
            max_rate_db_per_s: None,
        },
        TICK_SECONDS,
    )
}

#[test]
fn holds_at_unity_when_ratio_is_within_the_tolerance_band() {
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();

    let mut result = engine.process_tick(RatioResult {
        ratio_lu: config.target_ratio_lu,
        valid: true,
        com_currently_silent: false,
    });
    for _ in 0..50 {
        result = engine.process_tick(RatioResult {
            ratio_lu: config.target_ratio_lu,
            valid: true,
            com_currently_silent: false,
        });
    }

    assert_eq!(result.gain_reduction_db, 0.0);
    assert_eq!(result.gain_linear, 1.0);
}

#[test]
fn increases_reduction_when_ratio_is_below_the_lower_limit() {
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();
    let com_lo_lim = config.target_ratio_lu - config.min_tolerance_lu;
    let very_low_ratio = com_lo_lim - 10.0;

    let mut last = 0.0;
    for _ in 0..100 {
        let result = engine.process_tick(RatioResult {
            ratio_lu: very_low_ratio,
            valid: true,
            com_currently_silent: false,
        });
        assert!(
            result.gain_reduction_db >= last - 1e-9,
            "reduction should never decrease while ratio stays below the lower limit"
        );
        last = result.gain_reduction_db;
    }

    assert!(last > 5.0, "expected meaningful reduction, got {last}dB");
}

#[test]
fn releases_back_toward_unity_when_ratio_is_above_the_upper_limit() {
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();
    let com_lo_lim = config.target_ratio_lu - config.min_tolerance_lu;
    let com_hi_lim = config.target_ratio_lu + config.max_tolerance_lu;

    // Build up some reduction first.
    for _ in 0..100 {
        engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 10.0,
            valid: true,
            com_currently_silent: false,
        });
    }
    let reduced = engine.current_gain_reduction_db();
    assert!(reduced > 1.0);

    // Now ratio is comfortably above the upper limit: reduction should ease back down.
    for _ in 0..100 {
        engine.process_tick(RatioResult {
            ratio_lu: com_hi_lim + 10.0,
            valid: true,
            com_currently_silent: false,
        });
    }
    assert!(
        engine.current_gain_reduction_db() < reduced - 1.0,
        "expected reduction to release, was {reduced}dB, now {}dB",
        engine.current_gain_reduction_db()
    );
}

#[test]
fn invalid_ratio_holds_the_last_gain() {
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();
    let com_lo_lim = config.target_ratio_lu - config.min_tolerance_lu;

    for _ in 0..100 {
        engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 10.0,
            valid: true,
            com_currently_silent: false,
        });
    }
    let converged = engine.current_gain_reduction_db();

    for _ in 0..50 {
        let result = engine.process_tick(RatioResult {
            ratio_lu: -100.0, // would otherwise look like an even bigger shortfall
            valid: false,
            com_currently_silent: true,
        });
        assert_eq!(result.gain_reduction_db, converged);
    }
}

#[test]
fn currently_silent_holds_the_last_gain_even_when_valid_latch_is_still_true() {
    // Regression test for the bug found while listening-testing M2: `valid` is a one-way latch
    // (never reverts once true), so during a long silence after the first-ever speech burst it
    // stays true while `ratio_lu` is a frozen, increasingly stale number. AutomixEngine must not
    // react to it - it should hold, the same as the `!valid` case.
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();
    let com_lo_lim = config.target_ratio_lu - config.min_tolerance_lu;

    for _ in 0..100 {
        engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 10.0,
            valid: true,
            com_currently_silent: false,
        });
    }
    let converged = engine.current_gain_reduction_db();

    for _ in 0..50 {
        let result = engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 1000.0, // a stale, wildly-out-of-range held value
            valid: true,                  // latch is still true...
            com_currently_silent: true,   // ...but COM has no signal right now
        });
        assert_eq!(result.gain_reduction_db, converged);
    }
}

#[test]
fn gain_reduction_never_exceeds_the_configured_maximum() {
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();
    let com_lo_lim = config.target_ratio_lu - config.min_tolerance_lu;

    for _ in 0..500 {
        let result = engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 1000.0, // absurdly large, persistent shortfall
            valid: true,
            com_currently_silent: false,
        });
        assert!(result.gain_reduction_db <= config.max_gain_reduction_db + 1e-9);
    }
}

#[test]
fn set_config_raises_the_ceiling_for_a_wrapper_that_only_reads_params_once_at_construction() {
    // Regression test: a host raising `max-gain-reduction-db` past its 24dB default (e.g. to
    // 40dB) mid-session had no effect, because VST3's `initialize()` only ever read the
    // parameter once, at construction - `AutomixEngine` kept using the config it was built with
    // forever after. `set_config` is what a wrapper now calls every tick to keep it current.
    let mut engine = fast_engine();
    let config = AutomixEngineConfig::default();
    let com_lo_lim = config.target_ratio_lu - config.min_tolerance_lu;

    for _ in 0..500 {
        engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 1000.0,
            valid: true,
            com_currently_silent: false,
        });
    }
    assert!((engine.current_gain_reduction_db() - config.max_gain_reduction_db).abs() < 1e-6);

    let raised_config = AutomixEngineConfig {
        max_gain_reduction_db: 40.0,
        ..config
    };
    engine.set_config(raised_config, GainComputerConfig {
        attack_seconds: 0.05,
        hold_seconds: 0.0,
        release_seconds: 0.2,
        max_rate_db_per_s: None,
    });

    for _ in 0..500 {
        engine.process_tick(RatioResult {
            ratio_lu: com_lo_lim - 1000.0,
            valid: true,
            com_currently_silent: false,
        });
    }
    assert!(
        (engine.current_gain_reduction_db() - 40.0).abs() < 1e-6,
        "expected the raised 40dB ceiling to take effect, got {}dB",
        engine.current_gain_reduction_db()
    );
}

#[test]
fn seed_target_reduction_db_lets_a_vad_onset_trigger_an_immediate_attack() {
    let mut engine = fast_engine();
    engine.seed_target_reduction_db(6.0);

    // No valid ratio ticks needed - the envelope should still chase the seeded target.
    for _ in 0..50 {
        engine.process_tick(RatioResult {
            ratio_lu: 0.0,
            valid: false,
            com_currently_silent: true,
        });
    }

    assert!(engine.current_gain_reduction_db() > 5.0);
}
