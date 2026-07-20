use com_is_assist_core::ratio::RatioEngine;

fn approx_eq(a: f64, b: f64, margin: f64) -> bool {
    (a - b).abs() <= margin
}

#[test]
fn holds_ratio_invalid_until_enough_consecutive_valid_ticks() {
    let mut engine = RatioEngine::default(); // defaults: floor -70dB, hold 20 ticks

    let mut result;
    for _ in 0..19 {
        result = engine.update(-25.0, -20.0); // comDb=-20 is above the floor
        assert!(!result.valid);
    }

    result = engine.update(-25.0, -20.0); // 20th consecutive valid tick
    assert!(result.valid);
    assert!(approx_eq(result.ratio_lu, 5.0, 1e-9)); // -20 - (-25)
}

#[test]
fn holds_the_last_ratio_when_the_signal_drops_below_the_floor() {
    let mut engine = RatioEngine::default();

    for _ in 0..19 {
        engine.update(-25.0, -20.0);
    }
    let valid_result = engine.update(-25.0, -20.0);
    assert!(valid_result.valid);
    assert!(approx_eq(valid_result.ratio_lu, 5.0, 1e-9));

    // Signal drops below the floor: ratio should hold, not chase toward -inf.
    let held_result = engine.update(-25.0, -90.0);
    assert!(held_result.valid);
    assert!(approx_eq(held_result.ratio_lu, 5.0, 1e-9));
}

#[test]
fn reset_returns_to_the_invalid_state() {
    let mut engine = RatioEngine::default();
    for _ in 0..20 {
        engine.update(-25.0, -20.0);
    }
    engine.reset();

    let result = engine.update(-25.0, -20.0);
    assert!(!result.valid);
}
