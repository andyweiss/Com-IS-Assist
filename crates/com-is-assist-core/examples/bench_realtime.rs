//! Measures what one audio block actually costs, split by component, against the real-time budget.
//!
//! Run with `cargo run --release --example bench_realtime -- [block_frames]`.
//!
//! The question this answers is not "how fast is the DSP on average" but "what is the worst block",
//! because a real-time host is killed by the worst one, not the mean. The VAD in particular does
//! nothing at all for most blocks and then runs a whole ONNX inference on one of them.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor};
use com_is_assist_core::voice_activity::{BackgroundVad, VoiceActivityConfig};
use std::time::Instant;

const RATE: u32 = 48_000;
const BED_CHANNELS: usize = 6;
const SECONDS: usize = 10;

fn sine(freq: f64, amp: f32, frames: usize, phase: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| amp * (2.0 * std::f64::consts::PI * freq * (i + phase) as f64 / RATE as f64).sin() as f32)
        .collect()
}

struct Stats {
    label: &'static str,
    samples: Vec<f64>,
}

impl Stats {
    fn new(label: &'static str) -> Self {
        Self { label, samples: Vec::new() }
    }
    fn report(&mut self, budget_us: f64) {
        self.samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = self.samples.len();
        let mean = self.samples.iter().sum::<f64>() / n as f64;
        let p50 = self.samples[n / 2];
        let p999 = self.samples[(n as f64 * 0.999) as usize];
        let max = *self.samples.last().unwrap();
        println!(
            "{:<26} mean {:7.1}us ({:5.1}%)  p50 {:7.1}us  p99.9 {:8.1}us  max {:8.1}us ({:6.1}% of budget)",
            self.label,
            mean,
            100.0 * mean / budget_us,
            p50,
            p999,
            max,
            100.0 * max / budget_us
        );
    }
}

fn main() {
    let block: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(64);
    let budget_us = block as f64 / RATE as f64 * 1e6;
    let blocks = SECONDS * RATE as usize / block;
    println!("block {block} frames @ {RATE}Hz -> {budget_us:.1}us of real time per block, {blocks} blocks\n");

    let mut processor =
        AutomixProcessor::new(BED_CHANNELS as u32, RATE, AutomixEngineConfig::default(), 0.1).expect("processor");
    let mut com_vad = BackgroundVad::new(RATE, VoiceActivityConfig::default(), true).expect("com vad");
    let mut is_vad = BackgroundVad::new(RATE, VoiceActivityConfig::default(), false).expect("is vad");

    let mut s_feed = Stats::new("  feed_dialogue");
    let mut s_bed = Stats::new("  process_bed");
    let mut s_ctl = Stats::new("  control step (tick)");
    let mut s_com = Stats::new("COM VAD feed");
    let mut s_is = Stats::new("IS VAD feed");
    let mut s_wrap = Stats::new("wrapper buffer shuffling");
    let mut s_all = Stats::new("TOTAL per block");

    for b in 0..blocks {
        let phase = b * block;
        let dialogue_planar = sine(700.0, 0.2, block, phase);
        let bed_planar: Vec<Vec<f32>> = (0..BED_CHANNELS).map(|_| sine(400.0, 0.4, block, phase)).collect();

        let t_all = Instant::now();

        // What the VST3 wrapper does to get from planar to the core's interleaved form.
        let t = Instant::now();
        let mut interleaved_bed = vec![0.0f32; block * BED_CHANNELS];
        for frame in 0..block {
            for ch in 0..BED_CHANNELS {
                interleaved_bed[frame * BED_CHANNELS + ch] = bed_planar[ch][frame];
            }
        }
        let dialogue: Vec<f32> = dialogue_planar[..block].to_vec();
        let mono_bed: Vec<f32> = interleaved_bed
            .chunks_exact(BED_CHANNELS)
            .map(|f| f.iter().sum::<f32>() / BED_CHANNELS as f32)
            .collect();
        s_wrap.samples.push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        com_vad.feed(&dialogue);
        s_com.samples.push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        is_vad.feed(&mono_bed);
        s_is.samples.push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        let _ = processor.feed_dialogue(&dialogue);
        s_feed.samples.push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        let _ = processor.process_bed(&mut interleaved_bed);
        s_bed.samples.push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        processor.maybe_run_control_step(com_vad.voice_active(), is_vad.voice_active());
        s_ctl.samples.push(t.elapsed().as_secs_f64() * 1e6);

        s_all.samples.push(t_all.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(&interleaved_bed);
    }

    for s in [&mut s_wrap, &mut s_com, &mut s_is, &mut s_feed, &mut s_bed, &mut s_ctl, &mut s_all] {
        s.report(budget_us);
    }
    // Expected to be large: this loop pushes ten seconds of audio in a fraction of a second, so
    // the producer outruns the worker by orders of magnitude. A host delivers audio at real-time
    // rate, where the worker needs ~3% of a core to keep up - `BackgroundVad`'s own unit test
    // feeds at that rate and asserts nothing is dropped.
    println!(
        "\nVAD samples dropped: com {}, is {} - expected here; the bench is not real-time paced.",
        com_vad.dropped_samples(),
        is_vad.dropped_samples()
    );
}
