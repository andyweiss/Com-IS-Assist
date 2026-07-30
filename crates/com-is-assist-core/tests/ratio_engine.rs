use com_is_assist_core::ratio::{RatioEngine, RatioEngineConfig};

fn approx_eq(a: f64, b: f64, margin: f64) -> bool {
    (a - b).abs() <= margin
}

/// A larger-than-default hold-ticks count, set explicitly rather than relying on
/// `RatioEngineConfig::default()`'s own value - so these tests stay meaningful (and don't
/// silently start passing trivially) regardless of whatever that default is tuned to later.
fn engine_with_hold_ticks(hold_ticks: i32) -> RatioEngine {
    RatioEngine::new(RatioEngineConfig { valid_signal_hold_ticks: hold_ticks })
}

#[test]
fn holds_ratio_invalid_until_enough_consecutive_valid_ticks() {
    let mut engine = engine_with_hold_ticks(5);

    let mut result;
    for _ in 0..4 {
        result = engine.update(-25.0, -20.0, true); // voice_active this tick
        assert!(!result.valid);
    }

    result = engine.update(-25.0, -20.0, true); // 5th consecutive voice-active tick
    assert!(result.valid);
    assert!(approx_eq(result.ratio_lu, 5.0, 1e-9)); // -20 - (-25)
}

#[test]
fn holds_the_last_ratio_when_voice_activity_stops() {
    let mut engine = engine_with_hold_ticks(5);

    for _ in 0..4 {
        engine.update(-25.0, -20.0, true);
    }
    let valid_result = engine.update(-25.0, -20.0, true);
    assert!(valid_result.valid);
    assert!(approx_eq(valid_result.ratio_lu, 5.0, 1e-9));

    // Voice activity stops: ratio should hold, not chase toward whatever COM's loudness reads.
    let held_result = engine.update(-25.0, -90.0, false);
    assert!(held_result.valid);
    assert!(approx_eq(held_result.ratio_lu, 5.0, 1e-9));
}

#[test]
fn reset_returns_to_the_invalid_state() {
    let mut engine = engine_with_hold_ticks(5);
    for _ in 0..5 {
        engine.update(-25.0, -20.0, true);
    }
    engine.reset();

    let result = engine.update(-25.0, -20.0, true);
    assert!(!result.valid);
}
