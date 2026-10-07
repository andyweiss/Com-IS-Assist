use com_is_assist_core::automix::{AutomixEngine, AutomixEngineConfig, LoudnessSnapshot, MixState};

const TICK: f64 = 0.1;

fn engine_with(config: AutomixEngineConfig) -> AutomixEngine {
    AutomixEngine::new(config)
}

/// A dry-signal snapshot where both Bed windows agree - the steady-state case.
fn snapshot(bed_lufs: f64, dialogue_lufs: f64) -> LoudnessSnapshot {
    LoudnessSnapshot {
        bed_short_term_lufs: bed_lufs,
        bed_momentary_lufs: bed_lufs,
        dialogue_short_term_lufs: dialogue_lufs,
    }
}

/// Runs `ticks` control steps and returns the final reduction.
fn run(engine: &mut AutomixEngine, ticks: usize, snap: LoudnessSnapshot, com: bool, is: bool) -> f64 {
    let mut last = 0.0;
    for _ in 0..ticks {
        last = engine.process_tick(snap, com, is, TICK).gain_reduction_db;
    }
    last
}

// ---------------------------------------------------------------------------
// State selection (unchanged behaviour - the state machine survives the redesign intact)
// ---------------------------------------------------------------------------

#[test]
fn each_vad_combination_selects_the_expected_state() {
    let config = AutomixEngineConfig::default();
    let snap = snapshot(-20.0, -20.0);

    let cases = [
        (false, false, MixState::ReleaseToUnity),
        (true, false, MixState::DuckToTarget),
        (false, true, MixState::InterviewPassthrough),
        (true, true, MixState::DuckToOvervoice),
    ];
    for (com, is, expected) in cases {
        let mut engine = engine_with(config);
        assert_eq!(engine.process_tick(snap, com, is, TICK).state, expected);
    }
}

// ---------------------------------------------------------------------------
// Feed-forward exactness - the property the whole redesign exists to obtain
// ---------------------------------------------------------------------------

/// Under the old feedback topology this test was impossible to write: the measured ratio already
/// contained the applied reduction, so there was no independent "correct answer" to compare
/// against. Feed-forward from the dry signals makes the required reduction exactly
/// `target - (COM - BED)`, so the settled value is predictable in advance.
#[test]
fn settles_on_exactly_the_reduction_the_dry_levels_imply() {
    let config = AutomixEngineConfig::default();
    let mut engine = engine_with(config);

    // Bed at -20, COM at -26: COM is 6 LU *below* Bed, target is +3, so 9 dB is required.
    let snap = snapshot(-20.0, -26.0);
    let settled = run(&mut engine, 400, snap, true, false);

    assert!(
        (settled - 9.0).abs() < 0.1,
        "expected exactly 9 dB (target 3 - ratio -6), got {settled}dB"
    );
}

#[test]
fn over_voice_settles_on_its_own_higher_target() {
    let config = AutomixEngineConfig::default();
    let snap = snapshot(-20.0, -26.0); // ratio -6

    let mut normal = engine_with(config);
    let normal_db = run(&mut normal, 400, snap, true, false);

    let mut over = engine_with(config);
    let over_db = run(&mut over, 400, snap, true, true);

    assert!((normal_db - (config.target_ratio_lu + 6.0)).abs() < 0.1, "got {normal_db}dB");
    assert!((over_db - (config.overvoice_ratio_lu + 6.0)).abs() < 0.1, "got {over_db}dB");
    assert!(over_db > normal_db + 1.0, "over-voice must duck harder");
}

#[test]
fn never_boosts_when_com_is_already_above_target() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    // COM 10 LU above Bed, target 3 - no reduction is warranted, and automix never boosts.
    let settled = run(&mut engine, 400, snapshot(-30.0, -20.0), true, false);
    assert_eq!(settled, 0.0);
}

#[test]
fn gain_reduction_never_exceeds_the_configured_maximum() {
    let config = AutomixEngineConfig::default();
    let mut engine = engine_with(config);
    let settled = run(&mut engine, 2000, snapshot(-10.0, -90.0), true, false);
    assert!(settled <= config.max_gain_reduction_db + 1e-9, "got {settled}dB");
}

