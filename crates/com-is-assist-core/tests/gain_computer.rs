use com_is_assist_core::automix::{GainComputer, GainComputerConfig};

const TICK_SECONDS: f64 = 0.1; // 10 ticks/s, matches the project's 100ms measurement tick

#[test]
fn attacks_faster_than_it_releases() {
    let mut gc = GainComputer::new(
        GainComputerConfig {
            attack_seconds: 0.05,
            hold_seconds: 0.0,
            release_seconds: 2.0,
            max_rate_db_per_s: None,
        },
        TICK_SECONDS,
    );

    let mut attack_ticks = 0;
    while gc.value() < 9.0 {
        gc.process_tick(10.0);
        attack_ticks += 1;
        assert!(attack_ticks < 100, "attack never converged");
    }

    let mut release_ticks = 0;
    while gc.value() > 1.0 {
        gc.process_tick(0.0);
        release_ticks += 1;
        assert!(release_ticks < 1000, "release never converged");
    }

    assert!(
        release_ticks > attack_ticks * 5,
        "release ({release_ticks} ticks) should be much slower than attack ({attack_ticks} ticks)"
    );
}

#[test]
fn hold_timer_blocks_release_until_it_expires() {
    let hold_seconds = 0.5; // 5 ticks at TICK_SECONDS=0.1
    let mut gc = GainComputer::new(
        GainComputerConfig {
            attack_seconds: 0.02,
            hold_seconds,
            release_seconds: 2.0,
            max_rate_db_per_s: None,
        },
        TICK_SECONDS,
    );

    // Attack up to near the target.
    for _ in 0..30 {
        gc.process_tick(10.0);
    }
    assert!(gc.value() > 9.9);
    let value_at_release_start = gc.value();

    // Drop the target to 0: for the hold duration, output must not decrease.
    let hold_ticks = (hold_seconds / TICK_SECONDS).round() as i32;
    for tick in 0..hold_ticks {
        gc.process_tick(0.0);
        assert!(
            gc.value() >= value_at_release_start - 1e-9,
            "should not release during hold (tick {tick})"
        );
    }

    // After the hold expires, it should start releasing.
    for _ in 0..100 {
        gc.process_tick(0.0);
    }
    assert!(
        gc.value() < value_at_release_start - 1.0,
        "should release after hold expires"
    );
}

#[test]
fn rate_limiter_caps_the_change_per_tick() {
    let max_rate_db_per_s = 6.0;
    let mut gc = GainComputer::new(
        GainComputerConfig {
            attack_seconds: 0.001, // near-instant, to isolate the rate limiter's effect
            hold_seconds: 0.0,
            release_seconds: 0.001,
            max_rate_db_per_s: Some(max_rate_db_per_s),
        },
        TICK_SECONDS,
    );

    let max_delta = max_rate_db_per_s * TICK_SECONDS;
    let first = gc.process_tick(20.0);
    assert!(
        first <= max_delta + 1e-9,
        "expected the first tick to be capped to {max_delta}dB, got {first}"
    );
}

#[test]
fn reset_sets_the_value_immediately_with_no_pending_hold() {
    let mut gc = GainComputer::new(GainComputerConfig::default(), TICK_SECONDS);
    for _ in 0..10 {
        gc.process_tick(10.0);
    }
    gc.reset(0.0);
    assert_eq!(gc.value(), 0.0);
    // Immediately after reset, a lower target should be free to release (no leftover hold).
    let next = gc.process_tick(0.0);
    assert_eq!(next, 0.0);
}
