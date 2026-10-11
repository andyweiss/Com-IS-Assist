use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor};

const TICK: f64 = 0.1;

fn sine(freq: f64, amp: f32, rate: u32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32)
        .collect()
}

fn processor(gate_lufs: f64) -> AutomixProcessor {
    AutomixProcessor::new(
        1,
        48_000,
        AutomixEngineConfig { com_gate_lufs: gate_lufs, ..AutomixEngineConfig::default() },
        TICK,
    )
    .expect("valid config")
}

/// Quiet COM - crosstalk, say - must not be treated as the commentator talking, even when the
/// detector reports speech. This is the whole point of the guard band.
#[test]
fn quiet_com_is_gated_out_even_when_voice_is_reported() {
    let rate = 48_000;
    let quiet = sine(400.0, 0.001, rate, 4_800); // ~ -60 LUFS
    let mut p = processor(-40.0);

    for _ in 0..40 {
        p.feed_dialogue(&quiet).unwrap();
        let mut bed = sine(400.0, 0.3, rate, 4_800);
        p.process_bed(&mut bed).unwrap();
        p.maybe_run_control_step(true, false); // detector insists there IS voice
    }

    assert!(
        !p.com_voice_gated(),
        "COM at {:.1} LUFS should be gated out below a -40 LUFS guard band",
        p.dialogue_momentary_lufs()
    );
    assert_eq!(p.mix_state(), com_is_assist_core::automix::MixState::ReleaseToUnity);
}

/// The same signal above the guard band must pass through untouched.
#[test]
fn loud_com_passes_the_guard_band() {
    let rate = 48_000;
    let loud = sine(400.0, 0.3, rate, 4_800); // well above -40 LUFS
    let mut p = processor(-40.0);

    for _ in 0..40 {
        p.feed_dialogue(&loud).unwrap();
        let mut bed = sine(400.0, 0.3, rate, 4_800);
        p.process_bed(&mut bed).unwrap();
        p.maybe_run_control_step(true, false);
    }

    assert!(
        p.com_voice_gated(),
        "COM at {:.1} LUFS should pass a -40 LUFS guard band",
        p.dialogue_momentary_lufs()
    );
}

/// The default gate is no longer open at the bottom of its range (-32 LUFS since the defaults were
/// retuned), so "it does not interfere" became a claim worth testing rather than one that held
/// trivially. The signal here is deliberately well clear of the threshold - a commentator at a
/// normal working level, not one hovering at it - since a test sitting a dB or two above the gate
/// would fail on any future retune without that meaning anything was wrong.
#[test]
fn the_default_guard_band_does_not_gate_normal_programme() {
    let rate = 48_000;
    let normal = sine(400.0, 0.15, rate, 4_800);
    let mut p = processor(AutomixEngineConfig::default().com_gate_lufs);

    for _ in 0..40 {
        p.feed_dialogue(&normal).unwrap();
        let mut bed = sine(400.0, 0.3, rate, 4_800);
        p.process_bed(&mut bed).unwrap();
        p.maybe_run_control_step(true, false);
    }
    assert!(
        p.com_voice_gated(),
        "a commentator at {:.1} LUFS must pass the default {} LUFS gate",
        p.dialogue_momentary_lufs(),
        AutomixEngineConfig::default().com_gate_lufs
    );
}