/// A reading at the silence sentinel is a placeholder, not a level. Acting on it would ask for an
/// enormous reduction on the strength of a number that means "no data".
#[test]
fn a_silence_sentinel_reading_holds_rather_than_driving_the_gain() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let established = run(&mut engine, 400, snapshot(-20.0, -26.0), true, false);
    assert!(established > 1.0);

    let silent = snapshot(-20.0, -100.0);
    let after = run(&mut engine, 200, silent, true, false);
    assert!(
        (after - established).abs() < 0.01,
        "sentinel reading moved the gain from {established}dB to {after}dB"
    );
}

// ---------------------------------------------------------------------------
// Multi-loop behaviour
// ---------------------------------------------------------------------------

/// The mid stage may only trim the slow stage within its authority, so a momentary window that
/// disagrees wildly with the short-term one cannot drag the total far.
#[test]
fn the_mid_stage_authority_bounds_how_far_momentary_can_pull_the_total() {
    let config = AutomixEngineConfig::default();
    let mut engine = engine_with(config);

    // The Bed's short-term window says 9 dB is needed; its momentary window sees a far louder Bed
    // (a surge) and would ask for much more.
    let conflicting = LoudnessSnapshot {
        bed_short_term_lufs: -20.0,
        bed_momentary_lufs: 10.0,
        dialogue_short_term_lufs: -26.0,
    };
    let settled = run(&mut engine, 600, conflicting, true, false);
    let contributions = engine.contributions();

    assert!(contributions.mid_db.abs() <= 6.0 + 1e-6, "mid stage exceeded its authority: {contributions:?}");
    assert!(
        settled <= 9.0 + 6.0 + 1e-6,
        "total {settled}dB exceeds slow estimate plus mid authority"
    );
}

/// The Speed macro is the only timing control left, so it has to do something measurable.
///
/// Measured early (200ms) deliberately: by ~800ms both settings have essentially arrived, because
/// the mid stage rushes in to cover whatever the slow stage has not yet done. That masking is the
/// cascade working as intended - it is also why "how fast is it?" has to be asked of the first few
/// hundred milliseconds rather than of the settled value.
#[test]
fn a_higher_speed_setting_reacts_faster() {
    let snap = snapshot(-20.0, -26.0);
    let mut slow = engine_with(AutomixEngineConfig { speed: 0.5, ..Default::default() });
    let mut fast = engine_with(AutomixEngineConfig { speed: 4.0, ..Default::default() });

    let slow_db = run(&mut slow, 2, snap, true, false);
    let fast_db = run(&mut fast, 2, snap, true, false);

    assert!(
        fast_db > slow_db * 1.5,
        "after 200ms speed 4.0 reached {fast_db}dB where speed 0.5 reached {slow_db}dB"
    );
}

/// Both settings must still converge on the same place - Speed changes how quickly the mixer gets
/// there, never where "there" is. (Feed-forward is what makes that separation clean.)
#[test]
fn speed_changes_the_journey_not_the_destination() {
    let snap = snapshot(-20.0, -26.0);
    let mut slow = engine_with(AutomixEngineConfig { speed: 0.5, ..Default::default() });
    let mut fast = engine_with(AutomixEngineConfig { speed: 4.0, ..Default::default() });

    let slow_db = run(&mut slow, 600, snap, true, false);
    let fast_db = run(&mut fast, 600, snap, true, false);

    assert!((slow_db - fast_db).abs() < 0.1, "settled at {slow_db}dB vs {fast_db}dB");
    assert!((slow_db - 9.0).abs() < 0.1, "both should land on the dry-level answer, got {slow_db}dB");
}

