//! The COM side of the ratio is the commentator's level **while speaking**, not the 3s average.
//!
//! The 3s short-term window over speech-with-gaps also averages in the pauses, so it reads COM too
//! low - asking for more reduction than the balance needs - and moves with every pause, so the
//! reduction moves within the sentence. These drive the real `AutomixProcessor` with speech-like
//! COM (bursts with gaps between them, the voice flag held through the gaps as the detector's
//! hangover does) against a steady Bed at the same level as the bursts.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor};
use com_is_assist_core::loudness::BedLayout;

const RATE: u32 = 48_000;
const BLOCK: usize = 64;

fn tone(i: usize, amp: f32) -> f32 {
    amp * (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / RATE as f32).sin()
}

/// Runs `seconds` of 1s-on/1s-off COM bursts against a steady Bed of the same level, voice flag
/// held throughout, and returns the reduction sampled once per block.
fn run(seconds: f64) -> Vec<f64> {
    let config = AutomixEngineConfig { com_gate_lufs: -70.0, ..AutomixEngineConfig::default() };
    let mut p = AutomixProcessor::with_layout(BedLayout::Mono, RATE, config);
    let blocks = (seconds * RATE as f64) as usize / BLOCK;
    let mut gr = Vec::with_capacity(blocks);
    let mut com = [0.0_f32; BLOCK];
    let mut bed = [0.0_f32; BLOCK];
    for b in 0..blocks {
        for i in 0..BLOCK {
            let n = b * BLOCK + i;
            let speaking = (n / RATE as usize).is_multiple_of(2);
            com[i] = if speaking { tone(n, 0.3) } else { 0.0 };
            bed[i] = tone(n, 0.3);
        }
        p.feed_dialogue(&com).unwrap();
        p.set_voice_activity(true, false);
        p.process_bed(&mut bed).unwrap();
        gr.push(p.applied_gain_reduction_db());
    }
    gr
}

/// COM speaks at the Bed's level, so the balance needs exactly the target ratio of reduction. The
/// 3s average, half of it silence, reads COM ~3 dB lower and would ask for ~3 dB more (7.76 dB was
/// measured with the old reading).
#[test]
fn ducks_against_the_speech_level_not_the_gap_diluted_average() {
    let gr = run(30.0);
    let target = AutomixEngineConfig::default().target_ratio_lu;
    let settled = gr[gr.len() - 1];
    assert!(
        (settled - target).abs() < 1.0,
        "COM speaks at the Bed's level, so {target} dB is needed; got {settled:.2} dB"
    );
    assert!(settled < target + 2.0, "{settled:.2} dB is the gap-diluted answer (~{} dB)", target + 3.0);
}

/// And it holds still through the gaps between words: the reference does not move in a pause, so
/// neither does the reduction. (Settled against a steady Bed, so what is left is the reference.)
#[test]
fn the_reduction_holds_steady_within_an_utterance() {
    let gr = run(30.0);
    let blocks_per_second = RATE as usize / BLOCK;
    let last_4s = &gr[gr.len() - 4 * blocks_per_second..];
    let (lo, hi) = last_4s.iter().fold((f64::MAX, f64::MIN), |(lo, hi), g| (lo.min(*g), hi.max(*g)));
    // 0.14 dB measured; the 3s-window reading this replaced moved 0.40 dB here.
    assert!(hi - lo < 0.25, "reduction moved {:.2} dB over 4s of speech-with-gaps", hi - lo);
}
