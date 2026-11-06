//! Compares how COM is measured for the ratio: the 3s short-term window (up to v2.2), or COM
//! loudness integrated only while voice is detected (`SpeechLoudness`, the processor's reading
//! since). Kept as the record of that decision and to re-check it on new material.
//!
//! `cargo run --release --example speech_gated -- <com.wav> <bed.wav>... [--start S] [--seconds N]
//!     [--com-gain dB]`
//!
//! Inputs are mono WAVs; several bed files are interleaved in the order given. The engine is run
//! directly (not through `AutomixProcessor`), so the loudness it is handed can be swapped per
//! variant while everything else - detector, guard band, gain loop - stays identical.
//!
//! Scored **per utterance** (a span of gated COM voice of at least 1.5s), which is how the result
//! is heard:
//! - **ratio error**: COM's integrated loudness over the utterance, minus the *ducked* Bed's over the
//!   same span, minus the target. Positive means the bed was ducked more than the target asks -
//!   the "too much gain reduction" complaint.
//! - **range**: how far the reduction moved within the utterance (max - min), and **travel**, the
//!   total movement per second - the "heavy movements within a sentence" complaint.

use com_is_assist_core::automix::{
    AutomixEngine, AutomixEngineConfig, KWeightedLoudnessMeter, LoudnessSnapshot, SpeechLoudness,
};
use com_is_assist_core::loudness::{BedLayout, Ebur128Meter};
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};

const BLOCK: usize = 64;
/// BS.1770's gating-block hop.
const HOP_SECONDS: f64 = 0.1;
const MIN_UTTERANCE_SECONDS: f64 = 1.5;
/// How many 100ms blocks the previous utterance counts for at the start of the next: 3s of speech.
const PRIOR_WEIGHT_BLOCKS: f64 = SpeechLoudness::PRIOR_WEIGHT_BLOCKS;

fn read_mono(path: &str, start: f64, seconds: f64) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "{path}: expected a mono file");
    let rate = spec.sample_rate;
    reader.seek((start * rate as f64) as u32).expect("seek");
    let take = (seconds * rate as f64) as usize;
    let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().take(take).map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => reader.samples::<i32>().take(take).map(|s| s.unwrap() as f32 * scale).collect(),
    };
    (samples, rate)
}

fn energy(lufs: f64) -> f64 {
    10f64.powf((lufs + 0.691) / 10.0)
}
fn lufs(energy: f64) -> f64 {
    -0.691 + 10.0 * energy.max(1e-30).log10()
}

/// BS.1770 integrated loudness of a run of momentary readings: absolute gate at -70, relative gate
/// 10 LU below the absolutely-gated mean.
fn integrated(momentary: &[f64]) -> Option<f64> {
    let above: Vec<f64> = momentary.iter().copied().filter(|l| *l > -70.0).collect();
    if above.is_empty() {
        return None;
    }
    let ungated = lufs(above.iter().map(|l| energy(*l)).sum::<f64>() / above.len() as f64);
    let gated: Vec<f64> = above.into_iter().filter(|l| *l > ungated - 10.0).collect();
    Some(lufs(gated.iter().map(|l| energy(*l)).sum::<f64>() / gated.len() as f64))
}

#[derive(Clone, Copy)]
enum Reference {
    /// COM short-term vs Bed short-term - the current design.
    ShortTerm,
    /// COM speech-gated vs Bed short-term.
    SpeechCom,
    /// COM speech-gated vs Bed integrated over the same voice-active spans.
    SpeechBoth,
}

struct Utterance {
    com: Vec<f64>,
    bed_out_energy: Vec<f64>,
    gr: Vec<f64>,
}

#[derive(Default)]
struct Score {
    errors: Vec<f64>,
    ranges: Vec<f64>,
    travel: f64,
    gr_sum: f64,
    voice_blocks: f64,
}