/// Adaptive ballistics: the fast stage must move long before the slow stage has, which is the
/// whole justification for a cascade rather than one envelope.
#[test]
fn the_fast_stage_responds_before_the_slow_stage_has_moved() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    // Establish the ducking state without letting the slow stage build anything yet.
    engine.process_tick(snapshot(-20.0, -20.0), true, false, TICK);

    // 20ms of fast-stage stepping against dry levels needing a large reduction.
    for _ in 0..15 {
        engine.process_fast(-20.0, -32.0, 0.0013);
    }

    let contributions = engine.contributions();
    assert!(
        contributions.fast_db > 0.5,
        "fast stage should have moved within 20ms, got {contributions:?}"
    );
    assert!(
        contributions.fast_db <= 6.0 + 1e-6,
        "fast stage exceeded its authority: {contributions:?}"
    );
}

// ---------------------------------------------------------------------------
// Release behaviour
// ---------------------------------------------------------------------------

#[test]
fn no_voice_anywhere_releases_toward_unity() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let established = run(&mut engine, 400, snapshot(-20.0, -26.0), true, false);
    assert!(established > 1.0);

    let released = run(&mut engine, 400, snapshot(-20.0, -100.0), false, false);
    assert!(released < 1.0, "expected release toward unity, was {established}dB, now {released}dB");
}

/// Short gaps must not swell the Bed: measured commentary pauses run ~1.2s. Recovery is no longer
/// blocked outright (a hard freeze put an audible corner in the gain curve - see
/// `LoopBank::HOLD_RELEASE_STRETCH`); it simply begins far too slowly to hear.
#[test]
fn a_short_gap_recovers_only_negligibly() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let established = run(&mut engine, 400, snapshot(-20.0, -26.0), true, false);

    // 500ms of silence.
    let after_gap = run(&mut engine, 5, snapshot(-20.0, -100.0), false, false);
    let recovered_fraction = (established - after_gap) / established;
    assert!(
        recovered_fraction < 0.05,
        "a 500ms gap recovered {:.1}% of the reduction ({established}dB -> {after_gap}dB)",
        recovered_fraction * 100.0
    );
}

/// Release must **never** increase the reduction. The cascade's mid stage is a *signed* trim and is
/// frequently negative while ducking, so releasing each stage independently toward zero drove a
/// negative mid stage upward - which adds reduction, ducking the Bed harder at the exact moment the
/// commentator stopped (+0.6dB on real commentary). Release now collapses the cascade into one
/// value first, making recovery monotonic by construction.
#[test]
fn release_never_adds_reduction_even_with_a_negative_mid_stage() {
    let mut engine = engine_with(AutomixEngineConfig::default());

    // Bed momentary far quieter than its short-term reading drives the mid stage negative.
    let negative_mid = LoudnessSnapshot {
        bed_short_term_lufs: -20.0,
        bed_momentary_lufs: -30.0,
        dialogue_short_term_lufs: -26.0,
    };
    run(&mut engine, 400, negative_mid, true, false);
    assert!(
        engine.contributions().mid_db < -0.5,
        "setup failed: expected a negative mid stage, got {:?}",
        engine.contributions()
    );

    // Now release, watching every tick.
    let mut previous = engine.current_gain_reduction_db();
    for _ in 0..200 {
        let value = engine
            .process_tick(snapshot(-20.0, -100.0), false, false, TICK)
            .gain_reduction_db;
        assert!(
            value <= previous + 1e-9,
            "reduction rose during release: {previous}dB -> {value}dB"
        );
        previous = value;
    }
}

/// Release must ease in rather than step. A hard freeze followed by a normal release produced a
/// discontinuity in the *rate* of change - measured as exactly 0.000dB/tick for 0.8s and then
/// -0.48dB/tick - which is audible as "nothing happens, then it moves".
#[test]
fn release_eases_in_rather_than_starting_abruptly() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    run(&mut engine, 400, snapshot(-20.0, -26.0), true, false);

    let quiet = snapshot(-20.0, -100.0);
    let mut values = vec![engine.current_gain_reduction_db()];
    for _ in 0..25 {
        values.push(engine.process_tick(quiet, false, false, TICK).gain_reduction_db);
    }
    let step = |i: usize| values[i] - values[i + 1];

    let first = step(0);
    let later = step(15);
    assert!(first > 0.0, "release must actually begin, not freeze: first step {first}dB");
    assert!(
        first < later * 0.25,
        "release should start far slower than it ends - first step {first}dB vs {later}dB later"
    );
}

