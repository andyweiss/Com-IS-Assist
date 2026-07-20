use com_is_assist_core::automix::{EnvelopeDetector, EnvelopeDetectorConfig};

fn approx_eq(a: f64, b: f64, margin: f64) -> bool {
    (a - b).abs() <= margin
}

#[test]
fn attacks_faster_than_it_releases() {
    let mut detector = EnvelopeDetector::new(
        EnvelopeDetectorConfig {
            attack_seconds: 0.1,
            release_seconds: 2.0,
        },
        10.0, // 10 ticks/s, i.e. 100ms ticks
    );

    // Attack: step from 0 to 10, count ticks to get within 1.0 of the target.
    let mut attack_ticks = 0;
    while detector.value() < 9.0 {
        detector.process(10.0);
        attack_ticks += 1;
        assert!(attack_ticks < 100, "attack never converged");
    }

    // Release: step back down to 0, count ticks to get within 1.0 of the target.
    let mut release_ticks = 0;
    while detector.value() > 1.0 {
        detector.process(0.0);
        release_ticks += 1;
        assert!(release_ticks < 1000, "release never converged");
    }

    assert!(
        release_ticks > attack_ticks * 5,
        "release ({release_ticks} ticks) should be much slower than attack ({attack_ticks} ticks)"
    );
}

#[test]
fn converges_to_a_steady_target() {
    let mut detector = EnvelopeDetector::new(
        EnvelopeDetectorConfig {
            attack_seconds: 0.2,
            release_seconds: 0.2,
        },
        10.0,
    );

    for _ in 0..200 {
        detector.process(6.0);
    }

    assert!(approx_eq(detector.value(), 6.0, 0.01));
}

#[test]
fn reset_sets_the_current_value_immediately() {
    let mut detector = EnvelopeDetector::new(
        EnvelopeDetectorConfig {
            attack_seconds: 1.0,
            release_seconds: 1.0,
        },
        10.0,
    );
    detector.reset(12.0);
    assert!(approx_eq(detector.value(), 12.0, 1e-9));
}