fn run(
    com: &[f32],
    bed: &[f32],
    layout: BedLayout,
    rate: u32,
    vad_flags: &[bool],
    config: AutomixEngineConfig,
    reference: Reference,
) -> Score {
    let channels = layout.channel_count() as usize;
    let mut com_meter = KWeightedLoudnessMeter::new(&Ebur128Meter::dialogue_channel_map(), rate);
    let mut bed_meter = KWeightedLoudnessMeter::new(layout.channel_map(), rate);
    let mut engine = AutomixEngine::new(config);
    let mut com_speech = SpeechLoudness::for_commentary();
    let mut bed_speech = SpeechLoudness::new(PRIOR_WEIGHT_BLOCKS, None);

    let dt = BLOCK as f64 / rate as f64;
    let hop_blocks = (HOP_SECONDS / dt).round() as usize;
    let mut score = Score::default();
    let mut utterance: Option<Utterance> = None;
    let mut previous_gr = 0.0;

    for (i, start) in (0..com.len() - BLOCK).step_by(BLOCK).enumerate() {
        com_meter.push_frames(&com[start..start + BLOCK]);
        bed_meter.push_frames(&bed[start * channels..(start + BLOCK) * channels]);
        // The same guard band the processor applies.
        let voice = vad_flags[i] && com_meter.momentary_loudness_db() >= config.com_gate_lufs;

        let hop = i % hop_blocks == 0;
        if hop {
            com_speech.push(com_meter.momentary_loudness_db(), voice);
            bed_speech.push(bed_meter.momentary_loudness_db(), voice);
        }

        let com_st = com_meter.short_term_loudness_db();
        let bed_st = bed_meter.short_term_loudness_db();
        let snapshot = match reference {
            Reference::ShortTerm => LoudnessSnapshot { bed_short_term_lufs: bed_st, dialogue_short_term_lufs: com_st },
            Reference::SpeechCom => LoudnessSnapshot {
                bed_short_term_lufs: bed_st,
                dialogue_short_term_lufs: com_speech.level_lufs().unwrap_or(com_st),
            },
            Reference::SpeechBoth => LoudnessSnapshot {
                bed_short_term_lufs: bed_speech.level_lufs().unwrap_or(bed_st),
                dialogue_short_term_lufs: com_speech.level_lufs().unwrap_or(com_st),
            },
        };
        let gr = engine.step(snapshot, voice, false, dt).gain_reduction_db;

        if voice {
            score.gr_sum += gr;
            score.voice_blocks += 1.0;
            score.travel += (gr - previous_gr).abs();
            let u = utterance.get_or_insert_with(|| Utterance { com: vec![], bed_out_energy: vec![], gr: vec![] });
            u.gr.push(gr);
            if hop {
                u.com.push(com_meter.momentary_loudness_db());
                u.bed_out_energy.push(energy(bed_meter.momentary_loudness_db()) * 10f64.powf(-gr / 10.0));
            }
        } else if let Some(u) = utterance.take() {
            if u.com.len() as f64 * HOP_SECONDS >= MIN_UTTERANCE_SECONDS {
                if let Some(com_level) = integrated(&u.com) {
                    let bed_out = lufs(u.bed_out_energy.iter().sum::<f64>() / u.bed_out_energy.len() as f64);
                    score.errors.push(com_level - bed_out - config.target_ratio_lu);
                    let (lo, hi) = u.gr.iter().fold((f64::MAX, f64::MIN), |(lo, hi), g| (lo.min(*g), hi.max(*g)));
                    score.ranges.push(hi - lo);
                }
            }
        }
        previous_gr = gr;
    }
    score
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn report(label: &str, mut s: Score, rate: u32) {
    s.errors.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s.ranges.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.errors.len() as f64;
    let seconds = s.voice_blocks * BLOCK as f64 / rate as f64;
    println!(
        "  {label:44} {:3} utt  error mean {:+5.1} |err| {:4.1}  p10/p90 {:+5.1}/{:+5.1} LU   \
         GR mean {:4.1} dB   range p50/p90 {:4.1}/{:4.1} dB   travel {:4.2} dB/s",
        s.errors.len(),
        s.errors.iter().sum::<f64>() / n,
        s.errors.iter().map(|e| e.abs()).sum::<f64>() / n,
        pct(&s.errors, 0.1),
        pct(&s.errors, 0.9),
        s.gr_sum / s.voice_blocks,
        pct(&s.ranges, 0.5),
        pct(&s.ranges, 0.9),
        s.travel / seconds,
    );
}

fn main() {
    let mut files = Vec::new();
    let (mut start, mut seconds, mut com_gain_db) = (0.0, 600.0, 0.0);
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--start" => start = it.next().and_then(|v| v.parse().ok()).expect("--start S"),
            "--seconds" => seconds = it.next().and_then(|v| v.parse().ok()).expect("--seconds N"),
            "--com-gain" => com_gain_db = it.next().and_then(|v| v.parse().ok()).expect("--com-gain dB"),
            _ => files.push(arg),
        }
    }
    assert!(files.len() >= 3, "usage: speech_gated <com.wav> <bed.wav>... (2 or 6 bed files)");

    let (com_raw, rate) = read_mono(&files[0], start, seconds);
    let bed_channels: Vec<Vec<f32>> = files[1..].iter().map(|p| read_mono(p, start, seconds).0).collect();
    let layout = BedLayout::from_channel_count(bed_channels.len() as u32).expect("bed layout");
    let channels = bed_channels.len();
    let n = bed_channels.iter().map(Vec::len).min().unwrap().min(com_raw.len());
    let gain = 10f32.powf(com_gain_db as f32 / 20.0);
    let com: Vec<f32> = com_raw[..n].iter().map(|s| s * gain).collect();
    let mut bed = vec![0.0_f32; n * channels];
    for (c, channel) in bed_channels.iter().enumerate() {
        for (f, s) in channel[..n].iter().enumerate() {
            bed[f * channels + c] = *s;
        }
    }

    let mut vad = SileroVad::new(rate, VoiceActivityConfig::default()).expect("vad");
    let vad_flags: Vec<bool> = (0..n - BLOCK)
        .step_by(BLOCK)
        .map(|s| {
            vad.feed(&com[s..s + BLOCK]).expect("feed");
            vad.voice_active()
        })
        .collect();

    println!("{:.0}s from {start:.0}s, {channels}-channel bed, COM {com_gain_db:+.0} dB, target 5 LU\n", n as f64 / rate as f64);
    let default = AutomixEngineConfig::default();
    let attack = |attack_seconds| AutomixEngineConfig { attack_seconds, ..default };
    let variants = [
        ("A  COM short-term, attack 2.5s (v2.2)", attack(2.5), Reference::ShortTerm),
        ("B  COM speech-gated, attack 2.5s", attack(2.5), Reference::SpeechCom),
        ("B' COM speech-gated, attack 1s (default)", default, Reference::SpeechCom),
        ("C  COM + Bed speech-gated, attack 2.5s", attack(2.5), Reference::SpeechBoth),
        ("C' COM + Bed speech-gated, attack 1s", default, Reference::SpeechBoth),
    ];
    for (label, config, reference) in variants {
        report(label, run(&com, &bed, layout, rate, &vad_flags, config, reference), rate);
    }
}