/// Collapsing the cascade at the start of a release must preserve the gain exactly - the fold is
/// a bookkeeping change, not an audible one.
#[test]
fn collapsing_the_cascade_into_one_stage_does_not_move_the_gain() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let snap = snapshot(-20.0, -26.0);
    run(&mut engine, 400, snap, true, false);
    for _ in 0..200 {
        engine.process_fast(0.0, -26.0, 0.0013);
    }
    let before = engine.current_gain_reduction_db();
    let contributions = engine.contributions();
    assert!(contributions.fast_db > 0.5, "setup: expected an engaged fast stage, got {contributions:?}");

    // First release tick: the stages fold into one, but the total must be unchanged beyond the
    // tiny amount this tick's own (heavily eased-in) release accounts for.
    let after = engine
        .process_tick(snapshot(-20.0, -100.0), false, false, TICK)
        .gain_reduction_db;
    assert!(
        (before - after) < 0.05,
        "the fold moved the gain: {before}dB -> {after}dB"
    );
}

#[test]
fn interview_passthrough_recovers_faster_than_an_ordinary_release() {
    let config = AutomixEngineConfig::default();
    let quiet = snapshot(-20.0, -100.0);

    let mut ordinary = engine_with(config);
    let start = run(&mut ordinary, 400, snapshot(-20.0, -26.0), true, false);
    let ordinary_after = run(&mut ordinary, 20, quiet, false, false);

    let mut interview = engine_with(config);
    let interview_start = run(&mut interview, 400, snapshot(-20.0, -26.0), true, false);
    let interview_after = run(&mut interview, 20, quiet, false, true);

    assert!((start - interview_start).abs() < 1e-9, "both must start from the same reduction");
    assert!(
        interview_after < ordinary_after,
        "interview recovered to {interview_after}dB, ordinary release only to {ordinary_after}dB"
    );
}

/// After a gap the mixer must re-engage quickly, but **without** an instantaneous jump.
///
/// The feedback design achieved the first by seeding the envelope on voice onset, which produced a
/// ~19dB step in a single tick. Feed-forward needs no seed - the dry levels are already correct -
/// and the cascade covers the onset with its faster stages, so the gain climbs quickly but
/// continuously. This test pins both halves of that: fast re-engagement, no step.
#[test]
fn re_engages_quickly_after_a_gap_without_an_instantaneous_jump() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let snap = snapshot(-20.0, -26.0); // 9 dB required
    let established = run(&mut engine, 400, snap, true, false);
    assert!(established > 1.0);

    let released = run(&mut engine, 600, snapshot(-20.0, -100.0), false, false);
    assert!(released < 1.0, "expected a full release during the gap");

    // Step back into speech, watching every tick.
    let mut previous = released;
    let mut max_step: f64 = 0.0;
    let mut after_300ms = 0.0;
    for tick in 0..3 {
        let value = engine.process_tick(snap, true, false, TICK).gain_reduction_db;
        max_step = max_step.max((value - previous).abs());
        previous = value;
        if tick == 2 {
            after_300ms = value;
        }
    }

    assert!(
        after_300ms > established * 0.5,
        "expected the cascade to recover past half of {established}dB within 300ms, got {after_300ms}dB"
    );
    assert!(
        max_step < established * 0.75,
        "re-engagement stepped {max_step}dB in one tick - it should climb continuously"
    );
}

// ---------------------------------------------------------------------------
// Live reconfiguration
// ---------------------------------------------------------------------------

#[test]
fn set_config_applies_a_lowered_ceiling_live() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    run(&mut engine, 2000, snapshot(-10.0, -90.0), true, false);

    engine.set_config(AutomixEngineConfig { max_gain_reduction_db: 6.0, ..Default::default() });
    let after = run(&mut engine, 400, snapshot(-10.0, -90.0), true, false);
    assert!(after <= 6.0 + 1e-6, "ceiling not applied: {after}dB");
}
