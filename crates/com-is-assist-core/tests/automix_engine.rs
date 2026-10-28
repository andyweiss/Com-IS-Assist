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

/// The reduction the feed-forward law implies for these dry levels: `target - (COM - BED)`.
/// Derived from the config rather than written in as a number, so retuning a shipped default
/// retunes what these tests expect instead of breaking them - which is exactly what happened when
/// the target ratio moved from 3 LU to 5.
fn required(config: &AutomixEngineConfig, bed_lufs: f64, dialogue_lufs: f64) -> f64 {
    config.target_ratio_lu - (dialogue_lufs - bed_lufs)
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

    // Bed at -20, COM at -26: COM is 6 LU *below* Bed, so the target ratio plus 6 dB is required.
    let snap = snapshot(-20.0, -26.0);
    let expected = required(&config, -20.0, -26.0);
    let settled = run(&mut engine, 400, snap, true, false);

    assert!(
        (settled - expected).abs() < 0.1,
        "expected exactly {expected}dB (target {} - ratio -6), got {settled}dB",
        config.target_ratio_lu
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

    // The Bed's short-term window implies `required(...)`; its momentary window sees a far louder
    // Bed (a surge) and would ask for much more.
    let conflicting = LoudnessSnapshot {
        bed_short_term_lufs: -20.0,
        bed_momentary_lufs: 10.0,
        dialogue_short_term_lufs: -26.0,
    };
    let settled = run(&mut engine, 600, conflicting, true, false);
    let contributions = engine.contributions();

    let slow_estimate = required(&config, -20.0, -26.0);
    assert!(contributions.mid_db.abs() <= 6.0 + 1e-6, "mid stage exceeded its authority: {contributions:?}");
    assert!(
        settled <= slow_estimate + 6.0 + 1e-6,
        "total {settled}dB exceeds slow estimate {slow_estimate}dB plus mid authority"
    );
}

fn with_times(attack_seconds: f64, release_seconds: f64) -> AutomixEngineConfig {
    AutomixEngineConfig { attack_seconds, release_seconds, ..Default::default() }
}

/// Ducks to a steady answer, then releases for `release_ticks` and returns where it got to.
fn duck_then_release(config: AutomixEngineConfig, release_ticks: usize) -> (f64, f64) {
    let snap = snapshot(-20.0, -26.0);
    let mut engine = engine_with(config);
    let ducked = run(&mut engine, 600, snap, true, false);
    let released = run(&mut engine, release_ticks, snap, false, false);
    (ducked, released)
}

/// Attack has to do something measurable.
///
/// Measured early (200ms) deliberately: by ~800ms both settings have essentially arrived, because
/// the mid stage rushes in to cover whatever the slow stage has not yet done. That masking is the
/// cascade working as intended - it is also why "how fast is it?" has to be asked of the first few
/// hundred milliseconds rather than of the settled value.
#[test]
fn a_shorter_attack_reacts_faster() {
    let snap = snapshot(-20.0, -26.0);
    let mut calm = engine_with(with_times(0.3, 3.0));
    let mut quick = engine_with(with_times(0.0375, 3.0));

    let calm_db = run(&mut calm, 2, snap, true, false);
    let quick_db = run(&mut quick, 2, snap, true, false);

    assert!(
        quick_db > calm_db * 1.5,
        "after 200ms a 37.5ms attack reached {quick_db}dB where a 300ms attack reached {calm_db}dB"
    );
}

/// Attack changes how quickly the mixer gets there, never where "there" is. (Feed-forward is what
/// makes that separation clean.)
#[test]
fn attack_changes_the_journey_not_the_destination() {
    let snap = snapshot(-20.0, -26.0);
    let mut calm = engine_with(with_times(0.3, 3.0));
    let mut quick = engine_with(with_times(0.0375, 3.0));

    let calm_db = run(&mut calm, 600, snap, true, false);
    let quick_db = run(&mut quick, 600, snap, true, false);

    let expected = required(&AutomixEngineConfig::default(), -20.0, -26.0);
    assert!((calm_db - quick_db).abs() < 0.1, "settled at {calm_db}dB vs {quick_db}dB");
    assert!((calm_db - expected).abs() < 0.1, "both should land on the dry-level answer {expected}dB, got {calm_db}dB");
}

/// The point of splitting the old Speed macro: Release must move recovery on its own. A longer
/// release holds more of the reduction after the same time...
#[test]
fn a_longer_release_recovers_more_slowly() {
    // 3s into the release - well past the ease-in, well before either has fully recovered.
    let (ducked, quick) = duck_then_release(with_times(0.15, 1.5), 30);
    let (_, slow) = duck_then_release(with_times(0.15, 6.0), 30);

    assert!(ducked > 2.0, "need a real reduction to release from, got {ducked}dB");
    assert!(slow > quick + 1.0, "after 3s a 6s release held {slow}dB, a 1.5s release {quick}dB");
}

/// ...and must not be affected by Attack, nor Attack by it. Under the old single macro these could
/// only move together.
#[test]
fn attack_and_release_are_independent() {
    let (_, release_after_quick_attack) = duck_then_release(with_times(0.0375, 3.0), 30);
    let (_, release_after_calm_attack) = duck_then_release(with_times(0.3, 3.0), 30);
    assert!(
        (release_after_quick_attack - release_after_calm_attack).abs() < 0.05,
        "attack changed the release: {release_after_quick_attack}dB vs {release_after_calm_attack}dB"
    );

    let snap = snapshot(-20.0, -26.0);
    let mut short_release = engine_with(with_times(0.15, 0.4));
    let mut long_release = engine_with(with_times(0.15, 12.0));
    let a = run(&mut short_release, 2, snap, true, false);
    let b = run(&mut long_release, 2, snap, true, false);
    assert!((a - b).abs() < 1e-9, "release changed the attack: {a}dB vs {b}dB");
}

/// Guards the ends of both ranges (Attack 20-600ms, Release 0.4-12s, as the wrappers expose
/// them). The extremes are the settings most likely to expose a numerical problem - the quickest
/// attack drives the fast stage's time constant below the sub-chunk step that advances it - so
/// they are checked for a finite, in-range result rather than trusted to behave because the
/// middle of the range does.
#[test]
fn the_time_extremes_stay_well_behaved() {
    let snap = snapshot(-20.0, -26.0);
    let expected = required(&AutomixEngineConfig::default(), -20.0, -26.0);
    for (attack, release) in [(0.02, 0.4), (0.02, 12.0), (0.6, 0.4), (0.6, 12.0)] {
        let mut engine = engine_with(with_times(attack, release));
        let db = run(&mut engine, 600, snap, true, false);
        assert!(db.is_finite(), "attack {attack}s / release {release}s produced {db}");
        assert!(
            (db - expected).abs() < 0.1,
            "attack {attack}s / release {release}s settled at {db}dB, expected the {expected}dB dry answer"
        );
        let released = run(&mut engine, 600, snap, false, false);
        assert!((0.0..=db).contains(&released), "release went out of bounds: {released}dB");
    }
}

/// Switching the fast stage off must take it out entirely - for an A/B comparison, anything left
/// over would be the thing being compared.
#[test]
fn the_fast_stage_can_be_switched_off() {
    let fast_contribution = |fast_stage: bool| {
        let mut engine = engine_with(AutomixEngineConfig { fast_stage, ..Default::default() });
        run(&mut engine, 50, snapshot(-20.0, -26.0), true, false);
        engine.set_state_from_vad(true, false);
        // A 6 dB Bed surge seen only by the fast detector, for 100ms of sub-chunks.
        for _ in 0..75 {
            engine.process_fast(-14.0, -26.0, 64.0 / 48_000.0);
        }
        engine.contributions().fast_db
    };
    assert!(fast_contribution(true) > 1.0, "the fast stage should catch a surge when on");
    assert_eq!(fast_contribution(false), 0.0, "the fast stage must contribute nothing when off");
}

/// Tolerance: a deviation inside the band moves neither trim stage, and one beyond it is acted on
/// minus the band - a soft threshold, so there is no jump at the band's edge.
#[test]
fn the_trim_stages_ignore_deviations_within_the_tolerance() {
    let fast_after_surge = |tolerance_lu: f64, surge_db: f64| {
        let mut engine = engine_with(AutomixEngineConfig { tolerance_lu, ..Default::default() });
        // Settled first, so the surge is the only deviation the fast stage sees.
        run(&mut engine, 600, snapshot(-20.0, -26.0), true, false);
        engine.set_state_from_vad(true, false);
        for _ in 0..750 {
            engine.process_fast(-20.0 + surge_db, -26.0, 64.0 / 48_000.0);
        }
        engine.contributions().fast_db
    };
    assert!(fast_after_surge(2.0, 1.5) < 1e-9, "a 1.5 dB surge is inside a 2 LU tolerance");
    assert!((fast_after_surge(2.0, 3.0) - 1.0).abs() < 0.05, "3 dB beyond a 2 LU band is 1 dB");
    assert!((fast_after_surge(0.0, 3.0) - 3.0).abs() < 0.05, "no tolerance takes all of it");

    // The mid stage likewise: a 400ms-window excursion inside the band leaves it at zero.
    let mut engine = engine_with(AutomixEngineConfig { tolerance_lu: 2.0, ..Default::default() });
    let snap = LoudnessSnapshot { bed_momentary_lufs: -18.5, ..snapshot(-20.0, -26.0) };
    run(&mut engine, 600, snap, true, false);
    assert!(engine.contributions().mid_db.abs() < 1e-6, "{:?}", engine.contributions());
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

/// Each stage must release at its **own** time constant, so the multi-loop character carries
/// through the recovery: the fast stage lets go first, the slow stage carries the tail.
#[test]
fn each_stage_releases_at_its_own_rate() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let snap = snapshot(-20.0, -26.0);
    run(&mut engine, 400, snap, true, false);
    for _ in 0..400 {
        engine.process_fast(0.0, -26.0, 0.0013);
    }
    let before = engine.contributions();
    assert!(before.slow_db > 1.0 && before.fast_db > 0.5, "setup: {before:?}");

    // Half a second of recovery - long against the fast stage's constant, short against the slow's.
    let quiet = snapshot(-20.0, -100.0);
    for _ in 0..5 {
        engine.process_tick(quiet, false, false, TICK);
        for _ in 0..77 {
            engine.process_fast(0.0, -100.0, 0.0013);
        }
    }
    let after = engine.contributions();

    let fast_remaining = after.fast_db / before.fast_db;
    let slow_remaining = after.slow_db / before.slow_db;
    assert!(
        fast_remaining < slow_remaining,
        "the fast stage should let go first: fast kept {fast_remaining:.3}, slow kept {slow_remaining:.3}"
    );
    assert!(slow_remaining > 0.5, "the slow stage should still be carrying the tail: {slow_remaining:.3}");
}

/// A negative mid trim must not raise the total as it returns to zero - it is a subtraction being
/// removed. It hands exactly that much to the slow stage as it goes, so the total stays monotonic
/// while the mid stage still visibly decays at its own rate (rather than being snapped to zero,
/// which looks on the meters like a stage having died).
#[test]
fn a_negative_mid_trim_decays_visibly_without_raising_the_total() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let negative_mid = LoudnessSnapshot {
        bed_short_term_lufs: -20.0,
        bed_momentary_lufs: -30.0,
        dialogue_short_term_lufs: -26.0,
    };
    run(&mut engine, 400, negative_mid, true, false);
    let before = engine.contributions();
    assert!(before.mid_db < -0.5, "setup: expected a negative mid stage, got {before:?}");

    // One tick in, the mid stage must have moved *partway* - not been snapped straight to zero.
    let quiet = snapshot(-20.0, -100.0);
    let mut previous = engine.current_gain_reduction_db();
    let after_one = engine.process_tick(quiet, false, false, TICK).gain_reduction_db;
    let mid_after_one = engine.contributions().mid_db;
    assert!(
        mid_after_one < 0.0 && mid_after_one > before.mid_db,
        "mid should decay gradually, not snap to zero: {before:?} -> {mid_after_one}"
    );
    assert!(after_one <= previous + 1e-9, "total rose on the first release tick");
    previous = after_one;

    for _ in 0..200 {
        let value = engine.process_tick(quiet, false, false, TICK).gain_reduction_db;
        assert!(value <= previous + 1e-9, "total rose during release: {previous} -> {value}");
        previous = value;
    }
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

// ---------------------------------------------------------------------------
// COM voice guard band
// ---------------------------------------------------------------------------

/// The guard band is applied in `AutomixProcessor` (it needs COM's measured level), so these
/// exercise the engine's half of the contract: the state machine simply believes whatever COM
/// flag it is handed, and the gate decides what that flag is.
#[test]
fn the_engine_follows_whatever_com_flag_it_is_given() {
    let mut engine = engine_with(AutomixEngineConfig::default());
    let snap = snapshot(-20.0, -26.0);

    // Gated off -> the mixer must behave exactly as if COM were silent.
    assert_eq!(
        engine.process_tick(snap, false, false, TICK).state,
        MixState::ReleaseToUnity
    );
    // Gated on -> normal ducking.
    assert_eq!(engine.process_tick(snap, true, false, TICK).state, MixState::DuckToTarget);
}

/// The guard band now ships **engaged** (-32 LUFS) rather than open at the bottom of its range.
/// That is a deliberate behaviour change, not an oversight: a gate parked at -70 protects nothing,
/// and bed crosstalk ducking the bed against its own bleed is the failure it exists to prevent. It
/// has to stay inside the range both wrappers expose and well below a commentator's own momentary
/// level, or it would start gating out real speech.
#[test]
fn the_guard_band_ships_engaged_and_inside_the_exposed_range() {
    let gate = AutomixEngineConfig::default().com_gate_lufs;
    assert!((-70.0..=-10.0).contains(&gate), "default gate {gate} LUFS is outside the exposed range");
    assert!(gate > -70.0, "a gate at the bottom of its range would protect nothing");
    assert!(gate <= -25.0, "default gate {gate} LUFS is high enough to risk gating real speech");
}
