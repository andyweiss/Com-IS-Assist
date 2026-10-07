use com_is_assist_core::automix::EnvelopeDetector;

const DT: f64 = 0.1;

#[test]
fn attacks_faster_than_it_releases() {
    let attack = 0.1;
    let release = 2.0;

    let mut rising = EnvelopeDetector::new(0.0);
    rising.process(1.0, attack, release, DT);

    let mut falling = EnvelopeDetector::new(1.0);
    falling.process(0.0, attack, release, DT);

    // After one identical step, the rising envelope should have covered far more of its distance.
    assert!(
        rising.value() > 1.0 - falling.value(),
        "attack covered {:.3}, release covered {:.3}",
        rising.value(),
        1.0 - falling.value()
    );
}

#[test]
fn converges_to_a_steady_target() {
    let mut detector = EnvelopeDetector::new(0.0);
    for _ in 0..500 {
        detector.process(6.0, 0.05, 0.05, DT);
    }
    assert!((detector.value() - 6.0).abs() < 1e-6, "got {}", detector.value());
}

#[test]
fn a_zero_time_constant_jumps_immediately() {
    let mut detector = EnvelopeDetector::new(0.0);
    assert_eq!(detector.process(5.0, 0.0, 0.0, DT), 5.0);
}

/// The same elapsed time must give the same result regardless of how it is subdivided - the fast
/// loop is stepped at whatever sub-chunk size the host's block size happens to produce.
#[test]
fn stepping_is_independent_of_how_the_interval_is_subdivided() {
    let mut coarse = EnvelopeDetector::new(0.0);
    coarse.process(1.0, 0.5, 0.5, 0.1);

    let mut fine = EnvelopeDetector::new(0.0);
    for _ in 0..10 {
        fine.process(1.0, 0.5, 0.5, 0.01);
    }

    assert!(
        (coarse.value() - fine.value()).abs() < 1e-9,
        "coarse {} vs fine {}",
        coarse.value(),
        fine.value()
    );
}

#[test]
fn reset_sets_the_current_value_immediately() {
    let mut detector = EnvelopeDetector::new(0.0);
    detector.process(10.0, 1.0, 1.0, DT);
    detector.reset(2.5);
    assert_eq!(detector.value(), 2.5);
}
